use crate::{
    decode::Photo,
    model::{Album, View},
    shell::{self, Action, Done},
    wic,
    worker::Worker,
};
use eframe::egui::{self, Color32, Key, Pos2, Rect, Vec2};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use v2_ui::{Button, Icon, IconButton};

/// How native actions (Explorer, Open with, clipboard) are started. Swapped in tests so the
/// window's behaviour can be checked without touching Explorer or the real clipboard.
pub type Launcher = fn(Action, isize, Box<dyn FnOnce() + Send>) -> mpsc::Receiver<Done>;
fn native_launcher(
    action: Action,
    owner: isize,
    wake: Box<dyn FnOnce() + Send>,
) -> mpsc::Receiver<Done> {
    shell::spawn(action, owner, wake)
}

/// A short message near the bottom of the photo, gone by itself. The countdown starts when it
/// is first drawn, so a slow first frame cannot use up its time before anyone could read it.
struct Notice {
    text: String,
    ok: bool,
    lifetime: Duration,
    first_drawn: Option<Instant>,
}

/// What Luma last learned from Windows about each codec family, kept apart from whether the
/// sheet is showing. At most one question is ever outstanding, however often the sheet is
/// opened and closed: a native codec call cannot be cancelled, so it must not pile up.
struct CodecStatus {
    open: bool,
    job: Option<mpsc::Receiver<Vec<wic::FamilyStatus>>>,
    latest: Option<Vec<wic::FamilyStatus>>,
}

