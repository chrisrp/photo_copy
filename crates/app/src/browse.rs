//! Photo browser: folder navigation, a thumbnail grid and a full-size viewer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread;

use eframe::egui;
use egui_extras::{Column as TableColumn, TableBuilder};
use photo_copy_core::browse::{list_dir, Listing};
use photo_copy_core::meta::read_dates;
use photo_copy_core::opener::{open_path, reveal, OpenAction};

use crate::i18n::{tr, trf};
use crate::list::{build_rows, sort_rows, Column, ListRow, Sort, Target};
use crate::model::{Model, Side};
use crate::paths::{breadcrumbs, format_size, join_dir, parent_dir, shorten};
use crate::selection::Selection;
use crate::theme;
use crate::thumbs::{Key, ThumbLoader};

/// Name of the system file manager, for "Show in ..." menu entries.
pub fn file_manager() -> &'static str {
    if cfg!(target_os = "macos") {
        "Finder"
    } else {
        tr("file manager")
    }
}

pub const THUMB_SIZE: u32 = 256;
pub const VIEW_SIZE: u32 = 1600;
const CELL_W: f32 = 160.0;
const IMAGE_H: f32 = 160.0;
const CELL_H: f32 = 180.0;
const GRID_GAP: f32 = 4.0;

#[derive(Clone)]
pub enum Tex {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

/// Decoded images as GPU textures, loaded in the background on first use.
pub struct Textures {
    loader: ThumbLoader,
    map: HashMap<Key, Tex>,
}

impl Textures {
    pub fn new(ctx: &egui::Context) -> Self {
        let ctx = ctx.clone();
        let workers = thread::available_parallelism().map_or(2, |n| n.get().min(4));
        Textures {
            loader: ThumbLoader::new(workers, move || ctx.request_repaint()),
            map: HashMap::new(),
        }
    }

    /// Current state of an image; queues the decode the first time it is asked for.
    pub fn get(&mut self, path: &Path, size: u32) -> Tex {
        let key = (path.to_path_buf(), size);
        if let Some(tex) = self.map.get(&key) {
            return tex.clone();
        }
        self.loader.request(key.clone());
        self.map.insert(key, Tex::Loading);
        Tex::Loading
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        for (key, result) in self.loader.poll() {
            let tex = match result {
                Ok(thumb) => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [thumb.width as usize, thumb.height as usize],
                        &thumb.rgba,
                    );
                    let name = key.0.display().to_string();
                    Tex::Ready(ctx.load_texture(name, image, egui::TextureOptions::LINEAR))
                }
                Err(_) => Tex::Failed,
            };
            self.map.insert(key, tex);
        }
    }

    fn clear(&mut self) {
        self.loader.forget_all();
        self.map.clear();
    }

    /// Free the large preview textures except the one for `keep`.
    pub fn drop_previews_except(&mut self, keep: Option<&Path>) {
        let stale: Vec<Key> = self
            .map
            .keys()
            .filter(|(path, size)| *size == VIEW_SIZE && Some(path.as_path()) != keep)
            .cloned()
            .collect();
        for key in stale {
            self.loader.forget(&key);
            self.map.remove(&key);
        }
    }
}

enum Action {
    Enter(String),
    Open(usize),
    /// A click on a file: select it (Cmd/Ctrl toggles, Shift extends a range).
    Click {
        file_index: usize,
        toggle: bool,
        range: bool,
    },
    /// Move just this file to the destination.
    Move(usize),
    /// Move every selected file to the destination.
    MoveSelection,
    /// Hand a file or folder to the operating system.
    External {
        path: PathBuf,
        how: OpenAction,
    },
    /// A list column header was clicked.
    SortBy(Column),
}

/// What the user asked the app to do with staging files.
pub enum BrowseRequest {
    /// Move these files (relative paths) to the destination.
    Move(Vec<String>),
    /// Move this file to the `.trash`, after a confirmation placed on `anchor`.
    Trash { rel: String, anchor: egui::Rect },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Grid,
    List,
}

const ROW_H: f32 = 22.0;

