//! "Move to destination" dialog for one file or a batch: pick (or create) a
//! destination subfolder and confirm. A single file can also be renamed; in a batch
//! files keep their names and those whose name is taken are skipped. The dialog only
//! reports what the user asked for; the app performs the operation and reports errors
//! back into the dialog.

use std::path::Path;

use eframe::egui;
use photo_copy_core::browse::list_dir;
use photo_copy_core::ops::{
    name_problem as existing_name_problem, portable_name_problem, suggest_free_name,
};

use crate::i18n::{tr, trf};
use crate::model::{Model, Side};
use crate::paths::{breadcrumbs, file_name, join_dir, parent_dir};
use crate::theme;

const WARNING: egui::Color32 = theme::AMBER;

pub enum DialogAction {
    Cancel,
    /// Move the single source under this name.
    Move {
        dest_dir: String,
        name: String,
    },
    /// Move all sources, keeping their names.
    MoveMany {
        dest_dir: String,
    },
    CreateFolder {
        parent: String,
        name: String,
    },
}

pub struct MoveDialog {
    /// Staging files being moved, relative to the staging root.
    sources: Vec<String>,
    /// File name to use in the destination (single file only; editable).
    name: String,
    /// Destination folder currently shown, relative to the destination root.
    dest_dir: String,
    new_folder: String,
    /// Subfolders of `dest_dir`; `None` means "read them again".
    subdirs: Option<Vec<String>>,
    /// Batch: how many sources already have a file with their name in `dest_dir`.
    /// `None` means "count again".
    collisions: Option<usize>,
    /// Destination files identical to the single source (computed on first show).
    copies: Option<Vec<String>>,
    /// Batch: how many sources are identical to files in the destination.
    duplicates: Option<usize>,
    pub error: Option<String>,
}

impl MoveDialog {
    pub fn new(sources: Vec<String>, start_dir: String) -> Self {
        MoveDialog {
            name: sources
                .first()
                .map_or_else(String::new, |s| file_name(s).to_string()),
            sources,
            dest_dir: start_dir,
            new_folder: String::new(),
            subdirs: None,
            collisions: None,
            copies: None,
            duplicates: None,
            error: None,
        }
    }

    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    fn is_batch(&self) -> bool {
        self.sources.len() > 1
    }

    /// Show another destination folder.
    pub fn enter(&mut self, dir: String) {
        self.dest_dir = dir;
        self.subdirs = None;
        self.collisions = None;
        self.new_folder.clear();
        self.error = None;
    }

