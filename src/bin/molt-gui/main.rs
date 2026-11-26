//! Molt GUI — open an archive, pick files, extract, and watch the archive
//! shed its skin. Ships as a single portable executable.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window on Windows

#[cfg(windows)]
mod dragout;
#[cfg(windows)]
mod shell;
mod theme;
#[cfg(windows)]
mod update;

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Stroke, Vec2};
use molt::formats::{self, Event};
use molt::util::human;
use std::fs;
use std::path::{Path, PathBuf};
use theme::dim;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

const ARCHIVE_EXTS: &[&str] = &[
    "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "tzst",
];

// ---------------------------------------------------------------- entry data

#[derive(Clone)]
struct Entry {
    name: String,
    size: u64,
    is_dir: bool,
    selected: bool,
    status: Status,
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Pending,
    Extracting,
    Freed,     // extracted, verified, bytes punched
    Extracted, // extracted; bytes not (yet) freed
    Resumed,   // already extracted and hollowed by an earlier run
    Failed,
}

/// How the status-line message should read: plain, good news, heads-up, or
/// a failure. Drives the colour and icon in the status bar.
#[derive(Clone, Copy, PartialEq, Default)]
enum Tone {
    #[default]
    Info,
    Ok,
    Warn,
    Err,
}

impl Tone {
    fn color(self) -> Color32 {
        match self {
            Tone::Info => theme::TEXT_DIM,
            Tone::Ok => theme::OK,
            Tone::Warn => theme::WARN,
            Tone::Err => theme::ERR,
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Tone::Info => "",
            Tone::Ok => "✔",
            Tone::Warn => "⚠",
            Tone::Err => "✖",
        }
    }
}

// -------------------------------------------------------------- worker msgs

enum Msg {
    Started(usize),
    Done(usize),
    Resumed(usize),
    Freed { bytes: u64, indices: Vec<usize> },
    Error { index: usize, err: String },
    Note(String),
    Fatal(String),
    Finished { archive_deleted: bool, reclaimed: u64 },
}

// ---------------------------------------------------------------------- app

struct MoltApp {
    archive_path: Option<PathBuf>,
    /// File name of the open archive; kept after it's consumed so the
    /// header can still say what was molted.
    archive_name: String,
    /// The last run consumed the archive (it's gone from disk).
    consumed: bool,
    kind: &'static str,
    entries: Vec<Entry>,
    archive_size: u64,
    freed: u64,
    molt_mode: bool, // true = consume archive, false = classic extract
    out_dir: Option<PathBuf>,
    rx: Option<Receiver<Msg>>,
    busy: bool,
    log: String,
    tone: Tone,
    /// App icon as a texture, for the header and the "Molt here" dialog.
    logo: Option<egui::TextureHandle>,
    confirm_pending: bool,
    pending_title: Option<String>,
    jobs_total: usize,
    jobs_done: usize,
    /// Temp folders backing drag-out operations; removed on exit.
    drag_temp_dirs: Vec<PathBuf>,
    drag_seq: u32,
    /// Password for the currently open archive, once the user supplied one.
    password: Option<String>,
    /// A password dialog is showing for this archive path.
    password_request: Option<PathBuf>,
    password_input: String,
    password_wrong: bool,
    /// "Molt here": no file browser, just warn → consume → done. Launched
    /// via `--molt-here <archive>` from the Explorer context menu.
    minimal: bool,
    /// Self-updater state and the channel a background check/download reports on.
    #[cfg(windows)]
    update: UpdateUi,
    #[cfg(windows)]
    update_rx: Option<Receiver<UpdateMsg>>,
}

// ------------------------------------------------------------- self-update UI

#[cfg(windows)]
#[derive(Default)]
enum UpdateUi {
    /// No update dialog showing.
    #[default]
    Hidden,
    Checking,
    UpToDate(String),
    Available(update::Update),
    Downloading { got: u64, total: Option<u64> },
    Failed(String),
}

#[cfg(windows)]
enum UpdateMsg {
    Result(Result<update::Check, String>),
    Progress { got: u64, total: Option<u64> },
    /// The exe was replaced; relaunch is imminent.
    Applied,
    ApplyFailed(String),
}

impl Default for MoltApp {
    fn default() -> Self {
        Self {
            archive_path: None,
            archive_name: String::new(),
            consumed: false,
            kind: "",
            entries: Vec::new(),
            archive_size: 0,
            freed: 0,
            molt_mode: true,
            out_dir: None,
            rx: None,
            busy: false,
            log: String::from("Ready."),
            tone: Tone::Info,
            logo: None,
            confirm_pending: false,
            pending_title: None,
            jobs_total: 0,
            jobs_done: 0,
            drag_temp_dirs: Vec::new(),
            drag_seq: 0,
            password: None,
            password_request: None,
            password_input: String::new(),
            password_wrong: false,
            minimal: false,
            #[cfg(windows)]
            update: UpdateUi::Hidden,
            #[cfg(windows)]
            update_rx: None,
        }
    }
}

/// Default destination: folder named after the archive, next to it.
fn default_out_dir(path: &std::path::Path) -> PathBuf {
    let mut stem = path.file_stem().map(PathBuf::from).unwrap_or_default();
    if stem.extension().is_some_and(|e| e.eq_ignore_ascii_case("tar")) {
        stem = stem.with_extension("");
    }
    path.with_file_name(stem)
}

impl MoltApp {
    fn say(&mut self, tone: Tone, msg: impl Into<String>) {
        self.tone = tone;
        self.log = msg.into();
    }

    fn logo_texture(&mut self, ctx: &egui::Context) -> egui::TextureHandle {
        self.logo
            .get_or_insert_with(|| {
                let icon = load_icon();
                ctx.load_texture(
                    "molt-logo",
                    egui::ColorImage::from_rgba_unmultiplied([64, 64], &icon.rgba),
                    egui::TextureOptions::LINEAR,
                )
            })
            .clone()
    }