pub struct BrowseView {
    side: Side,
    /// Folder being shown, relative to the root, `/`-separated (empty = root).
    dir: String,
    listing: Option<Result<Listing, String>>,
    /// Index into `listing.files` of the photo open in the viewer.
    viewer: Option<usize>,
    /// Name of the photo to show in the viewer once the folder is re-read (the
    /// photo it showed was removed).
    viewer_next: Option<String>,
    textures: Textures,
    /// Problem to show the user (e.g. a file could not be opened).
    message: Option<String>,
    mode: ViewMode,
    sort: Sort,
    /// List rows for the current folder, built when the list is first shown.
    rows: Option<Vec<ListRow>>,
    selection: Selection,
}

impl BrowseView {
    pub fn new(ctx: &egui::Context) -> Self {
        BrowseView {
            side: Side::Staging,
            dir: String::new(),
            listing: None,
            viewer: None,
            viewer_next: None,
            textures: Textures::new(ctx),
            message: None,
            mode: ViewMode::Grid,
            sort: Sort::default(),
            rows: None,
            selection: Selection::default(),
        }
    }

    /// A message for the status line, if something went wrong since the last call.
    pub fn take_message(&mut self) -> Option<String> {
        self.message.take()
    }

    fn run_external(&mut self, path: &Path, how: OpenAction) {
        let result = match how {
            OpenAction::Open => open_path(path),
            OpenAction::Reveal => reveal(path),
        };
        if let Err(e) = result {
            self.message = Some(trf(
                "Could not open {path}: {error}",
                &[("path", &path.display()), ("error", &e)],
            ));
        }
    }

