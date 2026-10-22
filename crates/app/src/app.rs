use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

use eframe::egui::{self, Color32, Margin, RichText, Stroke};
use photo_copy_core::compare::StagingStatus;
use photo_copy_core::index::{Index, UpdateStats};
use photo_copy_core::meta::metadata_fields;
use photo_copy_core::opener::{open_path, reveal};
use photo_copy_core::trash_check::{self, check_trash, TrashedFile};
use photo_copy_core::verified_trash::{self, trash_verified, Candidate, Report};

use crate::browse::{
    file_manager, fit_size, paint_cover, BrowseRequest, BrowseView, Tex, Textures, THUMB_SIZE,
    VIEW_SIZE,
};
use crate::i18n::{self, tr, trf, Lang};
use crate::model::{
    build_index, metadata_differences, BatchReport, Difference, Folder, Model, Outcome, Side,
};
use crate::move_dialog::{DialogAction, MoveDialog};
use crate::paths::{format_age, format_size, home_dir, tilde};
use crate::theme;
use photo_copy_core::ops::TRASH_DIR;

const LIBRARY_KEY: &str = "library_root";
const STAGING_KEY: &str = "staging_root";

const GROUP_THUMB: f32 = 96.0;

/// The workflow, one step per screen: set the folders, remove duplicates, organize.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Folders,
    Duplicates,
    Organize,
}

/// The three places duplicates are looked for in the duplicates step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DupArea {
    Library,
    Staging,
    AgainstLibrary,
}

/// The check of staging's `.trash` against the destination: running, or its result.
enum TrashCheck {
    Running {
        rx: Receiver<Vec<TrashedFile>>,
        done: Arc<AtomicUsize>,
    },
    Done(Vec<TrashedFile>),
}

/// How a step is drawn in the stepper.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StepState {
    Done,
    /// Work left: shown in amber.
    Pending,
    NotStarted,
}

/// A trash request waiting for the user's confirmation. It opens on top of the
/// button that asked for it, so confirming is a second click in the same spot.
#[derive(Clone)]
struct Confirm {
    files: Vec<(Side, String)>,
    /// The button that asked for it.
    anchor: egui::Rect,
    /// Whether the confirm button already got keyboard focus (so Enter confirms).
    focused: bool,
}

impl Confirm {
    fn new(files: Vec<(Side, String)>, anchor: egui::Rect) -> Self {
        Confirm {
            files,
            anchor,
            focused: false,
        }
    }
}

/// The copies of one photo, open in the large preview of step 2.
struct DupPreview {
    files: Vec<(Side, String)>,
    /// For copies of the same photo whose metadata differs: the fields that differ,
    /// one value per file. `None` for identical copies.
    differences: Option<Vec<Difference>>,
}

impl DupPreview {
    fn new(files: Vec<(Side, String)>, same_image: bool, model: &Model) -> Self {
        let mut preview = DupPreview {
            files,
            differences: same_image.then(Vec::new),
        };
        preview.compare(model);
        preview
    }

    /// Read the metadata of the copies again (after one of them was removed).
    fn compare(&mut self, model: &Model) {
        let Some(differences) = &mut self.differences else {
            return;
        };
        let per_file: Vec<Vec<(String, String)>> = self
            .files
            .iter()
            .map(|(side, rel)| {
                let Some(folder) = model.folder(*side) else {
                    return Vec::new();
                };
                let path = folder.root.join(rel);
                let mut fields = metadata_fields(&path);
                if let Ok(meta) = std::fs::metadata(&path) {
                    fields.push((tr("File size").to_string(), format_size(meta.len())));
                }
                fields
            })
            .collect();
        *differences = metadata_differences(&per_file);
    }
}

/// What the user asked for in the duplicates list.
enum DupRequest {
    Trash(Confirm),
    Preview(DupPreview),
    /// Trash every staging file that is already in the destination.
    TrashAllInLibrary,
}

/// Trashing every staging file that is already in the destination, after
/// checking each pair on disk (see `verified_trash`).
enum Bulk {
    /// Waiting for the user's confirmation.
    Confirm(Vec<Candidate>),
    Running {
        rx: Receiver<io::Result<Report>>,
        done: Arc<AtomicUsize>,
        total: usize,
        cancel: Arc<AtomicBool>,
    },
    /// Shown until the user dismisses it.
    Finished {
        report: Report,
        total: usize,
        /// Set if the staging index could not be saved afterwards.
        save_error: Option<String>,
    },
}

/// Index build running on a worker thread.
struct Job {
    side: Side,
    rx: Receiver<io::Result<(Index, UpdateStats)>>,
}

pub struct PhotoCopyApp {
    model: Model,
    browse: BrowseView,
    step: Step,
    dup_area: DupArea,
    /// Thumbnails of the duplicate groups (the browser keeps its own per folder).
    dup_textures: Textures,
    job: Option<Job>,
    confirm: Option<Confirm>,
    dup_preview: Option<DupPreview>,
    move_dialog: Option<MoveDialog>,
    /// Result of the last batch move, shown until the user dismisses it.
    batch_report: Option<BatchReport>,
    /// Destination folder used by the last move, offered again as the starting point.
    last_move_dir: String,
    bulk: Option<Bulk>,
    trash_check: Option<TrashCheck>,
    /// The staging `.trash` screen is shown instead of the current step.
    trash_open: bool,
    /// Thumbnails of trashed files (kept apart from the duplicates previews).
    trash_textures: Textures,
    /// A trashed file open in the large view.
    trash_preview: Option<TrashedFile>,
    status: String,
}