    /// Ask for an archive with the system file picker and open it.
    fn pick_archive(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Archives", ARCHIVE_EXTS)
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.open_archive(p);
        }
    }

    fn pick_destination(&mut self) {
        let mut dlg = rfd::FileDialog::new();
        if let Some(parent) = self.out_dir.as_deref().and_then(Path::parent) {
            dlg = dlg.set_directory(parent);
        }
        if let Some(d) = dlg.pick_folder() {
            self.out_dir = Some(d);
        }
    }

    /// Files (not folders) in the archive.
    fn files(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| !e.is_dir)
    }

    /// Ticked files that haven't been extracted yet — what the next run does.
    fn to_extract(&self) -> (usize, u64) {
        self.files()
            .filter(|e| e.selected && e.status == Status::Pending)
            .fold((0, 0), |(n, b), e| (n + 1, b + e.size))
    }

    /// A dialog is up; everything behind it is blocked.
    fn modal_open(&self) -> bool {
        #[cfg(windows)]
        let updating = !matches!(self.update, UpdateUi::Hidden);
        #[cfg(not(windows))]
        let updating = false;
        self.password_request.is_some() || (self.confirm_pending && !self.minimal) || updating
    }

    fn open_archive(&mut self, path: PathBuf) {
        // A fresh slate, minus what belongs to the session rather than to
        // the previous archive.
        let fresh = MoltApp {
            password: self.password.take(),
            minimal: self.minimal,
            logo: self.logo.take(),
            drag_temp_dirs: std::mem::take(&mut self.drag_temp_dirs),
            drag_seq: self.drag_seq,
            #[cfg(windows)]
            update: std::mem::take(&mut self.update),
            #[cfg(windows)]
            update_rx: self.update_rx.take(),
            ..MoltApp::default()
        };
        *self = fresh;
        if self.minimal {
            self.molt_mode = true;
        }
        self.archive_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.say(Tone::Info, "Reading archive…");
        let mut needs_password = false;
        match (|| -> Result<(), String> {
            self.archive_size = fs::metadata(&path).map_err(|e| e.to_string())?.len();
            let backend = formats::open_with_password(&path, self.password.as_deref())
                .map_err(|e| {
                    if formats::is_password_error(&e) {
                        needs_password = true;
                    }
                    e.to_string()
                })?;
            needs_password = backend.needs_password();
            self.kind = backend.kind();
            self.entries = backend
                .entries()
                .iter()
                .map(|e| Entry {
                    name: e.name.clone(),
                    size: e.size,
                    is_dir: e.is_dir,
                    selected: true,
                    status: Status::Pending,
                })
                .collect();
            Ok(())
        })() {
            Ok(()) if needs_password => {
                // Contents are encrypted: ask, then reopen with the password.
                self.password_wrong = self.password.is_some();
                self.password = None;
                self.password_request = Some(path);
                self.say(Tone::Info, "This archive is password-protected.");
            }
            Ok(()) => {
                self.say(Tone::Info, "Ready — untick anything you don't need.");
                self.pending_title = Some(format!("Molt — {}", self.archive_name));
                self.out_dir = Some(default_out_dir(&path));
                self.archive_path = Some(path);
                if self.minimal {
                    self.confirm_pending = true;
                }
            }
            Err(e) if needs_password => {
                self.password_wrong = self.password.is_some();
                self.password = None;
                self.password_request = Some(path);
                self.say(Tone::Warn, e);
            }
            Err(e) => {
                self.archive_name.clear();
                self.say(Tone::Err, format!("Couldn't open that archive: {e}"));
            }
        }
    }

    fn start_extraction(&mut self) {
        let Some(archive) = self.archive_path.clone() else { return };
        let Some(out_dir) = self.out_dir.clone() else { return };
        let molt_mode = self.molt_mode;
        let password = self.password.clone();
        let selected: Vec<bool> = self
            .entries
            .iter()
            .map(|e| e.selected && e.status == Status::Pending)
            .collect();
        let n_selected = selected.iter().filter(|&&s| s).count();
        if n_selected == 0 {
            self.say(Tone::Warn, "Nothing selected — tick at least one file.");
            return;
        }
        self.say(Tone::Info, "Extracting…");
        self.jobs_total = n_selected;
        self.jobs_done = 0;

        let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
        self.rx = Some(rx);
        self.busy = true;

        thread::spawn(move || {
            // The archive's size — what's reclaimed when the run consumes
            // it, whether the bytes were punched incrementally or freed by
            // the final delete. (summary.freed counts only the incremental
            // punching, which is 0 for formats we can't punch, e.g. header-
            // encrypted rar — that was the "0 B reclaimed" bug.)
            let archive_size = fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
            let result = (|| -> Result<(bool, u64), String> {
                let mut backend =
                    formats::open_with_password(&archive, password.as_deref())
                        .map_err(|e| e.to_string())?;
                let summary = backend
                    .extract(
                        &formats::ExtractOptions {
                            out_dir: &out_dir,
                            selected: Some(&selected),
                            molt: molt_mode,
                        },
                        &mut |ev| {
                            let _ = tx.send(match ev {
                                Event::Started { index } => Msg::Started(index),
                                Event::Done { index } => Msg::Done(index),
                                Event::Resumed { index } => Msg::Resumed(index),
                                Event::Freed { bytes, indices } => Msg::Freed { bytes, indices },
                                Event::Error { index, message } => {
                                    Msg::Error { index, err: message }
                                }
                                Event::Note(m) => Msg::Note(m),
                            });
                        },
                    )
                    .map_err(|e| e.to_string())?;
                // Everything out and verified → delete the hollow shell.
                let mut deleted = false;
                if summary.all_out {
                    drop(backend);
                    deleted = fs::remove_file(&archive).is_ok();
                }
                // Consumed → the whole archive came back; otherwise report
                // just the bytes punched so far.
                let reclaimed = if deleted { archive_size } else { summary.freed };
                Ok((deleted, reclaimed))
            })();
            match result {
                Ok((deleted, reclaimed)) => {
                    let _ = tx.send(Msg::Finished { archive_deleted: deleted, reclaimed });
                }
                Err(e) => {
                    let _ = tx.send(Msg::Fatal(e));
                    let _ = tx.send(Msg::Finished { archive_deleted: false, reclaimed: 0 });
                }
            }
        });
    }

    /// Drag entry `index` (plus the rest of the selection, if the dragged
    /// row is part of it) out of the window: extract copies to a temp
    /// folder — never consuming the archive — then hand them to OLE.
    #[cfg(windows)]
    fn drag_out(&mut self, index: usize) {
        let Some(archive) = self.archive_path.clone() else { return };
        let indices: Vec<usize> = if self.entries[index].selected {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.selected && !e.is_dir)
                .map(|(i, _)| i)
                .collect()
        } else {
            vec![index]
        };

        let tmp = std::env::temp_dir()
            .join(format!("molt-dnd-{}-{}", std::process::id(), self.drag_seq));
        self.drag_seq += 1;
        let mut selected = vec![false; self.entries.len()];
        indices.iter().for_each(|&i| selected[i] = true);

        let result = (|| -> Result<Vec<PathBuf>, String> {
            let mut backend =
                formats::open_with_password(&archive, self.password.as_deref())
                    .map_err(|e| e.to_string())?;
            let summary = backend
                .extract(
                    &formats::ExtractOptions {
                        out_dir: &tmp,
                        selected: Some(&selected),
                        molt: false, // drag-out is always a non-destructive copy
                    },
                    &mut |_| {},
                )
                .map_err(|e| e.to_string())?;
            if summary.failed > 0 {
                return Err(format!("{} entr(ies) failed to extract", summary.failed));
            }
            let mut paths: Vec<PathBuf> = indices
                .iter()
                .filter_map(|&i| molt::util::safe_join(&tmp, &self.entries[i].name))
                .filter(|p| p.exists())
                .collect();
            paths.dedup();
            if paths.is_empty() {
                return Err("nothing extracted".into());
            }
            Ok(paths)
        })();

        match result {
            Ok(paths) => {
                self.drag_temp_dirs.push(tmp);
                let n = paths.len();
                if dragout::start_drag(&paths) {
                    self.say(
                        Tone::Ok,
                        format!(
                            "Copied {n} file{} out — the archive is unchanged.",
                            if n == 1 { "" } else { "s" }
                        ),
                    );
                } else {
                    self.say(Tone::Info, "Drag cancelled.");
                }
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                self.say(Tone::Err, format!("Drag-out failed: {e}"));
            }
        }
    }

    // ------------------------------------------------------------- updates

    /// Kick off a background "check for updates" (does nothing if one is
    /// already running).
    #[cfg(windows)]
    fn start_update_check(&mut self) {
        if matches!(self.update, UpdateUi::Checking | UpdateUi::Downloading { .. }) {
            return;
        }
        self.update = UpdateUi::Checking;
        let (tx, rx) = channel();
        self.update_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(UpdateMsg::Result(update::check()));
        });
    }

    /// Download and install the given update on a background thread.
    #[cfg(windows)]
    fn start_update_apply(&mut self, upd: update::Update) {
        self.update = UpdateUi::Downloading { got: 0, total: None };
        let (tx, rx) = channel();
        self.update_rx = Some(rx);
        thread::spawn(move || {
            let ptx = tx.clone();
            let result = update::download_and_apply(&upd, |got, total| {
                let _ = ptx.send(UpdateMsg::Progress { got, total });
            });
            match result {
                Ok(()) => {
                    let _ = tx.send(UpdateMsg::Applied);
                }
                Err(e) => {
                    let _ = tx.send(UpdateMsg::ApplyFailed(e));
                }
            }
        });
    }

    /// True while a check/download is in flight (keeps the UI repainting).
    #[cfg(windows)]
    fn update_active(&self) -> bool {
        matches!(self.update, UpdateUi::Checking | UpdateUi::Downloading { .. })
    }
    #[cfg(not(windows))]
    fn update_active(&self) -> bool {
        false
    }

    #[cfg(windows)]
    fn pump_update(&mut self) {
        let Some(rx) = &self.update_rx else { return };
        let mut done = false;
        let mut relaunch = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                UpdateMsg::Result(Ok(update::Check::Available(u))) => {
                    self.update = UpdateUi::Available(u);
                    done = true;
                }
                UpdateMsg::Result(Ok(update::Check::UpToDate(v))) => {
                    self.update = UpdateUi::UpToDate(v);
                    done = true;
                }
                UpdateMsg::Result(Err(e)) => {
                    self.update = UpdateUi::Failed(e);
                    done = true;
                }
                UpdateMsg::Progress { got, total } => {
                    self.update = UpdateUi::Downloading { got, total };
                }
                UpdateMsg::Applied => {
                    relaunch = true;
                }
                UpdateMsg::ApplyFailed(e) => {
                    self.update = UpdateUi::Failed(e);
                    done = true;
                }
            }
        }
        if relaunch {
            // Exe already swapped on disk → start the new one and quit.
            update::relaunch();
        }
        if done {
            self.update_rx = None;
        }
    }

    fn pump_messages(&mut self) {
        // Taken out while draining so the handlers below can borrow `self`.
        let Some(rx) = self.rx.take() else { return };
        let mut finished = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Started(i) => {
                    if let Some(e) = self.entries.get_mut(i) {
                        e.status = Status::Extracting;
                    }
                }
                Msg::Done(i) => {
                    self.jobs_done += 1;
                    if let Some(e) = self.entries.get_mut(i) {
                        if e.status != Status::Freed {
                            e.status = Status::Extracted;
                        }
                    }
                }
                Msg::Resumed(i) => {
                    self.jobs_done += 1;
                    if let Some(e) = self.entries.get_mut(i) {
                        e.status = Status::Resumed;
                    }
                }
                Msg::Freed { bytes, indices } => {
                    self.freed += bytes;
                    for i in indices {
                        if let Some(e) = self.entries.get_mut(i) {
                            e.status = Status::Freed;
                        }
                    }
                }
                Msg::Error { index, err } => {
                    self.jobs_done += 1;
                    if let Some(e) = self.entries.get_mut(index) {
                        e.status = Status::Failed;
                        let msg = format!("{}: {}", e.name, err);
                        self.say(Tone::Err, msg);
                    }
                }
                Msg::Note(m) => self.say(Tone::Warn, m),
                Msg::Fatal(e) => self.say(Tone::Err, format!("Extraction failed: {e}")),
                Msg::Finished { archive_deleted, reclaimed } => {
                    finished = true;
                    self.busy = false;
                    // Authoritative total from the worker (whole footprint on
                    // consume, punched bytes otherwise) — keeps the counter
                    // right even for formats we can't punch incrementally.
                    self.freed = reclaimed;
                    let failed = self.files().any(|e| e.status == Status::Failed);
                    if archive_deleted {
                        self.say(
                            Tone::Ok,
                            format!("Done — archive consumed, {} reclaimed.", human(self.freed)),
                        );
                        self.archive_path = None;
                        self.consumed = true;
                        self.pending_title = Some("Molt".to_string());
                    } else if failed || self.tone == Tone::Err {
                        // keep the error visible
                    } else if self.molt_mode {
                        if self.freed > 0 {
                            self.say(
                                Tone::Ok,
                                format!(
                                    "Done — {} reclaimed so far. The rest of the archive is kept.",
                                    human(self.freed)
                                ),
                            );
                        } else if self.tone != Tone::Warn {
                            self.say(Tone::Ok, "Done.");
                        }
                        // otherwise keep the last note visible
                    } else {
                        self.say(Tone::Ok, "Done — files extracted, archive left as it was.");
                    }
                }
            }
        }
        if !finished {
            self.rx = Some(rx);
        }
    }
}

