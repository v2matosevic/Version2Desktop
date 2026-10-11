use crate::{
    decoder::Metadata,
    engine::MAX_CROSSFADE_SECONDS,
    media_session::{Artwork, Due, MediaCommand, MediaSnapshot, Throttle},
    player::{Command, Event, Player, State},
    queue::{self, Queue, Repeat},
    settings::{self, Settings},
};
use eframe::egui::{self, Key, RichText, TextureHandle};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    time::{Duration, Instant},
};

/// What the crossfade shortcut turns on when nothing has been chosen yet.
const DEFAULT_CROSSFADE: f32 = 6.0;
const SETTINGS_SAVE_DELAY: Duration = Duration::from_millis(700);

// ---------------------------------------------------------------------------------------------
// Wording shared by the stage, the details sheet and the tests

pub fn khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f32 / 1000.0)
    }
}

pub fn channels_label(channels: u16) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        n => format!("{n} channels"),
    }
}

/// The crossfade length as a person would say it.
pub fn seconds_label(seconds: f32) -> String {
    if seconds <= 0.0 {
        "Off".into()
    } else if seconds.fract() == 0.0 {
        format!("{} s", seconds as u32)
    } else {
        format!("{seconds:.1} s")
    }
}

/// What the file is and what Windows is given, from facts the decoder and the output reported.
/// `FLAC · 24-bit · 96 kHz · stereo → 48 kHz` says both the source and the conversion.
pub fn tech_line(s: &State) -> Option<String> {
    let info = s.info.as_ref()?;
    let mut parts = vec![info.codec.to_string()];
    if let Some(bits) = info.bits.filter(|_| info.lossless) {
        parts.push(if info.float {
            format!("{bits}-bit float")
        } else {
            format!("{bits}-bit")
        });
    }
    parts.push(khz(info.rate));
    parts.push(channels_label(info.channels));
    let mut line = parts.join(" · ");
    if let Some(out) = s.output.as_ref() {
        let mut to = Vec::new();
        if s.resampled {
            to.push(khz(out.rate));
        }
        if s.channel_conversion && out.channels as u16 != info.channels {
            to.push(channels_label(out.channels as u16));
        }
        if !to.is_empty() {
            line.push_str(" → ");
            line.push_str(&to.join(" "));
        }
    }
    Some(line)
}

pub struct Sonora {
    pub queue: Queue,
    player: Player,
    pub metadata: Metadata,
    art: Option<TextureHandle>,
    /// A colour taken from the cover art, washed behind the stage. `None` uses the accent.
    tint: Option<egui::Color32>,
    selected: Option<usize>,
    volume: f32,
    muted: bool,
    error: Option<String>,
    /// A track that was skipped, or other news that is not a failure of what is playing.
    warning: Option<String>,
    /// Device changes and similar.
    info: Option<String>,
    notice: String,
    search: String,
    import_tx: SyncSender<Vec<PathBuf>>,
    import_rx: Receiver<Result<Vec<PathBuf>, String>>,
    importing: bool,
    failed: HashSet<PathBuf>,
    show_shortcuts: bool,
    show_details: bool,
    /// Crossfade length in seconds; 0 is off.
    crossfade: f32,
    last_crossfade: f32,
    open_crossfade: bool,
    /// The track last sent to the player to open ahead of time.
    hint: Option<PathBuf>,
    /// While a blend runs, the row of the track that is fading out.
    fading_from: Option<usize>,
    settings_dir: Option<PathBuf>,
    saved: Settings,
    settings_dirty: Option<Instant>,
    /// Test fixtures: show this state instead of the player's.
    shown: Option<(State, f64)>,
    /// What Windows' media controls say about the connection, shown in the details sheet.
    media_note: String,
    media_rx: Option<Receiver<MediaCommand>>,
    media_throttle: Throttle,
    art_rgba: Option<Arc<Artwork>>,
    art_serial: u64,
    #[cfg(windows)]
    media: Option<crate::media_session::MediaSession>,
}

impl Sonora {
    #[doc(hidden)]
    pub fn visual_error(&mut self, error: &str) {
        self.error = Some(error.into());
    }
    #[doc(hidden)]
    pub fn visual_warning(&mut self, text: &str) {
        self.warning = Some(text.into());
    }
    /// Test fixtures only: show cover art without decoding a file.
    #[doc(hidden)]
    pub fn visual_artwork(&mut self, ctx: &egui::Context, size: [usize; 2], rgba: &[u8]) {
        self.tint = v2_ui::chrome::ambient_color(rgba);
        self.art = Some(ctx.load_texture(
            "current-art",
            egui::ColorImage::from_rgba_unmultiplied(size, rgba),
            Default::default(),
        ));
        self.queue.current = Some(0);
    }
    #[doc(hidden)]
    pub fn visual_loading(&mut self) {
        self.importing = true;
        self.notice = "Opening local files…".into();
    }
    /// Test fixtures only: draw as if this track were playing at `position`.
    #[doc(hidden)]
    pub fn visual_state(&mut self, state: State, position: f64) {
        self.shown = Some((state, position));
    }
    #[doc(hidden)]
    pub fn visual_next(&mut self, path: Option<PathBuf>) {
        self.hint = path;
    }
    #[doc(hidden)]
    pub fn visual_fading_from(&mut self, row: Option<usize>) {
        self.fading_from = row;
    }
    #[doc(hidden)]
    pub fn visual_crossfade(&mut self, seconds: f32) {
        self.crossfade = seconds;
    }
    #[doc(hidden)]
    pub fn visual_open_crossfade(&mut self) {
        self.open_crossfade = true;
    }
    #[doc(hidden)]
    pub fn visual_open_details(&mut self) {
        self.show_details = true;
    }
    #[doc(hidden)]
    pub fn crossfade_seconds(&self) -> f32 {
        self.crossfade
    }

    pub fn new(ctx: egui::Context, paths: Vec<PathBuf>) -> Self {
        let dir = settings::directory();
        let loaded = dir.as_deref().map(settings::load_from).unwrap_or_default();
        Self::with_settings(ctx, paths, loaded, dir)
    }