impl PhotoCopyApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let mut app = PhotoCopyApp {
            model: Model::default(),
            browse: BrowseView::new(&cc.egui_ctx),
            step: Step::Folders,
            dup_area: DupArea::Library,
            dup_textures: Textures::new(&cc.egui_ctx),
            job: None,
            confirm: None,
            dup_preview: None,
            move_dialog: None,
            batch_report: None,
            last_move_dir: String::new(),
            bulk: None,
            trash_check: None,
            trash_open: false,
            trash_textures: Textures::new(&cc.egui_ctx),
            trash_preview: None,
            status: tr("Choose the destination and staging folders.").to_string(),
        };
        if let Some(storage) = cc.storage {
            for (side, key) in [(Side::Library, LIBRARY_KEY), (Side::Staging, STAGING_KEY)] {
                if let Some(path) = storage.get_string(key).map(PathBuf::from) {
                    if path.is_dir() {
                        app.select_folder(side, path);
                    }
                }
            }
        }
        if app.model.folders_ready() {
            app.step = Step::Duplicates;
        }
        app
    }

    fn select_folder(&mut self, side: Side, path: PathBuf) {
        match self.model.set_root(side, path.clone()) {
            Ok(()) => {
                self.browse.folder_changed(side);
                self.trash_check = None;
                let count = self.model.folder(side).map_or(0, |f| f.index.files.len());
                self.status = trf(
                    "{side}: {path} ({n} files in saved index)",
                    &[
                        ("side", &side.label()),
                        ("path", &path.display()),
                        ("n", &count),
                    ],
                );
            }
            Err(e) => {
                self.status = trf(
                    "Cannot use {path}: {error}",
                    &[("path", &path.display()), ("error", &e)],
                )
            }
        }
    }

    fn start_index(&mut self, side: Side, ctx: &egui::Context) {
        let Some(folder) = self.model.folder(side) else {
            return;
        };
        let root = folder.root.clone();
        let index = folder.index.clone();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(build_index(&root, index));
            ctx.request_repaint();
        });
        self.status = trf(
            "Indexing {side} folder...",
            &[("side", &side.label().to_lowercase())],
        );
        self.job = Some(Job { side, rx });
    }

    fn poll_job(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        match job.rx.try_recv() {
            Err(TryRecvError::Empty) => self.job = Some(job),
            Err(TryRecvError::Disconnected) => {
                self.status = tr("Indexing stopped unexpectedly.").into()
            }
            Ok(Err(e)) => self.status = trf("Indexing failed: {error}", &[("error", &e)]),
            Ok(Ok((index, stats))) => {
                let skipped = match stats.skipped.first() {
                    None => String::new(),
                    Some(first) => trf(
                        "; {n} skipped, e.g. {first}",
                        &[("n", &stats.skipped.len()), ("first", first)],
                    ),
                };
                self.status = trf(
                    "{side} indexed: {files} files ({hashed} hashed, {reused} unchanged, {removed} removed){skipped}",
                    &[
                        ("side", &job.side.label()),
                        ("files", &index.files.len()),
                        ("hashed", &stats.hashed),
                        ("reused", &stats.reused),
                        ("removed", &stats.removed),
                        ("skipped", &skipped),
                    ],
                );
                self.model.apply_index(job.side, index);
            }
        }
    }

    fn steps_bar(&mut self, ui: &mut egui::Ui) {
        let dups = self.model.duplicate_status();
        let staging_files = self
            .model
            .folder(Side::Staging)
            .filter(|f| f.is_indexed())
            .map(|f| f.index.files.len());

        let (folders_state, folders_sub) = self.folders_summary();
        let (dups_state, dups_sub) = match (dups.remaining(), dups.known_remaining()) {
            (Some(0), _) => (StepState::Done, tr("No duplicates").to_string()),
            (_, n) if n > 0 => (StepState::Pending, trf("{n} left", &[("n", &n)])),
            _ => (StepState::NotStarted, tr("Not checked yet").to_string()),
        };
        let ready = dups.remaining() == Some(0);
        let organize_sub = match staging_files {
            Some(n) if ready => trf("Ready · {n} in staging", &[("n", &n)]),
            Some(n) => trf("{n} in staging", &[("n", &n)]),
            None => tr("Browse & move staging").to_string(),
        };

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let steps = [
                (
                    Step::Folders,
                    tr("Folders"),
                    folders_state,
                    folders_sub,
                    false,
                ),
                (
                    Step::Duplicates,
                    tr("Duplicates"),
                    dups_state,
                    dups_sub,
                    false,
                ),
                (
                    Step::Organize,
                    tr("Organize"),
                    StepState::NotStarted,
                    organize_sub,
                    ready,
                ),
            ];
            let mut previous_done = None;
            for (number, (step, title, state, sub, ready)) in steps.into_iter().enumerate() {
                if let Some(done) = previous_done {
                    connector(ui, done);
                }
                let current = self.step == step;
                let current = current && !self.trash_open;
                if step_button(ui, number + 1, title, &sub, state, current, ready).clicked() {
                    self.step = step;
                    self.trash_open = false;
                }
                previous_done = Some(state == StepState::Done);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                // Right to left, so the list is reversed to show EN first.
                for (i, lang) in Lang::ALL.into_iter().rev().enumerate() {
                    if i > 0 {
                        ui.label(RichText::new("·").small().color(theme::MUTED));
                    }
                    let color = if i18n::lang() == lang {
                        theme::STRONG
                    } else {
                        theme::MUTED
                    };
                    let label = egui::Label::new(RichText::new(lang.label()).small().color(color))
                        .sense(egui::Sense::click());
                    if ui
                        .add(label)
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        i18n::set_lang(lang);
                    }
                }
            });
        });
    }

    fn folders_summary(&self) -> (StepState, String) {
        let missing: Vec<&str> = [Side::Library, Side::Staging]
            .into_iter()
            .filter(|&side| !self.model.folder(side).is_some_and(|f| f.is_indexed()))
            .map(Side::label)
            .collect();
        let any_selected = [Side::Library, Side::Staging]
            .into_iter()
            .any(|side| self.model.folder(side).is_some());
        let outdated = [Side::Library, Side::Staging]
            .into_iter()
            .any(|side| self.model.folder(side).is_some_and(|f| f.needs_rescan()));
        match missing.as_slice() {
            [] if outdated => (StepState::Pending, tr("Rescan needed").to_string()),
            [] => (StepState::Done, tr("Both indexed").to_string()),
            [one] => (
                StepState::Pending,
                trf("{side} not indexed", &[("side", one)]),
            ),
            _ if any_selected => (StepState::Pending, tr("Index both folders").to_string()),
            _ => (StepState::NotStarted, tr("Choose folders").to_string()),
        }
    }

    /// One quiet line with both folders, shown outside step 1. Clicking it goes back
    /// to step 1, where folders are changed and rescanned.
    fn summary_line(&mut self, ui: &mut egui::Ui) {
        let home = home_dir();
        let inner = ui.horizontal(|ui| {
            ui.add_space(14.0);
            let quiet = |ui: &mut egui::Ui, text: RichText| {
                ui.add(egui::Label::new(text).selectable(false));
            };
            for side in [Side::Library, Side::Staging] {
                let folder = self.model.folder(side);
                let indexed = folder.is_some_and(|f| f.is_indexed());
                theme::dot(
                    ui,
                    if indexed {
                        theme::GREEN
                    } else {
                        theme::CONTROL
                    },
                );
                quiet(ui, theme::caps(side.label()));
                match folder {
                    Some(f) => {
                        quiet(
                            ui,
                            theme::mono(tilde(&f.root, home.as_deref()), theme::TEXT).small(),
                        );
                        let info = match f.indexed_at {
                            Some(at) => format!(
                                "· {}",
                                trf(
                                    "{files} files · {age}",
                                    &[
                                        ("files", &f.index.files.len()),
                                        ("age", &format_age(age_secs(at))),
                                    ],
                                )
                            ),
                            None => format!("· {}", tr("not indexed")),
                        };
                        quiet(ui, RichText::new(info).small().color(theme::MUTED));
                    }
                    None => quiet(
                        ui,
                        RichText::new(tr("not selected"))
                            .small()
                            .color(theme::MUTED),
                    ),
                }
                ui.add_space(24.0);
            }
        });
        let response = ui
            .interact(
                inner.response.rect,
                ui.scope_id().with("folders_summary"),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(tr("Change folders or rescan"));
        if response.clicked() {
            self.step = Step::Folders;
            self.trash_open = false;
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.job.is_some() {
                ui.spinner();
            }
            ui.label(RichText::new(&self.status).small().color(theme::MUTED));
        });
    }

    fn main_view(&mut self, ui: &mut egui::Ui) {
        if self.trash_open {
            self.trash_screen(ui);
            return;
        }
        let request = match self.step {
            Step::Folders => {
                self.folders_view(ui);
                None
            }
            Step::Duplicates => self.duplicates_view(ui),
            Step::Organize => {
                self.organize_view(ui);
                None
            }
        };
        match request {
            Some(DupRequest::Trash(confirm)) => self.confirm = Some(confirm),
            Some(DupRequest::Preview(preview)) => self.dup_preview = Some(preview),
            Some(DupRequest::TrashAllInLibrary) => {
                self.bulk = Some(Bulk::Confirm(self.model.bulk_trash_candidates()))
            }
            None => {}
        }
    }

    /// Step 1: one row per folder with its picker field, index status and button.
    fn folders_view(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.as_ref().map(|j| j.side);
        let (mut pick, mut index, mut open_trash) = (None, None, false);
        let width = ui.available_width().min(880.0);
        let margin = ((ui.available_width() - width) / 2.0).max(0.0);
        ui.add_space(48.0);
        ui.horizontal(|ui| {
            ui.add_space(margin);
            ui.vertical(|ui| {
                ui.set_width(width);
                ui.label(
                    RichText::new(tr("Set up the folders"))
                        .size(22.0)
                        .strong()
                        .color(theme::STRONG),
                );
                ui.label(
                    RichText::new(tr("Choose the destination (your organized library) and the staging folder (new photos), then index both so duplicates can be found."))
                        .color(theme::MUTED),
                );
                ui.add_space(22.0);
                row_separator(ui);
                for side in [Side::Library, Side::Staging] {
                    match folder_row(ui, &self.model, side, busy) {
                        Some(RowAction::Pick) => pick = Some(side),
                        Some(RowAction::Index) => index = Some(side),
                        None => {}
                    }
                    row_separator(ui);
                }
                ui.add_space(26.0);
                ui.horizontal(|ui| {
                    let ready = self.model.folders_ready();
                    let next = theme::primary(tr("Continue to Duplicates  →"));
                    if ui.add_enabled(ready, next).clicked() {
                        self.step = Step::Duplicates;
                    }
                    if !ready {
                        ui.label(
                            RichText::new(tr("Available once both folders are indexed."))
                                .small()
                                .color(theme::MUTED),
                        );
                    }
                });
                ui.add_space(40.0);
                row_separator(ui);
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(tr("Staging .trash")).strong());
                        let detail = match self.trash_not_in_library() {
                            Some(0) => tr("Last check: every trashed file has a copy in the destination.").to_string(),
                            Some(n) => trf("Last check: {n} trashed file(s) have no copy in the destination.", &[("n", &n)]),
                            None => tr("Check that every file you trashed from staging has a copy in the destination.").to_string(),
                        };
                        ui.label(RichText::new(detail).small().color(theme::MUTED));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = self.model.folders_ready();
                        let button = ui
                            .add_enabled(ready, egui::Button::new(tr("Check .trash...")).min_size(egui::vec2(112.0, 30.0)))
                            .on_disabled_hover_text(tr("Available once both folders are indexed."));
                        if button.clicked() {
                            open_trash = true;
                        }
                    });
                });
            });
        });
        if open_trash {
            self.trash_open = true;
            if self.trash_check.is_none() {
                self.start_trash_check(&ui.ctx().clone());
            }
        }
        if let Some(side) = pick {
            if let Some(picked) = rfd::FileDialog::new().pick_folder() {
                self.select_folder(side, picked);
            }
        }
        if let Some(side) = index {
            self.start_index(side, &ui.ctx().clone());
        }
    }

    fn duplicates_view(&mut self, ui: &mut egui::Ui) -> Option<DupRequest> {
        let status = self.model.duplicate_status();
        egui::Panel::left("duplicate_areas")
            .exact_size(230.0)
            .resizable(false)
            .frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 16)))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let areas = [
                    (DupArea::Library, tr("Destination"), status.library),
                    (DupArea::Staging, tr("Staging"), status.staging),
                    (DupArea::AgainstLibrary, tr("Staging vs destination"), status.against_library),
                ];
                for (area, label, count) in areas {
                    if nav_item(ui, label, count, self.dup_area == area).clicked() {
                        self.dup_area = area;
                    }
                }
                ui.add_space(16.0);
                egui::Frame::new().inner_margin(Margin::symmetric(16, 0)).show(ui, |ui| {
                    ui.label(
                        RichText::new(tr("Remove copies until all three are green, then move on to Organize."))
                            .small()
                            .color(theme::MUTED),
                    );
                });
            });

        let mut request = None;
        egui::Frame::new()
            .inner_margin(Margin::symmetric(24, 16))
            .show(ui, |ui| {
                if status.remaining() == Some(0) {
                    if all_clean(ui, &self.model) {
                        self.step = Step::Organize;
                    }
                    return;
                }
                request = match self.dup_area {
                    DupArea::Library => {
                        groups_view(ui, &self.model, &mut self.dup_textures, Side::Library)
                    }
                    DupArea::Staging => {
                        groups_view(ui, &self.model, &mut self.dup_textures, Side::Staging)
                    }
                    DupArea::AgainstLibrary => {
                        let mut run = false;
                        let busy = self.job.is_some();
                        let request = against_library_view(
                            ui,
                            &self.model,
                            &mut self.dup_textures,
                            busy,
                            &mut run,
                        );
                        if run {
                            self.model.run_check();
                            self.status = tr("Staging checked against the saved indexes.").into();
                        }
                        request
                    }
                };
            });
        request
    }

    fn organize_view(&mut self, ui: &mut egui::Ui) {
        let dups = self.model.duplicate_status();
        if dups.remaining() != Some(0) {
            let text = match dups.known_remaining() {
                0 => tr("Duplicates have not been checked yet.").to_string(),
                n => trf(
                    "{n} duplicate(s) still open. Batch moves skip files whose name already exists in the target.",
                    &[("n", &n)],
                ),
            };
            if banner(ui, &text, tr("Review duplicates")) {
                self.step = Step::Duplicates;
            }
        }
        egui::Frame::new()
            .inner_margin(Margin::symmetric(16, 10))
            .show(ui, |ui| match self.browse.show(ui, &self.model) {
                Some(BrowseRequest::Move(rels)) => self.open_move_dialog(rels),
                Some(BrowseRequest::Trash { rel, anchor }) => {
                    self.confirm = Some(Confirm::new(vec![(Side::Staging, rel)], anchor))
                }
                None => {}
            });
        if let Some(message) = self.browse.take_message() {
            self.status = message;
        }
    }

    fn open_move_dialog(&mut self, sources: Vec<String>) {
        if self.model.folder(Side::Library).is_none() {
            self.status = tr("Choose the destination folder first.").into();
            return;
        }
        if !sources.is_empty() {
            self.move_dialog = Some(MoveDialog::new(sources, self.last_move_dir.clone()));
        }
    }

    fn move_dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.move_dialog.as_mut() else {
            return;
        };
        match dialog.show(ctx, &self.model) {
            None => {}
            Some(DialogAction::Cancel) => self.move_dialog = None,
            Some(DialogAction::CreateFolder { parent, name }) => {
                match self.model.create_folder(&parent, &name) {
                    Ok(created) => dialog.enter(created),
                    Err(e) => dialog.error = Some(e.to_string()),
                }
            }
            Some(DialogAction::MoveMany { dest_dir }) => {
                let sources = dialog.sources().to_vec();
                match self.model.move_many(&sources, &dest_dir) {
                    Ok(report) => {
                        self.finish_batch(report, dest_dir);
                        self.move_dialog = None;
                    }
                    Err(e) => dialog.error = Some(e.to_string()),
                }
            }
            Some(DialogAction::Move { dest_dir, name }) => {
                let source = dialog.sources().first().cloned().unwrap_or_default();
                match self.model.move_to_destination(&source, &dest_dir, &name) {
                    Ok(moved) => {
                        self.status = match &moved.source_kept {
                            None => trf("Moved to {path}", &[("path", &display_path(&moved.dest))]),
                            Some(reason) => trf(
                                "Copied to {path}, but the original could not be removed ({reason}). Both copies exist.",
                                &[("path", &display_path(&moved.dest)), ("reason", reason)],
                            ),
                        };
                        self.last_move_dir = dest_dir;
                        self.browse.invalidate();
                        self.move_dialog = None;
                    }
                    Err(e) => dialog.error = Some(e.to_string()),
                }
            }
        }
    }

    fn finish_batch(&mut self, report: BatchReport, dest_dir: String) {
        let (moved, skipped, failed, kept) = (
            report.moved(),
            report.skipped(),
            report.failed(),
            report.kept(),
        );
        self.status = trf(
            "Moved {moved} file(s), skipped {skipped}, failed {failed}.",
            &[
                ("moved", &moved),
                ("skipped", &skipped),
                ("failed", &failed),
            ],
        );
        if kept > 0 {
            self.status += &trf(
                " {kept} copied but the original could not be removed.",
                &[("kept", &kept)],
            );
        }
        if moved + kept > 0 {
            self.last_move_dir = dest_dir;
        }
        self.browse.invalidate();
        if skipped + failed + kept > 0 || report.save_error.is_some() {
            self.batch_report = Some(report);
        }
    }

    fn batch_report_ui(&mut self, ctx: &egui::Context) {
        let Some(report) = &self.batch_report else {
            return;
        };
        let mut close = false;
        let response = egui::Modal::new(egui::Id::unique("batch_report")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading(tr("Batch move finished"));
            ui.label(trf(
                "Moved {moved}, skipped {skipped}, failed {failed}, copied but original not removed {kept}.",
                &[
                    ("moved", &report.moved()),
                    ("skipped", &report.skipped()),
                    ("failed", &report.failed()),
                    ("kept", &report.kept()),
                ],
            ));
            if let Some(error) = &report.save_error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    trf(
                        "The files were moved, but an index could not be saved: {error}",
                        &[("error", error)],
                    ),
                );
            }
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    for (rel, outcome) in &report.outcomes {
                        match outcome {
                            Outcome::Moved(_) => {}
                            Outcome::CopiedSourceKept { reason, .. } => {
                                ui.horizontal_wrapped(|ui| {
                                    ui.colored_label(theme::AMBER, tr("COPIED, ORIGINAL NOT REMOVED"));
                                    ui.monospace(rel);
                                    ui.weak(reason);
                                });
                            }
                            Outcome::Skipped(reason) => {
                                ui.horizontal_wrapped(|ui| {
                                    ui.colored_label(theme::AMBER, tr("SKIPPED"));
                                    ui.monospace(rel);
                                    ui.weak(reason);
                                });
                            }
                            Outcome::Failed(reason) => {
                                ui.horizontal_wrapped(|ui| {
                                    ui.colored_label(ui.visuals().error_fg_color, tr("FAILED"));
                                    ui.monospace(rel);
                                    ui.weak(reason);
                                });
                            }
                        }
                    }
                });
            ui.add_space(8.0);
            if ui.button(tr("OK")).clicked() {
                close = true;
            }
        });
        if close || response.should_close() {
            self.batch_report = None;
        }
    }

    /// The confirmation popover, placed so its confirm button covers the button
    /// that asked for it.
    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm.as_mut() else {
            return;
        };
        const MARGIN: i8 = 10;
        let id = egui::Id::unique("confirm_trash");
        let area = egui::Area::new(id)
            .kind(egui::UiKind::Modal)
            .sense(egui::Sense::hover())
            .order(egui::Order::Foreground)
            .interactable(true)
            .constrain(true)
            .fixed_pos(confirm.anchor.min - egui::Vec2::splat(MARGIN as f32));
        let frame = egui::Frame::popup(&ctx.global_style()).inner_margin(Margin::same(MARGIN));
        let mut decision = None;
        let response = egui::Modal::new(id)
            .area(area)
            .frame(frame)
            .backdrop_color(Color32::from_black_alpha(60))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let size = confirm.anchor.size();
                    let ok = ui.add(theme::danger(tr("Move to .trash")).min_size(size));
                    if !confirm.focused {
                        ok.request_focus();
                        confirm.focused = true;
                    }
                    if ok.clicked() {
                        decision = Some(true);
                    }
                    if ui
                        .add(egui::Button::new(tr("Cancel")).min_size(size))
                        .clicked()
                    {
                        decision = Some(false);
                    }
                });
                ui.add_space(6.0);
                for (side, rel) in &confirm.files {
                    ui.label(theme::mono(format!("{}: {rel}", side.label()), theme::TEXT).small());
                }
                let note = if confirm.files.len() == 1 {
                    tr("The file is moved to the .trash folder inside that folder, not deleted.")
                } else {
                    tr("The files are moved to the .trash folder inside their folder, not deleted.")
                };
                ui.label(RichText::new(note).small().color(theme::MUTED));
            });
        if response.should_close() {
            decision.get_or_insert(false);
        }
        match decision {
            Some(true) => {
                let files = self.confirm.take().map(|c| c.files).unwrap_or_default();
                self.trash_files(&files);
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    /// Move `files` to their folder's `.trash`; a failure does not stop the others.
    fn trash_files(&mut self, files: &[(Side, String)]) {
        let mut moved = Vec::new();
        let mut errors = Vec::new();
        for (side, rel) in files {
            match self.model.trash(*side, rel) {
                Ok(dest) => {
                    self.browse.file_removed(*side, rel);
                    moved.push(dest)
                }
                Err(e) => errors.push(trf(
                    "Could not move {rel}: {error}",
                    &[("rel", rel), ("error", &e)],
                )),
            }
        }
        if !moved.is_empty() {
            self.browse.invalidate();
        }
        self.status = match (moved.as_slice(), errors.is_empty()) {
            (_, false) => errors.join("; "),
            ([dest], true) => trf("Moved to {path}", &[("path", &display_path(dest))]),
            (_, true) => trf("Moved {n} files to .trash", &[("n", &moved.len())]),
        };
        // A copy stays in the preview until it is gone from the index (a file that
        // was moved but whose index could not be saved is gone too).
        if let Some(preview) = &mut self.dup_preview {
            let model = &self.model;
            preview.files.retain(|(side, rel)| {
                model
                    .folder(*side)
                    .is_some_and(|f| f.index.files.contains_key(rel))
            });
            if preview.files.len() < 2 {
                self.dup_preview = None;
            } else if !moved.is_empty() {
                preview.compare(model);
            }
        }
    }

    /// Large preview of one duplicate with a trash button per copy and one for all.
    fn dup_preview_ui(&mut self, ctx: &egui::Context) {
        let Some(preview) = &self.dup_preview else {
            self.dup_textures.drop_previews_except(None);
            return;
        };
        // The copies show the same picture, so the first one is shown.
        let Some(path) = preview
            .files
            .first()
            .and_then(|(side, rel)| self.model.folder(*side).map(|f| f.root.join(rel)))
        else {
            self.dup_preview = None;
            return;
        };
        let tex = self.dup_textures.get(&path, VIEW_SIZE);
        let available = ctx.content_rect().size() * 0.8;
        let (mut close, mut request) = (false, None);
        let response = egui::Modal::new(egui::Id::unique("dup_preview")).show(ctx, |ui| {
            ui.set_min_width(available.x.min(900.0));
            let n = preview.files.len();
            ui.horizontal(|ui| {
                if preview.differences.is_some() {
                    ui.strong(trf(
                        "{n} copies of the same photo, metadata differs",
                        &[("n", &n)],
                    ));
                } else {
                    ui.strong(trf("{n} identical", &[("n", &n)]));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr("Close")).clicked() {
                        close = true;
                    }
                    let all = match preview.files.len() {
                        2 => tr("Trash both...").to_string(),
                        n => trf("Trash all {n}...", &[("n", &n)]),
                    };
                    let button = ui.add(theme::danger(all));
                    if button.clicked() {
                        request = Some(Confirm::new(preview.files.clone(), button.rect));
                    }
                });
            });
            ui.add_space(4.0);
            for (number, (side, rel)) in preview.files.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let button = ui.add(theme::danger(tr("Trash...")));
                    if button.clicked() {
                        request = Some(Confirm::new(vec![(*side, rel.clone())], button.rect));
                    }
                    if preview.differences.is_some() {
                        ui.label(RichText::new(format!("#{}", number + 1)).strong());
                    }
                    ui.label(theme::caps(side.label()));
                    ui.label(theme::mono(rel, theme::TEXT));
                });
            }
            let mut table_height = 0.0;
            if let Some(differences) = &preview.differences {
                ui.add_space(6.0);
                table_height = differences_table(ui, differences, n);
            }
            ui.separator();
            let rows = 40.0 + 28.0 * n as f32 + table_height;
            let max = egui::vec2(available.x, (available.y - rows).max(100.0));
            match &tex {
                Tex::Ready(handle) => {
                    ui.vertical_centered(|ui| {
                        ui.image((handle.id(), fit_size(handle.size_vec2(), max)));
                    });
                }
                Tex::Loading => {
                    ui.add_space(max.y / 2.0);
                    ui.vertical_centered(|ui| ui.spinner());
                    ui.add_space(max.y / 2.0);
                }
                Tex::Failed => {
                    ui.weak(tr("This file cannot be previewed here."));
                }
            }
        });
        if close || response.should_close() {
            self.dup_preview = None;
            self.dup_textures.drop_previews_except(None);
        }
        if request.is_some() {
            self.confirm = request;
        }
    }

    /// Trashed staging files without a copy in the destination, once checked.
    fn trash_not_in_library(&self) -> Option<usize> {
        match &self.trash_check {
            Some(TrashCheck::Done(files)) => {
                Some(files.iter().filter(|f| !f.copy.in_library()).count())
            }
            _ => None,
        }
    }

    fn start_trash_check(&mut self, ctx: &egui::Context) {
        let (Some(library), Some(staging)) = (
            self.model.folder(Side::Library),
            self.model.folder(Side::Staging),
        ) else {
            return;
        };
        let (library_root, library_index) = (library.root.clone(), library.index.clone());
        let staging_root = staging.root.clone();
        let done = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let (worker_done, ctx) = (done.clone(), ctx.clone());
        thread::spawn(move || {
            let library = trash_check::Folder {
                root: &library_root,
                index: &library_index,
            };
            let _ = tx.send(check_trash(&staging_root, library, &worker_done));
            ctx.request_repaint();
        });
        self.status = tr("Checking the staging .trash...").into();
        self.trash_check = Some(TrashCheck::Running { rx, done });
    }

    fn poll_trash_check(&mut self) {
        let Some(TrashCheck::Running { rx, .. }) = &self.trash_check else {
            return;
        };
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.status = tr("The .trash check stopped unexpectedly.").into();
                self.trash_check = None;
            }
            Ok(files) => {
                let missing = files.iter().filter(|f| !f.copy.in_library()).count();
                self.status = trf(
                    ".trash checked: {in} of {total} file(s) are in the destination, {out} are not.",
                    &[
                        ("in", &(files.len() - missing)),
                        ("total", &files.len()),
                        ("out", &missing),
                    ],
                );
                self.trash_check = Some(TrashCheck::Done(files));
            }
        }
    }

    /// Staging's `.trash` checked against the destination: how many trashed files
    /// have a copy there, and thumbnails of those that do not. Opened from step 1.
    fn trash_screen(&mut self, ui: &mut egui::Ui) {
        let (mut back, mut start, mut open) = (false, false, None);
        egui::Frame::new()
            .inner_margin(Margin::symmetric(24, 16))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    back = ui.button(tr("←  Back")).clicked();
                    ui.add_space(8.0);
                    section_title(ui, tr("Staging .trash"), "");
                    if matches!(self.trash_check, Some(TrashCheck::Done(_))) {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                            start = ui.button(tr("Check again")).clicked();
                        });
                    }
                });
                if let Some(staging) = self.model.folder(Side::Staging) {
                    let trash = staging.root.join(TRASH_DIR);
                    ui.label(theme::mono(tilde(&trash, home_dir().as_deref()), theme::MUTED).small());
                }
                if self
                    .model
                    .folder(Side::Library)
                    .is_some_and(|f| f.needs_rescan())
                {
                    ui.add_space(6.0);
                    rescan_note(ui, &[Side::Library]);
                }
                ui.add_space(10.0);
                match &self.trash_check {
                    None => {
                        empty_state(ui, tr("Not checked yet"), "", false);
                        ui.vertical_centered(|ui| {
                            start = ui.add(theme::primary(tr("Check .trash"))).clicked();
                        });
                    }
                    Some(TrashCheck::Running { done, .. }) => {
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(200));
                        let done = done.load(Ordering::Relaxed);
                        empty_state(
                            ui,
                            tr("Checking the staging .trash..."),
                            &trf("{n} file(s) read", &[("n", &done)]),
                            false,
                        );
                        ui.vertical_centered(|ui| ui.spinner());
                    }
                    Some(TrashCheck::Done(files)) => {
                        let missing: Vec<&TrashedFile> =
                            files.iter().filter(|f| !f.copy.in_library()).collect();
                        let summary = trf(
                            "{in} of {total} trashed file(s) have a copy in the destination · {out} do not",
                            &[
                                ("in", &(files.len() - missing.len())),
                                ("total", &files.len()),
                                ("out", &missing.len()),
                            ],
                        );
                        let color = if missing.is_empty() {
                            theme::GREEN
                        } else {
                            theme::AMBER
                        };
                        ui.label(RichText::new(summary).size(16.0).color(color));
                        ui.add_space(10.0);
                        if files.is_empty() {
                            empty_state(ui, tr("The staging .trash is empty"), "", true);
                        } else if missing.is_empty() {
                            empty_state(
                                ui,
                                tr("Every trashed file has a copy in the destination"),
                                "",
                                true,
                            );
                        } else {
                            ui.label(
                                RichText::new(tr("These trashed files have no copy in the destination. Click one to see it larger, right-click to open it or show it in the file manager."))
                                    .small()
                                    .color(theme::MUTED),
                            );
                            ui.add_space(8.0);
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);
                                        for file in missing {
                                            if trash_cell(ui, &mut self.trash_textures, file) {
                                                open = Some(file.clone());
                                            }
                                        }
                                    });
                                });
                        }
                    }
                }
            });
        if back {
            self.trash_open = false;
        }
        if start {
            self.start_trash_check(&ui.ctx().clone());
        }
        if open.is_some() {
            self.trash_preview = open;
        }
    }

    /// Large view of one trashed file.
    fn trash_preview_ui(&mut self, ctx: &egui::Context) {
        let Some(file) = &self.trash_preview else {
            self.trash_textures.drop_previews_except(None);
            return;
        };
        let tex = self.trash_textures.get(&file.path, VIEW_SIZE);
        let available = ctx.content_rect().size() * 0.8;
        let (mut close, mut action) = (false, None);
        let response = egui::Modal::new(egui::Id::unique("trash_preview")).show(ctx, |ui| {
            ui.set_min_width(available.x.min(900.0));
            ui.horizontal(|ui| {
                ui.strong(&file.rel);
                ui.weak(format_size(file.size));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr("Close")).clicked() {
                        close = true;
                    }
                    if ui
                        .button(trf("Show in {app}", &[("app", &file_manager())]))
                        .clicked()
                    {
                        action = Some(true);
                    }
                    if ui.button(tr("Open")).clicked() {
                        action = Some(false);
                    }
                });
            });
            ui.separator();
            let max = egui::vec2(available.x, (available.y - 60.0).max(100.0));
            match &tex {
                Tex::Ready(handle) => {
                    ui.vertical_centered(|ui| {
                        ui.image((handle.id(), fit_size(handle.size_vec2(), max)));
                    });
                }
                Tex::Loading => {
                    ui.add_space(max.y / 2.0);
                    ui.vertical_centered(|ui| ui.spinner());
                    ui.add_space(max.y / 2.0);
                }
                Tex::Failed => {
                    ui.weak(tr("This file cannot be previewed here."));
                }
            }
        });
        if let Some(reveal_it) = action {
            let path = file.path.clone();
            self.open_external(&path, reveal_it);
        }
        if close || response.should_close() {
            self.trash_preview = None;
        }
    }

    /// Open a file with its default app, or show it in the file manager.
    fn open_external(&mut self, path: &Path, reveal_it: bool) {
        let result = if reveal_it {
            reveal(path)
        } else {
            open_path(path)
        };
        if let Err(e) = result {
            self.status = trf(
                "Could not open {path}: {error}",
                &[("path", &path.display()), ("error", &e)],
            );
        }
    }

    fn start_bulk_trash(&mut self, candidates: Vec<Candidate>, ctx: &egui::Context) {
        let (Some(staging), Some(library)) = (
            self.model.folder(Side::Staging),
            self.model.folder(Side::Library),
        ) else {
            return;
        };
        let (staging_root, library_root) = (staging.root.clone(), library.root.clone());
        let total = candidates.len();
        let done = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (worker_done, worker_cancel, ctx) = (done.clone(), cancel.clone(), ctx.clone());
        thread::spawn(move || {
            let report = trash_verified(
                &staging_root,
                &library_root,
                &candidates,
                &worker_done,
                &worker_cancel,
            );
            let _ = tx.send(report);
            ctx.request_repaint();
        });
        self.status = tr("Checking staging files against the destination on disk...").into();
        self.bulk = Some(Bulk::Running {
            rx,
            done,
            total,
            cancel,
        });
    }

    fn poll_bulk(&mut self) {
        let Some(Bulk::Running { rx, total, .. }) = &self.bulk else {
            return;
        };
        let total = *total;
        let report = match rx.try_recv() {
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.status = tr("Trashing stopped unexpectedly.").into();
                self.bulk = None;
                return;
            }
            Ok(Err(e)) => {
                self.status = trf(
                    "Nothing was trashed: the log could not be created ({error}).",
                    &[("error", &e)],
                );
                self.bulk = None;
                return;
            }
            Ok(Ok(report)) => report,
        };
        let save_error = self
            .model
            .forget_staging_files(report.trashed())
            .err()
            .map(|e| e.to_string());
        if report.trashed_count() > 0 {
            self.browse.invalidate();
        }
        self.status = trf(
            "Moved {trashed} staging file(s) to .trash, kept {kept}, failed {failed}.",
            &[
                ("trashed", &report.trashed_count()),
                ("kept", &report.skipped()),
                ("failed", &report.failed()),
            ],
        );
        self.bulk = Some(Bulk::Finished {
            report,
            total,
            save_error,
        });
    }

    /// The confirmation, progress and result of trashing every staging file that
    /// is already in the destination.
    fn bulk_ui(&mut self, ctx: &egui::Context) {
        let Some(bulk) = &self.bulk else {
            return;
        };
        let id = egui::Id::unique("bulk_trash");
        let (mut close, mut start) = (false, false);
        match bulk {
            Bulk::Confirm(candidates) => {
                let response = egui::Modal::new(id).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading(trf(
                        "Trash {n} staging file(s) already in the destination?",
                        &[("n", &candidates.len())],
                    ));
                    ui.add_space(4.0);
                    ui.label(tr("Each staging file is first compared again with its destination copy, reading both files from disk. It is moved to .trash only if an identical copy still exists in the destination; otherwise it is kept and listed at the end."));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(tr("Files are moved to the .trash folder inside staging, not deleted. A log of every file is saved there too."))
                            .small()
                            .color(theme::MUTED),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        let ok = ui.add(theme::danger(tr("Check and move to .trash")));
                        ok.request_focus();
                        if ok.clicked() {
                            start = true;
                        }
                        if ui.button(tr("Cancel")).clicked() {
                            close = true;
                        }
                    });
                });
                close |= response.should_close();
            }
            Bulk::Running {
                done,
                total,
                cancel,
                ..
            } => {
                // Keep the progress bar moving while the worker runs.
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                let done = done.load(Ordering::Relaxed);
                let stopping = cancel.load(Ordering::Relaxed);
                egui::Modal::new(id).show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.heading(tr("Checking and moving to .trash..."));
                    ui.add_space(6.0);
                    let fraction = if *total == 0 {
                        1.0
                    } else {
                        done as f32 / *total as f32
                    };
                    ui.add(egui::ProgressBar::new(fraction).text(trf(
                        "{done} of {total}",
                        &[("done", &done), ("total", total)],
                    )));
                    ui.add_space(8.0);
                    let label = if stopping {
                        tr("Stopping...")
                    } else {
                        tr("Stop")
                    };
                    if ui
                        .add_enabled(!stopping, egui::Button::new(label))
                        .clicked()
                    {
                        cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            Bulk::Finished {
                report,
                total,
                save_error,
            } => {
                let response = egui::Modal::new(id).show(ctx, |ui| {
                    ui.set_width(560.0);
                    close = bulk_report(ui, report, *total, save_error.as_deref());
                });
                close |= response.should_close();
            }
        }
        if start {
            if let Some(Bulk::Confirm(candidates)) = self.bulk.take() {
                self.start_bulk_trash(candidates, ctx);
            }
        } else if close {
            self.bulk = None;
        }
    }
}