impl eframe::App for MoltApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Drop the temp copies backing past drag-outs. Explorer has long
        // finished its copy by the time the app closes.
        for dir in self.drag_temp_dirs.drain(..) {
            let _ = fs::remove_dir_all(dir);
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_messages();
        #[cfg(windows)]
        self.pump_update();
        if self.busy || self.update_active() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if let Some(t) = self.pending_title.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(t));
        }

        // Drag-and-drop an archive onto the window
        let modal = self.modal_open();
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if let Some(p) = dropped.into_iter().next() {
            if !self.busy && !modal {
                self.open_archive(p);
            }
        }

        if self.minimal {
            // "Molt here" is a bare dialog: no header, list or footer.
            self.minimal_ui(ctx);
        } else {
            let open_shortcut =
                egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
            if !self.busy && !modal && ctx.input_mut(|i| i.consume_shortcut(&open_shortcut)) {
                self.pick_archive();
            }
            self.header_ui(ctx);
            self.footer_ui(ctx);
            self.contents_ui(ctx);
        }

        if self.modal_open() {
            theme::dim_background(ctx);
        }
        self.password_dialog(ctx);
        if self.confirm_pending && !self.minimal {
            self.confirm_dialog(ctx);
        }
        #[cfg(windows)]
        self.update_dialog(ctx);
    }
}