pub struct Luma {
    worker: Option<Worker>,
    generation: u64,
    pub album: Album,
    pub photo: Option<Arc<Photo>>,
    /// The file `photo` was decoded from. While the next photo loads this still names the one on
    /// screen, so actions and the header never mix two photos.
    photo_path: Option<PathBuf>,
    pub error: Option<String>,
    pub loading: bool,
    loading_since: Option<Instant>,
    /// +1 or -1: which way the person is moving through the folder, for prefetching.
    direction: isize,
    pub view: View,
    texture: Option<egui::TextureHandle>,
    frame_index: usize,
    frame_deadline: Instant,
    animation_paused: bool,
    slideshow: bool,
    slideshow_deadline: Instant,
    slideshow_seconds: f32,
    info: bool,
    interval_focus: Option<egui::Id>,
    shortcuts: bool,
    formats: CodecStatus,
    fullscreen: bool,
    title: String,
    notice: Option<Notice>,
    /// At most one native action runs at a time; a second request waits for the first.
    native: Option<(mpsc::Receiver<Done>, &'static str)>,
    launcher: Launcher,
    hwnd: isize,
}
pub fn install(ctx: &egui::Context) {
    v2_ui::install(ctx, v2_ui::App::Luma);
}

/// How many codec-status questions have been started in this process. Opening and closing the
/// sheet repeatedly must not raise it past one while an answer is outstanding.
static STATUS_QUERIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub fn codec_status_queries_started() -> usize {
    STATUS_QUERIES.load(std::sync::atomic::Ordering::Relaxed)
}

/// The first time the busy badge may show while a photo is already on screen. Cached photos
/// arrive well inside this, so ordinary navigation never flashes it.
const BUSY_AFTER: Duration = Duration::from_millis(150);
const NOTICE_FOR: Duration = Duration::from_millis(3200);

impl Luma {
    pub fn new(ctx: &egui::Context, path: Option<PathBuf>) -> Self {
        let mut app = Self::headless();
        app.worker = Some(Worker::new(ctx.clone()));
        if let Some(path) = path {
            app.open(path);
        }
        app
    }
    pub fn headless() -> Self {
        Self {
            worker: None,
            generation: 0,
            album: Album::default(),
            photo: None,
            photo_path: None,
            error: None,
            loading: false,
            loading_since: None,
            direction: 1,
            view: View::default(),
            texture: None,
            frame_index: 0,
            frame_deadline: Instant::now(),
            animation_paused: false,
            slideshow: false,
            slideshow_deadline: Instant::now(),
            slideshow_seconds: 5.0,
            info: false,
            interval_focus: None,
            shortcuts: false,
            formats: CodecStatus {
                open: false,
                job: None,
                latest: None,
            },
            fullscreen: false,
            title: "Luma".into(),
            notice: None,
            native: None,
            launcher: native_launcher,
            hwnd: 0,
        }
    }
    #[doc(hidden)]
    pub fn with_launcher(mut self, launcher: Launcher) -> Self {
        self.launcher = launcher;
        self
    }
    pub fn show_photo(&mut self, photo: Photo, path: PathBuf) {
        self.album = Album {
            files: vec![path.clone()],
            index: 0,
            truncated: false,
        };
        self.photo = Some(Arc::new(photo));
        self.photo_path = Some(path);
        self.texture = None;
        self.view = View::default();
        self.frame_index = 0;
    }
    /// Visual fixtures only: open the details card without a key press.
    #[doc(hidden)]
    pub fn visual_details(&mut self) {
        self.info = true;
    }
    /// Visual fixtures only: show a notice, or a photo that is still loading its successor.
    #[doc(hidden)]
    pub fn visual_notice(&mut self, text: &str, ok: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            ok,
            lifetime: Duration::from_secs(3600),
            first_drawn: None,
        });
    }
    #[doc(hidden)]
    pub fn visual_shortcuts(&mut self) {
        self.shortcuts = true;
    }
    #[doc(hidden)]
    pub fn visual_busy(&mut self) {
        self.loading = true;
        self.loading_since = Some(Instant::now() - Duration::from_secs(1));
    }
    #[doc(hidden)]
    pub fn visual_formats(&mut self, statuses: Vec<wic::FamilyStatus>) {
        self.formats.latest = Some(statuses);
        self.formats.open = true;
    }
    fn clear_photo(&mut self) {
        self.photo = None;
        self.photo_path = None;
        self.texture = None;
        self.frame_index = 0;
        self.view = View::default();
        self.error = None;
        self.animation_paused = false;
    }
    fn open(&mut self, path: PathBuf) {
        self.clear_photo();
        self.album = Album::default();
        self.loading = true;
        self.loading_since = Some(Instant::now());
        self.slideshow = false;
        self.direction = 1;
        if let Some(w) = &self.worker {
            self.generation = w.open(path);
        }
    }
    fn navigate(&mut self, delta: isize) {
        if self.album.step(delta) {
            self.direction = delta.signum();
            self.load_current();
        }
    }
    /// Ask for the album's current photo. The photo already on screen stays there until the next
    /// one is decoded, so stepping through a folder never blinks through a loading screen.
    fn load_current(&mut self) {
        if let Some(path) = self.album.current().cloned() {
            self.error = None;
            self.loading = true;
            self.loading_since = Some(Instant::now());
            if let Some(w) = &self.worker {
                self.generation = w.navigate(path, self.album.neighbour_path(self.direction));
            }
        }
    }
    fn pick_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Open photo in Luma")
            .add_filter("Photos", crate::model::BUILT_IN_EXTENSIONS)
            // Listed separately and honestly: these open only if Windows has the codec.
            .add_filter(
                "HEIC, RAW, JPEG XR, JPEG XL (needs a Windows codec)",
                &[
                    "heic", "heif", "hif", "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf",
                    "pef", "srw", "jxr", "wdp", "hdp", "jxl",
                ],
            )
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.open(path);
        }
    }
    fn pick_folder(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Choose a photo folder")
            .pick_folder()
        {
            self.open(path);
        }
    }
    fn toggle_slideshow(&mut self) {
        if !self.album.files.is_empty() {
            self.slideshow = !self.slideshow;
            self.slideshow_deadline =
                Instant::now() + Duration::from_secs_f32(self.slideshow_seconds);
        }
    }
    fn say(&mut self, text: impl Into<String>, ok: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            ok,
            lifetime: NOTICE_FOR,
            first_drawn: None,
        });
    }

    // ---- native actions ---------------------------------------------------------------------

    /// Start an action unless one is already running: a held Ctrl+C or a double click must not
    /// queue up a second multi-megabyte copy or a second dialog.
    fn start(&mut self, ctx: &egui::Context, action: Action) {
        if let Some((_, label)) = &self.native {
            let text = format!("{label}… one moment.");
            self.say(text, true);
            return;
        }
        let label = action.label();
        let wake = ctx.clone();
        let receiver = (self.launcher)(action, self.hwnd, Box::new(move || wake.request_repaint()));
        self.native = Some((receiver, label));
    }
    fn poll_native(&mut self) {
        let Some((receiver, _)) = &self.native else {
            return;
        };
        match receiver.try_recv() {
            Ok(done) => {
                self.native = None;
                if !done.message.is_empty() {
                    self.say(done.message, done.ok);
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.native = None;
                self.say("That action ended unexpectedly.", false);
            }
        }
    }
    /// There is a photo on screen whose file the actions can name.
    fn has_target(&self) -> bool {
        self.photo.is_some() && self.photo_path.is_some()
    }
    /// Menu items are enabled only when an action could start right now.
    fn can_act(&self) -> bool {
        self.has_target() && self.native.is_none()
    }
    fn copy_image(&mut self, ctx: &egui::Context) {
        let Some(photo) = self.photo.clone() else {
            return;
        };
        let frame = self.frame_index.min(photo.frames.len().saturating_sub(1));
        self.start(
            ctx,
            Action::CopyImage {
                photo,
                frame,
                clockwise_quarter_turns: self.view.rotation,
            },
        );
    }
    fn act_on_file(&mut self, ctx: &egui::Context, make: fn(PathBuf) -> Action) {
        if let Some(path) = self.photo_path.clone() {
            self.start(ctx, make(path));
        }
    }
    /// Show the sheet and, unless a question is already out, ask Windows again (an extension
    /// may have been installed since). Earlier answers stay visible while it refreshes.
    fn open_formats(&mut self, ctx: &egui::Context) {
        self.formats.open = true;
        if self.formats.job.is_some() {
            return;
        }
        STATUS_QUERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        let wake = ctx.clone();
        let started = std::thread::Builder::new()
            .name("luma-codec-status".into())
            .spawn(move || {
                let _ = sender.send(wic::status());
                wake.request_repaint();
            });
        match started {
            Ok(_) => self.formats.job = Some(receiver),
            // Not being able to ask is not the same as "nothing is installed".
            Err(e) => {
                self.formats.latest = Some(wic::unknown_status(&format!(
                    "Luma could not start the check ({e})."
                )))
            }
        }
    }
    fn poll_formats(&mut self) {
        let Some(receiver) = &self.formats.job else {
            return;
        };
        match receiver.try_recv() {
            Ok(statuses) => {
                self.formats.latest = Some(statuses);
                self.formats.job = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                // The check ended without an answer; keep any earlier answer, else say so.
                if self.formats.latest.is_none() {
                    self.formats.latest =
                        Some(wic::unknown_status("The check ended without an answer."));
                }
                self.formats.job = None;
            }
        }
    }

    // ---- input ------------------------------------------------------------------------------

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(reply) = self.worker.as_ref().and_then(|w| w.take()) {
            self.accept_reply(reply, ctx);
        }
        self.poll_native();
        self.poll_formats();
        let drop = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()));
        if let Some(path) = drop {
            self.open(path);
        }
        v2_ui::drop_overlay(ctx, "Drop a photo or folder to open");
        self.handle_keys(ctx);
        let now = Instant::now();
        if self.slideshow {
            if now >= self.slideshow_deadline && !self.loading {
                self.navigate(1);
                self.slideshow_deadline = now + Duration::from_secs_f32(self.slideshow_seconds);
            }
            ctx.request_repaint_after(
                self.slideshow_deadline
                    .saturating_duration_since(now)
                    .max(Duration::from_millis(100)),
            );
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // A control reached with Tab (the interval slider, a dock button) owns Space and the
        // arrows. Every other viewer key stays global, so F11, Ctrl+O and Escape always work.
        let owned = ctx.memory(|m| m.focused()).is_some();
        // Copy belongs to whatever is being typed in or has text selected. Luma has no text
        // field and the shared theme makes labels unselectable, but if either ever appears, an
        // asynchronous picture copy must not overwrite what the person just copied from it.
        let typing = ctx.text_edit_focused();
        let k = ctx.input(|i| Keys {
            left: i.key_pressed(Key::ArrowLeft),
            right: i.key_pressed(Key::ArrowRight),
            page_up: i.key_pressed(Key::PageUp),
            page_down: i.key_pressed(Key::PageDown),
            home: i.key_pressed(Key::Home),
            end: i.key_pressed(Key::End),
            fit: (i.key_pressed(Key::F) || i.key_pressed(Key::Num0)) && !i.modifiers.command,
            actual: i.key_pressed(Key::Num1) && !i.modifiers.command,
            rotate: i.key_pressed(Key::R) && !i.modifiers.command,
            shift: i.modifiers.shift,
            space: i.key_pressed(Key::Space),
            info: i.key_pressed(Key::I) && !i.modifiers.command,
            pause: i.key_pressed(Key::P) && !i.modifiers.command,
            fullscreen: i.key_pressed(Key::F11),
            escape: i.key_pressed(Key::Escape),
            open: i.key_pressed(Key::O) && i.modifiers.command,
            // egui turns Ctrl+C into a Copy event (and still reports Shift), not a key press.
            copy: !typing && i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
            reveal: i.key_pressed(Key::E) && i.modifiers.command,
            back: i.pointer.button_pressed(egui::PointerButton::Extra1),
            forward: i.pointer.button_pressed(egui::PointerButton::Extra2),
        });
        if k.escape {
            // Escape closes the topmost thing first.
            if self.formats.open {
                self.formats.open = false;
                return;
            }
            if self.shortcuts {
                self.shortcuts = false;
                return;
            }
        }
        if self.shortcuts || self.formats.open {
            return;
        }
        if (k.left && !owned) || k.page_up || k.back {
            self.navigate(-1);
        }
        if (k.right && !owned) || k.page_down || k.forward {
            self.navigate(1);
        }
        if k.home && !self.album.files.is_empty() {
            self.album.index = 0;
            self.direction = 1;
            self.load_current();
        }
        if k.end && !self.album.files.is_empty() {
            self.album.index = self.album.files.len() - 1;
            self.direction = -1;
            self.load_current();
        }
        if k.fit {
            self.view.fit();
        }
        if k.actual {
            self.view.actual_physical(ctx.pixels_per_point());
        }
        if k.rotate {
            self.view.rotate(!k.shift);
        }
        if k.space && !owned {
            self.toggle_slideshow();
        }
        if k.pause && self.photo.as_ref().is_some_and(|p| p.frames.len() > 1) {
            self.animation_paused = !self.animation_paused;
            ctx.request_repaint();
        }
        if k.info {
            self.info = !self.info;
        }
        if k.fullscreen {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if k.escape {
            self.slideshow = false;
            self.fullscreen = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        if k.open {
            if k.shift {
                self.pick_folder();
            } else {
                self.pick_file();
            }
        }
        // A key pressed while an action is still running gets "one moment" from `start`, rather
        // than silence or a second copy queued behind the first.
        if k.copy && self.has_target() {
            if k.shift {
                self.act_on_file(ctx, Action::CopyPath);
            } else {
                self.copy_image(ctx);
            }
        }
        if k.reveal && self.has_target() {
            self.act_on_file(ctx, Action::Reveal);
        }
    }

    fn accept_reply(&mut self, reply: crate::worker::Reply, ctx: &egui::Context) {
        if reply.generation != self.generation {
            return;
        }
        self.loading = false;
        self.loading_since = None;
        if let Some(album) = reply.album {
            self.album = album;
        }
        match reply.photo {
            Ok(photo) => {
                self.photo_path = photo.is_some().then(|| reply.path.clone());
                self.photo = photo;
                self.texture = None;
                self.frame_index = 0;
                self.frame_deadline = Instant::now();
                self.animation_paused = false;
                self.error = None;
                self.view = View::default();
            }
            Err(e) => {
                // The photo on screen is not the one that was asked for: show the failure.
                self.photo = None;
                self.photo_path = None;
                self.texture = None;
                self.frame_index = 0;
                self.view = View::default();
                self.error = Some(e);
            }
        }
        self.title = format!(
            "{} · Luma",
            reply.path.file_name().unwrap_or_default().to_string_lossy()
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.title.clone()));
    }

    // ---- drawing ----------------------------------------------------------------------------

    pub fn render(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        if ctx.input(|i| i.key_pressed(Key::F1)) {
            self.shortcuts = !self.shortcuts;
        }
        let p = v2_ui::palette(&ctx);
        // Full screen is for looking: the header steps aside while a photo is showing.
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        if !(fullscreen && self.photo.is_some()) {
            egui::Panel::top("header")
                .exact_size(v2_ui::tokens::size::HEADER)
                .frame(v2_ui::chrome::frame_header(&p))
                .show(ui, |ui| self.header(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.stage))
            .show(ui, |ui| {
                let rect = ui.max_rect();
                // The details card takes a column on the right; the photo fits the rest.
                let (stage, card) = if self.info {
                    let card = Rect::from_min_max(
                        Pos2::new(rect.right() - 316.0, rect.top() + 16.0),
                        Pos2::new(rect.right() - 16.0, rect.bottom() - 16.0),
                    );
                    (rect.with_max_x(card.left() - 8.0), Some(card))
                } else {
                    (rect, None)
                };
                if self.photo.is_some() {
                    self.canvas(ui, stage);
                    self.edge_arrows(&ctx, ui, stage);
                    self.dock(&ctx, stage, &p);
                    self.busy_badge(&ctx, stage, &p);
                } else if self.loading {
                    loading_state(ui, stage);
                } else if let Some(error) = self.error.clone() {
                    self.error_state(ui, stage, &error);
                } else {
                    self.empty_state(ui, stage);
                }
                if let Some(card) = card {
                    self.details(&ctx, card, &p);
                }
                self.notice_toast(&ctx, stage, &p);
            });
        if self.shortcuts {
            self.shortcut_sheet(&ctx, &p);
        }
        if self.formats.open {
            self.format_sheet(&ctx, &p);
        }
    }

    fn shortcut_sheet(&mut self, ctx: &egui::Context, p: &v2_ui::Palette) {
        let close = modal_sheet(
            ctx,
            p,
            "luma-keys",
            "Keyboard shortcuts",
            460.0,
            |ui, height| {
                egui::ScrollArea::vertical()
                .max_height(height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    v2_ui::shortcut_table(
                        ui,
                        &[
                            ("Ctrl+O", "Open a photo"),
                            ("Ctrl+Shift+O", "Open a folder"),
                            ("Left / Right", "Previous or next photo"),
                            ("PgUp / PgDn", "Previous or next photo"),
                            ("Home / End", "First or last photo"),
                            ("F / 0", "Fit to window"),
                            ("1", "Actual pixels (1:1 on this screen)"),
                            ("+ / −", "Zoom in or out"),
                            ("R / Shift+R", "Rotate right or left"),
                            ("Space", "Start or pause the slideshow"),
                            ("P", "Play or pause an animation"),
                            ("I", "Details"),
                            ("Ctrl+C", "Copy the picture"),
                            ("Ctrl+Shift+C", "Copy the file path"),
                            ("Ctrl+E", "Show in Explorer"),
                            ("F11", "Full screen"),
                            ("Esc", "Close a sheet, leave full screen"),
                            ("F1", "This list"),
                        ],
                    );
                    ui.add_space(8.0);
                    ui.label(v2_ui::caption(
                        "Scroll to zoom at the pointer, drag to move, double-click to switch between fit and actual pixels. Right-click the photo for more. The mouse back and forward buttons step through the folder.",
                    ));
                });
            },
        );
        if close {
            self.shortcuts = false;
        }
    }

    fn format_sheet(&mut self, ctx: &egui::Context, p: &v2_ui::Palette) {
        let statuses = self.formats.latest.clone();
        let refreshing = self.formats.job.is_some();
        let close = modal_sheet(
            ctx,
            p,
            "luma-formats",
            "Format support",
            520.0,
            |ui, height| {
                ui.label(v2_ui::caption(
                "Luma decodes the formats below itself. HEIC, RAW, JPEG XR and JPEG XL are opened by a codec that Windows already has; Luma never installs or downloads one.",
            ));
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                .max_height((height - 56.0).max(80.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    built_in_row(
                        ui,
                        p,
                        "JPEG, PNG, BMP, TIFF",
                        "Large baseline JPEG opens as a scaled preview.",
                    );
                    built_in_row(
                        ui,
                        p,
                        "GIF, animated WebP, APNG",
                        "Animations play; P pauses them.",
                    );
                    built_in_row(ui, p, "AVIF", "Still images, 8-bit.");
                    ui.add_space(4.0);
                    v2_ui::section_label(ui, "Needs a Windows codec");
                    match &statuses {
                        None => {
                            v2_ui::loading(ui, "Asking Windows which codecs work…");
                        }
                        Some(list) => {
                            for status in list {
                                codec_row(ui, p, status);
                            }
                            if refreshing {
                                v2_ui::loading(ui, "Checking again…");
                            }
                        }
                    }
                    ui.add_space(4.0);
                    ui.label(v2_ui::caption(
                        "\"Working\" means a small built-in sample just decoded through that codec. A listed codec can still open some files and not others.",
                    ));
                });
            },
        );
        if close {
            self.formats.open = false;
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let p = v2_ui::palette(ui.ctx());
        let name = self
            .album
            .current()
            .and_then(|path| path.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        let position = (self.album.files.len() > 1)
            .then(|| format!("{} of {}", self.album.index + 1, self.album.files.len()));
        // Dimensions describe the photo on screen, so they wait while the next one loads.
        let showing_current = self.photo_path.as_ref() == self.album.current();
        let details = self
            .photo
            .as_ref()
            .filter(|_| showing_current)
            .map(|photo| {
                format!(
                    "{} × {} · {} · {}",
                    photo.width,
                    photo.height,
                    format_label(&photo.format)
                        .split(" / ")
                        .next()
                        .unwrap_or_default(),
                    file_size(photo.file_bytes)
                )
            });
        let wide = ui.available_width() > 860.0;
        let info = self.info;
        let act = self.can_act();
        let animated = self.photo.as_ref().is_some_and(|p| p.frames.len() > 1);
        let paused = self.animation_paused;
        let mut action = None;
        v2_ui::chrome::header(
            ui,
            |ui| {
                v2_ui::chrome::wordmark(ui, v2_ui::App::Luma);
                if let Some(name) = &name {
                    v2_ui::chrome::divider(ui);
                    let room = (ui.available_width() - 300.0).max(80.0);
                    ui.scope(|ui| {
                        ui.set_max_width(room.min(360.0));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(name)
                                    .font(egui::FontId::new(14.0, v2_ui::semibold()))
                                    .color(p.text),
                            )
                            .truncate(),
                        );
                    });
                    if let Some(position) = &position {
                        ui.label(v2_ui::caption(position));
                    }
                    if let (true, Some(details)) = (wide, &details) {
                        ui.label(v2_ui::caption("·"));
                        ui.label(v2_ui::caption(details));
                    }
                }
            },
            |ui| {
                v2_ui::chrome::more_menu(ui, |ui| {
                    let mut item = |ui: &mut egui::Ui,
                                    icon: Option<Icon>,
                                    label: &str,
                                    keys: Option<&str>,
                                    enabled: bool,
                                    id: &'static str| {
                        if v2_ui::chrome::menu_item(ui, icon, label, keys, enabled).clicked() {
                            action = Some(id);
                        }
                    };
                    item(
                        ui,
                        Some(Icon::Open),
                        "Open a photo…",
                        Some("Ctrl+O"),
                        true,
                        "file",
                    );
                    item(
                        ui,
                        Some(Icon::Folder),
                        "Open a folder…",
                        Some("Ctrl+Shift+O"),
                        true,
                        "folder",
                    );
                    v2_ui::chrome::menu_separator(ui);
                    item(
                        ui,
                        Some(Icon::Folder),
                        "Show in Explorer",
                        Some("Ctrl+E"),
                        act,
                        "reveal",
                    );
                    item(ui, Some(Icon::Open), "Open with…", None, act, "openwith");
                    item(ui, None, "Copy picture", Some("Ctrl+C"), act, "copyimage");
                    item(ui, None, "Copy file", None, act, "copyfile");
                    item(
                        ui,
                        None,
                        "Copy file path",
                        Some("Ctrl+Shift+C"),
                        act,
                        "copypath",
                    );
                    if animated {
                        v2_ui::chrome::menu_separator(ui);
                        item(
                            ui,
                            Some(if paused { Icon::Play } else { Icon::Pause }),
                            if paused {
                                "Play animation"
                            } else {
                                "Pause animation"
                            },
                            Some("P"),
                            true,
                            "anim",
                        );
                    }
                    v2_ui::chrome::menu_separator(ui);
                    v2_ui::chrome::theme_menu(ui);
                    v2_ui::chrome::menu_separator(ui);
                    item(ui, None, "Format support…", None, true, "formats");
                    item(
                        ui,
                        Some(Icon::Keyboard),
                        "Keyboard shortcuts",
                        Some("F1"),
                        true,
                        "keys",
                    );
                });
                if IconButton::new(Icon::Info, "Details")
                    .selected(info)
                    .shortcut("I")
                    .show(ui)
                    .clicked()
                {
                    action = Some("info");
                }
                if Button::primary("Open")
                    .icon(Icon::Open)
                    .pill()
                    .shortcut("Ctrl+O")
                    .show(ui)
                    .clicked()
                {
                    action = Some("file");
                }
            },
        );
        let ctx = ui.ctx().clone();
        self.apply_menu(&ctx, action);
    }

    /// One place that turns a menu choice (header or context menu) into its effect.
    fn apply_menu(&mut self, ctx: &egui::Context, action: Option<&'static str>) {
        match action {
            Some("file") => self.pick_file(),
            Some("folder") => self.pick_folder(),
            Some("keys") => self.shortcuts = true,
            Some("formats") => self.open_formats(ctx),
            Some("info") => self.info = !self.info,
            Some("reveal") => self.act_on_file(ctx, Action::Reveal),
            Some("openwith") => self.act_on_file(ctx, Action::OpenWith),
            Some("copyfile") => self.act_on_file(ctx, Action::CopyFile),
            Some("copypath") => self.act_on_file(ctx, Action::CopyPath),
            Some("copyimage") => self.copy_image(ctx),
            Some("anim") => self.animation_paused = !self.animation_paused,
            Some("fit") => self.view.fit(),
            Some("actual") => self.view.actual_physical(ctx.pixels_per_point()),
            Some("rotate-right") => self.view.rotate(true),
            Some("rotate-left") => self.view.rotate(false),
            _ => {}
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, stage: Rect) {
        let mut action = None;
        v2_ui::chrome::Welcome::new(Icon::Image, "Open a photo")
            .body(
                "Look at one photo or step through a whole folder. You can also drop a photo here.",
            )
            .foot("JPEG · PNG · WebP · AVIF · GIF · BMP · TIFF · HEIC and RAW if Windows has the codec")
            .show(ui, stage, |ui| {
                if Button::primary("Open a photo")
                    .icon(Icon::Open)
                    .pill()
                    .show(ui)
                    .clicked()
                {
                    action = Some(0);
                }
                if Button::secondary("Open a folder")
                    .icon(Icon::Folder)
                    .pill()
                    .show(ui)
                    .clicked()
                {
                    action = Some(1);
                }
                if Button::ghost("Format support")
                    .pill()
                    .show(ui)
                    .clicked()
                {
                    action = Some(2);
                }
            });
        let ctx = ui.ctx().clone();
        match action {
            Some(0) => self.pick_file(),
            Some(1) => self.pick_folder(),
            Some(_) => self.open_formats(&ctx),
            None => {}
        }
    }

    fn error_state(&mut self, ui: &mut egui::Ui, stage: Rect, error: &str) {
        let mut action = None;
        let more = self.album.files.len() > 1;
        v2_ui::chrome::Welcome::new(Icon::Error, "Couldn't open this photo")
            .tone(v2_ui::Tone::Danger)
            .body(error)
            .show(ui, stage, |ui| {
                if Button::primary("Open another photo")
                    .icon(Icon::Open)
                    .pill()
                    .show(ui)
                    .clicked()
                {
                    action = Some(0);
                }
                if more
                    && Button::secondary("Next photo")
                        .icon(Icon::ChevronRight)
                        .pill()
                        .show(ui)
                        .clicked()
                {
                    action = Some(1);
                }
                if Button::ghost("Format support").pill().show(ui).clicked() {
                    action = Some(2);
                }
            });
        let ctx = ui.ctx().clone();
        match action {
            Some(0) => self.pick_file(),
            Some(1) => self.navigate(1),
            Some(_) => self.open_formats(&ctx),
            None => {}
        }
    }

    /// Large previous/next targets on the photo's edges while the pointer is over it.
    fn edge_arrows(&mut self, ctx: &egui::Context, ui: &egui::Ui, stage: Rect) {
        if self.album.files.len() < 2 || !ui.rect_contains_pointer(stage) {
            return;
        }
        for (delta, icon, label, keys, x) in [
            (
                -1,
                Icon::ChevronLeft,
                "Previous photo",
                "Left arrow",
                stage.left() + 44.0,
            ),
            (
                1,
                Icon::ChevronRight,
                "Next photo",
                "Right arrow",
                stage.right() - 44.0,
            ),
        ] {
            let centre = Pos2::new(x, stage.center().y);
            let clicked = egui::Area::new(egui::Id::new(("luma-edge", delta)))
                .order(egui::Order::Middle)
                .pivot(egui::Align2::CENTER_CENTER)
                .fixed_pos(centre)
                .fade_in(false)
                .show(ctx, |ui| {
                    let r = Rect::from_center_size(centre, Vec2::splat(48.0));
                    ui.painter()
                        .circle_filled(r.center(), 24.0, Color32::from_black_alpha(120));
                    IconButton::new(icon, label)
                        .size(48.0)
                        .overlay()
                        .shortcut(keys)
                        .show(ui)
                        .clicked()
                })
                .inner;
            if clicked {
                self.navigate(delta);
            }
        }
    }

    /// Shown only once a request has been outstanding for a moment, over the photo that is
    /// still on screen. A cached neighbour never gets here.
    fn busy_badge(&mut self, ctx: &egui::Context, stage: Rect, p: &v2_ui::Palette) {
        let Some(since) = self.loading_since.filter(|_| self.loading) else {
            return;
        };
        let waited = since.elapsed();
        if waited < BUSY_AFTER {
            ctx.request_repaint_after(BUSY_AFTER - waited);
            return;
        }
        egui::Area::new(egui::Id::new("luma-busy"))
            .order(egui::Order::Middle)
            .pivot(egui::Align2::CENTER_TOP)
            .fixed_pos(Pos2::new(stage.center().x, stage.top() + 16.0))
            .fade_in(false)
            .interactable(false)
            .show(ctx, |ui| {
                v2_ui::chrome::frame_dock(p).show(ui, |ui| {
                    v2_ui::loading(ui, "Opening…");
                });
            });
    }

    /// The transient message: a copy succeeded, an action failed, something is still running.
    fn notice_toast(&mut self, ctx: &egui::Context, stage: Rect, p: &v2_ui::Palette) {
        let Some(notice) = &mut self.notice else {
            return;
        };
        let now = Instant::now();
        let until = *notice.first_drawn.get_or_insert(now) + notice.lifetime;
        if now >= until {
            self.notice = None;
            return;
        }
        let (text, ok) = (notice.text.clone(), notice.ok);
        ctx.request_repaint_after(until - now);
        egui::Area::new(egui::Id::new("luma-notice"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::CENTER_BOTTOM)
            .fixed_pos(Pos2::new(stage.center().x, stage.bottom() - 92.0))
            .fade_in(false)
            .interactable(false)
            .show(ctx, |ui| {
                v2_ui::chrome::frame_dock(p).show(ui, |ui| {
                    ui.set_max_width((stage.width() - 48.0).clamp(160.0, 520.0));
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).color(if ok {
                            p.text
                        } else {
                            p.danger
                        }))
                        .wrap(),
                    );
                });
            });
    }

    /// The floating view capsule: zoom, fit, rotate, slideshow, full screen.
    fn dock(&mut self, ctx: &egui::Context, stage: Rect, p: &v2_ui::Palette) {
        let Some(photo) = self.photo.clone() else {
            return;
        };
        let image = [photo.width as f32, photo.height as f32];
        let viewport = [stage.width(), stage.height()];
        let ppp = ctx.pixels_per_point();
        self.view.sync_display_scale(ppp);
        let percent = self.view.scale(image, viewport) * ppp * 100.0;
        let (slideshow, fit, fullscreen) = (self.slideshow, self.view.fit, self.fullscreen);
        let animated = photo.frames.len() > 1;
        let paused = self.animation_paused;
        // A narrow window drops the percentage readout and rotate-left; every action stays
        // reachable by key, from the More menu, or from the photo's right-click menu.
        let compact = stage.width() < 560.0;
        let mut action = None;
        egui::Area::new(egui::Id::new("luma-dock"))
            .order(egui::Order::Middle)
            .pivot(egui::Align2::CENTER_BOTTOM)
            .fixed_pos(Pos2::new(stage.center().x, stage.bottom() - 20.0))
            .fade_in(false)
            .show(ctx, |ui| {
                v2_ui::chrome::frame_dock(p).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        let mut button =
                            |ui: &mut egui::Ui,
                             icon: Icon,
                             label: &str,
                             keys: &'static str,
                             selected: bool,
                             id: &'static str| {
                                if IconButton::new(icon, label)
                                    .selected(selected)
                                    .shortcut(keys)
                                    .show(ui)
                                    .clicked()
                                {
                                    action = Some(id);
                                }
                            };
                        button(ui, Icon::ZoomOut, "Zoom out", "−", false, "out");
                        if !compact {
                            ui.add_sized(
                                [56.0, 32.0],
                                egui::Label::new(
                                    egui::RichText::new(format!("{percent:.0}%")).color(p.text),
                                ),
                            )
                            .on_hover_text("Zoom level");
                        }
                        button(ui, Icon::ZoomIn, "Zoom in", "+", false, "in");
                        v2_ui::chrome::divider(ui);
                        button(ui, Icon::Fit, "Fit to window", "F", fit, "fit");
                        button(ui, Icon::ActualSize, "Actual pixels", "1", false, "actual");
                        v2_ui::chrome::divider(ui);
                        if !compact {
                            button(
                                ui,
                                Icon::RotateLeft,
                                "Rotate left",
                                "Shift+R",
                                false,
                                "left",
                            );
                        }
                        button(ui, Icon::RotateRight, "Rotate right", "R", false, "right");
                        v2_ui::chrome::divider(ui);
                        if animated {
                            button(
                                ui,
                                if paused { Icon::Play } else { Icon::Pause },
                                if paused {
                                    "Play animation"
                                } else {
                                    "Pause animation"
                                },
                                "P",
                                paused,
                                "anim",
                            );
                        }
                        button(
                            ui,
                            Icon::Slideshow,
                            if slideshow {
                                "Pause slideshow"
                            } else {
                                "Start slideshow"
                            },
                            "Space",
                            slideshow,
                            "slideshow",
                        );
                        button(
                            ui,
                            if fullscreen {
                                Icon::ExitFullscreen
                            } else {
                                Icon::Fullscreen
                            },
                            if fullscreen {
                                "Leave full screen"
                            } else {
                                "Full screen"
                            },
                            "F11",
                            false,
                            "fullscreen",
                        );
                    });
                });
            });
        match action {
            Some("out") => self.view.zoom_at(0.8, [0.0, 0.0], image, viewport),
            Some("in") => self.view.zoom_at(1.25, [0.0, 0.0], image, viewport),
            Some("fit") => self.view.fit(),
            Some("actual") => self.view.actual_physical(ppp),
            Some("left") => self.view.rotate(false),
            Some("right") => self.view.rotate(true),
            Some("anim") => self.animation_paused = !self.animation_paused,
            Some("slideshow") => self.toggle_slideshow(),
            Some("fullscreen") => {
                self.fullscreen = !self.fullscreen;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
            }
            _ => return,
        }
        // The click painted this frame with the old view; show the new one now.
        ctx.request_repaint();
    }

    /// The details card: what the file is, how it was read, camera facts, and the slideshow pace.
    fn details(&mut self, ctx: &egui::Context, card: Rect, p: &v2_ui::Palette) {
        let mut close = false;
        let mut action = None;
        let can_act = self.can_act();
        egui::Area::new(egui::Id::new("luma-details"))
            .order(egui::Order::Middle)
            .fixed_pos(card.min)
            .fade_in(false)
            .show(ctx, |ui| {
                v2_ui::chrome::frame_sheet(p).show(ui, |ui| {
                    let inner = card.size() - Vec2::splat(2.0 * v2_ui::tokens::space::LG);
                    ui.set_min_size(inner);
                    ui.set_max_size(inner);
                    ui.horizontal(|ui| {
                        ui.label(v2_ui::title("Details"));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if IconButton::new(Icon::Close, "Close details")
                                .size(v2_ui::tokens::size::CONTROL_SM)
                                .shortcut("I")
                                .show(ui)
                                .clicked()
                            {
                                close = true;
                            }
                        });
                    });
                    ui.add_space(12.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if let Some(path) = self.photo_path.as_ref().or(self.album.current()) {
                                let name = path.file_name().unwrap_or_default().to_string_lossy();
                                ui.label(
                                    egui::RichText::new(name)
                                        .font(egui::FontId::new(15.0, v2_ui::semibold()))
                                        .color(p.text),
                                );
                                if let Some(folder) =
                                    path.parent().filter(|f| !f.as_os_str().is_empty())
                                {
                                    ui.label(v2_ui::caption(
                                        crate::model::user_path(folder).display().to_string(),
                                    ));
                                }
                                ui.add_space(8.0);
                                ui.horizontal_wrapped(|ui| {
                                    if Button::secondary("Show in Explorer")
                                        .icon(Icon::Folder)
                                        .small()
                                        .enabled(can_act)
                                        .show(ui)
                                        .clicked()
                                    {
                                        action = Some("reveal");
                                    }
                                    if Button::ghost("Copy path")
                                        .small()
                                        .enabled(can_act)
                                        .show(ui)
                                        .clicked()
                                    {
                                        action = Some("copypath");
                                    }
                                });
                                ui.add_space(14.0);
                            } else {
                                ui.label(egui::RichText::new("No photo open").color(p.muted));
                                ui.add_space(14.0);
                            }
                            if let Some(photo) = &self.photo {
                                egui::Grid::new("luma-facts")
                                    .num_columns(2)
                                    .spacing([16.0, 8.0])
                                    .show(ui, |ui| {
                                        let fact = |ui: &mut egui::Ui, k: &str, v: String| {
                                            ui.label(egui::RichText::new(k).color(p.muted));
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(v).color(p.text),
                                                )
                                                .wrap(),
                                            );
                                            ui.end_row();
                                        };
                                        fact(
                                            ui,
                                            "Dimensions",
                                            format!("{} × {}", photo.width, photo.height),
                                        );
                                        if let Some(source) = photo.preview_source {
                                            fact(
                                                ui,
                                                "Original",
                                                format!(
                                                    "{} × {} (shown scaled)",
                                                    source[0], source[1]
                                                ),
                                            );
                                        }
                                        fact(ui, "Format", format_label(&photo.format));
                                        if !photo.decoder.is_empty() {
                                            fact(ui, "Read by", photo.decoder.clone());
                                        }
                                        fact(ui, "File size", file_size(photo.file_bytes));
                                        if photo.frames.len() > 1 {
                                            fact(ui, "Frames", photo.frames.len().to_string());
                                        }
                                        if let Some(o) = photo.applied_orientation {
                                            fact(
                                                ui,
                                                "Orientation",
                                                format!("Turned upright (EXIF {o})"),
                                            );
                                        }
                                        fact(ui, "In memory", file_size(photo.byte_len() as u64));
                                    });
                                // Camera facts are their own group: they describe the shot,
                                // not the file or how Luma holds it.
                                let camera =
                                    photo.exif.as_ref().map(|e| e.rows()).unwrap_or_default();
                                if !camera.is_empty() {
                                    ui.add_space(14.0);
                                    v2_ui::section_label(ui, "Camera");
                                    egui::Grid::new("luma-camera")
                                        .num_columns(2)
                                        .spacing([16.0, 8.0])
                                        .show(ui, |ui| {
                                            for (label, value) in camera {
                                                ui.label(egui::RichText::new(label).color(p.muted));
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(value).color(p.text),
                                                    )
                                                    .wrap(),
                                                );
                                                ui.end_row();
                                            }
                                        });
                                }
                                ui.add_space(12.0);
                                ui.label(v2_ui::caption(
                                    "Rotating changes this view only. The file is never modified.",
                                ));
                                for warning in &photo.warnings {
                                    ui.add_space(6.0);
                                    ui.label(egui::RichText::new(warning).small().color(p.warning));
                                }
                            }
                            ui.add_space(16.0);
                            v2_ui::chrome::menu_separator(ui);
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("Slideshow")
                                    .font(egui::FontId::new(14.0, v2_ui::semibold()))
                                    .color(p.text),
                            );
                            ui.add_space(6.0);
                            ui.label(v2_ui::caption("Seconds per photo"));
                            ui.horizontal(|ui| {
                                let width = (ui.available_width() - 48.0).max(80.0);
                                let interval = v2_ui::slider(
                                    ui,
                                    &mut self.slideshow_seconds,
                                    1.0..=30.0,
                                    1.0,
                                    width,
                                    "Seconds per photo",
                                );
                                self.interval_focus = Some(interval.id);
                                ui.label(
                                    egui::RichText::new(format!("{:.0} s", self.slideshow_seconds))
                                        .color(p.text),
                                );
                            });
                            if self.album.truncated {
                                ui.add_space(12.0);
                                let _ = v2_ui::Banner::warning(
                                    "Only the first 20,000 entries of this folder are shown",
                                )
                                .show(ui, |_| {});
                            }
                        });
                });
            });
        if close {
            self.info = false;
            ctx.request_repaint();
        }
        self.apply_menu(ctx, action);
    }

    fn canvas(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let ctx = ui.ctx().clone();
        let photo = self.photo.as_ref().unwrap().clone();
        let now = Instant::now();
        if photo.frames.len() > 1 && !self.animation_paused {
            if self.frame_deadline <= now {
                if self.texture.is_some() {
                    self.frame_index = (self.frame_index + 1) % photo.frames.len();
                }
                self.texture = None;
                self.frame_deadline =
                    now + Duration::from_millis(photo.frames[self.frame_index].delay_ms as u64);
            }
            ctx.request_repaint_after(self.frame_deadline.saturating_duration_since(now));
        } else if photo.frames.len() > 1 {
            // Paused: keep the current frame, and make resuming immediate and not time-skewed.
            self.frame_deadline = now;
        }
        if self.texture.is_none() {
            self.texture = Some(ctx.load_texture(
                "photo",
                egui::ColorImage::from_rgba_unmultiplied(
                    [photo.width as usize, photo.height as usize],
                    &photo.frames[self.frame_index].rgba,
                ),
                egui::TextureOptions::LINEAR,
            ));
        }
        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        let ppp = ctx.pixels_per_point();
        self.view.sync_display_scale(ppp);
        if response.double_clicked() {
            if self.view.fit {
                self.view.actual_physical(ppp);
            } else {
                self.view.fit();
            }
        }
        let image = [photo.width as f32, photo.height as f32];
        let viewport = [rect.width(), rect.height()];
        if response.dragged() {
            self.view.pan[0] += response.drag_delta().x;
            self.view.pan[1] += response.drag_delta().y;
        }
        let cursor = ctx
            .input(|i| i.pointer.hover_pos())
            .unwrap_or(rect.center());
        let offset = cursor - rect.center();
        let scroll = if response.hovered() {
            ctx.input(|i| i.smooth_scroll_delta.y)
        } else {
            0.0
        };
        let keyboard = if !ctx.egui_wants_keyboard_input() {
            ctx.input(|i| {
                if i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals) {
                    1.25
                } else if i.key_pressed(Key::Minus) {
                    0.8
                } else {
                    1.0
                }
            })
        } else {
            1.0
        };
        if scroll.abs() > 0.1 {
            self.view.zoom_at(
                (scroll * 0.004).exp(),
                [offset.x, offset.y],
                image,
                viewport,
            );
        }
        if keyboard != 1.0 {
            self.view.zoom_at(keyboard, [0.0, 0.0], image, viewport);
        }
        // Whatever moved or zoomed it, the picture may not be dragged out of reach.
        self.view.clamp_pan(image, viewport);
        let scale = self.view.scale(image, viewport);
        let dimensions = self.view.dimensions(image);
        let size = Vec2::new(dimensions[0] * scale, dimensions[1] * scale);
        let destination = Rect::from_center_size(
            rect.center() + Vec2::new(self.view.pan[0], self.view.pan[1]),
            size,
        );
        let mut mesh = egui::Mesh::with_texture(self.texture.as_ref().unwrap().id());
        let uv = self.view.uv();
        let corners = [
            destination.left_top(),
            destination.right_top(),
            destination.right_bottom(),
            destination.left_bottom(),
        ];
        for i in 0..4 {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: corners[i],
                uv: Pos2::new(uv[i][0], uv[i][1]),
                color: Color32::WHITE,
            });
        }
        mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
        ui.painter()
            .with_clip_rect(rect)
            .add(egui::Shape::mesh(mesh));
        // The photo's own menu: the same actions as More, where the pointer already is.
        let can_act = self.can_act();
        let animated = photo.frames.len() > 1;
        let paused = self.animation_paused;
        let mut picked = None;
        response.context_menu(|ui| {
            ui.set_min_width(220.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut item = |ui: &mut egui::Ui,
                            icon: Option<Icon>,
                            label: &str,
                            keys: Option<&str>,
                            enabled: bool,
                            id: &'static str| {
                if v2_ui::chrome::menu_item(ui, icon, label, keys, enabled).clicked() {
                    picked = Some(id);
                    ui.close();
                }
            };
            item(
                ui,
                None,
                "Copy picture",
                Some("Ctrl+C"),
                can_act,
                "copyimage",
            );
            item(
                ui,
                None,
                "Copy file path",
                Some("Ctrl+Shift+C"),
                can_act,
                "copypath",
            );
            item(ui, None, "Copy file", None, can_act, "copyfile");
            v2_ui::chrome::menu_separator(ui);
            item(
                ui,
                Some(Icon::Folder),
                "Show in Explorer",
                Some("Ctrl+E"),
                can_act,
                "reveal",
            );
            item(
                ui,
                Some(Icon::Open),
                "Open with…",
                None,
                can_act,
                "openwith",
            );
            v2_ui::chrome::menu_separator(ui);
            item(ui, Some(Icon::Fit), "Fit to window", Some("F"), true, "fit");
            item(
                ui,
                Some(Icon::ActualSize),
                "Actual pixels",
                Some("1"),
                true,
                "actual",
            );
            item(
                ui,
                Some(Icon::RotateRight),
                "Rotate right",
                Some("R"),
                true,
                "rotate-right",
            );
            item(
                ui,
                Some(Icon::RotateLeft),
                "Rotate left",
                Some("Shift+R"),
                true,
                "rotate-left",
            );
            if animated {
                item(
                    ui,
                    Some(if paused { Icon::Play } else { Icon::Pause }),
                    if paused {
                        "Play animation"
                    } else {
                        "Pause animation"
                    },
                    Some("P"),
                    true,
                    "anim",
                );
            }
            v2_ui::chrome::menu_separator(ui);
            item(ui, Some(Icon::Info), "Details", Some("I"), true, "info");
        });
        self.apply_menu(&ctx, picked);
    }
}