/// Body of the finished bulk trash dialog. Returns true when the user closes it.
fn bulk_report(ui: &mut egui::Ui, report: &Report, total: usize, save_error: Option<&str>) -> bool {
    ui.heading(tr("Trashing finished"));
    ui.label(trf(
        "Moved {trashed} to .trash, kept {kept}, failed {failed}.",
        &[
            ("trashed", &report.trashed_count()),
            ("kept", &report.skipped()),
            ("failed", &report.failed()),
        ],
    ));
    if report.cancelled {
        ui.colored_label(
            theme::AMBER,
            trf(
                "Stopped after {done} of {total} files; the rest were not touched.",
                &[("done", &report.outcomes.len()), ("total", &total)],
            ),
        );
    }
    let error = ui.visuals().error_fg_color;
    if let Some(log_error) = &report.log_error {
        ui.colored_label(
            error,
            trf(
                "The log could not be written, so the batch stopped: {error}",
                &[("error", log_error)],
            ),
        );
    }
    if let Some(save_error) = save_error {
        ui.colored_label(
            error,
            trf(
                "The files were moved, but an index could not be saved: {error}",
                &[("error", &save_error)],
            ),
        );
    }
    let outdated = report.outcomes.iter().any(|(_, o)| {
        matches!(
            o,
            verified_trash::Outcome::CopyMissing | verified_trash::Outcome::CopyChanged
        )
    });
    if outdated {
        ui.label(
            RichText::new(tr("Some destination files changed since the last index. Rescan the destination in step 1, then check again."))
                .color(theme::AMBER),
        );
    }
    let not_trashed: Vec<_> = report
        .outcomes
        .iter()
        .filter_map(|(rel, outcome)| bulk_reason(outcome).map(|r| (rel, r)))
        .collect();
    if !not_trashed.is_empty() {
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(260.0)
            .show(ui, |ui| {
                for (rel, (failed, reason)) in not_trashed {
                    ui.horizontal_wrapped(|ui| {
                        if failed {
                            ui.colored_label(error, tr("FAILED"));
                        } else {
                            ui.colored_label(theme::AMBER, tr("KEPT"));
                        }
                        ui.monospace(rel);
                        ui.weak(reason);
                    });
                }
            });
    }
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(tr("Log:")).small().color(theme::MUTED));
        ui.label(theme::mono(display_path(&report.log_path), theme::TEXT).small());
    });
    ui.add_space(8.0);
    let mut close = false;
    ui.horizontal(|ui| {
        if ui.button(tr("OK")).clicked() {
            close = true;
        }
        if ui
            .button(trf("Show log in {app}", &[("app", &file_manager())]))
            .clicked()
        {
            let _ = reveal(&report.log_path);
        }
    });
    close
}