/// Width of the left-hand labels in the footer ("Extract to", "Mode").
const LABEL_W: f32 = 78.0;

fn row_label(ui: &mut egui::Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(LABEL_W, 26.0), Sense::hover());
    ui.painter().text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(14.0),
        theme::TEXT_DIM,
    );
}

/// A sunken, clickable box showing a path (truncated to fit).
fn path_box(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let size = Vec2::new(ui.available_width().max(60.0), 30.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let border = if enabled && resp.hovered() { theme::ACCENT } else { theme::BORDER };
    ui.painter().rect(rect, 6.0, theme::SUNKEN, Stroke::new(1.0_f32, border));
    let mut inner = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(10.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    inner.add(egui::Label::new(RichText::new(text).color(theme::TEXT)).truncate().selectable(false));
    let resp = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    resp.on_hover_text(text)
}

/// "photos/2024/IMG_1.jpg" → ("photos/2024/", "IMG_1.jpg")
fn split_name(name: &str) -> (&str, &str) {
    match name.rfind(['/', '\\']) {
        Some(p) => name.split_at(p + 1),
        None => ("", name),
    }
}

fn status_cell(ui: &mut egui::Ui, status: Status, selected: bool, busy: bool) {
    match status {
        Status::Pending if !selected => {
            ui.label(dim("—"));
        }
        Status::Pending => {
            ui.label(dim(if busy { "Queued" } else { "Ready" }));
        }
        Status::Extracting => {
            ui.add(egui::Spinner::new().size(13.0));
            ui.label(RichText::new("Extracting…").color(theme::INFO));
        }
        Status::Freed => {
            ui.label(RichText::new("✔ Extracted · freed").color(theme::OK))
                .on_hover_text("Extracted and verified; its space in the archive was given back");
        }
        Status::Extracted => {
            ui.label(RichText::new("✔ Extracted").color(theme::INFO));
        }
        Status::Resumed => {
            ui.label(RichText::new("✔ Already extracted").color(theme::OK))
                .on_hover_text("Extracted by an earlier, interrupted run");
        }
        Status::Failed => {
            ui.label(RichText::new("✖ Failed").color(theme::ERR));
        }
    }
}

/// Open a folder in the system file manager.
fn reveal(path: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Pin a right-aligned row of buttons to the bottom of the remaining space.
fn bottom_buttons(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), 32.0),
            Layout::right_to_left(Align::Center),
            add,
        );
    });
}

impl MoltApp {
    /// One line under the archive name: what it is and how big.
    fn subtitle(&self) -> String {
        if self.consumed {
            return format!("Consumed — {} reclaimed", human(self.freed));
        }
        let files = self.files().count();
        let total: u64 = self.files().map(|e| e.size).sum();
        format!(
            "{} archive · {} file{} · {} packed, {} unpacked",
            self.kind.to_uppercase(),
            files,
            if files == 1 { "" } else { "s" },
            human(self.archive_size),
            human(total),
        )
    }

    // ------------------------------------------------------------ header