#[derive(Clone, Copy, Default)]
struct Keys {
    left: bool,
    right: bool,
    page_up: bool,
    page_down: bool,
    home: bool,
    end: bool,
    fit: bool,
    actual: bool,
    rotate: bool,
    shift: bool,
    space: bool,
    info: bool,
    pause: bool,
    fullscreen: bool,
    escape: bool,
    open: bool,
    copy: bool,
    reveal: bool,
    back: bool,
    forward: bool,
}

fn loading_state(ui: &mut egui::Ui, stage: Rect) {
    let area = Rect::from_center_size(stage.center(), Vec2::new(stage.width().min(420.0), 64.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(area)
            .layout(egui::Layout::top_down(egui::Align::Center)),
        |ui| {
            v2_ui::chrome::centred_row(ui, egui::Id::new("luma-loading"), |ui| {
                v2_ui::loading(ui, "Opening photo…")
            });
            ui.add_space(6.0);
            ui.label(v2_ui::caption(
                "Decoding in the background. Large files take a moment.",
            ));
        },
    );
}

/// A centred sheet over a dimmed window, in the same family as the Details card. The dimmed
/// layer swallows clicks so nothing underneath moves; the sheet closes with its X or Escape.
/// `add` receives the room left for scrolling content. Returns true when it was closed.
fn modal_sheet(
    ctx: &egui::Context,
    p: &v2_ui::Palette,
    id: &str,
    title: &str,
    width: f32,
    add: impl FnOnce(&mut egui::Ui, f32),
) -> bool {
    let screen = ctx.content_rect();
    let width = (screen.width() - 32.0).clamp(280.0, width);
    let room = (screen.height() - 96.0).clamp(160.0, 680.0);
    let mut close = false;
    egui::Area::new(egui::Id::new((id, "scrim")))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .fade_in(false)
        .show(ctx, |ui| {
            let _ = ui.allocate_rect(screen, egui::Sense::click_and_drag());
            ui.painter().rect_filled(
                screen,
                0,
                Color32::from_black_alpha(if p.dark { 150 } else { 80 }),
            );
        });
    egui::Area::new(egui::Id::new((id, "sheet")))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::CENTER_CENTER)
        .fixed_pos(screen.center())
        .fade_in(false)
        .show(ctx, |ui| {
            v2_ui::chrome::frame_sheet(p).show(ui, |ui| {
                let inner = width - 2.0 * v2_ui::tokens::space::LG;
                ui.set_width(inner);
                ui.horizontal(|ui| {
                    ui.label(v2_ui::title(title));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if IconButton::new(Icon::Close, format!("Close {title}"))
                            .size(v2_ui::tokens::size::CONTROL_SM)
                            .shortcut("Esc")
                            .show(ui)
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
                ui.add_space(12.0);
                // An Area lets its content use only the size the Area had on the previous frame,
                // starting from 600 x 400, so a ScrollArea left to the Area stays near 400 px high
                // however tall the window is, and cannot grow when the window does. Asking for the
                // room here is independent of that; shorter content still takes only what it uses.
                let scroll = room - 72.0;
                ui.allocate_ui(egui::vec2(inner, scroll), |ui| add(ui, scroll));
            });
        });
    close
}

fn built_in_row(ui: &mut egui::Ui, p: &v2_ui::Palette, name: &str, note: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(name)
                .font(egui::FontId::new(14.0, v2_ui::semibold()))
                .color(p.text),
        );
        v2_ui::chip(ui, "Built in");
    });
    ui.label(v2_ui::caption(note));
    ui.add_space(6.0);
}