/// Why a staging file was not trashed: `(failed, reason)`, or `None` if it was.
fn bulk_reason(outcome: &verified_trash::Outcome) -> Option<(bool, String)> {
    use verified_trash::Outcome;
    match outcome {
        Outcome::Trashed { .. } => None,
        Outcome::StagingMissing => Some((false, tr("the staging file no longer exists").into())),
        Outcome::CopyMissing => Some((false, tr("the destination copy no longer exists").into())),
        Outcome::CopyChanged => Some((
            false,
            tr("the destination copy is no longer identical").into(),
        )),
        Outcome::Failed(error) => Some((true, error.clone())),
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn age_secs(at: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(at)
        .map_or(0, |d| d.as_secs())
}

/// A check mark drawn with lines, so it does not depend on the font.
fn paint_check(painter: &egui::Painter, center: egui::Pos2, size: f32, color: Color32) {
    let s = size / 2.0;
    let points = [
        center + egui::vec2(-s * 0.75, 0.0),
        center + egui::vec2(-s * 0.2, s * 0.55),
        center + egui::vec2(s * 0.8, -s * 0.55),
    ];
    let stroke = Stroke::new((size / 6.0).max(1.5), color);
    painter.line_segment([points[0], points[1]], stroke);
    painter.line_segment([points[1], points[2]], stroke);
}

/// One step of the stepper: a numbered circle, the title and a status line.
fn step_button(
    ui: &mut egui::Ui,
    number: usize,
    title: &str,
    sub: &str,
    state: StepState,
    current: bool,
    ready: bool,
) -> egui::Response {
    let border = if current {
        theme::CONTROL
    } else if ready {
        theme::GREEN
    } else {
        Color32::TRANSPARENT
    };
    let inner = egui::Frame::new()
        .inner_margin(Margin::symmetric(12, 6))
        .corner_radius(theme::RADIUS)
        .fill(if current {
            theme::PANEL
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(1.0, border))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
                let painter = ui.painter();
                let center = rect.center();
                let digit = |color| {
                    painter.text(
                        center,
                        egui::Align2::CENTER_CENTER,
                        number.to_string(),
                        egui::FontId::proportional(13.0),
                        color,
                    );
                };
                match state {
                    StepState::Done => {
                        painter.circle_filled(center, 12.0, theme::GREEN);
                        paint_check(painter, center, 12.0, theme::BG);
                    }
                    StepState::Pending => {
                        painter.circle_filled(center, 12.0, theme::AMBER);
                        digit(theme::BG);
                    }
                    StepState::NotStarted if current => {
                        painter.circle_filled(center, 12.0, theme::TEXT);
                        digit(theme::BG);
                    }
                    StepState::NotStarted => {
                        let color = if ready {
                            theme::GREEN
                        } else {
                            Color32::from_rgb(0x48, 0x48, 0x4a)
                        };
                        painter.circle_stroke(center, 11.0, Stroke::new(2.0, color));
                        digit(if ready { theme::GREEN } else { theme::MUTED });
                    }
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    let title_color = if current { theme::STRONG } else { theme::TEXT };
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("{number} · {title}"))
                                .strong()
                                .color(title_color),
                        )
                        .selectable(false),
                    );
                    let sub_color = match state {
                        StepState::Done => theme::GREEN,
                        StepState::Pending => theme::AMBER,
                        StepState::NotStarted if ready => theme::GREEN,
                        StepState::NotStarted => theme::MUTED,
                    };
                    ui.add(
                        egui::Label::new(RichText::new(sub).small().color(sub_color))
                            .selectable(false),
                    );
                });
            });
        });
    ui.interact(
        inner.response.rect,
        ui.scope_id().with(("step", number)),
        egui::Sense::click(),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Line between two steps: green once the step before it is done.
fn connector(ui: &mut egui::Ui, done: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(40.0, 24.0), egui::Sense::hover());
    let color = if done {
        theme::GREEN
    } else {
        Color32::from_rgb(0x2c, 0x2c, 0x2e)
    };
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(2.0, color),
    );
}