    /// Turn finished background decodes into textures. Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        self.textures.poll(ctx);
    }

    /// Re-read the current folder on the next frame (files were added or removed).
    pub fn invalidate(&mut self) {
        self.listing = None;
    }

    /// The file `rel` of `side` was removed (trashed). If the viewer shows it, it
    /// moves on to the next photo, or the previous one if it was the last; otherwise
    /// it keeps its photo (the folder is re-read, so positions may shift).
    pub fn file_removed(&mut self, side: Side, rel: &str) {
        if side != self.side {
            return;
        }
        if let Some(current) = self.viewer {
            if self.file_rel(current).as_deref() == Some(rel) {
                let order = self.file_order();
                let position = order.iter().position(|&i| i == current);
                let neighbour = position.and_then(|p| {
                    order
                        .get(p + 1)
                        .or_else(|| p.checked_sub(1).and_then(|p| order.get(p)))
                });
                self.viewer_next = neighbour.and_then(|&i| self.file_name(i));
            } else {
                self.viewer_next = self.file_name(current);
            }
            self.viewer = None;
        }
        self.invalidate();
    }

    /// The root of `side` changed: go back to its top if it is the one shown.
    pub fn folder_changed(&mut self, side: Side) {
        if self.side == side {
            self.go_to(String::new());
        }
    }

    fn go_to(&mut self, dir: String) {
        self.dir = dir;
        self.listing = None;
        self.viewer = None;
        self.viewer_next = None;
        self.selection.clear();
        self.textures.clear();
    }

    /// Draw the browser. Returns what the user asked to do with staging files, if anything.
    pub fn show(&mut self, ui: &mut egui::Ui, model: &Model) -> Option<BrowseRequest> {
        let before = self.side;
        ui.horizontal(|ui| {
            // Staging first: organizing starts from the new photos.
            ui.selectable_value(&mut self.side, Side::Staging, Side::Staging.label());
            ui.selectable_value(&mut self.side, Side::Library, Side::Library.label());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.selectable_value(&mut self.mode, ViewMode::List, tr("List"));
                ui.selectable_value(&mut self.mode, ViewMode::Grid, tr("Grid"));
            });
        });
        if self.side != before {
            self.go_to(String::new());
        }
        let Some(folder) = model.folder(self.side) else {
            ui.weak(tr("Choose this folder first."));
            return None;
        };
        if self.listing.is_none() {
            self.rows = None;
            self.listing = Some(list_dir(&folder.root, &self.dir).map_err(|e| {
                trf(
                    "Cannot open {path}: {error}",
                    &[
                        ("path", &folder.root.join(&self.dir).display()),
                        ("error", &e),
                    ],
                )
            }));
            if let Some(Ok(listing)) = &self.listing {
                let names: Vec<String> = listing
                    .files
                    .iter()
                    .map(|f| file_name_lossy(&f.path))
                    .collect();
                let existing: HashSet<&str> = names.iter().map(String::as_str).collect();
                self.selection.retain_existing(&existing);
                if let Some(next) = self.viewer_next.take() {
                    self.viewer = names.iter().position(|n| *n == next);
                }
            }
        }

        let root_name = folder.root.file_name().map_or_else(
            || folder.root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let mut goto = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!self.dir.is_empty(), egui::Button::new(tr("Up")))
                .clicked()
            {
                goto = Some(parent_dir(&self.dir).to_string());
            }
            if ui.link(&root_name).clicked() {
                goto = Some(String::new());
            }
            for (name, rel) in breadcrumbs(&self.dir) {
                ui.label("/");
                if ui.link(&name).clicked() {
                    goto = Some(rel);
                }
            }
        });

        let can_move = self.side == Side::Staging;
        let (mut move_selection, mut clear_selection) = (false, false);
        egui::Panel::bottom("selection_bar")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let count = self.selection.len();
                    let any = !self.selection.is_empty();
                    if any {
                        ui.label(egui::RichText::new(trf("{n} selected", &[("n", &count)])).strong());
                    }
                    ui.label(egui::RichText::new(if can_move {
                        tr("Click to select · Cmd/Ctrl adds · Shift selects a range · double-click opens")
                    } else {
                        tr("Double-click a photo to open it · right-click for more")
                    }).small().color(theme::MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if any {
                            if can_move
                                && ui
                                    .add(theme::primary(trf("Move {n} to destination folder...", &[("n", &count)])))
                                    .clicked()
                            {
                                move_selection = true;
                            }
                            if ui.button(tr("Clear")).clicked() {
                                clear_selection = true;
                            }
                        }
                    });
                });
            });

        if self.mode == ViewMode::List && self.rows.is_none() {
            if let Some(Ok(listing)) = &self.listing {
                let dates = read_dates(&listing.files);
                let mut rows = build_rows(listing, &dates);
                sort_rows(&mut rows, self.sort);
                self.rows = Some(rows);
            }
        }

        // Keyboard shortcuts, unless a dialog or the viewer is open.
        let modal_open = ui.ctx().memory(|m| m.top_modal_layer().is_some());
        if !modal_open {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
                let order = self.display_names();
                self.selection.select_all(&order);
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                clear_selection = true;
            }
        }

        let dir_path = folder.root.join(&self.dir);
        let view = ViewCtx {
            dir_path: &dir_path,
            can_move,
            selection: &self.selection,
        };
        let action = match &self.listing {
            Some(Ok(listing)) => match (self.mode, &self.rows) {
                (ViewMode::List, Some(rows)) => list_table(ui, rows, listing, self.sort, &view),
                _ => grid(ui, listing, &mut self.textures, &view),
            },
            Some(Err(message)) => {
                ui.colored_label(ui.visuals().error_fg_color, message);
                None
            }
            None => None,
        };

        let mut request = None;
        match action {
            Some(Action::Enter(name)) => goto = Some(join_dir(&self.dir, &name)),
            Some(Action::Open(index)) => self.viewer = Some(index),
            Some(Action::Click {
                file_index,
                toggle,
                range,
            }) => {
                let order = self.display_names();
                if let Some(name) = self.file_name(file_index) {
                    self.selection.click(&order, &name, toggle, range);
                }
            }
            Some(Action::Move(index)) => {
                request = self
                    .file_rel(index)
                    .map(|rel| BrowseRequest::Move(vec![rel]))
            }
            Some(Action::MoveSelection) => move_selection = true,
            Some(Action::External { path, how }) => self.run_external(&path, how),
            Some(Action::SortBy(column)) => {
                self.sort = self.sort.click(column);
                if let Some(rows) = &mut self.rows {
                    sort_rows(rows, self.sort);
                }
            }
            None => {}
        }
        if move_selection {
            let rels = self.selected_rels();
            if !rels.is_empty() {
                request = Some(BrowseRequest::Move(rels));
            }
        }
        if clear_selection {
            self.selection.clear();
        }
        if let Some(dir) = goto {
            self.go_to(dir);
        }
        if let Some(viewer_request) = self.show_viewer(ui.ctx()) {
            request = Some(viewer_request);
        }
        request
    }

    /// Name of the `index`-th photo of the current folder.
    fn file_name(&self, index: usize) -> Option<String> {
        let Some(Ok(listing)) = &self.listing else {
            return None;
        };
        Some(file_name_lossy(&listing.files.get(index)?.path))
    }

    /// Names of all photos in the order they are displayed.
    fn display_names(&self) -> Vec<String> {
        self.file_order()
            .into_iter()
            .filter_map(|i| self.file_name(i))
            .collect()
    }

    /// Selected photos as paths relative to the root, in display order.
    fn selected_rels(&self) -> Vec<String> {
        self.selection
            .in_order(&self.display_names())
            .iter()
            .map(|name| join_dir(&self.dir, name))
            .collect()
    }

    /// Path relative to the root of the `index`-th photo of the current folder.
    fn file_rel(&self, index: usize) -> Option<String> {
        let Some(Ok(listing)) = &self.listing else {
            return None;
        };
        let name = listing
            .files
            .get(index)?
            .path
            .file_name()?
            .to_string_lossy()
            .into_owned();
        Some(join_dir(&self.dir, &name))
    }

    /// Indexes into `listing.files` in the order the files are displayed.
    fn file_order(&self) -> Vec<usize> {
        let Some(Ok(listing)) = &self.listing else {
            return Vec::new();
        };
        match (&self.rows, self.mode) {
            (Some(rows), ViewMode::List) => rows
                .iter()
                .filter_map(|r| match r.target {
                    Target::File(i) => Some(i),
                    Target::Dir(_) => None,
                })
                .collect(),
            _ => (0..listing.files.len()).collect(),
        }
    }

    /// Draw the viewer if a photo is open. Returns what the user asked to do with
    /// the photo shown (move or trash it), if anything.
    fn show_viewer(&mut self, ctx: &egui::Context) -> Option<BrowseRequest> {
        let Some(current) = self.viewer else {
            self.textures.drop_previews_except(None);
            return None;
        };
        let order = self.file_order();
        let Some(Ok(listing)) = &self.listing else {
            self.viewer = None;
            return None;
        };
        let total = order.len();
        let Some(mut position) = order.iter().position(|&i| i == current) else {
            self.viewer = None;
            return None;
        };

        let (left, right) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
            )
        });
        if left && position > 0 {
            position -= 1;
        }
        if right && position + 1 < total {
            position += 1;
        }

        let index = order[position];
        let path = listing.files[index].path.clone();
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let tex = self.textures.get(&path, VIEW_SIZE);
        let available = ctx.content_rect().size() * 0.8;

        let can_move = self.side == Side::Staging;
        let (mut step, mut close, mut open_external) = (0i32, false, false);
        let (mut move_index, mut trash_anchor) = (None, None);
        let response = egui::Modal::new(egui::Id::unique("photo_viewer")).show(ctx, |ui| {
            ui.set_min_width(available.x.min(900.0));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(position > 0, egui::Button::new(tr("Previous")))
                    .clicked()
                {
                    step = -1;
                }
                if ui
                    .add_enabled(position + 1 < total, egui::Button::new(tr("Next")))
                    .clicked()
                {
                    step = 1;
                }
                ui.strong(&name);
                ui.weak(format!("{} / {total}", position + 1));
                if ui.button(tr("Close")).clicked() {
                    close = true;
                }
                if can_move {
                    if ui.button(tr("Move to destination...")).clicked() {
                        move_index = Some(index);
                    }
                    let trash = ui.add(theme::danger(tr("Trash...")));
                    if trash.clicked() {
                        trash_anchor = Some(trash.rect);
                    }
                }
            });
            ui.separator();
            let max = egui::vec2(available.x, (available.y - 60.0).max(100.0));
            match &tex {
                Tex::Ready(handle) => {
                    ui.image((handle.id(), fit_size(handle.size_vec2(), max)));
                }
                Tex::Loading => {
                    ui.add_space(max.y / 2.0);
                    ui.vertical_centered(|ui| ui.spinner());
                    ui.add_space(max.y / 2.0);
                }
                Tex::Failed => {
                    ui.weak(tr("This file cannot be previewed here."));
                    if ui.button(tr("Open with the default app")).clicked() {
                        open_external = true;
                    }
                }
            }
        });
        close |= response.should_close();

        let position = (position as i32 + step).clamp(0, total as i32 - 1) as usize;
        let index = order[position];
        if close {
            self.viewer = None;
            self.textures.drop_previews_except(None);
        } else {
            self.viewer = Some(index);
            self.textures
                .drop_previews_except(Some(&listing.files[index].path));
        }
        if open_external {
            self.run_external(&path, OpenAction::Open);
        }
        if let Some(anchor) = trash_anchor {
            // The photo shown when the button was clicked, before any step.
            let rel = join_dir(&self.dir, &name);
            return Some(BrowseRequest::Trash { rel, anchor });
        }
        move_index
            .and_then(|i| self.file_rel(i))
            .map(|rel| BrowseRequest::Move(vec![rel]))
    }
}