    fn header_ui(&mut self, ctx: &egui::Context) {
        let logo = self.logo_texture(ctx);
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::none()
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0)),
            )
            .show(ctx, |ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), 38.0),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        #[cfg(windows)]
                        self.settings_menu(ui);
                        if ui
                            .add_enabled(!self.busy, theme::secondary("Open archive…"))
                            .on_hover_text(
                                "Choose an archive to open (Ctrl+O).\nYou can also drop one onto the window.",
                            )
                            .clicked()
                        {
                            self.pick_archive();
                        }
                        ui.add_space(8.0);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                                logo.id(),
                                [34.0, 34.0],
                            )));
                            ui.add_space(2.0);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                let (title, sub) = if self.archive_name.is_empty() {
                                    (
                                        "Molt".to_string(),
                                        "Extract archives without doubling your disk usage"
                                            .to_string(),
                                    )
                                } else {
                                    (self.archive_name.clone(), self.subtitle())
                                };
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(title).font(theme::semibold(17.0)),
                                    )
                                    .truncate(),
                                );
                                ui.add(egui::Label::new(dim(sub).small()).truncate());
                            });
                        });
                    },
                );
            });
    }

    #[cfg(windows)]
    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy;
        ui.menu_button("⚙  Settings", |ui| {
            ui.set_min_width(260.0);
            ui.label(dim("Explorer right-click menu").small());
            if ui
                .button("Add “Open with Molt” and “Molt here”")
                .on_hover_text("Adds Molt to the right-click menu of archive files (just for you, no admin needed)")
                .clicked()
            {
                match shell::register() {
                    Ok(()) => self.say(Tone::Ok, "Added to the right-click menu for archives."),
                    Err(e) => self.say(Tone::Err, format!("Couldn't add to the right-click menu: {e}")),
                }
                ui.close_menu();
            }
            if ui.button("Remove Molt from the right-click menu").clicked() {
                match shell::unregister() {
                    Ok(()) => self.say(Tone::Ok, "Removed from the right-click menu."),
                    Err(e) => self.say(Tone::Err, format!("Couldn't remove from the right-click menu: {e}")),
                }
                ui.close_menu();
            }
            ui.separator();
            if ui
                .add_enabled(!busy, egui::Button::new("Check for updates…"))
                .on_disabled_hover_text("Wait for the extraction to finish")
                .clicked()
            {
                self.start_update_check();
                ui.close_menu();
            }
            ui.separator();
            ui.label(dim(format!("Molt {}", molt::VERSION)).small());
        });
    }

    // ------------------------------------------------------------ footer

    fn footer_ui(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("footer")
            .frame(
                egui::Frame::none()
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                if !self.entries.is_empty() {
                    self.destination_row(ui);
                    self.mode_row(ui);
                    ui.separator();
                }
                if self.busy && self.jobs_total > 0 {
                    let color = if self.molt_mode { theme::MOLT } else { theme::ACCENT };
                    ui.add(
                        egui::ProgressBar::new(self.jobs_done as f32 / self.jobs_total as f32)
                            .text(format!("{} of {} files", self.jobs_done, self.jobs_total))
                            .fill(color)
                            .desired_height(18.0),
                    );
                }
                self.status_row(ui);
            });
    }

    fn destination_row(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy;
        let mut pick = false;
        ui.horizontal(|ui| {
            row_label(ui, "Extract to");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(!busy, theme::secondary("Change…"))
                    .on_hover_text("Pick a different destination folder")
                    .clicked()
                {
                    pick = true;
                }
                if let Some(d) = self.out_dir.as_deref().filter(|d| d.is_dir()) {
                    if ui
                        .add(theme::secondary("Open folder"))
                        .on_hover_text("Open the destination folder")
                        .clicked()
                    {
                        reveal(d);
                    }
                }
                let text = self
                    .out_dir
                    .as_ref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_else(|| "Choose a folder…".into());
                if path_box(ui, &text, !busy).clicked() {
                    pick = true;
                }
            });
        });
        if pick {
            self.pick_destination();
        }
    }

    fn mode_row(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy;
        let consumed = self.consumed;
        let (n, bytes) = self.to_extract();
        let mut go = false;
        ui.horizontal(|ui| {
            row_label(ui, "Mode");
            theme::segmented(
                ui,
                &mut self.molt_mode,
                &[
                    (
                        false,
                        "Keep archive",
                        "Copy the files out and leave the archive exactly as it was.",
                        theme::ACCENT,
                    ),
                    (
                        true,
                        "Consume archive",
                        "Free the archive's disk space as each file is extracted and verified.\nOnce everything is out, the archive is deleted.",
                        theme::MOLT,
                    ),
                ],
                !busy && !consumed,
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (label, color) = if self.molt_mode {
                    ("Extract & consume", theme::MOLT)
                } else {
                    ("Extract", theme::ACCENT)
                };
                let blocked = if consumed {
                    Some("This archive has been consumed — open another one")
                } else if busy {
                    Some("Extraction in progress")
                } else if self.out_dir.is_none() {
                    Some("Choose a destination folder first")
                } else if n == 0 {
                    Some("Tick at least one file to extract")
                } else {
                    None
                };
                let resp = ui.add_enabled(
                    blocked.is_none(),
                    theme::primary(label, color).min_size(Vec2::new(150.0, 32.0)),
                );
                let resp = match blocked {
                    Some(why) => resp.on_disabled_hover_text(why),
                    None => resp.on_hover_text(format!(
                        "{n} file{}, {}",
                        if n == 1 { "" } else { "s" },
                        human(bytes)
                    )),
                };
                if resp.clicked() {
                    go = true;
                }
            });
        });
        ui.horizontal(|ui| {
            ui.add_space(LABEL_W + ui.spacing().item_spacing.x);
            let text = if self.molt_mode {
                "Space is freed as each file is verified, and the archive is deleted once everything is out. There's no undo."
            } else {
                "Files are copied out. The archive is left as it was."
            };
            ui.add(egui::Label::new(dim(text).small()).wrap());
        });
        if go {
            if self.molt_mode {
                self.confirm_pending = true;
            } else {
                self.start_extraction();
            }
        }
    }

    fn status_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.freed > 0 {
                    ui.label(
                        RichText::new(format!("{} reclaimed", human(self.freed)))
                            .font(theme::semibold(14.0))
                            .color(theme::OK),
                    )
                    .on_hover_text("Disk space given back so far");
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    if self.busy {
                        ui.add(egui::Spinner::new().size(14.0));
                    } else if !self.tone.icon().is_empty() {
                        ui.label(RichText::new(self.tone.icon()).color(self.tone.color()));
                    }
                    ui.add(
                        egui::Label::new(RichText::new(&self.log).color(self.tone.color()))
                            .truncate(),
                    );
                });
            });
        });
    }

    // ---------------------------------------------------------- contents

    fn contents_ui(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0)),
            )
            .show(ctx, |ui| {
                if self.entries.is_empty() {
                    self.empty_state(ui);
                } else {
                    self.file_table(ui);
                }
            });
    }

    /// Nothing open: a big drop target with one obvious button.
    fn empty_state(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let dropping = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let (fill, stroke) = if dropping {
            (Color32::from_rgb(0x1e, 0x27, 0x36), Stroke::new(2.0_f32, theme::ACCENT))
        } else {
            (Color32::from_rgb(0x1f, 0x20, 0x24), Stroke::new(1.0_f32, theme::BORDER))
        };
        ui.painter().rect(rect, 12.0, fill, stroke);

        let mut open = false;
        let mut inner = ui.new_child(
            egui::UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Center)),
        );
        inner.add_space(((rect.height() - 200.0) / 2.0).max(12.0));
        let empty_archive = self.archive_path.is_some();
        inner.label(RichText::new("📦").size(48.0).color(theme::TEXT_DIM));
        inner.add_space(4.0);
        let title = if dropping {
            "Drop to open"
        } else if empty_archive {
            "This archive is empty"
        } else {
            "Drop an archive here"
        };
        inner.label(RichText::new(title).font(theme::semibold(19.0)));
        inner.label(dim(if empty_archive { "There's nothing inside to extract." } else { "or" }));
        inner.add_space(6.0);
        let label = if empty_archive { "Open another archive…" } else { "Open archive…" };
        if inner
            .add(theme::primary(label, theme::ACCENT).min_size(Vec2::new(180.0, 34.0)))
            .on_hover_text("Ctrl+O")
            .clicked()
        {
            open = true;
        }
        inner.add_space(12.0);
        inner.label(dim("zip · 7z · rar · tar · gz · bz2 · xz · zst").small());

        ui.allocate_rect(rect, Sense::hover());
        if open {
            self.pick_archive();
        }
    }

    fn file_table(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy;
        let rows: Vec<usize> =
            self.entries.iter().enumerate().filter(|(_, e)| !e.is_dir).map(|(i, _)| i).collect();
        let n_files = rows.len();
        let (n_sel, sel_bytes) = self
            .files()
            .filter(|e| e.selected)
            .fold((0usize, 0u64), |(n, b), e| (n + 1, b + e.size));

        ui.horizontal(|ui| {
            ui.label(RichText::new("Contents").font(theme::semibold(15.0)));
            ui.label(dim(format!(
                "{n_sel} of {n_files} file{} selected · {}",
                if n_files == 1 { "" } else { "s" },
                human(sel_bytes)
            )));
            #[cfg(windows)]
            if !busy {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(dim("Tip: drag files into Explorer to copy them out").small());
                });
            }
        });
        ui.add_space(4.0);

        let mut drag_request: Option<usize> = None;
        let mut set_all: Option<bool> = None;
        let entries = &mut self.entries;
        egui::Frame::none()
            .fill(theme::SURFACE)
            .rounding(8.0)
            .stroke(Stroke::new(1.0_f32, theme::BORDER))
            .inner_margin(egui::Margin::symmetric(8.0, 6.0))
            .show(ui, |ui| {
                // Rows are drag sources, not text: no I-beam cursor anywhere
                // in the table, and the whole row (not just the name) drags.
                ui.style_mut().interaction.selectable_labels = false;
                use egui_extras::{Column, TableBuilder};
                TableBuilder::new(ui)
                    .striped(true)
                    .sense(Sense::click_and_drag())
                    .auto_shrink([false, false])
                    .max_scroll_height(f32::INFINITY)
                    .cell_layout(Layout::left_to_right(Align::Center))
                    .column(Column::exact(28.0))
                    .column(Column::remainder().clip(true))
                    .column(Column::exact(96.0))
                    .column(Column::exact(170.0))
                    .header(26.0, |mut header| {
                        header.col(|ui| {
                            let mut all = n_files > 0 && n_sel == n_files;
                            let mixed = n_sel > 0 && n_sel < n_files;
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Checkbox::without_text(&mut all).indeterminate(mixed),
                                )
                                .on_hover_text("Select all / none")
                                .changed()
                            {
                                set_all = Some(all);
                            }
                        });
                        header.col(|ui| {
                            ui.label(dim("Name"));
                        });
                        header.col(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.add_space(14.0);
                                ui.label(dim("Size"));
                            });
                        });
                        header.col(|ui| {
                            ui.label(dim("Status"));
                        });
                    })
                    .body(|body| {
                        body.rows(26.0, rows.len(), |mut row| {
                            let i = rows[row.index()];
                            let e = &mut entries[i];
                            let mut box_clicked = false;
                            row.col(|ui| {
                                box_clicked = ui
                                    .add_enabled(!busy, egui::Checkbox::without_text(&mut e.selected))
                                    .clicked();
                            });
                            row.col(|ui| {
                                let (dir, file) = split_name(&e.name);
                                let fg = if e.selected { theme::TEXT } else { theme::TEXT_DIM };
                                ui.spacing_mut().item_spacing.x = 0.0;
                                if !dir.is_empty() {
                                    ui.label(RichText::new(dir).color(theme::TEXT_DIM));
                                }
                                ui.label(RichText::new(file).color(fg));
                            });
                            row.col(|ui| {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.add_space(14.0);
                                    ui.label(dim(human(e.size)));
                                });
                            });
                            row.col(|ui| status_cell(ui, e.status, e.selected, busy));

                            if !busy {
                                let resp = row.response();
                                // Click anywhere on the row toggles it.
                                if resp.clicked() && !box_clicked {
                                    e.selected = !e.selected;
                                }
                                // Whole-row drag → copy the file out to Explorer.
                                if cfg!(windows) {
                                    let resp = resp.on_hover_cursor(egui::CursorIcon::Grab);
                                    if resp.drag_started() {
                                        drag_request = Some(i);
                                    }
                                }
                            }
                        });
                    });
            });

        if let Some(v) = set_all {
            self.entries.iter_mut().for_each(|e| e.selected = v);
        }
        #[cfg(windows)]
        if let Some(i) = drag_request {
            self.drag_out(i);
        }
        #[cfg(not(windows))]
        let _ = drag_request;
    }

    // ------------------------------------------------- "Molt here" dialog

    fn minimal_ui(&mut self, ctx: &egui::Context) {
        let logo = self.logo_texture(ctx);
        let (n, bytes) = self.to_extract();
        let mut start = false;
        let mut close = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::BG).inner_margin(egui::Margin::same(20.0)))
            .show(ctx, |ui| {
                if self.password_request.is_some() {
                    return; // the password dialog covers this
                }
                let (title, color) = if self.confirm_pending {
                    ("Consume this archive?", theme::TEXT)
                } else if self.busy {
                    ("Extracting…", theme::TEXT)
                } else {
                    match self.tone {
                        Tone::Ok => ("Done", theme::OK),
                        Tone::Err => ("Something went wrong", theme::ERR),
                        Tone::Warn => ("Finished — see note", theme::WARN),
                        Tone::Info => ("Molt", theme::TEXT),
                    }
                };
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(logo.id(), [36.0, 36.0])));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(title).font(theme::semibold(17.0)).color(color));
                        ui.add(egui::Label::new(dim(&self.archive_name)).truncate());
                    });
                });
                ui.add_space(12.0);

                if self.confirm_pending {
                    let dest = self
                        .out_dir
                        .as_deref()
                        .and_then(Path::file_name)
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.label(format!(
                        "{n} file{} ({}) will be extracted to the “{dest}” folder next to it.",
                        if n == 1 { "" } else { "s" },
                        human(bytes)
                    ));
                    ui.label(
                        RichText::new("The archive is deleted as it empties — there's no undo.")
                            .color(theme::WARN),
                    );
                    bottom_buttons(ui, |ui| {
                        if ui.add(theme::secondary("Cancel")).clicked() {
                            close = true;
                        }
                        if ui.add(theme::primary("Extract & consume", theme::MOLT)).clicked() {
                            start = true;
                        }
                    });
                } else if self.busy {
                    ui.add(
                        egui::ProgressBar::new(
                            self.jobs_done as f32 / self.jobs_total.max(1) as f32,
                        )
                        .text(format!("{} of {} files", self.jobs_done, self.jobs_total))
                        .fill(theme::MOLT)
                        .desired_height(18.0),
                    );
                    ui.add_space(4.0);
                    ui.label(dim(format!("{} reclaimed so far", human(self.freed))));
                } else {
                    ui.add(egui::Label::new(RichText::new(&self.log).color(self.tone.color())).wrap());
                    let folder = self.out_dir.clone().filter(|d| d.is_dir());
                    bottom_buttons(ui, |ui| {
                        if ui.add(theme::primary("Close", theme::ACCENT)).clicked() {
                            close = true;
                        }
                        if let Some(d) = &folder {
                            if ui.add(theme::secondary("Open folder")).clicked() {
                                reveal(d);
                                close = true;
                            }
                        }
                    });
                }
            });
        if self.confirm_pending && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        if start {
            self.confirm_pending = false;
            self.start_extraction();
        }
        if close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    // ------------------------------------------------------------ dialogs

    fn password_dialog(&mut self, ctx: &egui::Context) {
        let Some(path) = self.password_request.clone() else { return };
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut submit = false;
        let mut cancel = false;
        theme::dialog("password").show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(RichText::new("Password required").font(theme::semibold(17.0)));
            ui.add_space(4.0);
            ui.label(format!("“{name}” is encrypted. Enter its password to open it."));
            if self.password_wrong {
                ui.label(RichText::new("That password didn't work — try again.").color(theme::ERR));
            }
            ui.add_space(6.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.password_input)
                    .password(true)
                    .hint_text("Password")
                    .desired_width(f32::INFINITY),
            );
            edit.request_focus();
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            theme::button_row(ui, |ui| {
                if ui.add(theme::secondary("Cancel")).clicked() {
                    cancel = true;
                }
                if ui.add(theme::primary("Unlock", theme::ACCENT)).clicked() {
                    submit = true;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if submit {
            self.password = Some(std::mem::take(&mut self.password_input));
            self.password_request = None;
            self.open_archive(path);
        } else if cancel {
            self.password_request = None;
            self.password_input.clear();
            if self.minimal {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            self.archive_name.clear();
            self.say(Tone::Warn, "That archive is encrypted — no password given.");
        }
    }

    /// Last chance before a destructive run.
    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let (n, bytes) = self.to_extract();
        let dest = self.out_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
        let mut go = false;
        let mut cancel = false;
        theme::dialog("confirm").show(ctx, |ui| {
            ui.set_width(390.0);
            ui.label(RichText::new("Consume this archive?").font(theme::semibold(17.0)));
            ui.add_space(6.0);
            ui.label(format!(
                "{n} file{} ({}) will be extracted to:",
                if n == 1 { "" } else { "s" },
                human(bytes)
            ));
            ui.add(egui::Label::new(dim(dest)).wrap());
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    "As each file is verified, its space is freed from the archive. Once everything is out, the archive is deleted.",
                )
                .wrap(),
            );
            ui.add_space(4.0);
            ui.label(RichText::new("⚠  There's no undo — the archive can't be recovered.").color(theme::WARN));
            theme::button_row(ui, |ui| {
                if ui.add(theme::secondary("Cancel")).clicked() {
                    cancel = true;
                }
                if ui.add(theme::primary("Extract & consume", theme::MOLT)).clicked() {
                    go = true;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if go {
            self.confirm_pending = false;
            self.start_extraction();
        } else if cancel {
            self.confirm_pending = false;
        }
    }

    /// The "Check for updates" dialog. Renders whatever state the updater
    /// is in and drives the Update-now / dismiss actions.
    #[cfg(windows)]
    fn update_dialog(&mut self, ctx: &egui::Context) {
        if matches!(self.update, UpdateUi::Hidden) {
            return;
        }
        let mut dismiss = false;
        let mut apply: Option<update::Update> = None;
        theme::dialog("update").show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(RichText::new("Software update").font(theme::semibold(17.0)));
            ui.add_space(6.0);
            match &self.update {
                UpdateUi::Hidden => {}
                UpdateUi::Checking => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Checking for a newer version…");
                    });
                }
                UpdateUi::UpToDate(v) => {
                    ui.label(format!("You're on the latest version ({v})."));
                    theme::button_row(ui, |ui| {
                        if ui.add(theme::primary("OK", theme::ACCENT)).clicked() {
                            dismiss = true;
                        }
                    });
                }
                UpdateUi::Available(u) => {
                    ui.label(
                        RichText::new(format!("Version {} is available", u.version))
                            .font(theme::semibold(14.0))
                            .color(theme::OK),
                    );
                    ui.label(dim(format!("You have {}.", update::current_version())));
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            dim("Molt downloads the new build, checks its SHA-256, swaps itself in place and restarts.")
                                .small(),
                        )
                        .wrap(),
                    );
                    theme::button_row(ui, |ui| {
                        if ui.add(theme::secondary("Later")).clicked() {
                            dismiss = true;
                        }
                        if ui.add(theme::primary("Update now", theme::ACCENT)).clicked() {
                            apply = Some(u.clone());
                        }
                        if !u.notes_url.is_empty() {
                            ui.add_space(6.0);
                            if ui.link("Release notes").clicked() {
                                let _ = open_url(&u.notes_url);
                            }
                        }
                    });
                }
                UpdateUi::Downloading { got, total } => {
                    let frac = total.map(|t| *got as f32 / t.max(1) as f32);
                    ui.label("Downloading update…");
                    ui.add_space(4.0);
                    let bar = match frac {
                        Some(f) => egui::ProgressBar::new(f).show_percentage(),
                        None => egui::ProgressBar::new(0.0).animate(true).text(human(*got)),
                    };
                    ui.add(bar.desired_width(f32::INFINITY));
                }
                UpdateUi::Failed(e) => {
                    ui.label(RichText::new("✖  Update failed").color(theme::ERR));
                    ui.add(egui::Label::new(e.as_str()).wrap());
                    theme::button_row(ui, |ui| {
                        if ui.add(theme::primary("OK", theme::ACCENT)).clicked() {
                            dismiss = true;
                        }
                    });
                }
            }
        });
        let can_dismiss =
            matches!(self.update, UpdateUi::UpToDate(_) | UpdateUi::Available(_) | UpdateUi::Failed(_));
        if can_dismiss && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            dismiss = true;
        }
        if dismiss {
            self.update = UpdateUi::Hidden;
        }
        if let Some(u) = apply {
            self.start_update_apply(u);
        }
    }
}