/// What the user asked for in a folder row of step 1.
enum RowAction {
    Pick,
    Index,
}

fn row_separator(ui: &mut egui::Ui) {
    let rect = ui.available_rect_before_wrap();
    ui.painter()
        .hline(rect.x_range(), rect.top(), Stroke::new(1.0, theme::BORDER));
    ui.add_space(1.0);
}

/// Short description under the folder name in step 1.
fn side_hint(side: Side) -> &'static str {
    match side {
        Side::Library => tr("your library"),
        Side::Staging => tr("new photos"),
    }
}

/// One folder in step 1: name, picker field, index status and the index button.
fn folder_row(
    ui: &mut egui::Ui,
    model: &Model,
    side: Side,
    busy: Option<Side>,
) -> Option<RowAction> {
    let folder = model.folder(side);
    let indexed = folder.is_some_and(|f| f.is_indexed());
    let outdated = folder.is_some_and(|f| f.needs_rescan());
    let indexing = busy == Some(side);
    let enabled = busy.is_none();
    let mut action = None;
    ui.horizontal(|ui| {
        ui.set_min_height(66.0);
        ui.spacing_mut().item_spacing.x = 14.0;
        let dot = match (indexed, outdated) {
            (_, true) => theme::AMBER,
            (true, false) => theme::GREEN,
            (false, _) => theme::CONTROL,
        };
        theme::dot(ui, dot);
        ui.allocate_ui_with_layout(
            egui::vec2(100.0, 34.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(side.label()).strong());
                ui.label(RichText::new(side_hint(side)).small().color(theme::MUTED));
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let button_size = egui::vec2(112.0, 30.0);
            if indexing {
                ui.allocate_ui_with_layout(
                    button_size,
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| ui.spinner(),
                );
            } else if folder.is_some() {
                let (button, help) = if outdated {
                    (
                        theme::primary(tr("Rescan")),
                        tr("This index was built by an older version. Rescan once to also find copies of the same photo or video whose metadata differs; files without that information are read again."),
                    )
                } else if indexed {
                    (
                        egui::Button::new(tr("Rescan")),
                        tr("Look for new, changed and removed files and update the index. Only changed files are read again."),
                    )
                } else {
                    (
                        theme::primary(tr("Index files")),
                        tr("Read every file once so duplicates can be found. Later rescans only read changed files."),
                    )
                };
                let response = ui
                    .add_enabled(enabled, button.min_size(button_size))
                    .on_hover_text(help);
                if response.clicked() {
                    action = Some(RowAction::Index);
                }
            } else {
                ui.add_space(button_size.x);
            }
            ui.allocate_ui_with_layout(
                egui::vec2(96.0, 34.0),
                egui::Layout::top_down(egui::Align::Max),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    match folder {
                        _ if indexing => {
                            ui.label(RichText::new(tr("Indexing…")).color(theme::DIM));
                        }
                        Some(f) => match f.indexed_at {
                            Some(at) => {
                                ui.label(
                                    RichText::new(trf("{n} files", &[("n", &f.index.files.len())]))
                                        .color(theme::DIM),
                                );
                                if f.needs_rescan() {
                                    ui.label(
                                        RichText::new(tr("Rescan needed"))
                                            .small()
                                            .color(theme::AMBER),
                                    );
                                } else {
                                    ui.label(
                                        RichText::new(format_age(age_secs(at)))
                                            .small()
                                            .color(theme::MUTED),
                                    );
                                }
                            }
                            None => {
                                ui.label(RichText::new(tr("Not indexed")).color(theme::AMBER));
                            }
                        },
                        None => {}
                    }
                },
            );
            let root = folder.map(|f| f.root.as_path());
            if picker_field(ui, root, side, enabled).clicked() {
                action = Some(RowAction::Pick);
            }
        });
    });
    action
}