fn file_name_lossy(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// What the grid and the list need to know about the folder being shown.
struct ViewCtx<'a> {
    dir_path: &'a Path,
    can_move: bool,
    selection: &'a Selection,
}

/// Mouse input on a photo: double-click opens it, a click selects it (Cmd/Ctrl
/// toggles, Shift extends a range), and a right-click selects it first if it is not
/// selected yet, so the context menu acts on what the user sees highlighted.
fn file_click(
    ctx: &egui::Context,
    responses: &[&egui::Response],
    file_index: usize,
    selected: bool,
) -> Option<Action> {
    let modifiers = ctx.input(|i| i.modifiers);
    if responses.iter().any(|r| r.double_clicked()) {
        Some(Action::Open(file_index))
    } else if responses.iter().any(|r| r.clicked()) {
        Some(Action::Click {
            file_index,
            toggle: modifiers.command,
            range: modifiers.shift,
        })
    } else if !selected && responses.iter().any(|r| r.secondary_clicked()) {
        Some(Action::Click {
            file_index,
            toggle: false,
            range: false,
        })
    } else {
        None
    }
}

/// Draw folders then photos as a grid of cells. Only visible rows are drawn, so
/// only visible thumbnails are decoded.
fn grid(
    ui: &mut egui::Ui,
    listing: &Listing,
    textures: &mut Textures,
    view: &ViewCtx,
) -> Option<Action> {
    let items = listing.dirs.len() + listing.files.len();
    if items == 0 {
        ui.weak(tr("This folder has no subfolders or photos."));
        return None;
    }
    ui.spacing_mut().item_spacing = egui::vec2(GRID_GAP, GRID_GAP);
    let columns = grid_columns(ui.available_width(), CELL_W, GRID_GAP);
    let rows = items.div_ceil(columns);
    let mut action = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, CELL_H, rows, |ui, range| {
            for row in range {
                ui.horizontal(|ui| {
                    for index in row * columns..((row + 1) * columns).min(items) {
                        if index < listing.dirs.len() {
                            let name = &listing.dirs[index];
                            let response = folder_cell(ui, name);
                            if response.clicked() {
                                action = Some(Action::Enter(name.clone()));
                            }
                            response.context_menu(|ui| {
                                folder_menu(ui, view.dir_path.join(name), &mut action)
                            });
                        } else {
                            let file_index = index - listing.dirs.len();
                            let path = &listing.files[file_index].path;
                            let selected = view.selection.contains(&file_name_lossy(path));
                            let tex = textures.get(path, THUMB_SIZE);
                            let response = photo_cell(ui, path, &tex, selected);
                            if let Some(clicked) =
                                file_click(ui.ctx(), &[&response], file_index, selected)
                            {
                                action = Some(clicked);
                            }
                            response.context_menu(|ui| {
                                file_menu(ui, view, path, file_index, &mut action)
                            });
                        }
                    }
                });
            }
        });
    action
}