    /// `dir` is where choices are remembered; `None` remembers nothing (tests).
    pub fn with_settings(
        ctx: egui::Context,
        paths: Vec<PathBuf>,
        loaded: Settings,
        dir: Option<PathBuf>,
    ) -> Self {
        let loaded = loaded.sanitized();
        let player = Player::new(ctx.clone());
        let _ = player.send(Command::Volume(loaded.volume));
        let _ = player.send(Command::SetCrossfade(loaded.crossfade_seconds));
        let (import_tx, paths_rx) = mpsc::sync_channel::<Vec<PathBuf>>(1);
        let (result_tx, import_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("sonora-files".into())
            .spawn(move || {
                while let Ok(paths) = paths_rx.recv() {
                    let result = queue::collect_paths(&paths);
                    if result_tx.send(result).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("file worker");
        let mut app = Self {
            queue: Queue::default(),
            player,
            metadata: Metadata::default(),
            art: None,
            tint: None,
            selected: None,
            volume: loaded.volume,
            muted: false,
            error: None,
            warning: None,
            info: None,
            notice: String::new(),
            search: String::new(),
            import_tx,
            import_rx,
            importing: false,
            failed: HashSet::new(),
            show_shortcuts: false,
            show_details: false,
            crossfade: loaded.crossfade_seconds,
            last_crossfade: if loaded.crossfade_seconds > 0.0 {
                loaded.crossfade_seconds
            } else {
                DEFAULT_CROSSFADE
            },
            open_crossfade: false,
            hint: None,
            fading_from: None,
            settings_dir: dir,
            saved: loaded,
            settings_dirty: None,
            shown: None,
            media_note: "Not connected".into(),
            media_rx: None,
            media_throttle: Throttle::default(),
            art_rgba: None,
            art_serial: 0,
            #[cfg(windows)]
            media: None,
        };
        if !paths.is_empty() {
            app.import(paths);
        }
        app
    }

    /// Connects Sonora's own window to the Windows media controls: hardware media keys, the
    /// volume flyout and the lock screen. Failure leaves everything else working.
    #[cfg(windows)]
    pub fn attach_media(&mut self, hwnd: isize, ctx: egui::Context) {
        let (tx, rx) = crate::media_session::channel();
        match crate::media_session::MediaSession::start(hwnd, tx, move || ctx.request_repaint()) {
            Ok(session) => {
                self.media = Some(session);
                self.media_rx = Some(rx);
            }
            Err(e) => self.media_note = e,
        }
    }

    /// What to say about the Windows media controls in the details sheet.
    fn media_text(&self) -> String {
        #[cfg(windows)]
        if let Some(session) = &self.media {
            return session.status().describe();
        }
        self.media_note.clone()
    }

    /// Carries out what the Windows media controls asked for.
    fn media_commands(&mut self) {
        let Some(rx) = self.media_rx.take() else {
            return;
        };
        while let Ok(command) = rx.try_recv() {
            let s = self.state();
            match command {
                MediaCommand::Play => {
                    if s.rate == 0 && !s.loading {
                        self.toggle();
                    } else if !s.playing {
                        self.command(Command::Pause(false));
                    }
                }
                MediaCommand::Pause => {
                    if s.playing {
                        self.command(Command::Pause(true));
                    }
                }
                MediaCommand::Next => {
                    self.next(false);
                }
                MediaCommand::Previous => self.previous(),
                MediaCommand::Stop => self.command(Command::Stop),
                MediaCommand::SeekTo(position) => self.command(Command::Seek(position)),
            }
        }
        self.media_rx = Some(rx);
    }

    /// Tells Windows what is playing, only when it changes (and the position about once a second).
    #[cfg(windows)]
    fn sync_media(&mut self, s: &State) {
        let Some(session) = self.media.as_ref() else {
            return;
        };
        let has_tracks = !self.queue.tracks.is_empty();
        let snapshot = MediaSnapshot {
            active: s.rate != 0,
            playing: s.playing,
            title: if self.metadata.title.is_empty() {
                "Sonora".into()
            } else {
                self.metadata.title.clone()
            },
            artist: self.metadata.artist.clone(),
            album: self.metadata.album.clone(),
            duration: s.duration,
            can_next: has_tracks,
            can_previous: has_tracks,
            art_serial: self.art_serial,
        };
        let position = self.position();
        match self
            .media_throttle
            .decide(&snapshot, position, Instant::now())
        {
            Due::Snapshot => {
                let art = self.art_rgba.clone();
                let (active, duration) = (snapshot.active, snapshot.duration);
                session.update(snapshot, art);
                if active {
                    session.position(position, duration);
                }
            }
            Due::Position => session.position(position, s.duration),
            Due::Nothing => {}
        }
    }
    #[cfg(not(windows))]
    fn sync_media(&mut self, _s: &State) {}

    fn command(&mut self, c: Command) {
        if let Err(e) = self.player.send(c) {
            self.error = Some(e);
        }
    }
    fn state(&self) -> State {
        match &self.shown {
            Some((state, _)) => state.clone(),
            None => self.player.state(),
        }
    }
    fn position(&self) -> f64 {
        match &self.shown {
            Some((_, position)) => *position,
            None => self.player.position(),
        }
    }

    fn import(&mut self, mut paths: Vec<PathBuf>) {
        paths.truncate(queue::MAX_TRACKS);
        if paths.is_empty() {
            return;
        }
        match self.import_tx.try_send(paths) {
            Ok(()) => {
                self.importing = true;
                self.notice = "Opening local files…".into();
            }
            Err(_) => self.error = Some("A folder is already being opened".into()),
        }
    }
    fn files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Audio", queue::EXTENSIONS)
            .add_filter("Playlist", &["m3u8", "m3u", "sonora"])
            .pick_files()
        {
            self.import(paths);
        }
    }
    fn folder(&mut self) {
        if let Some(p) = rfd::FileDialog::new().pick_folder() {
            self.import(vec![p]);
        }
    }
    fn save_playlist(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Sonora playlist", &["sonora"])
            .add_filter("UTF-8 M3U", &["m3u8"])
            .set_file_name("My music.sonora")
            .save_file()
        {
            match queue::save_playlist(&p, &self.queue.tracks) {
                Ok(()) => self.notice = "Playlist saved".into(),
                Err(e) => self.error = Some(e),
            }
        }
    }
    fn load_playlist(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Playlist", &["sonora", "m3u8", "m3u"])
            .pick_file()
        {
            self.import(vec![p]);
        }
    }

    fn play(&mut self, i: usize, manual: bool) {
        if let Some(t) = self.queue.tracks.get(i) {
            let path = t.path.clone();
            self.queue.current = Some(i);
            self.selected = Some(i);
            if manual {
                self.failed.clear();
                self.error = None;
                self.warning = None;
            }
            self.metadata = Metadata {
                title: t.title.clone(),
                ..Default::default()
            };
            self.art = None;
            self.tint = None;
            // The player forgets the track it was told would follow; the new one is sent next frame.
            self.hint = None;
            self.fading_from = None;
            self.command(Command::Play(path, 0.0, false));
        }
    }
    fn toggle(&mut self) {
        let s = self.state();
        if s.rate == 0 && !s.loading {
            if !self.queue.tracks.is_empty() {
                self.play(self.queue.current.or(self.selected).unwrap_or(0), true);
            }
        } else {
            self.command(Command::Pause(s.playing));
        }
    }
    /// Starts the next playable track. Returns false when the queue has nothing more to play.
    fn next(&mut self, automatic: bool) -> bool {
        for _ in 0..self.queue.tracks.len() {
            let repeat = self.queue.repeat;
            if self
                .queue
                .current
                .is_some_and(|i| self.failed.contains(&self.queue.tracks[i].path))
                && repeat == Repeat::One
            {
                self.queue.repeat = Repeat::Off;
            }
            let next = self.queue.next_index(automatic);
            self.queue.repeat = repeat;
            let Some(i) = next else {
                return false;
            };
            if !self.failed.contains(&self.queue.tracks[i].path) {
                self.play(i, !automatic);
                return true;
            }
        }
        self.notice = "No playable tracks remain in this queue".into();
        false
    }
    fn previous(&mut self) {
        if self.position() > 3.0 {
            self.command(Command::Seek(0.0));
        } else if let Some(i) = self.queue.previous() {
            self.play(i, true);
        }
    }
    fn repeat(&mut self) {
        self.queue.repeat = match self.queue.repeat {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        };
    }
    fn volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        self.muted = false;
        self.command(Command::Volume(self.volume));
        self.settings_dirty = Some(Instant::now());
    }
    fn set_crossfade(&mut self, seconds: f32) {
        let seconds = seconds.clamp(0.0, MAX_CROSSFADE_SECONDS);
        if seconds == self.crossfade {
            return;
        }
        self.crossfade = seconds;
        if seconds > 0.0 {
            self.last_crossfade = seconds;
        }
        self.command(Command::SetCrossfade(seconds));
        self.settings_dirty = Some(Instant::now());
    }
    fn toggle_crossfade(&mut self) {
        let to = if self.crossfade > 0.0 {
            0.0
        } else {
            self.last_crossfade
        };
        self.set_crossfade(to);
        self.notice = format!("Crossfade {}", seconds_label(to).to_lowercase());
    }
    /// Remembers the volume and crossfade choice shortly after the last change.
    fn save_settings(&mut self, ctx: &egui::Context, force: bool) {
        let Some(since) = self.settings_dirty else {
            return;
        };
        if !force && since.elapsed() < SETTINGS_SAVE_DELAY {
            ctx.request_repaint_after(SETTINGS_SAVE_DELAY - since.elapsed());
            return;
        }
        self.settings_dirty = None;
        let now = Settings {
            crossfade_seconds: self.crossfade,
            volume: self.volume,
        }
        .sanitized();
        if now == self.saved {
            return;
        }
        if let Some(dir) = &self.settings_dir {
            match settings::save_to(dir, &now) {
                Ok(()) => self.saved = now,
                Err(e) => {
                    self.warning = Some(format!("{e}. Your choices apply until you close Sonora."))
                }
            }
        }
    }
    fn remove(&mut self) {
        if let Some(i) = self.selected {
            if self.queue.current == Some(i) {
                self.command(Command::Stop);
                self.metadata = Metadata::default();
                self.art = None;
                self.tint = None;
            }
            self.queue.remove(i);
            self.selected = None;
            self.fading_from = None;
        }
    }

    fn apply_meta(&mut self, ctx: &egui::Context, mut meta: Metadata) {
        if let Some(i) = self.queue.current {
            self.queue.tracks[i].title = meta.title.clone();
        }
        self.tint = meta
            .artwork
            .as_ref()
            .and_then(|(_, _, p)| v2_ui::chrome::ambient_color(p));
        self.art_serial += 1;
        self.art_rgba = meta.artwork.as_ref().map(|(w, h, p)| {
            Arc::new(Artwork {
                width: *w,
                height: *h,
                rgba: p.clone(),
            })
        });
        self.art = meta.artwork.take().map(|(w, h, p)| {
            ctx.load_texture(
                "current-art",
                egui::ColorImage::from_rgba_unmultiplied([w, h], &p),
                Default::default(),
            )
        });
        self.metadata = meta;
    }

    /// The player moved on to the next song by itself (gapless or blended): follow it.
    fn advanced(&mut self, ctx: &egui::Context, path: PathBuf, meta: Metadata) {
        let old = self.queue.current;
        let fading = self.state().fading.is_some();
        if let Some(i) = self.queue.jump_to_path(&path) {
            if old != Some(i) {
                self.fading_from = if fading { old } else { None };
                if self.selected == old {
                    self.selected = Some(i);
                }
            }
        }
        // The player has used up the track it was given; the one after must be sent.
        self.hint = None;
        self.apply_meta(ctx, meta);
    }

    fn events(&mut self, ctx: &egui::Context) {
        self.media_commands();
        for event in self.player.events() {
            match event {
                Event::Loaded(meta) => self.apply_meta(ctx, meta),
                Event::Advanced { path, meta } => self.advanced(ctx, path, meta),
                Event::Ended => {
                    if !self.next(true) {
                        // The queue is finished: return to the stopped state, ready to start over.
                        self.command(Command::Stop);
                    }
                }
                Event::NextFailed { path, error } => {
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    self.warning = Some(format!("{name}: {error}. Sonora will skip it."));
                    self.failed.insert(path);
                    self.hint = None;
                }
                Event::Notice(text) => self.info = Some(text),
                Event::Error(e) => {
                    if e.starts_with("Cannot open Windows audio output") {
                        self.error = Some(e);
                        continue;
                    }
                    if let Some(i) = self.queue.current {
                        self.failed.insert(self.queue.tracks[i].path.clone());
                        self.error = Some(format!(
                            "{}: {e}",
                            self.queue.tracks[i]
                                .path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                        ));
                    } else {
                        self.error = Some(e);
                    }
                    self.next(true);
                }
            }
        }
        while let Ok(result) = self.import_rx.try_recv() {
            self.importing = false;
            match result {
                Ok(paths) => {
                    let n = self.queue.add(paths);
                    self.notice = if n == 0 {
                        "No new audio files found".into()
                    } else {
                        format!("Added {n} tracks · {} / 5,000", self.queue.tracks.len())
                    };
                }
                Err(e) => self.error = Some(e),
            }
        }
    }

    /// Keeps the player told which track follows, so a change costs no opening time. The answer
    /// follows shuffle, repeat, failed files and queue edits, and only changes when they do.
    fn plan_next(&mut self, s: &State) {
        let active = self.queue.current.is_some() && (s.rate != 0 || s.loading);
        let want = if active {
            self.queue
                .peek_next(&self.failed)
                .map(|i| self.queue.tracks[i].path.clone())
        } else {
            None
        };
        if want != self.hint && self.player.send(Command::SetNext(want.clone())).is_ok() {
            self.hint = want;
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F1)) {
            self.show_shortcuts = true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            self.show_shortcuts = false;
            self.show_details = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F11)) {
            let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        if ctx.memory(|m| m.focused()) == Some(egui::Id::new("queue-search")) {
            return;
        }
        // A control reached with Tab owns Space, Enter and the arrows: they activate a focused
        // button or move a focused slider, and must not also play, seek or change the volume.
        let focused = ctx.memory(|m| m.focused()).is_some();
        let mut files = false;
        let mut folder = false;
        let mut action = None;
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, Key::O) {
                folder = true;
            } else if i.consume_key(egui::Modifiers::CTRL, Key::O) {
                files = true;
            }
            for (mods, key, name) in [
                (egui::Modifiers::CTRL, Key::ArrowLeft, "prev"),
                (egui::Modifiers::CTRL, Key::ArrowRight, "next"),
                (egui::Modifiers::SHIFT, Key::ArrowLeft, "back30"),
                (egui::Modifiers::SHIFT, Key::ArrowRight, "forward30"),
                (egui::Modifiers::NONE, Key::Enter, "selected"),
                (egui::Modifiers::NONE, Key::Space, "toggle"),
                (egui::Modifiers::NONE, Key::ArrowLeft, "back"),
                (egui::Modifiers::NONE, Key::ArrowRight, "forward"),
                (egui::Modifiers::NONE, Key::ArrowUp, "up"),
                (egui::Modifiers::NONE, Key::ArrowDown, "down"),
                (egui::Modifiers::NONE, Key::M, "mute"),
                (egui::Modifiers::NONE, Key::S, "shuffle"),
                (egui::Modifiers::NONE, Key::R, "repeat"),
                (egui::Modifiers::NONE, Key::C, "crossfade"),
                (egui::Modifiers::NONE, Key::I, "details"),
                (egui::Modifiers::NONE, Key::Delete, "remove"),
            ] {
                let owned = matches!(
                    key,
                    Key::Enter
                        | Key::Space
                        | Key::ArrowLeft
                        | Key::ArrowRight
                        | Key::ArrowUp
                        | Key::ArrowDown
                ) && mods == egui::Modifiers::NONE;
                if focused && owned {
                    continue;
                }
                if i.consume_key(mods, key) {
                    action = Some(name);
                }
            }
        });
        if files {
            self.files();
        }
        if folder {
            self.folder();
        }
        match action {
            Some("prev") => self.previous(),
            Some("next") => {
                self.next(false);
            }
            Some("toggle") => self.toggle(),
            Some("selected") => {
                if let Some(i) = self.selected {
                    self.play(i, true);
                }
            }
            Some("back30") => self.command(Command::Seek((self.position() - 30.0).max(0.0))),
            Some("forward30") => self.command(Command::Seek(self.position() + 30.0)),
            Some("back") => self.command(Command::Seek((self.position() - 5.0).max(0.0))),
            Some("forward") => self.command(Command::Seek(self.position() + 5.0)),
            Some("up") => self.volume(self.volume + 0.05),
            Some("down") => self.volume(self.volume - 0.05),
            Some("mute") => {
                self.muted = !self.muted;
                self.command(Command::Volume(if self.muted { 0.0 } else { self.volume }));
            }
            Some("shuffle") => self.queue.shuffle = !self.queue.shuffle,
            Some("repeat") => self.repeat(),
            Some("crossfade") => self.toggle_crossfade(),
            Some("details") => self.show_details = !self.show_details,
            Some("remove") => self.remove(),
            _ => {}
        }
    }

    pub fn draw(&mut self, root: &mut egui::Ui) {
        let context = root.ctx().clone();
        let ctx = &context;
        self.events(ctx);
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect::<Vec<_>>()
        });
        if !dropped.is_empty() {
            self.import(dropped);
        }
        let s = self.state();
        if s.fading.is_none() {
            self.fading_from = None;
        }
        if self.shown.is_none() {
            self.plan_next(&s);
            self.sync_media(&s);
        }
        self.save_settings(ctx, false);
        if s.playing && ctx.input(|i| i.viewport().minimized != Some(true)) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let p = v2_ui::palette(ctx);
        let narrow = root.available_width() < 760.0;

        if !narrow {
            // One ambient wash under the header and the stage, so the window reads as one room.
            let tint = self.tint.unwrap_or(p.accent);
            v2_ui::chrome::paint_ambient(root.painter(), root.max_rect(), tint, &p);
        }
        let header = v2_ui::chrome::frame_header(&p);
        egui::Panel::top("header")
            .exact_size(v2_ui::tokens::size::HEADER)
            .frame(if narrow {
                header
            } else {
                header.fill(egui::Color32::TRANSPARENT)
            })
            .show_separator_line(narrow)
            .show(root, |ui| self.header(ui, narrow));

        if narrow {
            egui::Panel::bottom("transport")
                .frame(
                    egui::Frame::new()
                        .fill(p.surface)
                        .inner_margin(egui::Margin::symmetric(16, 10)),
                )
                .show(root, |ui| {
                    let width = ui.available_width();
                    self.seek_row(ui, &s, width);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        self.transport(ui, &s);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            self.volume_controls(ui, 84.0);
                            self.stop_button(ui);
                        });
                    });
                });
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(p.bg)
                        .inner_margin(egui::Margin::symmetric(16, 8)),
                )
                .show(root, |ui| {
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(64.0, 64.0), egui::Sense::hover());
                        self.paint_art(ui, rect, &p);
                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            ui.add_space(8.0);
                            self.now_playing_text(ui, &s, 18.0);
                        });
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        self.crossfade_chip(ui, &s);
                        self.details_button(ui);
                    });
                    ui.add_space(6.0);
                    self.banners(ui);
                    self.queue_panel(ui, &p, false);
                });
        } else {
            egui::CentralPanel::default()
                .frame(egui::Frame::new())
                .show(root, |ui| {
                    let full = ui.max_rect();
                    let gutter = 24.0;
                    let queue_w = (full.width() * 0.38).clamp(300.0, 440.0);
                    let queue = egui::Rect::from_min_max(
                        egui::pos2(full.right() - queue_w - gutter, full.top() + 8.0),
                        egui::pos2(full.right() - gutter, full.bottom() - gutter),
                    );
                    let stage = egui::Rect::from_min_max(
                        egui::pos2(full.left() + gutter, full.top() + 8.0),
                        egui::pos2(queue.left() - gutter, full.bottom() - gutter),
                    );
                    self.stage(ui, stage, &s, &p);
                    // The queue sheet: a calm surface, the list on it.
                    ui.painter().rect(
                        queue,
                        v2_ui::tokens::radius::XL,
                        if p.dark {
                            p.surface.gamma_multiply(0.92)
                        } else {
                            p.raised
                        },
                        egui::Stroke::new(
                            1.0,
                            if p.dark {
                                egui::Color32::from_white_alpha(12)
                            } else {
                                p.border
                            },
                        ),
                        egui::StrokeKind::Inside,
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(queue.shrink(16.0)), |ui| {
                        self.queue_panel(ui, &p, true);
                    });
                });
        }

        egui::Window::new("Keyboard shortcuts")
            .open(&mut self.show_shortcuts)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                v2_ui::shortcut_table(
                    ui,
                    &[
                        ("Space", "Play or pause"),
                        ("Ctrl+Left / Ctrl+Right", "Previous or next track"),
                        ("Left / Right", "Seek 5 seconds"),
                        ("Shift+Left / Shift+Right", "Seek 30 seconds"),
                        ("Up / Down", "Volume"),
                        ("M", "Mute"),
                        ("S", "Shuffle"),
                        ("R", "Repeat"),
                        ("C", "Crossfade on or off"),
                        ("I", "Playback details"),
                        ("Ctrl+O", "Add music"),
                        ("Ctrl+Shift+O", "Add a folder"),
                        ("Enter", "Play the selected track"),
                        ("Delete", "Remove the selected track"),
                        ("F11", "Full screen"),
                        ("F1", "This list"),
                    ],
                );
                ui.add_space(6.0);
                ui.label(v2_ui::caption("Double-click a track to play it."));
            });
        self.details_window(ctx, &s);
        self.shortcuts(ctx);
    }

    fn header(&mut self, ui: &mut egui::Ui, narrow: bool) {
        let importing = self.importing;
        let notice = self.notice.clone();
        let has_tracks = !self.queue.tracks.is_empty();
        let mut action = None;
        v2_ui::chrome::header(
            ui,
            |ui| {
                v2_ui::chrome::wordmark(ui, v2_ui::App::Sonora);
                if importing {
                    ui.add_space(8.0);
                    v2_ui::loading(ui, "Opening local files…");
                } else if !notice.is_empty() && !narrow {
                    ui.add_space(8.0);
                    ui.label(v2_ui::caption(notice));
                }
            },
            |ui| {
                v2_ui::chrome::more_menu(ui, |ui| {
                    if v2_ui::chrome::menu_item(
                        ui,
                        Some(v2_ui::Icon::Folder),
                        "Add a folder…",
                        Some("Ctrl+Shift+O"),
                        !importing,
                    )
                    .clicked()
                    {
                        action = Some("folder");
                    }
                    if v2_ui::chrome::menu_item(
                        ui,
                        Some(v2_ui::Icon::Open),
                        "Open a playlist…",
                        None,
                        !importing,
                    )
                    .clicked()
                    {
                        action = Some("load");
                    }
                    if v2_ui::chrome::menu_item(
                        ui,
                        Some(v2_ui::Icon::Queue),
                        "Save queue as playlist…",
                        None,
                        has_tracks,
                    )
                    .clicked()
                    {
                        action = Some("save");
                    }
                    v2_ui::chrome::menu_separator(ui);
                    if v2_ui::chrome::menu_item(
                        ui,
                        Some(v2_ui::Icon::Equalizer),
                        "Crossfade…",
                        Some("C"),
                        true,
                    )
                    .clicked()
                    {
                        action = Some("crossfade");
                    }
                    if v2_ui::chrome::menu_item(
                        ui,
                        Some(v2_ui::Icon::Info),
                        "Playback details",
                        Some("I"),
                        true,
                    )
                    .clicked()
                    {
                        action = Some("details");
                    }
                    v2_ui::chrome::menu_separator(ui);
                    v2_ui::chrome::theme_menu(ui);
                    v2_ui::chrome::menu_separator(ui);
                    if v2_ui::chrome::menu_item(
                        ui,
                        Some(v2_ui::Icon::Keyboard),
                        "Keyboard shortcuts",
                        Some("F1"),
                        true,
                    )
                    .clicked()
                    {
                        action = Some("keys");
                    }
                });
                let add = v2_ui::Button::primary(if narrow { "Add" } else { "Add music" })
                    .icon(v2_ui::Icon::Plus)
                    .pill()
                    .enabled(!importing)
                    .shortcut("Ctrl+O")
                    .show(ui);
                if add.clicked() {
                    action = Some("files");
                }
            },
        );
        match action {
            Some("files") => self.files(),
            Some("folder") => self.folder(),
            Some("load") => self.load_playlist(),
            Some("save") => self.save_playlist(),
            Some("keys") => self.show_shortcuts = true,
            Some("crossfade") => self.open_crossfade = true,
            Some("details") => self.show_details = true,
            _ => {}
        }
    }

    /// The listening stage: artwork, title, seek and transport as one centred column.
    fn stage(&mut self, ui: &mut egui::Ui, stage: egui::Rect, s: &State, p: &v2_ui::Palette) {
        let mut top = stage.top();
        if self.has_banner() {
            // The banner is as tall as its wrapped detail; the column starts below it.
            let banner = egui::Rect::from_min_size(stage.min, egui::vec2(stage.width(), 76.0));
            let used = ui
                .scope_builder(egui::UiBuilder::new().max_rect(banner), |ui| {
                    self.banners(ui)
                })
                .response
                .rect;
            top = used.bottom() + 8.0;
        }
        let area = egui::Rect::from_min_max(egui::pos2(stage.left(), top), stage.max);
        let width = (area.width() - 32.0).clamp(240.0, 420.0);
        let art = (area.height() - 266.0).min(width).clamp(120.0, 360.0);
        let empty = self.queue.tracks.is_empty();
        // The start card is about 300 pt tall; it takes the artwork's place and the column is
        // centred on its own height, so no gap opens between the card and the transport.
        let card_h = 300.0;
        let block_h = if empty { card_h + 168.0 } else { art + 256.0 };
        let x0 = area.center().x - width / 2.0;
        let mut y = area.top() + ((area.height() - block_h) / 2.0).max(0.0);
        if empty {
            let card = egui::Rect::from_min_size(
                egui::pos2(x0 - 24.0, y),
                egui::vec2(width + 48.0, card_h),
            );
            self.drop_card(ui, card);
            y += card_h + 20.0;
        } else {
            let art_rect = egui::Rect::from_min_size(
                egui::pos2(area.center().x - art / 2.0, y),
                egui::vec2(art, art),
            );
            v2_ui::chrome::soft_shadow(ui.painter(), art_rect, v2_ui::tokens::radius::XL, p.dark);
            self.paint_art(ui, art_rect, p);
            y += art + 20.0;
            // Title, artist and the technical line: three rows that must clear the seek bar.
            let text = egui::Rect::from_min_size(egui::pos2(x0, y), egui::vec2(width, 80.0));
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(text)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
                |ui| self.now_playing_text(ui, s, 26.0),
            );
            y += 88.0;
        }
        let seek = egui::Rect::from_min_size(egui::pos2(x0, y), egui::vec2(width, 22.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(seek), |ui| {
            self.seek_row(ui, s, width)
        });
        y += 34.0;
        let transport = egui::Rect::from_min_size(egui::pos2(x0, y), egui::vec2(width, 56.0));
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(transport)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                // Centre the five transport controls in the column.
                let controls = 40.0 * 4.0 + 56.0 + 12.0 * 4.0;
                ui.add_space(((width - controls) / 2.0).max(0.0));
                ui.spacing_mut().item_spacing.x = 12.0;
                self.transport(ui, s);
            },
        );
        y += 64.0;
        let utility = egui::Rect::from_min_size(egui::pos2(x0, y), egui::vec2(width, 32.0));
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(utility)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                self.stop_button(ui);
                self.details_button(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // What is left of the row after Stop, Details, the chip and the mute button.
                    let slider = (width - (32.0 * 3.0 + 6.0 * 5.0 + 128.0)).clamp(56.0, 120.0);
                    self.volume_controls(ui, slider);
                    self.crossfade_chip(ui, s);
                });
            },
        );
    }

    fn paint_art(&self, ui: &mut egui::Ui, rect: egui::Rect, p: &v2_ui::Palette) {
        match &self.art {
            Some(art) => {
                egui::Image::new(art)
                    .corner_radius(if rect.width() > 100.0 {
                        v2_ui::tokens::radius::XL
                    } else {
                        v2_ui::tokens::radius::LG
                    })
                    .paint_at(ui, rect);
            }
            None => v2_ui::chrome::paint_placeholder_art(
                ui.painter(),
                rect,
                self.tint.unwrap_or(p.accent),
                v2_ui::Icon::Music,
                p,
            ),
        }
    }

    fn now_playing_text(&self, ui: &mut egui::Ui, s: &State, size: f32) {
        let p = v2_ui::palette(ui.ctx());
        let title = if self.metadata.title.is_empty() {
            "Nothing playing"
        } else {
            &self.metadata.title
        };
        ui.add(
            egui::Label::new(
                RichText::new(title)
                    .font(egui::FontId::new(size, v2_ui::semibold()))
                    .color(p.text),
            )
            .truncate(),
        );
        let mut meta = Vec::new();
        if !self.metadata.artist.is_empty() {
            meta.push(self.metadata.artist.as_str());
        }
        if !self.metadata.album.is_empty() {
            meta.push(self.metadata.album.as_str());
        }
        let line = if meta.is_empty() {
            "Choose a track from Up next".to_string()
        } else {
            meta.join(" · ")
        };
        ui.add(egui::Label::new(RichText::new(line).color(p.muted)).truncate());
        if let Some(tech) = tech_line(s) {
            ui.add(egui::Label::new(v2_ui::caption(tech)).truncate());
        }
    }

    /// Empty queue: the family's start card in the artwork's place.
    fn drop_card(&mut self, ui: &mut egui::Ui, area: egui::Rect) {
        let importing = self.importing;
        let mut action = None;
        v2_ui::chrome::Welcome::new(v2_ui::Icon::Music, "Drop music here")
            .body("Or add files and folders from this PC.")
            .foot("MP3 · FLAC · WAV · AAC/M4A · Ogg Vorbis · Opus · AIFF")
            .show(ui, area, |ui| {
                if v2_ui::Button::primary("Add music")
                    .icon(v2_ui::Icon::Plus)
                    .pill()
                    .enabled(!importing)
                    .show(ui)
                    .clicked()
                {
                    action = Some(false);
                }
                if v2_ui::Button::secondary("Add a folder")
                    .icon(v2_ui::Icon::Folder)
                    .pill()
                    .enabled(!importing)
                    .show(ui)
                    .clicked()
                {
                    action = Some(true);
                }
            });
        match action {
            Some(false) => self.files(),
            Some(true) => self.folder(),
            None => {}
        }
    }

    fn seek_row(&mut self, ui: &mut egui::Ui, s: &State, width: f32) {
        let pos = self.position();
        let p = v2_ui::palette(ui.ctx());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.add_sized(
                [40.0, 20.0],
                egui::Label::new(v2_ui::caption(v2_ui::format_duration(pos))),
            );
            let rail = (width - 100.0).max(48.0);
            let seek = ui
                .allocate_ui(egui::vec2(rail, 20.0), |ui| {
                    v2_ui::Seekbar::new(pos, Some(s.duration)).show(ui)
                })
                .inner;
            // Where the next song will start to blend in: the last stretch of the rail.
            if self.crossfade > 0.0 && s.duration > 0.0 && self.hint.is_some() {
                let zone = f64::from(self.crossfade).min(s.duration / 2.0);
                let track = seek
                    .response
                    .rect
                    .shrink2(egui::vec2(v2_ui::tokens::size::THUMB / 2.0, 0.0));
                let from = track.left() + track.width() * (1.0 - (zone / s.duration) as f32);
                let y = track.center().y;
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(from, y - 3.0),
                        egui::pos2(track.right(), y + 3.0),
                    ),
                    3.0,
                    p.accent.gamma_multiply(0.5),
                );
                // A thin tick where the blend begins, so the zone reads as a mark and not a thumb.
                ui.painter().rect_filled(
                    egui::Rect::from_center_size(egui::pos2(from, y), egui::vec2(2.0, 12.0)),
                    1.0,
                    p.accent_text,
                );
            }
            if let Some(pos) = seek.seek_to {
                self.command(Command::Seek(pos));
            }
            ui.add_sized(
                [40.0, 20.0],
                egui::Label::new(v2_ui::caption(v2_ui::format_duration(s.duration))),
            );
        });
    }

    fn transport(&mut self, ui: &mut egui::Ui, s: &State) {
        let has_tracks = !self.queue.tracks.is_empty();
        let lg = v2_ui::tokens::size::CONTROL_LG;
        if v2_ui::IconButton::new(v2_ui::Icon::Shuffle, "Shuffle")
            .size(lg)
            .selected(self.queue.shuffle)
            .shortcut("S")
            .show(ui)
            .clicked()
        {
            self.queue.shuffle = !self.queue.shuffle;
        }
        if v2_ui::IconButton::new(v2_ui::Icon::Prev, "Previous")
            .size(lg)
            .enabled(has_tracks)
            .shortcut("Ctrl+Left")
            .show(ui)
            .clicked()
        {
            self.previous();
        }
        let opening = s.loading && s.rate == 0;
        let label = if opening {
            "Loading…"
        } else if s.playing {
            "Pause"
        } else {
            "Play"
        };
        if v2_ui::IconButton::new(
            if s.playing {
                v2_ui::Icon::Pause
            } else {
                v2_ui::Icon::Play
            },
            label,
        )
        .prominent()
        .enabled(!opening && has_tracks)
        .shortcut("Space")
        .show(ui)
        .clicked()
        {
            self.toggle();
        }
        if v2_ui::IconButton::new(v2_ui::Icon::Next, "Next")
            .size(lg)
            .enabled(has_tracks)
            .shortcut("Ctrl+Right")
            .show(ui)
            .clicked()
        {
            self.next(false);
        }
        let label = match self.queue.repeat {
            Repeat::Off => "Repeat off",
            Repeat::All => "Repeat queue",
            Repeat::One => "Repeat one",
        };
        if v2_ui::IconButton::new(
            if self.queue.repeat == Repeat::One {
                v2_ui::Icon::RepeatOne
            } else {
                v2_ui::Icon::Repeat
            },
            label,
        )
        .size(lg)
        .selected(self.queue.repeat != Repeat::Off)
        .shortcut("R")
        .show(ui)
        .clicked()
        {
            self.repeat();
        }
    }

    fn stop_button(&mut self, ui: &mut egui::Ui) {
        if v2_ui::IconButton::new(v2_ui::Icon::Stop, "Stop")
            .enabled(!self.queue.tracks.is_empty())
            .show(ui)
            .clicked()
        {
            self.command(Command::Stop);
        }
    }

    fn details_button(&mut self, ui: &mut egui::Ui) {
        if v2_ui::IconButton::new(v2_ui::Icon::Info, "Playback details")
            .selected(self.show_details)
            .shortcut("I")
            .show(ui)
            .clicked()
        {
            self.show_details = !self.show_details;
        }
    }

    /// The crossfade setting at a glance, and a small panel to change it.
    fn crossfade_chip(&mut self, ui: &mut egui::Ui, s: &State) {
        let label = if let Some(left) = s.fading {
            format!("Crossfading · {} s", left.ceil() as u32)
        } else if self.crossfade > 0.0 {
            format!("Crossfade {}", seconds_label(self.crossfade))
        } else {
            "Crossfade off".to_string()
        };
        let button = if self.crossfade > 0.0 {
            v2_ui::Button::secondary(label)
        } else {
            v2_ui::Button::ghost(label)
        };
        let chip = button.small().shortcut("C").show(ui);
        if self.open_crossfade {
            self.open_crossfade = false;
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&chip));
        }
        let mut value = self.crossfade;
        egui::Popup::from_toggle_button_response(&chip)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .align(egui::RectAlign::TOP_END)
            .gap(8.0)
            .width(320.0)
            .show(|ui| Self::crossfade_panel(ui, &mut value));
        if value != self.crossfade {
            self.set_crossfade(value);
        }
    }

    fn crossfade_panel(ui: &mut egui::Ui, value: &mut f32) {
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.label(v2_ui::title("Crossfade"));
        ui.label(v2_ui::caption("Blend between songs. Off by default."));
        ui.horizontal(|ui| {
            let mut v = *value;
            if v2_ui::slider(
                ui,
                &mut v,
                0.0..=MAX_CROSSFADE_SECONDS,
                0.5,
                200.0,
                "Crossfade length",
            )
            .changed()
            {
                *value = v;
            }
            ui.label(RichText::new(seconds_label(*value)).monospace());
        });
        ui.horizontal(|ui| {
            for (label, seconds) in [("Off", 0.0), ("3 s", 3.0), ("6 s", 6.0), ("12 s", 12.0)] {
                let chosen = (*value - seconds).abs() < 0.01;
                let button = if chosen {
                    v2_ui::Button::secondary(label)
                } else {
                    v2_ui::Button::ghost(label)
                };
                if button.small().show(ui).clicked() {
                    *value = seconds;
                }
            }
        });
        ui.label(v2_ui::caption(
            "Applies when a song ends. Next and choosing a song blend for up to 1 s. Repeat one, songs of unknown length and very short songs play gapless.",
        ));
    }

    /// Facts about the track and the output, from what the decoder and the device reported.
    fn details_window(&mut self, ctx: &egui::Context, s: &State) {
        let crossfade = self.crossfade;
        let next_ready = s.next_ready;
        let has_next = self.hint.is_some();
        let media_note = self.media_text();
        egui::Window::new("Playback details")
            .open(&mut self.show_details)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                let p = v2_ui::palette(ui.ctx());
                let row = |ui: &mut egui::Ui, name: &str, value: String| {
                    ui.label(RichText::new(name).color(p.muted));
                    ui.label(value);
                    ui.end_row();
                };
                egui::Grid::new("playback-details").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                    match s.info.as_ref() {
                        Some(info) => {
                            row(ui, "Format", format!("{} ({})", info.codec, if info.lossless { "lossless" } else { "lossy" }));
                            row(
                                ui,
                                "Bit depth",
                                match (info.bits, info.float) {
                                    (Some(b), true) => format!("{b}-bit float"),
                                    (Some(b), false) => format!("{b}-bit"),
                                    (None, _) => "Not stored in this format".to_string(),
                                },
                            );
                            row(ui, "Sample rate", khz(info.rate));
                            row(ui, "Channels", channels_label(info.channels));
                            row(ui, "Length", if s.duration > 0.0 { v2_ui::format_duration(s.duration) } else { "Not stated by the file".into() });
                        }
                        None => row(ui, "Track", "Nothing is playing".into()),
                    }
                    match s.output.as_ref() {
                        Some(out) => {
                            row(ui, "Output device", out.name.clone());
                            row(ui, "Output format", format!("{} · {} · {}", khz(out.rate), channels_label(out.channels as u16), out.format));
                            let mut how = Vec::new();
                            if s.resampled {
                                how.push(format!("resampled {} → {}", s.info.as_ref().map_or(String::new(), |i| khz(i.rate)), khz(out.rate)));
                            }
                            if s.channel_conversion {
                                how.push(format!("channels {} → {}", s.info.as_ref().map_or(0, |i| i.channels), out.channels));
                            }
                            row(ui, "Conversion", if how.is_empty() { "None: samples go to Windows as decoded".to_string() } else { how.join(", ") });
                        }
                        None => row(ui, "Output device", "Opens when you play".into()),
                    }
                    row(ui, "Crossfade", seconds_label(crossfade));
                    row(ui, "Windows media keys", media_note.clone());
                    row(
                        ui,
                        "Next song",
                        if !has_next {
                            "None queued".to_string()
                        } else if next_ready {
                            "Open and ready".to_string()
                        } else {
                            "Opening".to_string()
                        },
                    );
                });
                ui.add_space(6.0);
                ui.set_max_width(380.0);
                ui.label(v2_ui::caption(
                    "Windows shared audio can apply its own processing, so there is no bit-perfect guarantee. Sound cannot be better than the file and the device.",
                ));
            });
    }

    /// Right-to-left context: the slider is added first so it sits at the trailing edge.
    fn volume_controls(&mut self, ui: &mut egui::Ui, width: f32) {
        if let (_, Some(v)) = v2_ui::volume_slider(ui, self.volume, self.muted, width) {
            self.volume(v);
        }
        if v2_ui::IconButton::new(
            if self.muted {
                v2_ui::Icon::VolumeMute
            } else {
                v2_ui::Icon::VolumeHigh
            },
            if self.muted { "Unmute" } else { "Mute" },
        )
        .shortcut("M")
        .show(ui)
        .clicked()
        {
            self.muted = !self.muted;
            self.command(Command::Volume(if self.muted { 0.0 } else { self.volume }));
        }
    }

    fn has_banner(&self) -> bool {
        self.error.is_some() || self.warning.is_some() || self.info.is_some()
    }

    fn banners(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = self.error.clone() {
            let mut next = false;
            if v2_ui::Banner::error("Couldn't play this file")
                .detail(&error)
                .dismissible()
                .show(ui, |ui| {
                    if v2_ui::Button::secondary("Next track")
                        .small()
                        .show(ui)
                        .clicked()
                    {
                        next = true;
                    }
                })
            {
                self.error = None;
            }
            if next {
                self.next(false);
            }
            ui.add_space(8.0);
        }
        if let Some(text) = self.warning.clone() {
            if v2_ui::Banner::warning("Skipped a track")
                .detail(&text)
                .dismissible()
                .show(ui, |_| {})
            {
                self.warning = None;
            }
            ui.add_space(8.0);
        }
        if let Some(text) = self.info.clone() {
            if v2_ui::Banner::info("Audio output changed")
                .detail(&text)
                .dismissible()
                .show(ui, |_| {})
            {
                self.info = None;
            }
            ui.add_space(8.0);
        }
    }

    /// "Up next": header with count and row actions, search, and the list.
    fn queue_panel(&mut self, ui: &mut egui::Ui, p: &v2_ui::Palette, sheet: bool) {
        let n = self.queue.tracks.len();
        let selected = self.selected;
        let mut moves = None;
        let mut remove = false;
        ui.horizontal(|ui| {
            ui.label(v2_ui::title("Up next"));
            if n > 0 {
                ui.label(v2_ui::caption(format!(
                    "{n} track{}",
                    if n == 1 { "" } else { "s" }
                )));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let sm = v2_ui::tokens::size::CONTROL_SM;
                if v2_ui::IconButton::new(v2_ui::Icon::Close, "Remove from queue")
                    .size(sm)
                    .enabled(selected.is_some())
                    .shortcut("Delete")
                    .show(ui)
                    .clicked()
                {
                    remove = true;
                }
                if v2_ui::IconButton::new(v2_ui::Icon::ChevronDown, "Move down")
                    .size(sm)
                    .enabled(selected.is_some_and(|i| i + 1 < n))
                    .show(ui)
                    .clicked()
                {
                    moves = Some(1_i32);
                }
                if v2_ui::IconButton::new(v2_ui::Icon::ChevronUp, "Move up")
                    .size(sm)
                    .enabled(selected.is_some_and(|i| i > 0))
                    .show(ui)
                    .clicked()
                {
                    moves = Some(-1);
                }
            });
        });
        if remove {
            self.remove();
        }
        if let (Some(delta), Some(i)) = (moves, self.selected) {
            let next = i as i32 + delta;
            if next >= 0 && (next as usize) < self.queue.tracks.len() {
                self.queue.reorder(i, next as usize);
                self.selected = Some(next as usize);
            }
        }
        ui.add_space(8.0);
        v2_ui::search_field(
            ui,
            &mut self.search,
            egui::Id::new("queue-search"),
            "Find in queue",
        );
        ui.add_space(8.0);
        if self.queue.tracks.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Nothing queued yet").color(p.muted));
                ui.label(v2_ui::caption(if sheet {
                    "Tracks you add appear here."
                } else {
                    "Add music or drop files here."
                }));
            });
            return;
        }
        let search = self.search.to_lowercase();
        let visible: Vec<usize> = self
            .queue
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| search.is_empty() || t.title.to_lowercase().contains(&search))
            .map(|(i, _)| i)
            .collect();
        if visible.is_empty() {
            ui.label(v2_ui::caption("No track matches that search."));
            return;
        }
        let mut play = None;
        let mut play_next = None;
        let mut remove_at = None;
        let hint = self.hint.clone();
        let fading_from = self.fading_from;
        let can_play_next = self.queue.current.is_some();
        egui::ScrollArea::vertical()
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .auto_shrink([false, false])
            .show_rows(
                ui,
                v2_ui::tokens::size::ROW_TALL,
                visible.len(),
                |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for row in range {
                        let i = visible[row];
                        let t = &self.queue.tracks[i];
                        let subtitle = t.path.file_name().unwrap_or_default().to_string_lossy();
                        let status = if fading_from == Some(i) {
                            Some("Fading out")
                        } else if self.failed.contains(&t.path) {
                            Some("Can't play")
                        } else if hint.as_ref() == Some(&t.path) && self.queue.current != Some(i) {
                            Some("Up next")
                        } else {
                            None
                        };
                        let mut item = v2_ui::ListRow::new(&t.title)
                            .number(i + 1)
                            .subtitle(&subtitle)
                            .selected(self.selected == Some(i))
                            .active(self.queue.current == Some(i));
                        if let Some(status) = status {
                            item = item.trailing(status);
                        }
                        let r = item.show(ui);
                        if r.clicked() {
                            self.selected = Some(i);
                        }
                        if r.double_clicked() {
                            play = Some(i);
                        }
                        let r = r.on_hover_text(t.path.display().to_string());
                        r.context_menu(|ui| {
                            if v2_ui::chrome::menu_item(
                                ui,
                                Some(v2_ui::Icon::Play),
                                "Play",
                                Some("Enter"),
                                true,
                            )
                            .clicked()
                            {
                                play = Some(i);
                            }
                            if v2_ui::chrome::menu_item(
                                ui,
                                Some(v2_ui::Icon::Next),
                                "Play next",
                                None,
                                can_play_next && self.queue.current != Some(i),
                            )
                            .clicked()
                            {
                                play_next = Some(i);
                            }
                            if v2_ui::chrome::menu_item(
                                ui,
                                Some(v2_ui::Icon::Close),
                                "Remove from queue",
                                Some("Delete"),
                                true,
                            )
                            .clicked()
                            {
                                remove_at = Some(i);
                            }
                        });
                    }
                },
            );
        if let Some(i) = play_next {
            if let Some(at) = self.queue.play_next(i) {
                self.selected = Some(at);
                self.notice = format!("“{}” plays next", self.queue.tracks[at].title);
            }
        }
        if let Some(i) = remove_at {
            self.selected = Some(i);
            self.remove();
        }
        if let Some(i) = play {
            self.play(i, true);
        }
    }
}
impl eframe::App for Sonora {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.events(ctx);
        if self.shown.is_none() {
            let s = self.state();
            self.sync_media(&s);
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let ctx = egui::Context::default();
        self.save_settings(&ctx, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decoder::StreamInfo, player::DeviceInfo};
    use std::sync::Arc;

    fn state(
        info: StreamInfo,
        out_rate: u32,
        resampled: bool,
        converted: bool,
        out_channels: usize,
    ) -> State {
        State {
            info: Some(info),
            resampled,
            channel_conversion: converted,
            output: Some(Arc::new(DeviceInfo {
                name: "Speakers".into(),
                rate: out_rate,
                channels: out_channels,
                format: "32-bit float".into(),
            })),
            ..Default::default()
        }
    }
    fn info(
        codec: &'static str,
        lossless: bool,
        bits: Option<u32>,
        rate: u32,
        channels: u16,
    ) -> StreamInfo {
        StreamInfo {
            codec,
            lossless,
            bits,
            rate,
            channels,
            ..Default::default()
        }
    }

    #[test]
    fn technical_line_states_only_what_the_decoder_and_device_reported() {
        let s = state(
            info("FLAC", true, Some(24), 96_000, 2),
            96_000,
            false,
            false,
            2,
        );
        assert_eq!(tech_line(&s).unwrap(), "FLAC · 24-bit · 96 kHz · stereo");
        let s = state(
            info("FLAC", true, Some(16), 44_100, 2),
            48_000,
            true,
            false,
            2,
        );
        assert_eq!(
            tech_line(&s).unwrap(),
            "FLAC · 16-bit · 44.1 kHz · stereo → 48 kHz"
        );
        let s = state(info("MP3", false, None, 44_100, 2), 44_100, false, false, 2);
        assert_eq!(
            tech_line(&s).unwrap(),
            "MP3 · 44.1 kHz · stereo",
            "no bit depth is invented for a lossy codec"
        );
        let s = state(
            info("PCM", true, Some(32), 192_000, 8),
            48_000,
            true,
            true,
            2,
        );
        assert_eq!(
            tech_line(&s).unwrap(),
            "PCM · 32-bit · 192 kHz · 8 channels → 48 kHz stereo"
        );
        let mut float = info("PCM", true, Some(32), 48_000, 1);
        float.float = true;
        assert_eq!(
            tech_line(&state(float, 48_000, false, true, 2)).unwrap(),
            "PCM · 32-bit float · 48 kHz · mono → stereo"
        );
        assert_eq!(
            tech_line(&State::default()),
            None,
            "nothing loaded, nothing claimed"
        );
    }

    #[test]
    fn seconds_read_naturally() {
        assert_eq!(seconds_label(0.0), "Off");
        assert_eq!(seconds_label(6.0), "6 s");
        assert_eq!(seconds_label(2.5), "2.5 s");
        assert_eq!(khz(48_000), "48 kHz");
        assert_eq!(khz(44_100), "44.1 kHz");
    }
}