/// The folder path as a clickable field (like a file input): clicking it opens the
/// folder dialog. Empty folders show a dashed field with a prompt.
fn picker_field(
    ui: &mut egui::Ui,
    root: Option<&Path>,
    side: Side,
    enabled: bool,
) -> egui::Response {
    let size = egui::vec2(ui.available_width().max(160.0), 34.0);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let hovered = enabled && response.hovered();
    let painter = ui.painter();
    let radius = egui::CornerRadius::same(theme::RADIUS);
    let fill = match (hovered, root) {
        (true, _) => Color32::from_rgb(0x1c, 0x1c, 0x1e),
        (false, Some(_)) => Color32::from_rgb(0x14, 0x14, 0x16),
        (false, None) => theme::BG,
    };
    painter.rect_filled(rect, radius, fill);
    let border = Stroke::new(
        1.0,
        if hovered {
            theme::MUTED
        } else {
            theme::CONTROL
        },
    );
    if root.is_some() {
        painter.rect_stroke(rect, radius, border, egui::StrokeKind::Inside);
    } else {
        let r = rect.shrink(0.5);
        let corners = [
            r.left_top(),
            r.right_top(),
            r.right_bottom(),
            r.left_bottom(),
            r.left_top(),
        ];
        painter.extend(egui::Shape::dashed_line(&corners, border, 4.0, 3.0));
    }

    let icon_color = match (hovered, enabled) {
        (true, _) => theme::STRONG,
        (false, true) => theme::DIM,
        (false, false) => theme::CONTROL,
    };
    paint_folder_icon(
        painter,
        egui::pos2(rect.right() - 18.0, rect.center().y),
        icon_color,
    );
    let mut text_right = rect.right() - 36.0;
    if hovered && root.is_some() {
        let change = painter.text(
            egui::pos2(text_right, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            tr("Change…"),
            egui::FontId::proportional(12.0),
            theme::TEXT,
        );
        text_right = change.left() - 12.0;
    }

    let left = rect.left() + 10.0;
    let (text, font, color) = match root {
        Some(path) => (
            path.display().to_string(),
            egui::FontId::monospace(13.0),
            if enabled { theme::TEXT } else { theme::MUTED },
        ),
        None => (
            match side {
                Side::Library => tr("Choose the destination folder…"),
                Side::Staging => tr("Choose the staging folder…"),
            }
            .to_string(),
            egui::FontId::proportional(13.0),
            theme::MUTED,
        ),
    };
    // Long paths keep their end (the folder name), with an ellipsis in front.
    let fits = |t: &str| {
        painter
            .layout_no_wrap(t.to_string(), font.clone(), color)
            .size()
            .x
            <= text_right - left
    };
    let mut shown = text.clone();
    let mut skip = 0;
    while !fits(&shown) && skip < text.chars().count() {
        skip += 1;
        shown = format!("…{}", text.chars().skip(skip).collect::<String>());
    }
    painter.text(
        egui::pos2(left, rect.center().y),
        egui::Align2::LEFT_CENTER,
        shown,
        font,
        color,
    );

    let response = if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    };
    match root {
        Some(path) => {
            response.on_hover_text(format!("{}\n{}", path.display(), tr("Change folder…")))
        }
        None => response,
    }
}