fn folder_menu(ui: &mut egui::Ui, path: PathBuf, action: &mut Option<Action>) {
    if ui
        .button(trf("Open folder in {app}", &[("app", &file_manager())]))
        .clicked()
    {
        *action = Some(Action::External {
            path,
            how: OpenAction::Open,
        });
        ui.close();
    }
}

fn file_menu(
    ui: &mut egui::Ui,
    view: &ViewCtx,
    path: &Path,
    file_index: usize,
    action: &mut Option<Action>,
) {
    if ui.button(tr("Open")).clicked() {
        *action = Some(Action::External {
            path: path.to_path_buf(),
            how: OpenAction::Open,
        });
        ui.close();
    }
    if ui
        .button(trf("Show in {app}", &[("app", &file_manager())]))
        .clicked()
    {
        *action = Some(Action::External {
            path: path.to_path_buf(),
            how: OpenAction::Reveal,
        });
        ui.close();
    }
    if view.can_move {
        ui.separator();
        let selected = view.selection.len();
        if selected > 1 && view.selection.contains(&file_name_lossy(path)) {
            if ui
                .button(trf(
                    "Move {n} selected to destination...",
                    &[("n", &selected)],
                ))
                .clicked()
            {
                *action = Some(Action::MoveSelection);
                ui.close();
            }
        } else if ui.button(tr("Move to destination...")).clicked() {
            *action = Some(Action::Move(file_index));
            ui.close();
        }
    }
}