#[cfg(windows)]
fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
}

fn load_icon() -> egui::IconData {
    // 64x64 raw RGBA generated from assets/molt_256.png at build-prep time.
    const RGBA: &[u8] = include_bytes!("../../../assets/icon-64.rgba");
    egui::IconData { rgba: RGBA.to_vec(), width: 64, height: 64 }
}

#[cfg(windows)]
fn message_box(text: &str) {
    rfd::MessageDialog::new()
        .set_title("Molt")
        .set_description(text)
        .show();
}

fn main() -> Result<(), eframe::Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Shell-integration switches (Windows): handle and exit, no window.
    #[cfg(windows)]
    match args.first().map(String::as_str) {
        Some("--register") => {
            match shell::register() {
                Ok(()) => message_box(
                    "Molt added to the Explorer right-click menu for archives.\n\
                     Remove it any time with: molt-gui --unregister",
                ),
                Err(e) => message_box(&format!("Could not register: {e}")),
            }
            return Ok(());
        }
        Some("--unregister") => {
            match shell::unregister() {
                Ok(()) => message_box("Molt removed from the Explorer right-click menu."),
                Err(e) => message_box(&format!("Could not unregister: {e}")),
            }
            return Ok(());
        }
        _ => {}
    }

    // `--molt-here <archive>`: open with the consume-confirmation ready.
    let (molt_here, archive_arg) = match args.first().map(String::as_str) {
        Some("--molt-here") => (true, args.get(1).cloned()),
        Some(a) => (false, Some(a.to_string())),
        None => (false, None),
    };

    let viewport = if molt_here {
        // A small fixed-size confirm dialog, not the full file browser.
        egui::ViewportBuilder::default()
            .with_inner_size([440.0, 220.0])
            .with_resizable(false)
            .with_maximize_button(false)
    } else {
        egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_min_inner_size([620.0, 420.0])
    };
    let options = eframe::NativeOptions {
        viewport: viewport.with_drag_and_drop(true).with_icon(load_icon()),
        // Center the "Molt here" dialog on the screen.
        centered: molt_here,
        ..Default::default()
    };
    eframe::run_native(
        "Molt",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            let mut app = MoltApp { minimal: molt_here, ..MoltApp::default() };
            // Support `molt-gui archive.zip` / "Open with…" / "Molt here"
            // (open_archive arms the consume-confirmation itself in
            // minimal mode, including after a password prompt).
            if let Some(arg) = archive_arg {
                app.open_archive(PathBuf::from(arg));
            }
            Ok(Box::new(app))
        }),
    )
}