fn paint_folder_icon(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let p = |x: f32, y: f32| center + egui::vec2(x, y);
    let outline = vec![
        p(-7.0, 6.0),
        p(-7.0, -6.0),
        p(-2.0, -6.0),
        p(0.0, -4.0),
        p(7.0, -4.0),
        p(7.0, 6.0),
    ];
    painter.add(egui::Shape::closed_line(outline, Stroke::new(1.5, color)));
}

/// One entry of the duplicates area list: name plus an amber count, a green check
/// when clean, or a dash when not checked yet.
fn nav_item(
    ui: &mut egui::Ui,
    label: &str,
    count: Option<usize>,
    selected: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::click());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 0.0, theme::PANEL);
        let bar = egui::Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height()));
        painter.rect_filled(bar, 0.0, theme::STRONG);
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, theme::HOVER);
    }
    let body = egui::TextStyle::Body.resolve(ui.style());
    painter.text(
        rect.left_center() + egui::vec2(16.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        body.clone(),
        if selected { theme::STRONG } else { theme::DIM },
    );
    let right = rect.right_center() - egui::vec2(16.0, 0.0);
    match count {
        Some(0) => paint_check(painter, right - egui::vec2(6.0, 0.0), 12.0, theme::GREEN),
        Some(n) => {
            painter.text(
                right,
                egui::Align2::RIGHT_CENTER,
                n.to_string(),
                egui::FontId::monospace(13.0),
                theme::AMBER,
            );
        }
        None => {
            painter.text(right, egui::Align2::RIGHT_CENTER, "–", body, theme::MUTED);
        }
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Thin amber strip with a message and a link. Returns true when the link is clicked.
fn banner(ui: &mut egui::Ui, text: &str, link: &str) -> bool {
    let mut clicked = false;
    egui::Frame::new()
        .fill(theme::AMBER_BG)
        .stroke(Stroke::new(1.0, theme::AMBER_BORDER))
        .inner_margin(Margin::symmetric(20, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::dot(ui, theme::AMBER);
                ui.label(RichText::new(text).color(theme::AMBER_TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let link = egui::Button::new(RichText::new(link).strong().color(theme::AMBER))
                        .frame(false);
                    clicked = ui.add(link).clicked();
                });
            });
        });
    clicked
}

fn section_title(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .size(20.0)
                .strong()
                .color(theme::STRONG),
        );
        ui.label(RichText::new(detail).color(theme::AMBER));
    });
    ui.add_space(8.0);
}

/// Centered message for an area with nothing to do (or nothing known yet).
fn empty_state(ui: &mut egui::Ui, title: &str, detail: &str, clean: bool) {
    ui.add_space(60.0);
    ui.vertical_centered(|ui| {
        if clean {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 20.0, theme::GREEN);
            paint_check(ui.painter(), rect.center(), 20.0, theme::BG);
            ui.add_space(8.0);
        }
        let color = if clean { theme::GREEN } else { theme::TEXT };
        ui.label(RichText::new(title).size(18.0).strong().color(color));
        if !detail.is_empty() {
            ui.label(RichText::new(detail).color(theme::MUTED));
        }
    });
}

/// Shown when destination, staging and staging-vs-destination are all clean.
/// Returns true when the user asks to continue to the next step.
fn all_clean(ui: &mut egui::Ui, model: &Model) -> bool {
    let new_files = model.check().map_or(0, |items| {
        items
            .iter()
            .filter(|i| matches!(i.status, StagingStatus::Unique))
            .count()
    });
    let mut next = false;
    ui.add_space((ui.available_height() / 2.0 - 120.0).max(24.0));
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 28.0, theme::GREEN);
        paint_check(ui.painter(), rect.center(), 28.0, theme::BG);
        ui.add_space(12.0);
        ui.label(
            RichText::new(tr("No duplicates left"))
                .size(26.0)
                .strong()
                .color(theme::STRONG),
        );
        ui.label(
            RichText::new(trf(
                "Destination and staging are clean. {n} staging file(s) are new.",
                &[("n", &new_files)],
            ))
            .color(theme::MUTED),
        );
        ui.add_space(14.0);
        next = ui
            .add(theme::primary(tr("Continue to Organize  →")))
            .clicked();
    });
    next
}

/// Square thumbnail of `path`; the image is only requested once it is on screen.
/// Clicking it opens the large preview.
fn group_thumbnail(ui: &mut egui::Ui, textures: &mut Textures, path: &Path) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(GROUP_THUMB, GROUP_THUMB), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let tex = textures.get(path, THUMB_SIZE);
        paint_cover(ui.painter(), rect, path, &tex);
        if response.hovered() {
            ui.painter().rect_stroke(
                rect,
                0.0,
                Stroke::new(1.0, theme::STRONG),
                egui::StrokeKind::Inside,
            );
        }
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tr("Click to preview"))
        .clicked()
}

fn group_separator(ui: &mut egui::Ui) {
    ui.add_space(10.0);
    let rect = ui.available_rect_before_wrap();
    ui.painter()
        .hline(rect.x_range(), rect.top(), Stroke::new(1.0, theme::BORDER));
    ui.add_space(12.0);
}

/// One trashed file in the grid: thumbnail and name. Returns
/// true when clicked; right-click offers Open and Show in the file manager.
fn trash_cell(ui: &mut egui::Ui, textures: &mut Textures, file: &TrashedFile) -> bool {
    let mut clicked = false;
    ui.allocate_ui_with_layout(
        egui::vec2(GROUP_THUMB + 24.0, GROUP_THUMB + 30.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(GROUP_THUMB + 24.0, GROUP_THUMB + 24.0),
                egui::Sense::click(),
            );
            if ui.is_rect_visible(rect) {
                let tex = textures.get(&file.path, THUMB_SIZE);
                paint_cover(ui.painter(), rect, &file.path, &tex);
                if response.hovered() {
                    ui.painter().rect_stroke(
                        rect,
                        0.0,
                        Stroke::new(1.0, theme::STRONG),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            let response = response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("{}\n{}", file.rel, format_size(file.size)));
            response.context_menu(|ui| {
                if ui.button(tr("Open")).clicked() {
                    let _ = open_path(&file.path);
                    ui.close();
                }
                if ui
                    .button(trf("Show in {app}", &[("app", &file_manager())]))
                    .clicked()
                {
                    let _ = reveal(&file.path);
                    ui.close();
                }
            });
            clicked = response.clicked();
            let name = file.rel.rsplit('/').next().unwrap_or(&file.rel);
            ui.add(egui::Label::new(theme::mono(name, theme::TEXT).small()).truncate());
        },
    );
    clicked
}

/// Amber note for a folder indexed by an older version (no image hashes yet).
fn rescan_note(ui: &mut egui::Ui, sides: &[Side]) {
    let names: Vec<&str> = sides.iter().map(|s| s.label()).collect();
    ui.label(
        RichText::new(trf(
            "{folders}: indexed by an older version. Rescan in step 1 to also find copies whose metadata differs (files without that information are read once more).",
            &[("folders", &names.join(", "))],
        ))
        .color(theme::AMBER),
    );
    ui.add_space(10.0);
}

/// The metadata fields that differ between copies, one column per copy (numbered
/// like the rows above it). Returns the height it took.
fn differences_table(ui: &mut egui::Ui, differences: &[Difference], copies: usize) -> f32 {
    let top = ui.cursor().top();
    ui.label(RichText::new(tr("What differs")).strong());
    if differences.is_empty() {
        ui.label(
            RichText::new(tr("No difference in the readable metadata: the files differ in data this app does not show (for example an embedded preview or vendor data)."))
                .small()
                .color(theme::MUTED),
        );
        return ui.cursor().top() - top;
    }
    egui::ScrollArea::vertical()
        .id_salt("differences")
        .max_height(180.0)
        .show(ui, |ui| {
            egui::Grid::new("differences_grid")
                .striped(true)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    ui.label(RichText::new(tr("Field")).small().color(theme::MUTED));
                    for number in 1..=copies {
                        ui.label(RichText::new(format!("#{number}")).small().strong());
                    }
                    ui.end_row();
                    for (name, values) in differences {
                        ui.label(theme::mono(name, theme::TEXT).small());
                        for value in values {
                            match value {
                                Some(v) => ui.label(theme::mono(v, theme::TEXT).small()),
                                None => ui.label(
                                    RichText::new(tr("(missing)")).small().color(theme::AMBER),
                                ),
                            };
                        }
                        ui.end_row();
                    }
                });
        });
    ui.cursor().top() - top
}

/// Size of an indexed file, for display.
fn indexed_size(model: &Model, side: Side, rel: &str) -> String {
    model
        .folder(side)
        .and_then(|f| f.index.files.get(rel))
        .map_or_else(String::new, |r| format_size(r.size))
}