    fn load_subdirs(&mut self, root: &Path, ctx: &egui::Context) {
        match list_dir(root, &self.dest_dir) {
            Ok(listing) => self.subdirs = Some(listing.dirs),
            Err(_) if !self.dest_dir.is_empty() => {
                // The remembered folder is gone: fall back to the top level.
                self.dest_dir.clear();
                self.collisions = None;
                self.error =
                    Some(tr("That folder no longer exists; showing the top level.").into());
                ctx.request_repaint();
            }
            Err(e) => {
                self.subdirs = Some(Vec::new());
                self.error = Some(trf(
                    "Cannot read the destination folder: {error}",
                    &[("error", &e)],
                ));
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, model: &Model) -> Option<DialogAction> {
        let Some(library) = model.folder(Side::Library) else {
            return Some(DialogAction::Cancel);
        };
        let root = library.root.clone();
        if self.subdirs.is_none() {
            self.load_subdirs(&root, ctx);
        }
        if self.is_batch() {
            if self.duplicates.is_none() {
                self.duplicates = Some(model.count_with_library_copies(&self.sources));
            }
            if self.collisions.is_none() {
                let folder = root.join(&self.dest_dir);
                let taken = |s: &String| folder.join(file_name(s)).exists();
                self.collisions = Some(self.sources.iter().filter(|s| taken(s)).count());
            }
        } else if self.copies.is_none() {
            self.copies = Some(
                self.sources
                    .first()
                    .map_or_else(Vec::new, |s| model.library_copies_of(s)),
            );
        }

        let target = root.join(&self.dest_dir).join(&self.name);
        // A name kept as it is only has to be usable here; a name the user typed must
        // also be one Windows can store, so the library stays portable.
        let original_name = self.sources.first().map_or("", |s| file_name(s));
        let name_problem = if self.name == original_name {
            existing_name_problem(&self.name)
        } else {
            portable_name_problem(&self.name)
        };
        let taken = !self.is_batch() && name_problem.is_none() && target.exists();
        let root_name = root.file_name().map_or_else(
            || root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );

        let mut action = None;
        let mut goto = None;
        let response = egui::Modal::new(egui::Id::unique("move_dialog")).show(ctx, |ui| {
            ui.set_width(520.0);
            if self.is_batch() {
                ui.heading(trf("Move {n} files to destination", &[("n", &self.sources.len())]));
                egui::ScrollArea::vertical()
                    .id_salt("move_sources")
                    .max_height(90.0)
                    .show(ui, |ui| {
                        for source in &self.sources {
                            ui.monospace(source);
                        }
                    });
                if let Some(count) = self.duplicates.filter(|c| *c > 0) {
                    ui.colored_label(
                        WARNING,
                        trf(
                            "{n} of them are identical to files already in the destination.",
                            &[("n", &count)],
                        ),
                    );
                }
            } else {
                ui.heading(tr("Move to destination"));
                ui.label(trf(
                    "From staging: {path}",
                    &[("path", &self.sources.first().map_or("", String::as_str))],
                ));
                if let Some(copies) = self.copies.as_ref().filter(|c| !c.is_empty()) {
                    ui.colored_label(
                        WARNING,
                        trf(
                            "Identical file already in the destination: {paths}",
                            &[("paths", &copies.join(", "))],
                        ),
                    );
                }
            }
            ui.separator();

            ui.strong(tr("Destination folder"));
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(!self.dest_dir.is_empty(), egui::Button::new(tr("Up")))
                    .clicked()
                {
                    goto = Some(parent_dir(&self.dest_dir).to_string());
                }
                if ui.link(&root_name).clicked() {
                    goto = Some(String::new());
                }
                for (name, rel) in breadcrumbs(&self.dest_dir) {
                    ui.label("/");
                    if ui.link(&name).clicked() {
                        goto = Some(rel);
                    }
                }
            });
            egui::ScrollArea::vertical()
                .id_salt("move_folders")
                .max_height(180.0)
                .show(ui, |ui| match self.subdirs.as_deref() {
                    Some([]) | None => {
                        ui.weak(tr("(no subfolders)"));
                    }
                    Some(dirs) => {
                        for dir in dirs {
                            if ui.button(dir).clicked() {
                                goto = Some(join_dir(&self.dest_dir, dir));
                            }
                        }
                    }
                });
            let mut folder_problem = None;
            ui.horizontal(|ui| {
                ui.label(tr("New folder here:"));
                ui.text_edit_singleline(&mut self.new_folder);
                folder_problem = portable_name_problem(&self.new_folder);
                let valid = folder_problem.is_none();
                if ui.add_enabled(valid, egui::Button::new(tr("Create"))).clicked() {
                    action = Some(DialogAction::CreateFolder {
                        parent: self.dest_dir.clone(),
                        name: self.new_folder.clone(),
                    });
                }
            });
            if !self.new_folder.is_empty() {
                if let Some(problem) = &folder_problem {
                    ui.colored_label(ui.visuals().error_fg_color, problem);
                }
            }
            ui.separator();

            if self.is_batch() {
                match self.collisions {
                    Some(count) if count > 0 => {
                        ui.colored_label(
                            WARNING,
                            trf(
                                "{n} file(s) already have a file with the same name in this folder and will be skipped.",
                                &[("n", &count)],
                            ),
                        );
                    }
                    _ => {
                        ui.weak(tr("Files keep their names. A file whose name is already taken in the destination folder is skipped."));
                    }
                }
            } else {
                ui.horizontal(|ui| {
                    ui.label(tr("File name:"));
                    ui.text_edit_singleline(&mut self.name);
                });
                if let Some(problem) = &name_problem {
                    ui.colored_label(ui.visuals().error_fg_color, problem);
                } else if taken {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            tr("A file with this name already exists in that folder."),
                        );
                        if ui.button(tr("Use a free name")).clicked() {
                            if let Some(free) = suggest_free_name(&target).file_name() {
                                self.name = free.to_string_lossy().into_owned();
                            }
                        }
                    });
                }
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if self.is_batch() {
                    let label = trf("Move {n} files here", &[("n", &self.sources.len())]);
                    if ui.button(label).clicked() {
                        action = Some(DialogAction::MoveMany {
                            dest_dir: self.dest_dir.clone(),
                        });
                    }
                } else {
                    let can_move = name_problem.is_none() && !taken;
                    if ui.add_enabled(can_move, egui::Button::new(tr("Move here"))).clicked() {
                        action = Some(DialogAction::Move {
                            dest_dir: self.dest_dir.clone(),
                            name: self.name.clone(),
                        });
                    }
                }
                if ui.button(tr("Cancel")).clicked() {
                    action = Some(DialogAction::Cancel);
                }
            });
        });

        if let Some(dir) = goto {
            self.enter(dir);
        }
        if response.should_close() {
            action.get_or_insert(DialogAction::Cancel);
        }
        action
    }
}