/// Finder-like list: sortable column headers, one row per folder and file.
fn list_table(
    ui: &mut egui::Ui,
    rows: &[ListRow],
    listing: &Listing,
    sort: Sort,
    view: &ViewCtx,
) -> Option<Action> {
    if rows.is_empty() {
        ui.weak(tr("This folder has no subfolders or photos."));
        return None;
    }
    let mut action = None;
    let headers = [
        (Column::Name, tr("Name")),
        (Column::DateTaken, tr("Date taken")),
        (Column::Size, tr("Size")),
        (Column::Kind, tr("Kind")),
    ];

    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .sense(egui::Sense::click())
        .column(TableColumn::remainder().at_least(160.0).clip(true))
        .column(TableColumn::initial(150.0).at_least(110.0).clip(true))
        .column(TableColumn::initial(90.0).at_least(60.0).clip(true))
        .column(TableColumn::initial(150.0).at_least(80.0).clip(true))
        .header(24.0, |mut header| {
            for (column, title) in headers {
                header.col(|ui| {
                    let arrow = match (sort.column == column, sort.ascending) {
                        (false, _) => "",
                        (true, true) => " \u{2191}",
                        (true, false) => " \u{2193}",
                    };
                    let text = egui::RichText::new(format!("{title}{arrow}")).strong();
                    if ui.add(egui::Button::new(text).frame(false)).clicked() {
                        action = Some(Action::SortBy(column));
                    }
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, rows.len(), |mut row| {
                let item = &rows[row.index()];
                let is_dir = matches!(item.target, Target::Dir(_));
                let selected = match item.target {
                    Target::File(_) => view.selection.contains(&item.name),
                    Target::Dir(_) => false,
                };
                row.set_selected(selected);
                let mut cells = Vec::with_capacity(4);

                let name = egui::RichText::new(&item.name);
                let name = if is_dir { name.strong() } else { name };
                cells.push(row.col(|ui| {
                    ui.add(egui::Label::new(name).selectable(false).truncate());
                }));
                cells.push(row.col(|ui| match item.date {
                    Some(date) => {
                        let text = date.when.format("%Y-%m-%d %H:%M").to_string();
                        // Dimmed when the date is only the file's modified time.
                        let text = if date.from_exif {
                            egui::RichText::new(text)
                        } else {
                            egui::RichText::new(text).weak()
                        };
                        ui.add(egui::Label::new(text).selectable(false));
                    }
                    None => {
                        ui.weak("--");
                    }
                }));
                cells.push(row.col(|ui| match item.size {
                    Some(bytes) => {
                        ui.add(egui::Label::new(format_size(bytes)).selectable(false));
                    }
                    None => {
                        ui.weak("--");
                    }
                }));
                cells.push(row.col(|ui| {
                    ui.add(egui::Label::new(tr(item.kind)).selectable(false));
                }));

                let responses: Vec<&egui::Response> = cells.iter().map(|(_, r)| r).collect();
                match item.target {
                    Target::Dir(i) => {
                        if responses.iter().any(|r| r.clicked()) {
                            action = Some(Action::Enter(listing.dirs[i].clone()));
                        }
                    }
                    Target::File(i) => {
                        if let Some(clicked) =
                            file_click(&responses[0].ctx, &responses, i, selected)
                        {
                            action = Some(clicked);
                        }
                    }
                }
                for response in responses {
                    response.context_menu(|ui| match item.target {
                        Target::Dir(i) => {
                            folder_menu(ui, view.dir_path.join(&listing.dirs[i]), &mut action)
                        }
                        Target::File(i) => {
                            file_menu(ui, view, &listing.files[i].path, i, &mut action)
                        }
                    });
                }
            });
        });
    action
}

fn cell(ui: &mut egui::Ui) -> (egui::Rect, egui::Response, egui::Painter) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(CELL_W, CELL_H), egui::Sense::click());
    let painter = ui.painter_at(rect);
    if response.hovered() {
        painter.rect_filled(rect, egui::CornerRadius::same(2), theme::HOVER);
    }
    (rect, response, painter)
}