fn codec_row(ui: &mut egui::Ui, p: &v2_ui::Palette, status: &wic::FamilyStatus) {
    use wic::Availability::*;
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(status.family.label())
                .font(egui::FontId::new(14.0, v2_ui::semibold()))
                .color(p.text),
        );
        let (word, colour) = match &status.availability {
            Working { .. } => ("Working", p.success),
            Registered { .. } => ("Codec found", p.muted),
            Failing { .. } => ("Not working", p.warning),
            Missing => ("Not installed", p.muted),
            Unknown { .. } => ("Could not check", p.warning),
        };
        ui.label(egui::RichText::new(word).color(colour));
    });
    ui.label(v2_ui::caption(status.family.extensions()));
    match &status.availability {
        Working { decoder } => {
            ui.label(v2_ui::caption(format!("Decoded by: {decoder}")));
        }
        Registered { decoder } => {
            ui.label(v2_ui::caption(format!(
                "Windows lists \"{decoder}\". Luma has no sample to check it with, so each file decides."
            )));
        }
        Failing { decoder, error } => {
            ui.label(
                egui::RichText::new(format!(
                    "\"{decoder}\" is installed but its test sample failed: {error}"
                ))
                .small()
                .color(p.warning),
            );
        }
        Missing => {
            ui.label(v2_ui::caption(status.family.how_to_get()));
        }
        Unknown { reason } => {
            ui.label(
                egui::RichText::new(format!(
                    "Windows could not be asked, so Luma does not know whether this works: {reason}"
                ))
                .small()
                .color(p.warning),
            );
        }
    }
    ui.add_space(8.0);
}

