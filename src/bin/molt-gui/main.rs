//! Molt GUI — open an archive, pick files, extract, and watch the archive
//! shed its skin. Ships as a single portable executable.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window on Windows

#[cfg(windows)]
mod dragout;
#[cfg(windows)]
mod shell;

use eframe::egui;
use molt::formats::{self, Event};
use molt::util::human;
use std::fs;
use std::path::PathBuf;
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
    kind: &'static str,
    entries: Vec<Entry>,
    archive_size: u64,
    freed: u64,
    molt_mode: bool, // true = consume archive, false = classic extract
    out_dir: Option<PathBuf>,
    rx: Option<Receiver<Msg>>,
    busy: bool,
    log: String,
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
}

impl Default for MoltApp {
    fn default() -> Self {
        Self {
            archive_path: None,
            kind: "",
            entries: Vec::new(),
            archive_size: 0,
            freed: 0,
            molt_mode: true,
            out_dir: None,
            rx: None,
            busy: false,
            log: String::from(
                "Open an archive (zip, 7z, rar, tar.*, gz…), or drag one onto this window.",
            ),
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
    fn open_archive(&mut self, path: PathBuf) {
        let password = self.password.take();
        let minimal = self.minimal;
        *self = MoltApp::default();
        self.password = password;
        self.minimal = minimal;
        if minimal {
            self.molt_mode = true;
        }
        self.log = "Reading archive…".into();
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
            }
            Ok(()) => {
                let total: u64 = self.entries.iter().map(|e| e.size).sum();
                let fname = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.log = format!(
                    "{} ({}) — {} entries, {} compressed, {} extracted.",
                    fname,
                    self.kind,
                    self.entries.iter().filter(|e| !e.is_dir).count(),
                    human(self.archive_size),
                    human(total),
                );
                self.pending_title = Some(format!("Molt — {}", fname));
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
                self.log = e;
            }
            Err(e) => self.log = format!("Could not open archive: {e}"),
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
            self.log = "Nothing selected.".into();
            return;
        }
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
                    self.log = format!(
                        "Dragged {n} file{} out — copies, the archive is unchanged.",
                        if n == 1 { "" } else { "s" }
                    );
                } else {
                    self.log = "Drag cancelled.".into();
                }
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                self.log = format!("Drag-out failed: {e}");
            }
        }
    }

    fn pump_messages(&mut self) {
        let Some(rx) = &self.rx else { return };
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
                        self.log = format!("{}: {}", e.name, err);
                    }
                }
                Msg::Note(m) => self.log = m,
                Msg::Fatal(e) => self.log = format!("Extraction failed: {e}"),
                Msg::Finished { archive_deleted, reclaimed } => {
                    finished = true;
                    self.busy = false;
                    // Authoritative total from the worker (whole footprint on
                    // consume, punched bytes otherwise) — keeps the counter
                    // right even for formats we can't punch incrementally.
                    self.freed = reclaimed;
                    if archive_deleted {
                        self.log =
                            format!("Done — {} reclaimed, archive fully consumed.", human(self.freed));
                        self.archive_path = None;
                        self.pending_title = Some("Molt".to_string());
                    } else if self.molt_mode {
                        if self.freed > 0 {
                            self.log = format!("Done — {} reclaimed so far.", human(self.freed));
                        }
                        // otherwise keep the last note/error visible
                    } else {
                        self.log = "Done — archive untouched (Molt mode was off).".into();
                    }
                }
            }
        }
        if finished {
            self.rx = None;
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
        if self.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if let Some(t) = self.pending_title.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(t));
        }

        // Drag-and-drop an archive onto the window
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if let Some(p) = dropped.into_iter().next() {
            if !self.busy {
                self.open_archive(p);
            }
        }

        // Minimal ("Molt here") mode is a bare centered dialog — no toolbar,
        // no status bar, just the message and buttons in the central panel.
        if !self.minimal {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Molt");
                ui.separator();
                if ui.add_enabled(!self.busy, egui::Button::new("Open archive…")).clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Archives", ARCHIVE_EXTS)
                        .add_filter("All files", &["*"])
                        .pick_file()
                    {
                        self.open_archive(p);
                    }
                }
                if ui.add_enabled(!self.busy, egui::Button::new("Extract to…")).clicked() {
                    if let Some(d) = rfd::FileDialog::new().pick_folder() {
                        self.out_dir = Some(d);
                    }
                }
                ui.checkbox(&mut self.molt_mode, "Molt mode").on_hover_text(
                    "Free the archive's disk space as each file is extracted and verified. The archive is consumed — no undo.",
                );

                let can_go = self.archive_path.is_some() && self.out_dir.is_some() && !self.busy;
                let label = if self.molt_mode { "Extract & Free" } else { "Extract" };
                if ui
                    .add_enabled(can_go, egui::Button::new(label))
                    .on_disabled_hover_text("Open an archive and pick a destination first")
                    .clicked()
                {
                    if self.molt_mode {
                        self.confirm_pending = true;
                    } else {
                        self.start_extraction();
                    }
                }

                #[cfg(windows)]
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button("⚙", |ui| {
                        if ui.button("Add to Explorer right-click menu").clicked() {
                            self.log = match shell::register() {
                                Ok(()) => "Added to the right-click menu for archives.".into(),
                                Err(e) => format!("Could not register: {e}"),
                            };
                            ui.close_menu();
                        }
                        if ui.button("Remove from right-click menu").clicked() {
                            self.log = match shell::unregister() {
                                Ok(()) => "Removed from the right-click menu.".into(),
                                Err(e) => format!("Could not unregister: {e}"),
                            };
                            ui.close_menu();
                        }
                    });
                });
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(2.0);
            if self.busy && self.jobs_total > 0 {
                ui.add(
                    egui::ProgressBar::new(self.jobs_done as f32 / self.jobs_total as f32)
                        .desired_height(6.0),
                );
            }
            ui.horizontal(|ui| {
                ui.label(&self.log);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.freed > 0 {
                        ui.strong(format!("{} reclaimed", human(self.freed)));
                    }
                    if let Some(d) = &self.out_dir {
                        ui.weak(format!("→ {}", d.display()));
                    }
                });
            });
            ui.add_space(2.0);
        });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.minimal {
                let full_w = ui.available_width();
                const BW: f32 = 88.0; // button width
                const BH: f32 = 28.0; // button height

                if self.password_request.is_some() {
                    // The password window (below) covers this frame.
                } else if self.confirm_pending {
                    ui.add_space(28.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("Molt will extract and consume this archive.")
                                .strong(),
                        );
                        ui.add_space(2.0);
                        ui.label("Are you sure?");
                    });
                    ui.add_space(20.0);
                    ui.horizontal(|ui| {
                        let gap = ui.spacing().item_spacing.x;
                        ui.add_space(((full_w - (BW * 2.0 + gap)) * 0.5).max(0.0));
                        if ui.add_sized([BW, BH], egui::Button::new("Confirm")).clicked() {
                            self.confirm_pending = false;
                            self.start_extraction();
                        }
                        if ui.add_sized([BW, BH], egui::Button::new("Cancel")).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                } else if self.busy {
                    ui.add_space(52.0);
                    ui.vertical_centered(|ui| {
                        ui.label(format!("Extracting… {}/{}", self.jobs_done, self.jobs_total));
                    });
                } else {
                    // Done (consumed, kept after a failure, or never opened).
                    ui.add_space(34.0);
                    ui.vertical_centered(|ui| {
                        ui.label(&self.log);
                    });
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        ui.add_space(((full_w - BW) * 0.5).max(0.0));
                        if ui.add_sized([BW, BH], egui::Button::new("Close")).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                return;
            }

            if self.entries.is_empty() {
                ui.centered_and_justified(|ui| {
                    if self.archive_path.is_some() {
                        ui.weak("This archive is empty.");
                    } else {
                        ui.weak("Drop an archive here — zip, 7z, rar, tar.gz, …");
                    }
                });
                return;
            }
            ui.horizontal(|ui| {
                if ui.small_button("Select all").clicked() {
                    self.entries.iter_mut().for_each(|e| e.selected = true);
                }
                if ui.small_button("Select none").clicked() {
                    self.entries.iter_mut().for_each(|e| e.selected = false);
                }
            });
            ui.separator();

            let busy = self.busy;
            let mut drag_request: Option<usize> = None;
            // Rows are drag sources, not text: no I-beam cursor anywhere
            // in the table, and the whole row (not just the name) drags.
            ui.style_mut().interaction.selectable_labels = false;
            egui_extras::TableBuilder::new(ui)
                .striped(true)
                .sense(egui::Sense::click_and_drag())
                .column(egui_extras::Column::exact(24.0))
                .column(egui_extras::Column::remainder().clip(true))
                .column(egui_extras::Column::auto().at_least(90.0))
                .column(egui_extras::Column::auto().at_least(120.0))
                .header(22.0, |mut header| {
                    header.col(|_| {});
                    header.col(|ui| {
                        ui.strong("Name");
                    });
                    header.col(|ui| {
                        ui.strong("Size");
                    });
                    header.col(|ui| {
                        ui.strong("Status");
                    });
                })
                .body(|mut body| {
                    for (i, e) in
                        self.entries.iter_mut().enumerate().filter(|(_, e)| !e.is_dir)
                    {
                        body.row(20.0, |mut row| {
                            row.col(|ui| {
                                ui.add_enabled(
                                    !busy,
                                    egui::Checkbox::without_text(&mut e.selected),
                                );
                            });
                            row.col(|ui| {
                                ui.label(&e.name);
                            });
                            row.col(|ui| {
                                ui.label(human(e.size));
                            });
                            row.col(|ui| {
                                match e.status {
                                    Status::Pending => ui.weak("—"),
                                    Status::Extracting => ui.label("extracting…"),
                                    Status::Freed => ui.colored_label(
                                        egui::Color32::from_rgb(120, 220, 120),
                                        "✔ freed",
                                    ),
                                    Status::Extracted => ui.colored_label(
                                        egui::Color32::from_rgb(120, 180, 220),
                                        "✔ extracted",
                                    ),
                                    Status::Resumed => ui.colored_label(
                                        egui::Color32::from_rgb(150, 190, 150),
                                        "✔ out earlier",
                                    ),
                                    Status::Failed => ui.colored_label(
                                        egui::Color32::from_rgb(230, 120, 120),
                                        "✖ failed",
                                    ),
                                };
                            });

                            // Whole-row drag → copy the file out to Explorer.
                            if cfg!(windows) && !busy {
                                let resp = row
                                    .response()
                                    .on_hover_cursor(egui::CursorIcon::Grab)
                                    .on_hover_text(
                                        "Drag out of the window to copy this file into Explorer",
                                    );
                                if resp.drag_started() {
                                    drag_request = Some(i);
                                }
                            }
                        });
                    }
                });

            #[cfg(windows)]
            if let Some(i) = drag_request {
                self.drag_out(i);
            }
            #[cfg(not(windows))]
            let _ = drag_request;
        });

        // Password prompt for encrypted archives
        if let Some(path) = self.password_request.clone() {
            egui::Window::new("Password required")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "{} is encrypted.",
                        path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default()
                    ));
                    if self.password_wrong {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 120, 120),
                            "That password didn't work — try again.",
                        );
                    }
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.password_input)
                            .password(true)
                            .hint_text("Password"),
                    );
                    edit.request_focus();
                    let submitted =
                        edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Unlock").clicked() || submitted {
                            self.password = Some(std::mem::take(&mut self.password_input));
                            self.password_request = None;
                            self.open_archive(path.clone());
                        }
                        if ui.button("Cancel").clicked() {
                            self.password_request = None;
                            self.password_input.clear();
                            self.log = "Archive is encrypted — no password given.".into();
                        }
                    });
                });
        }

        // Destructive-action confirmation (minimal mode renders this inline
        // in the central panel instead — there's no file browser to overlay).
        if self.confirm_pending && !self.minimal {
            egui::Window::new("Consume archive?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(
                        "Molt mode frees the archive's disk space as files are\nextracted and verified. The archive cannot be recovered.",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Consume it").clicked() {
                            self.confirm_pending = false;
                            self.start_extraction();
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_pending = false;
                        }
                    });
                });
        }
    }
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
            .with_inner_size([380.0, 160.0])
            .with_resizable(false)
    } else {
        egui::ViewportBuilder::default()
            .with_inner_size([680.0, 440.0])
            .with_min_inner_size([560.0, 360.0])
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
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
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