fn caption(painter: &egui::Painter, rect: egui::Rect, text: &str, color: egui::Color32) {
    painter.text(
        egui::pos2(rect.center().x, rect.top() + IMAGE_H + 3.0),
        egui::Align2::CENTER_TOP,
        shorten(text, 24),
        egui::FontId::proportional(11.0),
        color,
    );
}

fn folder_cell(ui: &mut egui::Ui, name: &str) -> egui::Response {
    let (rect, response, painter) = cell(ui);
    let color = egui::Color32::from_rgb(0x5a, 0x5a, 0x5e);
    let body = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + IMAGE_H / 2.0 + 6.0),
        egui::vec2(88.0, 62.0),
    );
    let tab = egui::Rect::from_min_size(
        body.left_top() - egui::vec2(0.0, 10.0),
        egui::vec2(36.0, 14.0),
    );
    painter.rect_filled(tab, egui::CornerRadius::same(2), color);
    painter.rect_filled(body, egui::CornerRadius::same(3), color);
    caption(&painter, rect, name, theme::TEXT);
    response.on_hover_text(name)
}

/// Photo tile: a square thumbnail; the name is only shown (in white) while the photo
/// is selected, so the grid stays about the photos.
fn photo_cell(ui: &mut egui::Ui, path: &Path, tex: &Tex, selected: bool) -> egui::Response {
    let (rect, response, painter) = cell(ui);
    let image_area = egui::Rect::from_min_size(rect.min, egui::vec2(CELL_W, IMAGE_H));
    paint_cover(&painter, image_area, path, tex);
    let name = file_name_lossy(path);
    if selected {
        painter.rect_stroke(
            image_area,
            egui::CornerRadius::ZERO,
            egui::Stroke::new(2.0, theme::STRONG),
            egui::StrokeKind::Inside,
        );
        caption(&painter, rect, &name, theme::STRONG);
    }
    response.on_hover_text(name)
}

/// Fill `area` with the image, cropped to the area's shape (like CSS `object-fit: cover`).
/// While loading or when the file cannot be previewed, a dark tile with a hint is drawn.
pub fn paint_cover(painter: &egui::Painter, area: egui::Rect, path: &Path, tex: &Tex) {
    match tex {
        Tex::Ready(handle) => {
            let uv = cover_uv(handle.size_vec2(), area.size());
            painter.image(handle.id(), area, uv, egui::Color32::WHITE);
        }
        Tex::Loading => {
            painter.rect_filled(area, egui::CornerRadius::ZERO, theme::PANEL);
        }
        Tex::Failed => {
            painter.rect_filled(area, egui::CornerRadius::ZERO, theme::PANEL);
            let ext = path
                .extension()
                .map_or_else(String::new, |e| e.to_string_lossy().to_uppercase());
            painter.text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                ext,
                egui::FontId::proportional(14.0),
                theme::MUTED,
            );
        }
    }
}

/// Texture coordinates that crop an image of `image` size to the shape of `area`,
/// keeping the centre.
fn cover_uv(image: egui::Vec2, area: egui::Vec2) -> egui::Rect {
    let full = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    if image.x <= 0.0 || image.y <= 0.0 || area.x <= 0.0 || area.y <= 0.0 {
        return full;
    }
    let image_ratio = image.x / image.y;
    let area_ratio = area.x / area.y;
    if image_ratio > area_ratio {
        let w = area_ratio / image_ratio;
        egui::Rect::from_min_max(
            egui::pos2((1.0 - w) / 2.0, 0.0),
            egui::pos2((1.0 + w) / 2.0, 1.0),
        )
    } else {
        let h = image_ratio / area_ratio;
        egui::Rect::from_min_max(
            egui::pos2(0.0, (1.0 - h) / 2.0),
            egui::pos2(1.0, (1.0 + h) / 2.0),
        )
    }
}