/// The decoder names some formats after its Rust enum ("Png", "Jpeg"); people know PNG and JPEG.
/// Anything after " / " (the decoder or a preview note) is kept as written.
fn format_label(raw: &str) -> String {
    let (head, rest) = match raw.split_once(" / ") {
        Some((h, r)) => (h, Some(r)),
        None => (raw, None),
    };
    let head = match head {
        "Png" => "PNG",
        "Jpeg" => "JPEG",
        "Gif" => "GIF",
        "Bmp" => "BMP",
        "Tiff" => "TIFF",
        "Avif" => "AVIF",
        "WebP" => "WebP",
        other => other,
    };
    match rest {
        Some(r) => format!("{head} / {r}"),
        None => head.to_owned(),
    }
}

/// Windows' convention: 1024-based, labelled KB / MB / GB.
fn file_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

/// The native window handle clipboard and dialogs are owned by, or 0 if there is none yet.
fn native_window(frame: &eframe::Frame) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
        _ => 0,
    }
}

impl eframe::App for Luma {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.hwnd == 0 {
            self.hwnd = native_window(frame);
        }
        self.render(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn stale_success_and_error_cannot_replace_current_view() {
        let mut app = Luma::headless();
        app.generation = 7;
        app.loading = true;
        let ctx = egui::Context::default();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/pattern.png");
        let photo = crate::decode::decode(&path, &crate::decode::Cancel::never()).unwrap();
        app.accept_reply(
            crate::worker::Reply {
                generation: 6,
                album: None,
                path: path.clone(),
                photo: Ok(Some(Arc::new(photo))),
            },
            &ctx,
        );
        assert!(app.photo.is_none());
        assert!(app.loading);
        app.accept_reply(
            crate::worker::Reply {
                generation: 6,
                album: None,
                path,
                photo: Err("stale failure".into()),
            },
            &ctx,
        );
        assert!(app.error.is_none());
        assert!(app.loading);
    }
    #[test]
    fn decoder_format_names_read_as_people_write_them() {
        assert_eq!(format_label("Png"), "PNG");
        assert_eq!(format_label("Jpeg"), "JPEG");
        assert_eq!(format_label("WebP"), "WebP");
        assert_eq!(
            format_label("JPEG / scaled preview"),
            "JPEG / scaled preview"
        );
        assert_eq!(format_label("AVIF / libaom"), "AVIF / libaom");
        assert_eq!(format_label("HEIC / Windows codec"), "HEIC / Windows codec");
        // Unknown names pass through rather than being guessed at.
        assert_eq!(format_label("Qoi"), "Qoi");
    }
    #[test]
    fn window_keys_still_work_while_a_control_has_focus() {
        let (mut app, ctx) = keyboard_app();
        app.info = true;
        keyboard_frame(&mut app, &ctx, None);
        ctx.memory_mut(|m| m.request_focus(app.interval_focus.unwrap()));
        keyboard_frame(&mut app, &ctx, None);
        assert!(
            ctx.memory(|m| m.focused()).is_some(),
            "the slider holds focus"
        );
        // F11 is not a control key: it reaches the viewer even with the slider focused.
        keyboard_frame(&mut app, &ctx, Some(Key::F11));
        assert!(
            app.fullscreen,
            "F11 must toggle full screen while a control has focus"
        );
        // Arrows stay with the slider, which keeps focus rather than handing it to a neighbour.
        let before = app.slideshow_seconds;
        keyboard_frame(&mut app, &ctx, Some(Key::ArrowRight));
        keyboard_frame(&mut app, &ctx, Some(Key::ArrowRight));
        assert_eq!(app.album.index, 0);
        assert_eq!(
            app.slideshow_seconds,
            before + 2.0,
            "both presses reach the focused slider"
        );
        assert_eq!(
            ctx.memory(|m| m.focused()),
            app.interval_focus,
            "focus stays on the slider"
        );
        // Escape always leaves full screen.
        keyboard_frame(&mut app, &ctx, Some(Key::Escape));
        assert!(!app.fullscreen);
    }
    fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1100.0, 740.0))),
            events,
            ..Default::default()
        }
    }
    fn key_event(key: Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }
    fn keyboard_frame(app: &mut Luma, ctx: &egui::Context, key: Option<Key>) {
        let events = key
            .map(|k| vec![key_event(k, Default::default())])
            .unwrap_or_default();
        let _ = ctx.run_ui(raw_input(events), |ui| app.render(ui));
    }
    fn keyboard_app() -> (Luma, egui::Context) {
        let ctx = egui::Context::default();
        super::install(&ctx);
        let mut app = Luma::headless();
        app.album.files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        (app, ctx)
    }
    #[test]
    fn real_ui_keyboard_arrow_navigates() {
        let (mut app, ctx) = keyboard_app();
        keyboard_frame(&mut app, &ctx, None);
        keyboard_frame(&mut app, &ctx, Some(Key::ArrowRight));
        assert_eq!(app.album.index, 1);
    }
    #[test]
    fn escape_closes_the_top_sheet_first_and_open_sheets_hold_the_viewer_keys() {
        let (mut app, ctx) = keyboard_app();
        keyboard_frame(&mut app, &ctx, None);
        app.shortcuts = true;
        app.formats.open = true;
        app.fullscreen = true;
        // With a sheet open the arrows must not move through the folder behind it.
        keyboard_frame(&mut app, &ctx, Some(Key::ArrowRight));
        assert_eq!(app.album.index, 0);
        keyboard_frame(&mut app, &ctx, Some(Key::Escape));
        assert!(
            !app.formats.open,
            "the format sheet, drawn last, closes first"
        );
        assert!(app.shortcuts && app.fullscreen);
        keyboard_frame(&mut app, &ctx, Some(Key::Escape));
        assert!(!app.shortcuts && app.fullscreen, "then the shortcut list");
        keyboard_frame(&mut app, &ctx, Some(Key::Escape));
        assert!(!app.fullscreen, "only then does Escape leave full screen");
        // The sheets draw without a photo and without an answer from Windows yet.
        app.formats.open = true;
        app.shortcuts = true;
        keyboard_frame(&mut app, &ctx, None);
    }
    fn screen_frame(app: &mut Luma, ctx: &egui::Context, size: Vec2) {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            ..Default::default()
        };
        let _ = ctx.run_ui(raw, |ui| app.render(ui));
    }
    #[test]
    fn sheets_take_the_room_the_window_gives_and_grow_with_it() {
        let (mut app, ctx) = keyboard_app();
        app.shortcuts = true;
        let sheet = egui::Id::new(("luma-keys", "sheet"));
        let height = |ctx: &egui::Context| ctx.memory(|m| m.area_rect(sheet)).map(|r| r.height());
        // A small window first: the sheet fits inside it.
        for _ in 0..5 {
            screen_frame(&mut app, &ctx, Vec2::new(640.0, 440.0));
        }
        let small = height(&ctx).expect("the sheet is drawn");
        assert!(small < 440.0, "{small}");
        // Then a large one: the same open sheet takes the room, not the 400 px an Area starts
        // with and not the small window's height it last remembered.
        for _ in 0..5 {
            screen_frame(&mut app, &ctx, Vec2::new(1100.0, 740.0));
        }
        let large = height(&ctx).expect("the sheet is still drawn");
        assert!(large > 560.0 && large < 740.0, "{large}");
        assert!(large > small + 100.0, "{small} -> {large}");
    }
    #[test]
    fn a_right_click_on_the_photo_opens_its_menu() {
        let (mut app, ctx) = photo_app();
        let at = Pos2::new(550.0, 330.0);
        let secondary = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: Default::default(),
        };
        // As a real mouse arrives: hover, press, release on later frames, then time to draw.
        let mut painted = String::new();
        for events in [
            vec![egui::Event::PointerMoved(at)],
            vec![],
            vec![secondary(true)],
            vec![secondary(false)],
            vec![],
            vec![],
        ] {
            let out = ctx.run_ui(raw_input(events), |ui| app.render(ui));
            painted.clear();
            for clipped in &out.shapes {
                shape_text(&clipped.shape, &mut painted);
            }
        }
        assert!(egui::Popup::is_any_open(&ctx), "the photo's menu is open");
        // Open is not the same as drawn: the menu's own items must have been painted.
        assert!(
            painted.contains("Copy picture") && painted.contains("Show in Explorer"),
            "the menu is open but was not painted; text on screen: {painted:?}"
        );
    }
    /// All text painted by a shape, so a test can see what a frame really shows.
    fn shape_text(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(text) => {
                out.push_str(text.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(shapes) => {
                for inner in shapes {
                    shape_text(inner, out);
                }
            }
            _ => {}
        }
    }
    #[test]
    fn page_keys_and_the_mouse_side_buttons_step_through_the_folder() {
        let (mut app, ctx) = keyboard_app();
        keyboard_frame(&mut app, &ctx, None);
        keyboard_frame(&mut app, &ctx, Some(Key::PageDown));
        assert_eq!(app.album.index, 1);
        keyboard_frame(&mut app, &ctx, Some(Key::PageUp));
        assert_eq!(app.album.index, 0);
        assert_eq!(
            app.direction, -1,
            "the next prefetch follows the direction of travel"
        );
        let press = |button| egui::Event::PointerButton {
            pos: Pos2::new(500.0, 400.0),
            button,
            pressed: true,
            modifiers: Default::default(),
        };
        let _ = ctx.run_ui(raw_input(vec![press(egui::PointerButton::Extra2)]), |ui| {
            app.render(ui)
        });
        assert_eq!(app.album.index, 1, "forward button");
        let _ = ctx.run_ui(raw_input(vec![press(egui::PointerButton::Extra1)]), |ui| {
            app.render(ui)
        });
        assert_eq!(app.album.index, 0, "back button");
    }
    #[test]
    fn real_ui_interval_keyboard_does_not_navigate_photos() {
        let (mut app, ctx) = keyboard_app();
        app.info = true;
        keyboard_frame(&mut app, &ctx, None);
        ctx.memory_mut(|m| m.request_focus(app.interval_focus.unwrap()));
        let before = app.slideshow_seconds;
        keyboard_frame(&mut app, &ctx, Some(Key::ArrowRight));
        assert_eq!(app.album.index, 0);
        assert!(app.slideshow_seconds > before);
    }

    // ---- native action plumbing, with a fake launcher so nothing real is touched ------------

    static LAUNCHED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static REPLIES: Mutex<Vec<Done>> = Mutex::new(Vec::new());
    fn describe(action: &Action) -> String {
        match action {
            Action::Reveal(p) => format!("reveal {}", p.display()),
            Action::OpenWith(p) => format!("openwith {}", p.display()),
            Action::CopyPath(p) => format!("copypath {}", p.display()),
            Action::CopyFile(p) => format!("copyfile {}", p.display()),
            Action::CopyImage {
                frame,
                clockwise_quarter_turns,
                ..
            } => format!("copyimage frame {frame} turns {clockwise_quarter_turns}"),
        }
    }
    fn fake_launcher(
        action: Action,
        owner: isize,
        _wake: Box<dyn FnOnce() + Send>,
    ) -> mpsc::Receiver<Done> {
        LAUNCHED
            .lock()
            .unwrap()
            .push(format!("{} owner {owner}", describe(&action)));
        let (tx, rx) = mpsc::channel();
        if let Some(done) = REPLIES.lock().unwrap().pop() {
            let _ = tx.send(done);
        } else {
            // Never answers: the action stays "running", so a second request can be refused.
            std::mem::forget(tx);
        }
        rx
    }
    fn photo_app() -> (Luma, egui::Context) {
        // One test at a time may use the shared launcher log.
        let ctx = egui::Context::default();
        super::install(&ctx);
        let mut app = Luma::headless().with_launcher(fake_launcher);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/pattern.png");
        let photo = crate::decode::decode(&path, &crate::decode::Cancel::never()).unwrap();
        app.show_photo(photo, path);
        app.hwnd = 4242;
        (app, ctx)
    }
    static SERIAL: Mutex<()> = Mutex::new(());
    #[test]
    fn copy_shortcuts_and_menu_choices_start_the_right_single_action() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        LAUNCHED.lock().unwrap().clear();
        REPLIES.lock().unwrap().clear();
        let (mut app, ctx) = photo_app();
        let ctrl = egui::Modifiers::COMMAND;
        let ctrl_shift = egui::Modifiers {
            shift: true,
            ..egui::Modifiers::COMMAND
        };
        // View rotation is part of "copy picture".
        app.view.rotate(true);
        let mut raw = raw_input(vec![egui::Event::Copy]);
        raw.modifiers = ctrl;
        let _ = ctx.run_ui(raw, |ui| app.render(ui));
        {
            let log = LAUNCHED.lock().unwrap();
            assert_eq!(log.len(), 1, "{log:?}");
            assert_eq!(log[0], "copyimage frame 0 turns 1 owner 4242");
        }
        // A second request while the first is running is refused, not queued.
        let mut raw = raw_input(vec![egui::Event::Copy]);
        raw.modifiers = ctrl;
        let _ = ctx.run_ui(raw, |ui| app.render(ui));
        assert_eq!(LAUNCHED.lock().unwrap().len(), 1);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("one moment"))
        );
        // Finish it; then Ctrl+Shift+C copies the path and Ctrl+E reveals.
        app.native = None;
        let mut raw = raw_input(vec![egui::Event::Copy]);
        raw.modifiers = ctrl_shift;
        let _ = ctx.run_ui(raw, |ui| app.render(ui));
        app.native = None;
        let mut raw = raw_input(vec![key_event(Key::E, ctrl)]);
        raw.modifiers = ctrl;
        let _ = ctx.run_ui(raw, |ui| app.render(ui));
        app.native = None;
        app.apply_menu(&ctx, Some("openwith"));
        app.native = None;
        app.apply_menu(&ctx, Some("copyfile"));
        let log = LAUNCHED.lock().unwrap().clone();
        assert!(
            log[1].starts_with("copypath ") && log[1].ends_with("pattern.png owner 4242"),
            "{log:?}"
        );
        assert!(log[2].starts_with("reveal "), "{log:?}");
        assert!(log[3].starts_with("openwith "), "{log:?}");
        assert!(log[4].starts_with("copyfile "), "{log:?}");
        assert_eq!(log.len(), 5);
    }
    #[test]
    fn copy_belongs_to_a_focused_text_field_and_labels_cannot_hold_a_selection() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        LAUNCHED.lock().unwrap().clear();
        let (mut app, ctx) = photo_app();
        assert!(
            !ctx.global_style().interaction.selectable_labels,
            "the details text cannot own a Ctrl+C selection"
        );
        // A text field with focus: Ctrl+C is its, and must not start a picture copy.
        let mut text = String::from("copied from here");
        let id = egui::Id::new("some-text-field");
        let frame = |events: Vec<egui::Event>, focus: bool, app: &mut Luma, text: &mut String| {
            let mut raw = raw_input(events);
            raw.modifiers = egui::Modifiers::COMMAND;
            let _ = ctx.run_ui(raw, |ui| {
                let field = ui.add(egui::TextEdit::singleline(text).id(id));
                if focus {
                    field.request_focus();
                }
                app.render(ui);
            });
        };
        frame(vec![], true, &mut app, &mut text);
        frame(vec![], false, &mut app, &mut text);
        assert!(ctx.text_edit_focused(), "the field holds keyboard focus");
        frame(vec![egui::Event::Copy], false, &mut app, &mut text);
        assert!(
            LAUNCHED.lock().unwrap().is_empty(),
            "the text field's Copy was left alone"
        );
        // With nothing being typed in, the same key starts the picture copy.
        ctx.memory_mut(|m| m.surrender_focus(id));
        let _ = ctx.run_ui(raw_input(vec![]), |ui| app.render(ui));
        let mut raw = raw_input(vec![egui::Event::Copy]);
        raw.modifiers = egui::Modifiers::COMMAND;
        let _ = ctx.run_ui(raw, |ui| app.render(ui));
        assert_eq!(LAUNCHED.lock().unwrap().len(), 1);
    }
    #[test]
    fn opening_the_format_sheet_repeatedly_keeps_one_question_outstanding() {
        let ctx = egui::Context::default();
        super::install(&ctx);
        let mut app = Luma::headless();
        let before = codec_status_queries_started();
        app.open_formats(&ctx);
        app.formats.open = false; // closed again before the answer arrives
        app.open_formats(&ctx);
        app.formats.open = false;
        app.open_formats(&ctx);
        assert_eq!(
            codec_status_queries_started() - before,
            1,
            "reopening while a question is out does not start another"
        );
        assert!(app.formats.open);
        // The answer is kept apart from the sheet: it arrives even though the sheet was closed.
        let start = Instant::now();
        while app.formats.job.is_some() && start.elapsed() < Duration::from_secs(20) {
            app.poll_formats();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            app.formats
                .latest
                .as_ref()
                .is_some_and(|s| s.len() == wic::Family::ALL.len())
        );
        // With no question outstanding, the next open asks again (an extension may be new).
        let before = codec_status_queries_started();
        app.open_formats(&ctx);
        assert_eq!(codec_status_queries_started() - before, 1);
    }
    #[test]
    fn action_results_become_notices_and_failures_are_visible() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        LAUNCHED.lock().unwrap().clear();
        REPLIES.lock().unwrap().clear();
        REPLIES.lock().unwrap().push(Done {
            ok: false,
            message: "The clipboard is in use by another program. Try again.".into(),
        });
        let (mut app, ctx) = photo_app();
        app.apply_menu(&ctx, Some("copyimage"));
        keyboard_frame(&mut app, &ctx, None);
        let notice = app
            .notice
            .as_ref()
            .expect("a failure is shown, not swallowed");
        assert!(!notice.ok && notice.text.contains("clipboard is in use"));
        assert!(
            app.native.is_none(),
            "the action finished, so the next one may start"
        );
        // A silent success (Explorer opened) leaves no notice behind.
        app.notice = None;
        REPLIES.lock().unwrap().push(Done {
            ok: true,
            message: String::new(),
        });
        app.apply_menu(&ctx, Some("reveal"));
        keyboard_frame(&mut app, &ctx, None);
        assert!(app.notice.is_none());
    }
    #[test]
    fn native_actions_are_unavailable_without_a_photo_or_while_one_is_loading_another() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        LAUNCHED.lock().unwrap().clear();
        let ctx = egui::Context::default();
        super::install(&ctx);
        let mut empty = Luma::headless().with_launcher(fake_launcher);
        empty.apply_menu(&ctx, Some("copyimage"));
        empty.apply_menu(&ctx, Some("reveal"));
        assert!(
            LAUNCHED.lock().unwrap().is_empty(),
            "no photo, nothing to act on"
        );
        assert!(!empty.can_act());
    }

    // ---- navigation that keeps the photo on screen ------------------------------------------

    #[test]
    fn navigating_keeps_the_current_photo_until_the_next_arrives() {
        let (mut app, ctx) = {
            let ctx = egui::Context::default();
            super::install(&ctx);
            let mut app = Luma::headless();
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
            let a = dir.join("pattern.png");
            let photo = crate::decode::decode(&a, &crate::decode::Cancel::never()).unwrap();
            app.show_photo(photo, a.clone());
            app.album.files = vec![a, dir.join("pattern.jpg")];
            (app, ctx)
        };
        keyboard_frame(&mut app, &ctx, Some(Key::ArrowRight));
        assert!(app.loading, "a request is out");
        assert!(app.photo.is_some(), "the old photo stays up while it loads");
        assert_eq!(app.album.index, 1);
        // Header and actions still describe the photo that is on screen.
        assert_eq!(
            app.photo_path.as_ref().unwrap().file_name().unwrap(),
            "pattern.png"
        );
        // The reply for the new photo replaces it and resets the view.
        let jpg = app.album.files[1].clone();
        let photo = crate::decode::decode(&jpg, &crate::decode::Cancel::never()).unwrap();
        app.view.rotate(true);
        let generation = app.generation;
        app.accept_reply(
            crate::worker::Reply {
                generation,
                album: None,
                path: jpg.clone(),
                photo: Ok(Some(Arc::new(photo))),
            },
            &ctx,
        );
        assert!(!app.loading);
        assert_eq!(app.photo_path.as_ref(), Some(&jpg));
        assert_eq!(app.view.rotation, 0);
    }
    #[test]
    fn a_failed_navigation_shows_the_error_instead_of_the_old_photo() {
        let (mut app, ctx) = photo_app();
        app.loading = true;
        let generation = app.generation;
        app.accept_reply(
            crate::worker::Reply {
                generation,
                album: None,
                path: PathBuf::from("broken.heic"),
                photo: Err("HEIC / HEIF needs a Windows codec".into()),
            },
            &ctx,
        );
        assert!(app.photo.is_none() && app.photo_path.is_none());
        assert!(app.error.as_deref().unwrap().contains("HEIF"));
        assert!(!app.can_act());
    }
    #[test]
    fn the_busy_badge_waits_so_cached_neighbours_never_flash_it() {
        let (mut app, ctx) = photo_app();
        app.loading = true;
        app.loading_since = Some(Instant::now());
        let frames = |app: &mut Luma| {
            let out = ctx.run_ui(raw_input(vec![]), |ui| app.render(ui));
            out.shapes.len()
        };
        let quick = frames(&mut app);
        app.loading_since = Some(Instant::now() - Duration::from_secs(1));
        let waited = frames(&mut app);
        assert!(
            waited > quick,
            "the badge appears only after the wait ({quick} vs {waited})"
        );
    }
}