/// Duplicate groups inside one folder, each path with a "Trash..." button: exact
/// copies first, then copies of the same photo whose metadata differs.
fn groups_view(
    ui: &mut egui::Ui,
    model: &Model,
    textures: &mut Textures,
    side: Side,
) -> Option<DupRequest> {
    let Some(folder) = model.folder(side) else {
        empty_state(
            ui,
            &trf("{side} is not selected", &[("side", &side.label())]),
            tr("Choose it in step 1, Folders."),
            false,
        );
        return None;
    };
    if !folder.is_indexed() {
        empty_state(
            ui,
            &trf("{side} is not indexed yet", &[("side", &side.label())]),
            tr("Index it in step 1, Folders."),
            false,
        );
        return None;
    }
    if folder.needs_rescan() {
        rescan_note(ui, &[side]);
    }
    if folder.duplicates.is_empty() && folder.same_image.is_empty() {
        let lower = side.label().to_lowercase();
        if folder.needs_rescan() {
            empty_state(
                ui,
                &trf("No identical files in {side}", &[("side", &lower)]),
                "",
                false,
            );
        } else {
            empty_state(
                ui,
                &trf("No duplicates in {side}", &[("side", &lower)]),
                "",
                true,
            );
        }
        return None;
    }
    let mut request = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if !folder.duplicates.is_empty() {
                let reclaimable: u64 = folder
                    .duplicates
                    .iter()
                    .map(|g| g.size * (g.paths.len() as u64 - 1))
                    .sum();
                section_title(
                    ui,
                    side.label(),
                    &trf(
                        "{groups} group(s) · {size} reclaimable",
                        &[
                            ("groups", &folder.duplicates.len()),
                            ("size", &format_size(reclaimable)),
                        ],
                    ),
                );
                for group in &folder.duplicates {
                    let caption = trf(
                        "{n} identical · {size}",
                        &[
                            ("n", &group.paths.len()),
                            ("size", &format_size(group.size)),
                        ],
                    );
                    let row = group_row(
                        ui,
                        model,
                        textures,
                        side,
                        &group.hash,
                        &group.paths,
                        &caption,
                        false,
                    );
                    request = row.or(request.take());
                }
            }
            if !folder.same_image.is_empty() {
                same_image_title(ui, folder.same_image.len());
                for group in &folder.same_image {
                    let caption = trf(
                        "{n} copies · metadata differs",
                        &[("n", &group.paths.len())],
                    );
                    let row = group_row(
                        ui,
                        model,
                        textures,
                        side,
                        &group.image_hash,
                        &group.paths,
                        &caption,
                        true,
                    );
                    request = row.or(request.take());
                }
            }
        });
    request
}

fn same_image_title(ui: &mut egui::Ui, groups: usize) {
    ui.add_space(8.0);
    section_title(
        ui,
        tr("Same photo, different metadata"),
        &trf("{n} group(s)", &[("n", &groups)]),
    );
    ui.label(
        RichText::new(tr("The picture is the same, but the files differ in their metadata (export ID, GPS, date, thumbnail...). Click a thumbnail to see what differs, then keep the copy you prefer."))
            .small()
            .color(theme::MUTED),
    );
    ui.add_space(10.0);
}

/// One group of copies inside a folder: thumbnail (opens the preview), caption and
/// a "Trash..." button per path. `same_image` marks copies whose metadata differs.
#[allow(clippy::too_many_arguments)]
fn group_row(
    ui: &mut egui::Ui,
    model: &Model,
    textures: &mut Textures,
    side: Side,
    id: &str,
    paths: &[String],
    caption: &str,
    same_image: bool,
) -> Option<DupRequest> {
    let folder = model.folder(side)?;
    let mut request = None;
    ui.push_id((id, same_image), |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            if let Some(first) = paths.first() {
                if group_thumbnail(ui, textures, &folder.root.join(first)) {
                    let files = paths.iter().map(|p| (side, p.clone())).collect();
                    request = Some(DupRequest::Preview(DupPreview::new(
                        files, same_image, model,
                    )));
                }
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(RichText::new(caption).strong());
                for rel in paths {
                    ui.horizontal(|ui| {
                        let button = ui.add(theme::danger(tr("Trash...")));
                        if button.clicked() {
                            request = Some(DupRequest::Trash(Confirm::new(
                                vec![(side, rel.clone())],
                                button.rect,
                            )));
                        }
                        ui.label(theme::mono(rel, theme::TEXT));
                        if same_image {
                            ui.label(theme::mono(indexed_size(model, side, rel), theme::DIM));
                        }
                    });
                }
            });
        });
        group_separator(ui);
    });
    request
}

/// Staging files that are already in the destination, identical or as the same
/// photo with other metadata. Sets `run` when the user asks to run the check (it is
/// never run automatically the first time).
fn against_library_view(
    ui: &mut egui::Ui,
    model: &Model,
    textures: &mut Textures,
    busy: bool,
    run: &mut bool,
) -> Option<DupRequest> {
    let Some(items) = model.check() else {
        empty_state(
            ui,
            tr("Not checked yet"),
            tr("Compare staging with the destination to find photos you already have."),
            false,
        );
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            if ui
                .add_enabled(
                    model.folders_ready(),
                    theme::primary(tr("Check staging against destination")),
                )
                .clicked()
            {
                *run = true;
            }
        });
        return None;
    };
    let staging = model.folder(Side::Staging)?;
    let stale: Vec<Side> = [Side::Library, Side::Staging]
        .into_iter()
        .filter(|&side| model.folder(side).is_some_and(|f| f.needs_rescan()))
        .collect();
    if !stale.is_empty() {
        rescan_note(ui, &stale);
    }
    let (mut in_library, mut same_image) = (Vec::new(), Vec::new());
    for item in items {
        match &item.status {
            StagingStatus::DuplicateOfLibrary { library_paths } => {
                in_library.push((&item.path, library_paths))
            }
            StagingStatus::SameImageInLibrary { library_paths } => {
                same_image.push((&item.path, library_paths))
            }
            _ => {}
        }
    }
    if in_library.is_empty() && same_image.is_empty() {
        let title = tr("No staging file is already in the destination");
        empty_state(ui, title, "", stale.is_empty());
        return None;
    }
    let mut request = None;
    if !in_library.is_empty() {
        ui.horizontal(|ui| {
            section_title(
                ui,
                tr("Staging vs destination"),
                &trf(
                    "{n} staging file(s) already in the destination",
                    &[("n", &in_library.len())],
                ),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let button = ui
                    .add_enabled(
                        !busy,
                        theme::danger(trf("Trash all {n} from staging...", &[("n", &in_library.len())])),
                    )
                    .on_hover_text(tr("Each file is checked against its destination copy on disk before it is moved to .trash."))
                    .on_disabled_hover_text(tr("Available once indexing has finished."));
                if button.clicked() {
                    request = Some(DupRequest::TrashAllInLibrary);
                }
            });
        });
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (rel, library_paths) in in_library {
                let row = staging_row(ui, model, textures, staging, rel, library_paths, false);
                request = row.or(request.take());
            }
            if !same_image.is_empty() {
                same_image_title(ui, same_image.len());
                for (rel, library_paths) in same_image {
                    let row = staging_row(ui, model, textures, staging, rel, library_paths, true);
                    request = row.or(request.take());
                }
            }
        });
    request
}

/// One staging file and its copies in the destination.
fn staging_row(
    ui: &mut egui::Ui,
    model: &Model,
    textures: &mut Textures,
    staging: &Folder,
    rel: &String,
    library_paths: &[String],
    same_image: bool,
) -> Option<DupRequest> {
    let mut request = None;
    ui.push_id((rel, same_image), |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            if group_thumbnail(ui, textures, &staging.root.join(rel)) {
                let files = std::iter::once((Side::Staging, rel.clone()))
                    .chain(library_paths.iter().map(|p| (Side::Library, p.clone())))
                    .collect();
                request = Some(DupRequest::Preview(DupPreview::new(
                    files, same_image, model,
                )));
            }
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let button = ui.add(theme::danger(tr("Trash...")));
                    if button.clicked() {
                        request = Some(DupRequest::Trash(Confirm::new(
                            vec![(Side::Staging, rel.clone())],
                            button.rect,
                        )));
                    }
                    ui.label(theme::mono(rel, theme::TEXT));
                    if same_image {
                        ui.label(theme::mono(
                            indexed_size(model, Side::Staging, rel),
                            theme::DIM,
                        ));
                    }
                });
                for path in library_paths {
                    let text = if same_image {
                        trf(
                            "same photo as {path} ({size})",
                            &[
                                ("path", path),
                                ("size", &indexed_size(model, Side::Library, path)),
                            ],
                        )
                    } else {
                        trf("same as {path}", &[("path", path)])
                    };
                    ui.label(theme::mono(text, theme::DIM));
                }
            });
        });
        group_separator(ui);
    });
    request
}

impl eframe::App for PhotoCopyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_job();
        self.poll_bulk();
        self.poll_trash_check();
        let ctx = ui.ctx().clone();
        self.browse.poll(&ctx);
        self.dup_textures.poll(&ctx);
        self.trash_textures.poll(&ctx);
        let bar = |x, y| {
            egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(Margin::symmetric(x, y))
        };
        let summary = self.step != Step::Folders;
        egui::Panel::top("steps")
            .frame(bar(20, 10))
            .show_separator_line(!summary)
            .show(ui, |ui| self.steps_bar(ui));
        if summary {
            egui::Panel::top("folders_summary")
                .frame(bar(20, 8))
                .show(ui, |ui| self.summary_line(ui));
        }
        egui::Panel::bottom("status")
            .frame(bar(20, 5))
            .show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG))
            .show(ui, |ui| self.main_view(ui));
        self.dup_preview_ui(&ctx);
        self.confirm_dialog(&ctx);
        self.move_dialog_ui(&ctx);
        self.batch_report_ui(&ctx);
        self.bulk_ui(&ctx);
        self.trash_preview_ui(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        for (side, key) in [(Side::Library, LIBRARY_KEY), (Side::Staging, STAGING_KEY)] {
            if let Some(folder) = self.model.folder(side) {
                storage.set_string(key, folder.root.display().to_string());
            }
        }
    }
}