/// How many cells of `cell_width` (separated by `spacing`) fit in `available`.
fn grid_columns(available: f32, cell_width: f32, spacing: f32) -> usize {
    (((available + spacing) / (cell_width + spacing)).floor() as usize).max(1)
}

/// Scale `size` to fit inside `max`, keeping the aspect ratio.
pub fn fit_size(size: egui::Vec2, max: egui::Vec2) -> egui::Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    size * (max.x / size.x).min(max.y / size.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_columns_fit_cells_and_spacing() {
        assert_eq!(grid_columns(1000.0, 160.0, 8.0), 6); // 6*160 + 5*8 = 1000
        assert_eq!(grid_columns(999.0, 160.0, 8.0), 5);
        assert_eq!(grid_columns(100.0, 160.0, 8.0), 1); // always at least one
    }

    #[test]
    fn cover_uv_crops_to_the_centre() {
        let full = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        assert_eq!(
            cover_uv(egui::vec2(100.0, 100.0), egui::vec2(50.0, 50.0)),
            full
        );
        let wide = cover_uv(egui::vec2(200.0, 100.0), egui::vec2(50.0, 50.0));
        assert_eq!(
            wide,
            egui::Rect::from_min_max(egui::pos2(0.25, 0.0), egui::pos2(0.75, 1.0))
        );
        let tall = cover_uv(egui::vec2(100.0, 400.0), egui::vec2(50.0, 50.0));
        assert_eq!(
            tall,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.375), egui::pos2(1.0, 0.625))
        );
    }

    #[test]
    fn fit_size_keeps_aspect_ratio() {
        let fitted = fit_size(egui::vec2(400.0, 200.0), egui::vec2(100.0, 100.0));
        assert_eq!(fitted, egui::vec2(100.0, 50.0));
        let tall = fit_size(egui::vec2(100.0, 400.0), egui::vec2(100.0, 100.0));
        assert_eq!(tall, egui::vec2(25.0, 100.0));
        assert_eq!(
            fit_size(egui::Vec2::ZERO, egui::vec2(10.0, 10.0)),
            egui::Vec2::ZERO
        );
    }

    /// Browser showing a staging folder with these files, the viewer on `open`.
    fn viewing(dir: &Path, names: &[&str], open: &str) -> BrowseView {
        for name in names {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let mut view = BrowseView::new(&egui::Context::default());
        let listing = list_dir(dir, "").unwrap();
        view.viewer = listing
            .files
            .iter()
            .position(|f| file_name_lossy(&f.path) == open);
        view.listing = Some(Ok(listing));
        view
    }

    #[test]
    fn viewer_moves_to_the_next_photo_after_a_trash() {
        let dir = tempfile::tempdir().unwrap();
        let mut view = viewing(dir.path(), &["a.jpg", "b.jpg", "c.jpg"], "b.jpg");

        view.file_removed(Side::Staging, "b.jpg");

        assert_eq!(view.viewer_next.as_deref(), Some("c.jpg"));
        assert!(view.listing.is_none(), "folder is re-read");
    }

    #[test]
    fn viewer_moves_back_after_trashing_the_last_photo() {
        let dir = tempfile::tempdir().unwrap();
        let mut view = viewing(dir.path(), &["a.jpg", "b.jpg"], "b.jpg");

        view.file_removed(Side::Staging, "b.jpg");

        assert_eq!(view.viewer_next.as_deref(), Some("a.jpg"));
    }

    #[test]
    fn viewer_closes_after_trashing_the_only_photo() {
        let dir = tempfile::tempdir().unwrap();
        let mut view = viewing(dir.path(), &["a.jpg"], "a.jpg");

        view.file_removed(Side::Staging, "a.jpg");

        assert_eq!((view.viewer, view.viewer_next), (None, None));
    }

    #[test]
    fn removing_another_file_keeps_the_viewer() {
        let dir = tempfile::tempdir().unwrap();
        let mut view = viewing(dir.path(), &["a.jpg", "b.jpg"], "a.jpg");

        view.file_removed(Side::Library, "a.jpg");
        assert_eq!(view.viewer, Some(0)); // other side: nothing changes

        view.file_removed(Side::Staging, "b.jpg");
        assert_eq!(view.viewer_next.as_deref(), Some("a.jpg"));
    }
}
