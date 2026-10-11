use crate::video::Video;
use eframe::{egui, glow};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use v2_ui::{App as Product, Icon};
use vesper::{
    input::{self, Cmd, Focus},
    media::{self, DropPlan, History, Validated},
    mpv::{Api, Player},
    openq::{Checked, OpenQueue, Step, Validate},
    settings::Settings,
    subsearch::{self, SearchForm},
    subtext,
};

fn idle_window_complete(start: Instant, end: Instant) -> bool {
    end.saturating_duration_since(start) >= Duration::from_secs(5)
}

/// A subtitle file read, and converted to UTF-8 when it was not, on a worker thread.
struct PreparedSubtitle {
    /// The video request it belongs to; a result for an earlier video is dropped.
    generation: u64,
    original: PathBuf,
    /// Found beside the video by its name (selected only if nothing else is), not chosen.
    auto: bool,
    language: Option<String>,
    /// An encoding change: remove the subtitle track now showing and put this one in its place.
    replace_selected: bool,
    result: Result<subtext::Prepared, String>,
}

/// Decoder counters at one moment of a benchmark, for deltas over the steady window.
#[derive(Clone, Copy)]
struct BenchSnapshot {
    at: Instant,
    vo_drops: f64,
    decoder_drops: f64,
    delayed: f64,
    position: f64,
    cpu_seconds: f64,
}

/// Auto-ending first-file benchmark through the real UI/decoder/GL path (a native window that is requested hidden but shown by the toolkit after its first frame).
#[derive(Clone)]
struct Bench {
    hwdec: String,
    device: bool,
    play_ms: u128,
    /// Extra libmpv `key=value` options for experiments (`--mpv-opt key=value`, repeatable).
    extra: Vec<(String, String)>,
    window: [f32; 2],
}

pub fn run(file: Option<PathBuf>) -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |key: &str| {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    if args.iter().any(|a| a == "--no-prefetch") {
        // A/B measurement switch for the benchmark only.
        vesper::prefetch::disable();
    }
    let bench_file = flag("--startup-bench").map(PathBuf::from);
    let bench = bench_file.is_some().then(|| Bench {
        hwdec: flag("--hwdec").unwrap_or_else(|| "auto-copy".into()),
        device: args.iter().any(|a| a == "--bench-device"),
        play_ms: flag("--play-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000)
            .clamp(0, 30_000),
        extra: args
            .windows(2)
            .filter(|w| w[0] == "--mpv-opt")
            .filter_map(|w| w[1].split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .take(16)
            .collect(),
        window: flag("--window-size")
            .and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some([w.parse().ok()?, h.parse().ok()?])
            })
            .map(|[w, h]: [f32; 2]| [w.clamp(320.0, 3840.0), h.clamp(240.0, 2160.0)])
            .unwrap_or([1100.0, 720.0]),
    });
    let native = args
        .iter()
        .position(|a| a == "--native-smoke")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .or(bench_file);
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("vesper-native-smoke"));
    let dll = args
        .iter()
        .position(|a| a == "--dll")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(vesper::mpv::bundled_dll);
    let mut options = v2_ui::native_options(Product::Vesper);
    options.viewport = options
        .viewport
        .with_inner_size([1100.0, 720.0])
        .with_min_inner_size([680.0, 440.0]);
    if native.is_some() {
        options.viewport = options.viewport.with_visible(false);
        if let Some(bench) = &bench {
            options.viewport = options.viewport.with_inner_size(bench.window);
        }
        std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    } else if let Some(file) = &file {
        // A file named on the command line: warm the decoder and the file's index while the
        // window is still being created, instead of faulting them in one page at a time later.
        vesper::prefetch::start_library(dll.clone());
        vesper::prefetch::start_media(file.clone());
    }
    eframe::run_native(
        "Vesper",
        options,
        Box::new(move |cc| {
            vesper::timing::mark("window_ready");
            Ok(Box::new(Vesper::new(
                cc,
                native.clone().or(file),
                dll,
                native.is_some().then_some(out),
                bench,
            )))
        }),
    )
    .map_err(|e| e.to_string())
}

/// Height of the seek-and-transport block above the bottom edge. Subtitles are lifted above it.
const BAR: f32 = 112.0;

#[derive(Clone, Copy)]
enum Action {
    File,
    Folder,
    Subtitle,
    Search,
    Help,
    Queue,
}

/// libmpv options for the first open, from what the user set before any video existed. Mute is
/// its own option so the chosen volume survives it: a muted start keeps the level, and unmuting
/// (which only sets `mute=no`) brings the sound back.
fn decoder_options(
    volume: f64,
    muted: bool,
    speed: f64,
    hwdec: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("hwdec", hwdec.into()),
        ("keep-open", "yes".into()),
        ("pause", "yes".into()),
        ("volume", volume.to_string()),
        ("mute", if muted { "yes" } else { "no" }.into()),
        ("speed", speed.to_string()),
    ]
}

struct Vesper {
    player: Option<Arc<Player>>,
    video: Option<Arc<Mutex<Video>>>,
    decoder_dll: PathBuf,
    decoder_ctx: egui::Context,
    decoder_proc: Option<crate::video::Proc>,
    error: Option<String>,
    queue: Vec<PathBuf>,
    index: usize,
    current: Option<PathBuf>,
    loading: bool,
    position: f64,
    duration: f64,
    volume: f64,
    speed: f64,
    paused: bool,
    muted: bool,
    fullscreen: bool,
    show_queue: bool,
    help: bool,
    history: History,
    history_path: PathBuf,
    last_save: Instant,
    last_input: Instant,
    last_pointer: Option<egui::Pos2>,
    folder_rx: Option<mpsc::Receiver<Result<Vec<PathBuf>, String>>>,
    native_out: Option<PathBuf>,
    native_started: Instant,
    captured: bool,
    native_reported: bool,
    native_open_pending: Option<PathBuf>,
    native_painter: Option<egui_glow::Painter>,
    native_idle: Option<(usize, usize, Instant)>,
    native_pause_requested: bool,
    native_pause_ack_at: Option<Instant>,
    native_composite: bool,
    pending_subtitles: std::collections::VecDeque<PathBuf>,
    subtitle_position: Option<f64>,
    bench: Option<Bench>,
    bench_first_render: Option<Instant>,
    bench_usage_first: Option<vesper::timing::Usage>,
    bench_frames_at_load: Option<usize>,
    bench_hash_first: Option<u64>,
    bench_steady: Option<BenchSnapshot>,
    opens: OpenQueue<PathBuf, Validated>,
    init_rx: Option<mpsc::Receiver<Result<Arc<Player>, String>>>,
    check_tx: mpsc::Sender<Result<Validated, String>>,
    check_rx: mpsc::Receiver<Result<Validated, String>>,
    /// File name shown while the file is being checked or the decoder is starting.
    opening_label: Option<String>,
    loading_since: Option<Instant>,
    settings: Settings,
    settings_path: PathBuf,
    search: Option<SearchForm>,
    search_fresh: bool,
    info: bool,
    info_observed: bool,
    sub_delay: f64,
    sub_scale: f64,
    sub_visible: bool,
    toast: Option<(String, Instant)>,
    drop_tx: mpsc::Sender<DropPlan>,
    drop_rx: mpsc::Receiver<DropPlan>,
    /// Subtitles dropped together with a video: attached when that video starts loading.
    attach_subtitles: Vec<PathBuf>,
    /// Native capture harness only: names a loading phase that cannot be reached on demand.
    capture_label: Option<&'static str>,
    /// Native capture harness only: opens the Subtitles menu with its encoding list expanded.
    capture_tracks: bool,
    browser_tx: mpsc::Sender<(String, Result<(), String>)>,
    browser_rx: mpsc::Receiver<(String, Result<(), String>)>,
    subtitle_cache: PathBuf,
    subtitle_tx: mpsc::Sender<PreparedSubtitle>,
    subtitle_rx: mpsc::Receiver<PreparedSubtitle>,
    /// Counts video requests, so subtitle work for a video that was replaced is ignored.
    file_generation: u64,
    /// The owner's choice of text encoding, or `None` for Automatic.
    subtitle_encoding: Option<u32>,
    last_subtitle: Option<PathBuf>,
}
impl Vesper {
    fn new(
        cc: &eframe::CreationContext<'_>,
        file: Option<PathBuf>,
        dll: PathBuf,
        native_out: Option<PathBuf>,
        bench: Option<Bench>,
    ) -> Self {
        v2_ui::install(&cc.egui_ctx, Product::Vesper);
        let history_path = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Version2/Vesper/resume.json");
        let native_open_pending = native_out.as_ref().and(file.clone());
        let (check_tx, check_rx) = mpsc::channel();
        let (drop_tx, drop_rx) = mpsc::channel();
        let (browser_tx, browser_rx) = mpsc::channel();
        let (subtitle_tx, subtitle_rx) = mpsc::channel();
        let subtitle_cache = history_path.with_file_name("subtitles");
        let settings_path = history_path.with_file_name("settings.json");
        let settings = Settings::load(&settings_path);
        let mut s = Self {
            player: None,
            video: None,
            decoder_dll: dll,
            decoder_ctx: cc.egui_ctx.clone(),
            decoder_proc: cc.get_proc_address.clone(),
            error: None,
            queue: Vec::new(),
            index: 0,
            current: None,
            loading: false,
            position: 0.0,
            duration: 0.0,
            volume: 80.0,
            speed: 1.0,
            paused: true,
            muted: false,
            fullscreen: false,
            show_queue: false,
            help: false,
            history: History::load(&history_path),
            history_path,
            last_save: Instant::now(),
            last_input: Instant::now(),
            last_pointer: None,
            folder_rx: None,
            native_out,
            native_started: Instant::now(),
            captured: false,
            native_reported: false,
            native_open_pending,
            native_painter: None,
            native_idle: None,
            native_pause_requested: false,
            native_pause_ack_at: None,
            native_composite: false,
            pending_subtitles: std::collections::VecDeque::new(),
            subtitle_position: None,
            bench,
            bench_first_render: None,
            bench_usage_first: None,
            bench_frames_at_load: None,
            bench_hash_first: None,
            bench_steady: None,
            opens: OpenQueue::default(),
            init_rx: None,
            check_tx,
            check_rx,
            opening_label: None,
            loading_since: None,
            settings,
            settings_path,
            search: None,
            search_fresh: false,
            info: false,
            info_observed: false,
            sub_delay: 0.0,
            sub_scale: 1.0,
            sub_visible: true,
            toast: None,
            drop_tx,
            drop_rx,
            attach_subtitles: Vec::new(),
            capture_label: None,
            capture_tracks: false,
            browser_tx,
            browser_rx,
            subtitle_cache,
            subtitle_tx,
            subtitle_rx,
            file_generation: 0,
            subtitle_encoding: None,
            last_subtitle: None,
        };
        if s.native_out.is_some()
            && let Some(gl) = cc.gl.clone()
        {
            match egui_glow::Painter::new(gl, "", None, false) {
                Ok(p) => s.native_painter = Some(p),
                Err(e) => s.error = Some(e.to_string()),
            }
        }
        if s.native_out.is_none()
            && let Some(f) = file
        {
            s.queue = vec![f.clone()];
            s.open(&f);
        }
        // Decoder/context initialization is excluded from the fixture playback deadline.
        s.native_started = Instant::now();
        s
    }
    /// What the decoder worker needs, captured by value so the UI thread is never involved.
    fn decoder_plan(&self) -> (PathBuf, Vec<(String, String)>, bool) {
        let hwdec = self
            .bench
            .as_ref()
            .map_or("auto-copy", |b| b.hwdec.as_str());
        let mut opts: Vec<(String, String)> =
            decoder_options(self.volume, self.muted, self.speed, hwdec)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
        // Subtitles beside the video are found by Vesper (encoding-aware), not by the decoder.
        opts.push(("sub-auto".to_string(), "no".to_string()));
        opts.extend(self.language_options());
        if let Some(bench) = &self.bench {
            // Silent but real: the audio device is still opened when requested.
            opts.push((
                "ao".into(),
                if bench.device { "wasapi" } else { "null" }.into(),
            ));
            opts.push(("volume".into(), "0".into()));
            opts.extend(bench.extra.iter().cloned());
        } else if let Some(dir) = &self.native_out {
            let wav = dir.join("native-audio.wav").to_string_lossy().into_owned();
            opts.extend([
                ("ao".to_string(), "pcm".to_string()),
                ("ao-pcm-file".to_string(), wav),
                ("audio-format".to_string(), "s16".to_string()),
            ]);
        }
        (self.decoder_dll.clone(), opts, self.bench.is_some())
    }
    /// libmpv options for the preferred subtitle language: which embedded track to pick. Text
    /// encoding is not the decoder's job here: this build cannot convert code pages, so `subtext`
    /// reads every subtitle file first.
    fn language_options(&self) -> Vec<(String, String)> {
        self.settings
            .subtitle_language
            .as_deref()
            .and_then(subsearch::language)
            .map(|language| vec![("slang".to_string(), language.tags.to_string())])
            .unwrap_or_default()
    }
    /// A short message over the picture, gone after a moment.
    fn say(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }
    /// Load the decoder library and create the player on a worker thread. An empty window needs no
    /// decoder; this runs once, after a local file has been validated. The UI thread keeps painting
    /// and answering input meanwhile, and only creates the GL render context when the worker is done.
    fn start_decoder(&mut self) {
        if self.init_rx.is_some() || self.player.is_some() {
            return;
        }
        let (dll, opts, observe_extra) = self.decoder_plan();
        let (tx, rx) = mpsc::sync_channel(1);
        let ctx = self.decoder_ctx.clone();
        self.init_rx = Some(rx);
        let spawned = std::thread::Builder::new()
            .name("vesper-decoder-init".into())
            .spawn(move || {
                let result = (|| {
                    vesper::timing::mark("decoder_init_start");
                    let api = Api::load(&dll)?;
                    vesper::timing::mark("decoder_dll_loaded");
                    let refs: Vec<(&str, &str)> =
                        opts.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    let player = Player::new(api, &refs)?;
                    if observe_extra {
                        player.observe_numbers(&[
                            "frame-drop-count",
                            "decoder-frame-drop-count",
                            "vo-delayed-frame-count",
                            "estimated-vf-fps",
                        ])?;
                    }
                    vesper::timing::mark("decoder_player_ready");
                    Ok::<_, String>(player)
                })();
                let _ = tx.send(result);
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.init_rx = None;
            self.finish_decoder(Err("The video engine could not start a worker.".into()));
        }
    }
    /// UI thread only: take the worker's result, create the render context, start the one
    /// surviving open request.
    fn poll_decoder(&mut self) {
        let Some(rx) = &self.init_rx else { return };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("The video engine stopped while starting. Open the file again.".into())
            }
        };
        self.init_rx = None;
        self.finish_decoder(result);
    }
    fn finish_decoder(&mut self, result: Result<Arc<Player>, String>) {
        let created = result.and_then(|player| {
            let video = Video::new(
                self.decoder_ctx.clone(),
                self.decoder_proc.clone(),
                player.clone(),
            )?;
            Ok((player, video))
        });
        match created {
            Ok((player, video)) => {
                vesper::timing::mark("decoder_render_ready");
                self.player = Some(player);
                self.video = Some(video);
                // Volume, mute and speed may have changed while the decoder was starting.
                if self.bench.is_none()
                    && let Some(p) = self.player.clone()
                {
                    let synced = p
                        .set_number("volume", self.volume)
                        .and_then(|_| p.set_string("mute", if self.muted { "yes" } else { "no" }))
                        .and_then(|_| p.set_number("speed", self.speed));
                    self.apply(synced);
                }
                if self.native_out.is_some() {
                    // Decoder start-up is excluded from the fixture playback deadline.
                    self.native_started = Instant::now();
                }
                if let Some(file) = self.opens.decoder_finished(true)
                    && let Err(e) = self.start_playback(&file)
                {
                    self.abandon_open();
                    self.error = Some(e);
                }
            }
            Err(e) => {
                self.opens.decoder_finished(false);
                self.abandon_open();
                self.error = Some(e);
            }
        }
    }
    /// Return to the start screen after an open that cannot continue.
    fn abandon_open(&mut self) {
        self.file_generation += 1;
        self.loading = false;
        self.loading_since = None;
        self.opening_label = None;
        self.current = None;
        self.paused = true;
        self.position = 0.0;
        self.duration = 0.0;
        self.pending_subtitles.clear();
    }
    /// The user gave up on an open that has not finished. Whatever has not reached the decoder is
    /// dropped; a file already sent to it is stopped. Never needed once the file has loaded.
    fn cancel_open(&mut self) {
        let checking = self.opens.checking();
        if !self.loading && !checking {
            return;
        }
        self.opens.cancel();
        if self.loading {
            if self.player.is_some() {
                self.command(&["stop"]);
            }
            self.abandon_open();
            self.error = None;
        } else {
            // Only a file check was running: whatever is already playing keeps playing.
            self.opening_label = None;
            self.loading_since = None;
        }
    }
    fn apply(&mut self, result: Result<(), String>) {
        if let Err(e) = result {
            self.error = Some(e);
        }
    }
    fn command(&mut self, args: &[&str]) {
        if let Some(p) = self.player.clone() {
            self.apply(p.command_async(args));
        }
    }
    fn save_resume(&mut self) {
        if let Some(path) = &self.current {
            self.history.remember(path, self.position, self.duration);
            if let Err(e) = self.history.save(&self.history_path) {
                self.error = Some(format!(
                    "Playback works, but resume could not be saved: {e}"
                ));
            }
        }
        self.last_save = Instant::now();
    }
    fn open(&mut self, file: &Path) {
        self.save_resume();
        if matches!(
            file.extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str(),
            "srt" | "ass" | "ssa" | "vtt"
        ) {
            self.subtitle(file);
            return;
        }
        vesper::timing::mark("open_requested");
        self.last_input = Instant::now();
        self.opening_label = file.file_name().map(|n| n.to_string_lossy().into_owned());
        self.loading_since = Some(Instant::now());
        // Start the decoder now, beside the file check rather than after it. Measured: waiting for
        // the check to be seen by a frame and only then starting the decoder cost the open two extra
        // frames. Both finish before the next frame, which then sends the load. Loading a file still
        // goes only through the validated request (`open_validated`).
        if self.player.is_none() {
            vesper::prefetch::start_library(self.decoder_dll.clone());
            if self.opens.begin_decoder() {
                self.start_decoder();
            }
        }
        // The disk may be slow or busy: the checks run on a worker and the window keeps painting.
        match self.opens.request_check(file.to_path_buf()) {
            Validate::Start(path) => self.spawn_check(path),
            Validate::Queued => {}
        }
    }
    fn spawn_check(&mut self, path: PathBuf) {
        let tx = self.check_tx.clone();
        let ctx = self.decoder_ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("vesper-file-check".into())
            .spawn({
                let tx = tx.clone();
                let ctx = ctx.clone();
                move || {
                    let _ = tx.send(media::validate(&path));
                    ctx.request_repaint();
                }
            });
        if spawned.is_err() {
            let _ = tx.send(Err("The file check could not start a worker.".into()));
            ctx.request_repaint();
        }
    }
    /// UI thread: take finished file checks. Only the newest request's result is used.
    fn poll_checks(&mut self) {
        while let Ok(result) = self.check_rx.try_recv() {
            match self.opens.check_finished() {
                Checked::Discard => {}
                Checked::Restart(next) => self.spawn_check(next),
                Checked::Deliver => self.checked(result),
            }
        }
    }
    fn checked(&mut self, result: Result<Validated, String>) {
        let file = match result {
            Ok(file) => file,
            Err(e) => {
                // A rejected replacement leaves a file already in the decoder untouched. With no
                // decoder yet, the superseded waiting file was dropped, so there is nothing to show.
                if self.player.is_none() {
                    self.abandon_open();
                }
                self.opening_label = None;
                self.loading_since = None;
                self.error = Some(e);
                return;
            }
        };
        vesper::timing::mark("file_validated");
        // The choice is made: read the file's index and the decoder ahead of need (both bounded,
        // both local, both idempotent).
        vesper::prefetch::start_media(file.path().to_path_buf());
        vesper::prefetch::start_library(self.decoder_dll.clone());
        match self.opens.request(file.clone()) {
            Step::LoadNow(file) => match self.start_playback(&file) {
                Ok(()) => self.begin_loading(file.into_path()),
                Err(e) => {
                    self.opening_label = None;
                    self.loading_since = None;
                    self.error = Some(e);
                }
            },
            Step::WaitForDecoder { start_decoder } => {
                self.begin_loading(file.into_path());
                if start_decoder {
                    self.start_decoder();
                }
            }
        }
    }
    fn begin_loading(&mut self, path: PathBuf) {
        self.file_generation += 1;
        self.last_subtitle = None;
        self.current = Some(path);
        self.opening_label = None;
        self.pending_subtitles = std::mem::take(&mut self.attach_subtitles).into();
        self.loading = true;
        self.error = None;
        self.position = 0.0;
        self.duration = 0.0;
        self.paused = false;
    }
    /// Send the load to the decoder: the one place a request enters it. No filesystem access.
    fn start_playback(&mut self, file: &Validated) -> Result<(), String> {
        let resume = file
            .identity()
            .and_then(|id| self.history.position_for(file.path(), id))
            .unwrap_or(0.0);
        let player = self
            .player
            .clone()
            .ok_or("The video engine is not running.")?;
        player.open_validated(file, resume)?;
        vesper::timing::mark("load_sent");
        self.command(&["set", "pause", "no"]);
        if resume > 0.0 {
            self.say(format!(
                "Resumed at {}. Press Home to start over.",
                v2_ui::format_duration(resume)
            ));
        }
        Ok(())
    }
    fn choose_file(&mut self) {
        // The picker stays open for seconds; use them to read the decoder from disk.
        vesper::prefetch::start_library(self.decoder_dll.clone());
        if let Some(f) = rfd::FileDialog::new()
            .set_title("Open a local video")
            .pick_file()
        {
            self.queue = vec![f.clone()];
            self.index = 0;
            self.open(&f);
        }
    }
    fn choose_folder(&mut self, ctx: &egui::Context) {
        vesper::prefetch::start_library(self.decoder_dll.clone());
        if self.folder_rx.is_none()
            && let Some(f) = rfd::FileDialog::new()
                .set_title("Open one local folder")
                .pick_folder()
        {
            self.scan(f, ctx);
        }
    }
    fn scan(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.folder_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.folder_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = media::folder(&path);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn subtitle(&mut self, path: &Path) {
        if self.current.is_none() {
            self.error = Some("Open a video before loading subtitles.".into());
            return;
        }
        if self.loading {
            if self.pending_subtitles.len() < 16 {
                self.pending_subtitles.push_back(path.to_path_buf());
            }
            return;
        }
        self.spawn_subtitle(path.to_path_buf(), false, None, false);
    }
    /// Read a subtitle file on a worker: check it, decide how it is encoded and convert it to UTF-8
    /// when it is not. The result is used only if the same video is still the current one.
    fn spawn_subtitle(
        &mut self,
        path: PathBuf,
        auto: bool,
        language: Option<String>,
        replace_selected: bool,
    ) {
        let generation = self.file_generation;
        let preference = self.settings.subtitle_language.clone();
        let forced = self.subtitle_encoding;
        let cache = self.subtitle_cache.clone();
        let tx = self.subtitle_tx.clone();
        let ctx = self.decoder_ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("vesper-subtitle".into())
            .spawn(move || {
                let result = subtext::prepare(&path, preference.as_deref(), forced, &cache);
                let _ = tx.send(PreparedSubtitle {
                    generation,
                    original: path,
                    auto,
                    language,
                    replace_selected,
                    result,
                });
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.error = Some("The subtitle file could not be read.".into());
        }
    }
    /// Subtitles saved beside the video under its name (`movie.srt`, `movie.hr.srt`) are read the
    /// same way as chosen ones, so a legacy-encoded file shows correctly here too.
    fn autoload_siblings(&mut self) {
        let Some(video) = self.current.clone() else {
            return;
        };
        let generation = self.file_generation;
        let preference = self.settings.subtitle_language.clone();
        let forced = self.subtitle_encoding;
        let cache = self.subtitle_cache.clone();
        let tx = self.subtitle_tx.clone();
        let ctx = self.decoder_ctx.clone();
        let _ = std::thread::Builder::new()
            .name("vesper-subtitle-siblings".into())
            .spawn(move || {
                for sibling in subtext::siblings(&video) {
                    let result =
                        subtext::prepare(&sibling.path, preference.as_deref(), forced, &cache);
                    let _ = tx.send(PreparedSubtitle {
                        generation,
                        original: sibling.path,
                        auto: true,
                        language: sibling.language,
                        replace_selected: false,
                        result,
                    });
                    ctx.request_repaint();
                }
            });
    }
    fn poll_subtitles(&mut self) {
        while let Ok(m) = self.subtitle_rx.try_recv() {
            if m.generation != self.file_generation {
                continue;
            }
            let prepared = match m.result {
                Ok(prepared) => prepared,
                Err(e) => {
                    // A file found beside the video that cannot be read is skipped quietly; one
                    // the owner chose reports why.
                    if !m.auto {
                        self.error = Some(e);
                    }
                    continue;
                }
            };
            let shown = m
                .original
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if m.replace_selected
                && let Some(id) = self.player.as_ref().and_then(|p| p.number("sid"))
            {
                self.command(&["sub-remove", &format!("{id:.0}")]);
            }
            let path = prepared.path.to_string_lossy().into_owned();
            if m.auto {
                let mut args = vec!["sub-add", path.as_str(), "auto", shown.as_str()];
                if let Some(language) = m.language.as_deref() {
                    args.push(language);
                }
                self.command(&args);
            } else {
                self.command(&["sub-add", &path, "select"]);
            }
            self.last_subtitle = Some(m.original);
            let how = prepared
                .read_as
                .map(|label| format!(" (read as {label})"))
                .unwrap_or_default();
            self.say(if m.auto {
                format!("Subtitles found next to the video: {shown}{how}")
            } else {
                format!("Subtitles loaded: {shown}{how}")
            });
        }
    }
    /// Read the last subtitle again after a different text encoding was chosen.
    fn reread_subtitle(&mut self) {
        if let Some(path) = self.last_subtitle.clone() {
            self.spawn_subtitle(path, false, None, true);
        }
    }
    /// Ask for a subtitle file. After a browser search the picker starts in Downloads, where the
    /// file most likely is; the owner still chooses the file themselves.
    fn pick_subtitle(&mut self, start: Option<PathBuf>) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Choose a subtitle file")
            .add_filter("Subtitles", &["srt", "ass", "ssa", "vtt"]);
        if let Some(dir) = start.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(f) = dialog.pick_file() {
            self.subtitle(&f);
        }
    }
    fn next(&mut self, offset: isize) {
        let next = self.index as isize + offset;
        if next >= 0 && (next as usize) < self.queue.len() {
            self.index = next as usize;
            let p = self.queue[self.index].clone();
            self.open(&p);
        }
    }
    fn toggle(&mut self) {
        if self.current.is_some() && !self.loading {
            if self.paused
                && self
                    .player
                    .as_ref()
                    .and_then(|p| p.string("eof-reached"))
                    .as_deref()
                    == Some("yes")
            {
                self.seek(0.0);
            }
            self.paused = !self.paused;
            self.command(&["set", "pause", if self.paused { "yes" } else { "no" }]);
        }
    }
    fn apply_drop(&mut self, plan: DropPlan, ctx: &egui::Context) {
        if let Some(e) = plan.errors.into_iter().next() {
            self.error = Some(e);
        }
        if !plan.videos.is_empty() {
            self.queue = plan.videos;
            self.index = 0;
            self.show_queue = self.queue.len() > 1;
            // Subtitles dropped with the video belong to it.
            self.attach_subtitles = plan.subtitles;
            let file = self.queue[0].clone();
            self.open(&file);
        } else if let Some(path) = plan.folder {
            self.scan(path, ctx);
            self.attach_subtitles = plan.subtitles;
        } else {
            for subtitle in plan.subtitles {
                self.subtitle(&subtitle);
            }
        }
    }
    fn seek(&mut self, value: f64) {
        let value = media::seek(value, 0.0, self.duration);
        self.command(&["seek", &format!("{value:.3}"), "absolute+exact"]);
        self.position = value;
    }
    fn fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        self.last_input = Instant::now();
    }
    /// The start screen's header: the themed window chrome shown before anything plays.
    fn header(&mut self, ui: &mut egui::Ui) {
        let reading = self.folder_rx.is_some();
        let mut action = None;
        v2_ui::chrome::header(
            ui,
            |ui| {
                v2_ui::chrome::wordmark(ui, Product::Vesper);
                if reading {
                    ui.add_space(8.0);
                    v2_ui::loading(ui, "Reading the folder…");
                }
            },
            |ui| {
                v2_ui::chrome::more_menu(ui, |ui| action = self.menu(ui, false));
                if v2_ui::Button::primary("Open")
                    .icon(Icon::Open)
                    .pill()
                    .shortcut("Ctrl+O")
                    .show(ui)
                    .clicked()
                {
                    action = Some(Action::File);
                }
            },
        );
        self.act(action, ui.ctx());
    }
    /// The one menu. `watching` adds the actions that need an open video.
    fn menu(&self, ui: &mut egui::Ui, watching: bool) -> Option<Action> {
        use v2_ui::chrome::{menu_item, menu_separator, theme_menu};
        let mut action = None;
        if menu_item(ui, Some(Icon::Open), "Open a video…", Some("Ctrl+O"), true).clicked() {
            action = Some(Action::File);
        }
        if menu_item(
            ui,
            Some(Icon::Folder),
            "Open a folder…",
            None,
            self.folder_rx.is_none(),
        )
        .clicked()
        {
            action = Some(Action::Folder);
        }
        if watching
            && menu_item(
                ui,
                Some(Icon::Subtitles),
                "Load a subtitle file…",
                None,
                true,
            )
            .clicked()
        {
            action = Some(Action::Subtitle);
        }
        if menu_item(
            ui,
            Some(Icon::Search),
            "Find subtitles online…",
            Some("Ctrl+F"),
            true,
        )
        .clicked()
        {
            action = Some(Action::Search);
        }
        menu_separator(ui);
        theme_menu(ui);
        menu_separator(ui);
        if menu_item(
            ui,
            Some(Icon::Keyboard),
            "Keyboard shortcuts",
            Some("F1"),
            true,
        )
        .clicked()
        {
            action = Some(Action::Help);
        }
        action
    }
    fn act(&mut self, action: Option<Action>, ctx: &egui::Context) {
        match action {
            Some(Action::File) => self.choose_file(),
            Some(Action::Folder) => self.choose_folder(ctx),
            Some(Action::Subtitle) => self.pick_subtitle(None),
            Some(Action::Search) => self.open_search(),
            Some(Action::Help) => self.help = true,
            Some(Action::Queue) => self.show_queue = !self.show_queue,
            None => {}
        }
    }
    /// Before anything plays: one card that says what Vesper does and how to start.
    fn start(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let mut area = rect;
        if let Some(e) = self.error.clone() {
            let width = (rect.width() - 48.0).min(640.0);
            let banner = egui::Rect::from_min_size(
                egui::pos2(rect.center().x - width / 2.0, rect.top() + 20.0),
                egui::vec2(width, 72.0),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(banner), |ui| {
                if v2_ui::Banner::error("That didn't open")
                    .detail(&e)
                    .dismissible()
                    .show(ui, |_| {})
                {
                    self.error = None;
                }
            });
            area = rect.with_min_y(banner.bottom());
        }
        let mut action = None;
        v2_ui::chrome::Welcome::new(Icon::Film, "Watch something")
            .body("Open a video, or a folder of episodes to play them in order. You can also drop files here.")
            .foot("MP4 · MKV · WebM · MOV · AVI · WMV · TS · MPG · FLV · OGV · 3GP, with SRT, ASS or VTT subtitles")
            .show(ui, area, |ui| {
                if v2_ui::Button::primary("Open a video").icon(Icon::Open).pill().show(ui).clicked() {
                    action = Some(Action::File);
                }
                if v2_ui::Button::secondary("Open a folder")
                    .icon(Icon::Folder)
                    .pill()
                    .enabled(self.folder_rx.is_none())
                    .show(ui)
                    .clicked()
                {
                    action = Some(Action::Folder);
                }
            });
        self.act(action, ui.ctx());
    }
    /// Title and window actions over the top of the picture, on a fading scrim.
    fn top_bar(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let scrim = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 104.0));
        v2_ui::chrome::paint_scrim(ui.painter(), scrim, true, 170);
        let p = v2_ui::palette_of(ui);
        let name = self
            .current
            .as_ref()
            .and_then(|c| c.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| self.opening_label.clone())
            .unwrap_or_default();
        let position =
            (self.queue.len() > 1).then(|| format!("{} of {}", self.index + 1, self.queue.len()));
        let several = self.queue.len() > 1;
        let queue_open = self.show_queue;
        let mut action = None;
        let row = egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width(), v2_ui::tokens::size::HEADER),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(row), |ui| {
            v2_ui::chrome::header(
                ui,
                |ui| {
                    v2_ui::chrome::app_mark(ui, Product::Vesper, 22.0);
                    ui.add_space(4.0);
                    let room = (ui.available_width() - 260.0).clamp(80.0, 640.0);
                    ui.scope(|ui| {
                        ui.set_max_width(room);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&name)
                                    .font(egui::FontId::new(15.0, v2_ui::semibold()))
                                    .color(p.text),
                            )
                            .truncate(),
                        );
                    });
                    if let Some(position) = &position {
                        ui.label(v2_ui::caption(position));
                    }
                },
                |ui| {
                    v2_ui::chrome::more_menu(ui, |ui| action = self.menu(ui, true));
                    if several
                        && v2_ui::IconButton::new(Icon::Queue, "Up next")
                            .selected(queue_open)
                            .show(ui)
                            .clicked()
                    {
                        action = Some(Action::Queue);
                    }
                    if v2_ui::Button::primary("Open")
                        .icon(Icon::Open)
                        .pill()
                        .shortcut("Ctrl+O")
                        .show(ui)
                        .clicked()
                    {
                        action = Some(Action::File);
                    }
                },
            );
        });
        self.act(action, ui.ctx());
    }
    /// Seek and transport over the bottom of the picture, on a fading scrim.
    fn transport(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let scrim =
            egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 190.0), rect.max);
        v2_ui::chrome::paint_scrim(ui.painter(), scrim, false, 215);
        let p = v2_ui::palette_of(ui);
        let inset = (rect.width() * 0.03).clamp(16.0, 32.0);
        let block = egui::Rect::from_min_max(
            egui::pos2(rect.left() + inset, rect.bottom() - BAR),
            egui::pos2(rect.right() - inset, rect.bottom() - 12.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(block), |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let seek = ui
                .allocate_ui(egui::vec2(block.width(), 20.0), |ui| {
                    v2_ui::Seekbar::new(self.position, Some(self.duration))
                        .overlay()
                        .show(ui)
                })
                .inner;
            if let Some(seconds) = seek.seek_to {
                self.seek(seconds);
            }
            ui.add_space(10.0);
            ui.allocate_ui_with_layout(
                egui::vec2(block.width(), v2_ui::tokens::size::PLAY),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let lg = v2_ui::tokens::size::CONTROL_LG;
                    if v2_ui::IconButton::new(
                        if self.paused { Icon::Play } else { Icon::Pause },
                        if self.paused { "Play" } else { "Pause" },
                    )
                    .prominent()
                    .enabled(self.current.is_some() && !self.loading)
                    .shortcut("Space")
                    .show(ui)
                    .clicked()
                    {
                        self.toggle();
                    }
                    ui.add_space(4.0);
                    if v2_ui::IconButton::new(Icon::Rewind, "Back 10 seconds")
                        .size(lg)
                        .shortcut("J")
                        .show(ui)
                        .clicked()
                    {
                        self.seek(self.position - 10.0);
                    }
                    if v2_ui::IconButton::new(Icon::FastForward, "Forward 10 seconds")
                        .size(lg)
                        .shortcut("L")
                        .show(ui)
                        .clicked()
                    {
                        self.seek(self.position + 10.0);
                    }
                    if self.queue.len() > 1 {
                        if v2_ui::IconButton::new(Icon::Prev, "Previous video")
                            .size(lg)
                            .enabled(self.index > 0)
                            .show(ui)
                            .clicked()
                        {
                            self.next(-1);
                        }
                        if v2_ui::IconButton::new(Icon::Next, "Next video")
                            .size(lg)
                            .enabled(self.index + 1 < self.queue.len())
                            .show(ui)
                            .clicked()
                        {
                            self.next(1);
                        }
                    }
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  /  {}",
                            v2_ui::format_duration(self.position),
                            v2_ui::format_duration(self.duration)
                        ))
                        .color(p.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if v2_ui::IconButton::new(
                            if self.fullscreen {
                                Icon::ExitFullscreen
                            } else {
                                Icon::Fullscreen
                            },
                            if self.fullscreen {
                                "Leave full screen"
                            } else {
                                "Full screen"
                            },
                        )
                        .shortcut("F")
                        .show(ui)
                        .clicked()
                        {
                            self.fullscreen(ui.ctx());
                        }
                        let tracks = v2_ui::IconButton::new(Icon::Subtitles, "Subtitles and audio")
                            .shortcut("S")
                            .show(ui);
                        let popup = egui::Popup::menu(&tracks)
                            .align(egui::RectAlign::TOP_END)
                            .gap(6.0);
                        let popup = if self.capture_tracks {
                            popup.open(true)
                        } else {
                            popup
                        };
                        popup.show(|ui| {
                            ui.set_min_width(260.0);
                            ui.spacing_mut().item_spacing.y = 0.0;
                            // Many tracks or the expanded encoding list scroll instead of running
                            // off the window.
                            vesper::menu::scrolling(ui, |ui| self.tracks(ui));
                        });
                        let speed = v2_ui::Button::ghost(format!("{}×", self.speed))
                            .small()
                            .show(ui)
                            .on_hover_text("Playback speed ([ and ])");
                        egui::Popup::menu(&speed)
                            .align(egui::RectAlign::TOP_END)
                            .gap(6.0)
                            .show(|ui| {
                                ui.set_min_width(160.0);
                                ui.spacing_mut().item_spacing.y = 0.0;
                                v2_ui::chrome::menu_heading(ui, "Speed");
                                for speed in [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0] {
                                    if v2_ui::chrome::menu_check(
                                        ui,
                                        self.speed == speed,
                                        &format!("{speed}×"),
                                    )
                                    .clicked()
                                    {
                                        self.speed = speed;
                                        if let Some(p) = self.player.clone() {
                                            self.apply(p.set_number("speed", speed));
                                        }
                                    }
                                }
                            });
                        let width = if block.width() > 760.0 { 104.0 } else { 72.0 };
                        let (_, change) =
                            v2_ui::volume_slider(ui, self.volume as f32 / 100.0, self.muted, width);
                        if let Some(v) = change {
                            self.volume = v as f64 * 100.0;
                            if let Some(p) = self.player.clone() {
                                self.apply(p.set_number("volume", self.volume));
                            }
                        }
                        if v2_ui::IconButton::new(
                            if self.muted {
                                Icon::VolumeMute
                            } else {
                                Icon::VolumeHigh
                            },
                            if self.muted { "Unmute" } else { "Mute" },
                        )
                        .shortcut("M")
                        .show(ui)
                        .clicked()
                        {
                            self.muted = !self.muted;
                            self.command(&["set", "mute", if self.muted { "yes" } else { "no" }]);
                        }
                    });
                },
            );
        });
    }
    /// Subtitle and audio tracks. Read from the decoder only while this menu is open.
    fn tracks(&mut self, ui: &mut egui::Ui) {
        use v2_ui::chrome::{menu_check, menu_heading, menu_item, menu_separator};
        let mut tracks = Vec::new();
        if let Some(p) = self.player.clone() {
            let count = p
                .number("track-list/count")
                .unwrap_or(0.0)
                .clamp(0.0, 256.0) as usize;
            for i in 0..count {
                let key = format!("track-list/{i}/");
                let kind = p.string(&(key.clone() + "type")).unwrap_or_default();
                if kind != "audio" && kind != "sub" {
                    continue;
                }
                let id = p.number(&(key.clone() + "id")).unwrap_or(0.0) as i64;
                let lang = p.string(&(key.clone() + "lang")).filter(|l| l != "und");
                let title = p.string(&(key.clone() + "title")).filter(|t| !t.is_empty());
                let selected = p.string(&(key + "selected")).as_deref() == Some("yes");
                let label = match (lang, title) {
                    (Some(l), Some(t)) => format!("{l} · {t}"),
                    (Some(l), None) => l,
                    (None, Some(t)) => t,
                    (None, None) => format!("Track {id}"),
                };
                tracks.push((kind, id, label, selected));
            }
        }
        menu_heading(ui, "Subtitles");
        if menu_item(ui, Some(Icon::Open), "Load a subtitle file…", None, true).clicked() {
            self.act(Some(Action::Subtitle), ui.ctx());
        }
        if menu_item(
            ui,
            Some(Icon::Search),
            "Find subtitles online…",
            Some("Ctrl+F"),
            true,
        )
        .clicked()
        {
            self.act(Some(Action::Search), ui.ctx());
        }
        let subs_on = tracks.iter().any(|t| t.0 == "sub" && t.3);
        if menu_check(ui, !subs_on, "Off").clicked() {
            self.command(&["set", "sid", "no"]);
        }
        for (kind, id, label, selected) in tracks.iter().filter(|t| t.0 == "sub") {
            if menu_check(ui, *selected, label).clicked() {
                self.command(&[
                    "set",
                    if kind == "audio" { "aid" } else { "sid" },
                    &id.to_string(),
                ]);
            }
        }
        menu_separator(ui);
        menu_heading(ui, "Text encoding");
        let automatic = self
            .settings
            .subtitle_language
            .as_deref()
            .and_then(subsearch::language)
            .and_then(|l| {
                l.codepage
                    .map(|page| format!("{} · {}", l.name, subtext::code_page_name(page)))
            })
            .unwrap_or_else(|| "your Windows language".to_string());
        let chosen = self.subtitle_encoding;
        let mut picked = chosen;
        egui::CollapsingHeader::new(match chosen {
            None => format!("Automatic ({automatic})"),
            Some(id) => subtext::code_page_name(id),
        })
        .default_open(self.capture_tracks)
        .show(ui, |ui| {
            if menu_check(ui, chosen.is_none(), &format!("Automatic ({automatic})")).clicked() {
                picked = None;
            }
            for page in subtext::CODE_PAGES {
                if menu_check(ui, chosen == Some(page.id), page.name).clicked() {
                    picked = Some(page.id);
                }
            }
        });
        if picked != chosen {
            self.subtitle_encoding = picked;
            self.reread_subtitle();
        }
        menu_separator(ui);
        menu_heading(ui, "Adjust subtitles");
        ui.horizontal(|ui| {
            ui.label(format!("Delay {:+.1} s", self.sub_delay));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if v2_ui::IconButton::new(Icon::Plus, "Subtitles later by 0.1 seconds")
                    .size(v2_ui::tokens::size::CONTROL_SM)
                    .shortcut("X")
                    .show(ui)
                    .clicked()
                {
                    self.run(Cmd::SubtitleDelayBy(0.1), ui.ctx());
                }
                if v2_ui::IconButton::new(Icon::Minus, "Subtitles earlier by 0.1 seconds")
                    .size(v2_ui::tokens::size::CONTROL_SM)
                    .shortcut("Z")
                    .show(ui)
                    .clicked()
                {
                    self.run(Cmd::SubtitleDelayBy(-0.1), ui.ctx());
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(format!("Size {:.0}%", self.sub_scale * 100.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if v2_ui::IconButton::new(Icon::Plus, "Larger subtitles")
                    .size(v2_ui::tokens::size::CONTROL_SM)
                    .show(ui)
                    .clicked()
                {
                    self.sub_scale = (self.sub_scale + 0.1).clamp(0.5, 2.5);
                    self.apply_sub_scale();
                }
                if v2_ui::IconButton::new(Icon::Minus, "Smaller subtitles")
                    .size(v2_ui::tokens::size::CONTROL_SM)
                    .show(ui)
                    .clicked()
                {
                    self.sub_scale = (self.sub_scale - 0.1).clamp(0.5, 2.5);
                    self.apply_sub_scale();
                }
            });
        });
        if tracks.iter().any(|t| t.0 == "audio") {
            menu_separator(ui);
            menu_heading(ui, "Audio");
            for (_, id, label, selected) in tracks.iter().filter(|t| t.0 == "audio") {
                if menu_check(ui, *selected, label).clicked() {
                    self.command(&["set", "aid", &id.to_string()]);
                }
            }
        }
    }
    /// The brief message and the details card, over the picture.
    fn toast_and_info(&mut self, ctx: &egui::Context, watching: bool) {
        if !watching {
            return;
        }
        if let Some((text, at)) = self.toast.clone() {
            let age = at.elapsed();
            if age > Duration::from_millis(1800) {
                self.toast = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(1800) - age);
                egui::Area::new(egui::Id::new("vesper-toast"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_TOP, [0.0, 76.0])
                    .interactable(false)
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::from_black_alpha(190))
                            .corner_radius(v2_ui::tokens::radius::PILL)
                            .inner_margin(egui::Margin::symmetric(16, 8))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(text)
                                        .color(egui::Color32::WHITE)
                                        .size(14.0),
                                );
                            });
                    });
            }
        }
        if self.info {
            let lines = self.info_lines();
            egui::Area::new(egui::Id::new("vesper-info"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::LEFT_TOP, [16.0, 72.0])
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_black_alpha(200))
                        .corner_radius(v2_ui::tokens::radius::LG)
                        .inner_margin(egui::Margin::same(14))
                        .show(ui, |ui| {
                            // A fixed width keeps the card beside the picture, not across it.
                            ui.set_width(280.0);
                            for (name, value) in lines {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(name)
                                            .color(egui::Color32::from_white_alpha(150))
                                            .size(13.0),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new(value)
                                                    .color(egui::Color32::WHITE)
                                                    .size(13.0),
                                            );
                                        },
                                    );
                                });
                            }
                        });
                });
        }
    }
    /// What the decoder reports about the open file, as it reports it.
    fn info_lines(&self) -> Vec<(String, String)> {
        let Some(p) = self.player.as_ref() else {
            return vec![("Video".into(), "nothing open".into())];
        };
        let number = |key: &str| p.number(key);
        let text = |key: &str| p.string(key).filter(|v| !v.is_empty());
        let mut lines = Vec::new();
        if let (Some(w), Some(h)) = (number("video-params/w"), number("video-params/h")) {
            lines.push(("Picture".into(), format!("{w:.0} × {h:.0}")));
        }
        if let Some(codec) = text("video-codec") {
            lines.push((
                "Video".into(),
                codec.split(" / ").next().unwrap_or(&codec).to_string(),
            ));
        }
        let decoder = match text("hwdec-current").as_deref() {
            Some("no") | None => "software".to_string(),
            Some(name) => format!("hardware ({name})"),
        };
        lines.push(("Decoding".into(), decoder));
        if let Some(fps) = number("estimated-vf-fps") {
            lines.push(("Frame rate".into(), format!("{fps:.1} fps")));
        }
        if let Some(codec) = text("audio-codec") {
            lines.push((
                "Audio".into(),
                codec.split(" / ").next().unwrap_or(&codec).to_string(),
            ));
        }
        if let Some(container) = text("file-format") {
            lines.push(("Container".into(), container));
        }
        let dropped = number("frame-drop-count").unwrap_or(0.0)
            + number("decoder-frame-drop-count").unwrap_or(0.0);
        lines.push(("Dropped frames".into(), format!("{dropped:.0}")));
        lines
    }
    /// "Up next" for a chosen folder, floating on the right of the picture.
    fn queue_sheet(&mut self, ctx: &egui::Context) {
        let screen = ctx.content_rect();
        let card = egui::Rect::from_min_max(
            egui::pos2(screen.right() - 316.0, screen.top() + 64.0),
            egui::pos2(screen.right() - 16.0, screen.bottom() - BAR - 12.0),
        );
        if card.height() < 140.0 {
            return;
        }
        let mut open = None;
        let mut close = false;
        egui::Area::new(egui::Id::new("vesper-queue"))
            .order(egui::Order::Middle)
            .fixed_pos(card.min)
            .fade_in(false)
            .show(ctx, |ui| {
                ui.set_style(ctx.style_of(egui::Theme::Dark));
                let p = v2_ui::palette_of(ui);
                v2_ui::chrome::frame_sheet(&p).show(ui, |ui| {
                    let inner = card.size() - egui::Vec2::splat(2.0 * v2_ui::tokens::space::LG);
                    ui.set_min_size(inner);
                    ui.set_max_size(inner);
                    ui.horizontal(|ui| {
                        ui.label(v2_ui::title("Up next"));
                        ui.label(v2_ui::caption(format!("{} videos", self.queue.len())));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if v2_ui::IconButton::new(Icon::Close, "Close Up next")
                                .size(v2_ui::tokens::size::CONTROL_SM)
                                .show(ui)
                                .clicked()
                            {
                                close = true;
                            }
                        });
                    });
                    if self.folder_rx.is_some() {
                        ui.add_space(6.0);
                        v2_ui::loading(ui, "Reading the folder…");
                    }
                    ui.add_space(4.0);
                    ui.label(v2_ui::caption(
                        "One folder you chose. Nothing else is scanned.",
                    ));
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show_rows(
                            ui,
                            v2_ui::tokens::size::ROW,
                            self.queue.len(),
                            |ui, range| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for i in range {
                                    let name = self.queue[i]
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned();
                                    if v2_ui::ListRow::new(&name)
                                        .number(i + 1)
                                        .active(i == self.index)
                                        .show(ui)
                                        .clicked()
                                    {
                                        open = Some(i);
                                    }
                                }
                            },
                        );
                });
            });
        if close {
            self.show_queue = false;
        }
        if let Some(i) = open {
            self.index = i;
            let f = self.queue[i].clone();
            self.open(&f);
        }
    }
    /// Same App::ui, widgets and video paint callback in an independent, no-input
    /// egui context. Reads the native backbuffer; sends no input to the desktop.
    fn capture_composites(&mut self, frame: &mut eframe::Frame, out: &Path) -> Result<(), String> {
        use glow::HasContext;
        let Some(mut painter) = self.native_painter.take() else {
            return Err("Native capture painter unavailable.".into());
        };
        let gl = painter.gl().clone();
        let capture_ctx = egui::Context::default();
        v2_ui::install_with(
            &capture_ctx,
            Product::Vesper,
            v2_ui::Options {
                reduced_motion: Some(true),
                ..Default::default()
            },
        );
        let was_fullscreen = self.fullscreen;
        let last_input = self.last_input;
        let result = (|| {
            // (file, theme, fullscreen, start screen, width px, height px, scale)
            for (name, theme, fullscreen, start, w, h, scale) in [
                (
                    "composite-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-light.png",
                    egui::ThemePreference::Light,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-fullscreen.png",
                    egui::ThemePreference::Dark,
                    true,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-compact-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    680,
                    440,
                    1.0,
                ),
                (
                    "composite-dark-150.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    1100,
                    720,
                    1.5,
                ),
                (
                    "composite-start-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    true,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-start-light.png",
                    egui::ThemePreference::Light,
                    false,
                    true,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-loading-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-error-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-start-error-light.png",
                    egui::ThemePreference::Light,
                    false,
                    true,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-engine-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-search-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-search-done-light.png",
                    egui::ThemePreference::Light,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-info-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-drop-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-search-compact-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    680,
                    440,
                    1.0,
                ),
                (
                    "composite-tracks-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    1100,
                    720,
                    1.0,
                ),
                (
                    "composite-tracks-compact-dark.png",
                    egui::ThemePreference::Dark,
                    false,
                    false,
                    680,
                    440,
                    1.0,
                ),
            ] {
                capture_ctx.set_theme(theme);
                capture_ctx.set_pixels_per_point(scale);
                self.fullscreen = fullscreen;
                self.last_input = Instant::now();
                // The start screen is the same App::ui with nothing open; the video stays loaded.
                let current = if start { self.current.take() } else { None };
                // Loading and error are the same App::ui with that state set for this capture
                // only; the messages are ones Vesper actually produces (ui.rs, media.rs).
                let (was_loading, was_error) = (self.loading, self.error.clone());
                let (was_search, was_info, was_toast) =
                    (self.search.take(), self.info, self.toast.clone());
                let was_since = self.loading_since;
                let mut hovered = Vec::new();
                if name.contains("loading") {
                    self.loading = true;
                }
                if name.contains("engine") {
                    self.loading = true;
                    self.capture_label = Some("Starting the video engine…");
                    self.loading_since = Some(Instant::now() - Duration::from_secs(8));
                }
                if name.contains("search") {
                    let mut form = SearchForm::new(
                        "Blade Runner 2049".into(),
                        Some("hrv".into()),
                        subsearch::Provider::OpenSubtitlesOrg,
                    );
                    if name.contains("done") {
                        // The state after a successful press, without pressing anything.
                        form.status = Some(Ok("Opened OpenSubtitles.org in your browser.".into()));
                    }
                    self.search = Some(form);
                    self.search_fresh = false;
                }
                if name.contains("tracks") {
                    self.capture_tracks = true;
                }
                if name.contains("info") {
                    self.info = true;
                    self.toast = Some(("Subtitle delay +0.2 s".into(), Instant::now()));
                }
                if name.contains("drop") {
                    hovered.push(egui::HoveredFile::default());
                }
                if name.contains("start-error") {
                    self.error = Some(
                        "Network and device paths are unavailable in this offline player.".into(),
                    );
                } else if name.contains("error") {
                    self.error = Some("Playback works, but resume could not be saved: Access is denied. (os error 5)".into());
                }
                // Bounded layout passes let foreground Area and popup placement settle.
                for _ in 0..3 {
                    let input = egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(w as f32 / scale, h as f32 / scale),
                        )),
                        hovered_files: hovered.clone(),
                        ..Default::default()
                    };
                    let output = capture_ctx.run_ui(input, |ui| eframe::App::ui(self, ui, frame));
                    // Wait only in this bounded capture harness for the async
                    // subtitle-layout command to be reflected before readback.
                    let layout_start = Instant::now();
                    while layout_start.elapsed() < Duration::from_millis(200) {
                        let Some(p) = self.player.as_ref() else {
                            break;
                        };
                        p.poll();
                        if p.number("sub-pos") == self.subtitle_position {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    let primitives = capture_ctx.tessellate(output.shapes, output.pixels_per_point);
                    let p = v2_ui::palette(&capture_ctx);
                    let rgba = p.bg.to_normalized_gamma_f32();
                    unsafe {
                        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                        gl.disable(glow::SCISSOR_TEST);
                        gl.clear_color(rgba[0], rgba[1], rgba[2], 1.0);
                        gl.clear(glow::COLOR_BUFFER_BIT);
                    }
                    painter.paint_and_update_textures(
                        [1100, 720],
                        output.pixels_per_point,
                        &primitives,
                        &output.textures_delta,
                    );
                }
                let mut rgba = vec![0_u8; 1100 * 720 * 4];
                unsafe {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                    gl.read_pixels(
                        0,
                        0,
                        1100,
                        720,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelPackData::Slice(Some(&mut rgba)),
                    );
                }
                if start {
                    self.current = current;
                }
                self.loading = was_loading;
                self.error = was_error;
                self.search = was_search;
                self.info = was_info;
                self.toast = was_toast;
                self.loading_since = was_since;
                self.capture_label = None;
                self.capture_tracks = false;
                // A smaller layout occupies the top-left of the 1100 x 720 backbuffer.
                let mut bgr = vec![0_u8; w * h * 4];
                for y in 0..h {
                    for x in 0..w {
                        let from = ((719 - y) * 1100 + x) * 4;
                        let to = (y * w + x) * 4;
                        bgr[to..to + 4].copy_from_slice(&[
                            rgba[from + 2],
                            rgba[from + 1],
                            rgba[from],
                            255,
                        ]);
                    }
                }
                vesper::smoke::write_png(&out.join(name), &bgr, w as u32, h as u32)?;
            }
            Ok(())
        })();
        capture_ctx.set_pixels_per_point(1.0);
        self.fullscreen = was_fullscreen;
        self.last_input = last_input;
        self.native_painter = Some(painter);
        result
    }
    /// Auto-ending first-file benchmark: the real open path, decoder, render context and
    /// libmpv GL renderer in a native window (requested hidden, shown by the toolkit after its first
    /// frame). It ends itself and writes one JSON report.
    fn bench_step(&mut self, ctx: &egui::Context, bench: &Bench, out: &Path) {
        if let (Some(v), Some(painter)) = (&self.video, &self.native_painter)
            && let Ok(mut v) = v.lock()
            && let Err(e) = v.hidden_render(painter)
        {
            v.error = Some(e);
        }
        let (frames, new_frames) = self
            .video
            .as_ref()
            .and_then(|v| v.lock().ok())
            .map_or((0, 0), |v| (v.frames, v.new_frames));
        let now = Instant::now();
        // A picture counts only when the decoder asked for a redraw after the file loaded.
        if vesper::timing::mark_time("event_file_loaded").is_some() {
            let at_load = *self.bench_frames_at_load.get_or_insert(new_frames);
            let sized = self
                .player
                .as_ref()
                .and_then(|p| p.number("video-params/w"))
                .is_some_and(|w| w > 0.0);
            if sized && new_frames > at_load && self.bench_first_render.is_none() {
                vesper::timing::mark("first_video_frame");
                self.bench_first_render = Some(now);
                self.bench_usage_first = Some(vesper::timing::usage());
            }
        }
        let since_first = self
            .bench_first_render
            .map(|t| now.saturating_duration_since(t).as_millis());
        // One hash of the picture shortly after the first frame, one after the steady window:
        // different hashes prove the decoded picture changed through the GL path.
        if since_first.is_some_and(|ms| ms >= 400)
            && self.bench_hash_first.is_none()
            && let (Some(v), Some(painter)) = (&self.video, &self.native_painter)
            && let Ok(mut v) = v.lock()
        {
            self.bench_hash_first = v.frame_hash(painter.gl());
        }
        // Counters are compared from one second in, so start-up catch-up is not counted.
        if since_first.is_some_and(|ms| ms >= 1000) && self.bench_steady.is_none() {
            self.bench_steady = Some(self.bench_snapshot(now));
        }
        let played = since_first.is_some_and(|ms| ms >= bench.play_ms);
        let timed_out = vesper::timing::since_start_ms() > 60_000.0;
        if (played || self.error.is_some() || timed_out) && !self.native_reported {
            let end = vesper::timing::usage();
            let last = self.bench_snapshot(now);
            let hash_last = self
                .video
                .as_ref()
                .zip(self.native_painter.as_ref())
                .and_then(|(v, painter)| v.lock().ok()?.frame_hash(painter.gl()));
            let steady = self.bench_steady.map(|first| {
                let window = last.at.saturating_duration_since(first.at).as_secs_f64();
                serde_json::json!({
                    "window_s": window,
                    "vo_frame_drops": last.vo_drops - first.vo_drops,
                    "decoder_frame_drops": last.decoder_drops - first.decoder_drops,
                    "vo_delayed_frames": last.delayed - first.delayed,
                    "media_seconds_advanced": last.position - first.position,
                    "realtime_ratio": (last.position - first.position) / window.max(0.001),
                    "cpu_core_fraction": (last.cpu_seconds - first.cpu_seconds) / window.max(0.001),
                })
            });
            let p = self.player.as_ref();
            let text = |key: &str| p.and_then(|p| p.string(key));
            let number = |key: &str| p.and_then(|p| p.number(key));
            let play_s = self
                .bench_first_render
                .map(|t| now.saturating_duration_since(t).as_secs_f64());
            let cpu_in_play = self
                .bench_usage_first
                .map(|first| end.cpu_seconds - first.cpu_seconds);
            let report = serde_json::json!({
                "ok": played && self.error.is_none() && frames > 0,
                "workload": "startup-bench: real UI open path, native GL window (requested hidden, shown by the toolkit after its first frame), libmpv OpenGL render; not owner-operated GUI use",
                "file": self.current.as_ref().map(|p| p.display().to_string()),
                "hwdec_requested": bench.hwdec,
                "audio_device": if bench.device { "wasapi (volume 0)" } else { "null" },
                "timeline_ms": vesper::timing::timeline(),
                "hwdec_current": text("hwdec-current"),
                "video_codec": text("video-codec"),
                "audio_codec": text("audio-codec"),
                "container": text("file-format"),
                "width": number("video-params/w"),
                "height": number("video-params/h"),
                "duration_s": self.duration,
                "position_s": self.position,
                "render_calls": frames,
                "decoder_requested_renders": new_frames,
                "playback_window_s": play_s,
                "cpu_seconds_in_playback_window": cpu_in_play,
                "cpu_core_fraction_in_playback_window": cpu_in_play.zip(play_s).map(|(c, w)| c / w.max(0.001)),
                "steady_state": steady,
                "picture_changed": self.bench_hash_first.zip(hash_last).map(|(a, b)| a != b),
                "picture_hashes": [self.bench_hash_first, hash_last],
                "vo_frame_drops": number("frame-drop-count"),
                "decoder_frame_drops": number("decoder-frame-drop-count"),
                "vo_delayed_frames": number("vo-delayed-frame-count"),
                "estimated_vf_fps": number("estimated-vf-fps"),
                "usage_at_end": end,
                "error": self.error.clone(),
                "timed_out": timed_out,
            });
            let _ = std::fs::write(
                out.join("bench-result.json"),
                serde_json::to_vec_pretty(&report).unwrap_or_default(),
            );
            println!("{report}");
            self.native_reported = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            ctx.request_repaint_after(Duration::from_millis(4));
        }
    }
    fn bench_snapshot(&self, at: Instant) -> BenchSnapshot {
        let number = |key: &str| {
            self.player
                .as_ref()
                .and_then(|p| p.number(key))
                .unwrap_or(0.0)
        };
        BenchSnapshot {
            at,
            vo_drops: number("frame-drop-count"),
            decoder_drops: number("decoder-frame-drop-count"),
            delayed: number("vo-delayed-frame-count"),
            position: number("time-pos"),
            cpu_seconds: vesper::timing::usage().cpu_seconds,
        }
    }
    fn keyboard(&mut self, ctx: &egui::Context) {
        let focus = Focus::from_context(ctx, self.search.is_some());
        for cmd in input::commands(ctx, focus) {
            self.run(cmd, ctx);
        }
    }
    fn apply_sub_scale(&mut self) {
        if let Some(p) = self.player.clone() {
            self.apply(p.set_number("sub-scale", self.sub_scale));
        }
    }
    /// One shortcut or menu command. Every key and button routes through here.
    fn run(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            Cmd::OpenFile => self.choose_file(),
            Cmd::ToggleHelp => self.help = !self.help,
            Cmd::TogglePause => self.toggle(),
            Cmd::SeekBy(seconds) => self.seek(self.position + seconds),
            Cmd::Restart => {
                self.seek(0.0);
                self.say("Starting over");
            }
            Cmd::VolumeBy(delta) => {
                self.volume = (self.volume + delta).clamp(0.0, 100.0);
                if let Some(p) = self.player.clone() {
                    self.apply(p.set_number("volume", self.volume));
                }
                self.say(format!("Volume {:.0}%", self.volume));
            }
            Cmd::ToggleMute => {
                self.muted = !self.muted;
                self.command(&["set", "mute", if self.muted { "yes" } else { "no" }]);
                self.say(if self.muted { "Muted" } else { "Sound on" });
            }
            Cmd::ToggleFullscreen => self.fullscreen(ctx),
            Cmd::Escape => {
                if self.search.is_some() {
                    self.search = None;
                } else if self.fullscreen {
                    self.fullscreen(ctx);
                } else {
                    self.cancel_open();
                }
            }
            Cmd::CycleSubtitles => {
                self.command(&["cycle", "sid"]);
                self.say("Next subtitle track");
            }
            Cmd::CycleAudio => {
                self.command(&["cycle", "aid"]);
                self.say("Next audio track");
            }
            Cmd::ToggleSubtitles => {
                self.sub_visible = !self.sub_visible;
                self.command(&[
                    "set",
                    "sub-visibility",
                    if self.sub_visible { "yes" } else { "no" },
                ]);
                self.say(if self.sub_visible {
                    "Subtitles shown"
                } else {
                    "Subtitles hidden"
                });
            }
            Cmd::SpeedBy(delta) => {
                self.speed = (self.speed + delta).clamp(0.25, 2.0);
                if let Some(p) = self.player.clone() {
                    self.apply(p.set_number("speed", self.speed));
                }
                self.say(format!("Speed {}×", self.speed));
            }
            Cmd::SpeedReset => {
                self.speed = 1.0;
                if let Some(p) = self.player.clone() {
                    self.apply(p.set_number("speed", 1.0));
                }
                self.say("Normal speed");
            }
            Cmd::FrameStep(direction) => {
                if self.current.is_some() && !self.loading {
                    self.command(&[if direction < 0 {
                        "frame-back-step"
                    } else {
                        "frame-step"
                    }]);
                    self.paused = true;
                }
            }
            Cmd::SubtitleDelayBy(delta) => {
                self.sub_delay = ((self.sub_delay + delta) * 10.0).round() / 10.0;
                if let Some(p) = self.player.clone() {
                    self.apply(p.set_number("sub-delay", self.sub_delay));
                }
                self.say(format!("Subtitle delay {:+.1} s", self.sub_delay));
            }
            Cmd::ToggleInfo => {
                self.info = !self.info;
                if self.info
                    && !self.info_observed
                    && let Some(p) = self.player.clone()
                {
                    self.apply(p.observe_numbers(&[
                        "frame-drop-count",
                        "decoder-frame-drop-count",
                        "vo-delayed-frame-count",
                        "estimated-vf-fps",
                    ]));
                    self.info_observed = true;
                }
            }
            Cmd::FindSubtitles => self.open_search(),
            Cmd::PreviousVideo => self.next(-1),
            Cmd::NextVideo => self.next(1),
        }
    }
    fn open_search(&mut self) {
        if self.search.is_some() {
            return;
        }
        let basis = self
            .current
            .clone()
            .or_else(|| self.opening_label.as_ref().map(PathBuf::from));
        let query = basis
            .as_deref()
            .map(subsearch::suggest_query)
            .unwrap_or_default();
        self.search = Some(SearchForm::new(
            query,
            self.settings.subtitle_language.clone(),
            self.settings.subtitle_provider,
        ));
        self.search_fresh = true;
    }
    /// The online subtitle search sheet. Opening it, editing it and choosing a site do nothing
    /// on the network; only the Search button hands the address to the owner's browser.
    fn search_window(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.search.take() else {
            return;
        };
        let focus_title = std::mem::take(&mut self.search_fresh);
        let events = vesper::search_ui::show(ctx, &mut form, focus_title);
        let (keep, submit, copy, downloaded) = (
            !events.closed,
            events.submit,
            events.copy_address,
            events.load_downloaded,
        );
        if copy && let Ok(url) = form.url() {
            ctx.copy_text(url);
            self.say("Address copied");
        }
        if submit && let Ok(url) = form.begin() {
            // Windows may take a moment to start the browser: hand it over on a worker.
            let tx = self.browser_tx.clone();
            let repaint = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("vesper-open-browser".into())
                .spawn({
                    let tx = tx.clone();
                    let repaint = repaint.clone();
                    let url = url.clone();
                    move || {
                        let result = subsearch::open_in_browser(&url);
                        let _ = tx.send((url, result));
                        repaint.request_repaint();
                    }
                });
            if spawned.is_err() {
                let _ = tx.send((url, Err("could not start a worker".into())));
                repaint.request_repaint();
            }
            self.settings.subtitle_language = form.language.clone();
            self.settings.subtitle_provider = form.provider;
            self.settings.language_chosen = true;
            if let Err(e) = self.settings.save(&self.settings_path) {
                self.error = Some(format!("Your subtitle language could not be saved: {e}"));
            }
            self.apply_language();
        }
        if downloaded {
            self.pick_subtitle(downloads_dir());
        }
        if keep {
            self.search = Some(form);
        }
    }
    /// A changed language preference applies to the next file the decoder opens.
    fn apply_language(&mut self) {
        for (key, value) in self.language_options() {
            if let Some(p) = self.player.clone() {
                self.apply(p.set_string(&key, &value));
            }
        }
    }
}

fn downloads_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join("Downloads"))
        .filter(|d| d.is_dir())
}

impl eframe::App for Vesper {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if let Some(file) = self.native_open_pending.take() {
            // Native validation exercises the same late-open path as a picker:
            // no decoder exists during construction, and it starts on this UI thread.
            debug_assert!(self.player.is_none() && self.video.is_none());
            self.queue = vec![file.clone()];
            self.open(&file);
            self.native_started = Instant::now();
        }
        self.poll_checks();
        self.poll_decoder();
        self.poll_subtitles();
        while let Ok((url, result)) = self.browser_rx.try_recv() {
            if let Some(form) = self.search.as_mut() {
                form.finished(&url, result);
            }
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            // Files are being dragged over the window: a drop is likely, so start reading the decoder.
            vesper::prefetch::start_library(self.decoder_dll.clone());
        }
        let render_error = self
            .video
            .as_ref()
            .and_then(|v| v.lock().ok())
            .and_then(|mut v| v.error.take());
        if let Some(e) = render_error {
            self.error = Some(format!("Video output failed: {e}"));
            self.command(&["set", "pause", "yes"]);
        }
        if let Some(p) = self.player.clone() {
            let batch = p.poll_batch();
            if batch.needs_drain {
                ctx.request_repaint();
            }
            for (id, error) in batch.events {
                if let Some(e) = error {
                    self.error = Some(format!("Cannot play this file: {e}"));
                    self.loading = false;
                    self.paused = true;
                }
                if id == 6 {
                    vesper::timing::mark("event_start_file");
                }
                if id == 8 {
                    vesper::timing::mark("event_file_loaded");
                    self.loading = false;
                    self.loading_since = None;
                    let pending = std::mem::take(&mut self.pending_subtitles);
                    for sub in pending {
                        self.subtitle(&sub);
                    }
                    self.autoload_siblings();
                }
                if id == 7 && self.error.is_none() {
                    self.loading = false;
                }
            }
            self.position = p.number("time-pos").unwrap_or(self.position);
            self.duration = p.number("duration").unwrap_or(self.duration);
            self.paused = p.string("pause").as_deref() != Some("no")
                || p.string("idle-active").as_deref() != Some("no");
        }
        if let Some(rx) = &self.folder_rx
            && let Ok(r) = rx.try_recv()
        {
            self.folder_rx = None;
            match r {
                Ok(files) => {
                    self.queue = files;
                    self.index = 0;
                    self.show_queue = true;
                    if let Some(f) = self.queue.first().cloned() {
                        self.open(&f);
                    } else {
                        self.error = Some("No video candidates found in this folder.".into());
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
        if !self.paused && self.current.is_some() {
            if self.last_save.elapsed() > Duration::from_secs(10) {
                self.save_resume();
            }
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            // Checking every dropped path touches the disk: do it off the UI thread.
            let tx = self.drop_tx.clone();
            let repaint = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("vesper-drop-check".into())
                .spawn(move || {
                    let _ = tx.send(media::classify_drops(dropped));
                    repaint.request_repaint();
                });
            if spawned.is_err() {
                self.error = Some("The dropped files could not be checked.".into());
            }
        }
        while let Ok(plan) = self.drop_rx.try_recv() {
            self.apply_drop(plan, ctx);
        }
        let pointer = ctx.input(|i| i.pointer.latest_pos());
        if pointer != self.last_pointer || ctx.input(|i| !i.events.is_empty()) {
            self.last_input = Instant::now();
            self.last_pointer = pointer;
        }
        self.keyboard(ctx);
        if let (Some(bench), Some(out)) = (self.bench.clone(), self.native_out.clone()) {
            self.bench_step(ctx, &bench, &out);
        } else if let Some(out) = self.native_out.clone() {
            let elapsed = self.native_started.elapsed();
            if !self.captured
                && elapsed > Duration::from_millis(500)
                && self.duration > 0.0
                && let Some(v) = &self.video
                && let Ok(mut v) = v.lock()
            {
                v.capture = Some(out.join("native-frame.png"));
                self.captured = true;
            }
            // Service render updates during loading as well as playback. libmpv's
            // video output initialization can depend on the first render call.
            if let (Some(v), Some(painter)) = (&self.video, &self.native_painter)
                && let Ok(mut v) = v.lock()
                && let Err(e) = v.hidden_render(painter)
            {
                v.error = Some(e);
            }
            if !self.native_composite && elapsed > Duration::from_millis(750) && self.duration > 0.0
            {
                match self.capture_composites(frame, &out) {
                    Ok(()) => self.native_composite = true,
                    Err(e) => self.error = Some(format!("Composite capture failed: {e}")),
                }
            }
            // Composite PNG encoding is synchronous. Measure after it finishes,
            // after pause is acknowledged, and after a short resize/render settle.
            let now = Instant::now();
            if self.native_composite && self.native_idle.is_none() {
                if !self.native_pause_requested {
                    self.command(&["set", "pause", "yes"]);
                    self.native_pause_requested = true;
                }
                if self
                    .player
                    .as_ref()
                    .and_then(|p| p.string("pause"))
                    .as_deref()
                    == Some("yes")
                {
                    let ack = *self.native_pause_ack_at.get_or_insert(now);
                    if now.saturating_duration_since(ack) >= Duration::from_secs(1)
                        && let Some(v) = &self.video
                        && let Ok(v) = v.lock()
                    {
                        self.native_idle = Some((v.frames, v.wakeups(), now));
                    }
                }
            }
            let idle_complete = self
                .native_idle
                .is_some_and(|(_, _, start)| idle_window_complete(start, now));
            let timed_out =
                now.saturating_duration_since(self.native_started) >= Duration::from_secs(30);
            if (idle_complete || timed_out) && !self.native_reported {
                let (frames, render_error) = self
                    .video
                    .as_ref()
                    .and_then(|v| v.lock().ok())
                    .map(|v| (v.frames, v.error.clone()))
                    .unwrap_or_default();
                let p = self.player.as_ref();
                let wakeups = self
                    .video
                    .as_ref()
                    .and_then(|v| v.lock().ok())
                    .map(|v| v.wakeups())
                    .unwrap_or(0);
                let idle = self
                    .native_idle
                    .map(|(r, w, _)| (frames.saturating_sub(r), wakeups.saturating_sub(w)));
                let report = serde_json::json!({"ok":idle_complete&&self.native_composite&&frames>2&&out.join("native-frame.png").exists()&&self.error.is_none()&&render_error.is_none(),"renderer":"libmpv OpenGL FBO/readback in a native GL window (requested hidden, shown by the toolkit after its first frame)", "composite_ui_verified":self.native_composite,"frames":frames,"frame_count_meaning":"render calls including framebuffer resize; decoded output is checked separately","hwdec":p.and_then(|p|p.string("hwdec-current")),"decoder":p.and_then(|p|p.string("mpv-version")),"error":self.error.clone().or(render_error),"audio_output":p.and_then(|p|p.string("current-ao")),"paused":p.and_then(|p|p.string("pause")),"renders_in_final_5s":idle.map(|(r,_)|r),"mpv_wakeups_in_final_5s":idle.map(|(_,w)|w),"idle_observation_ms":self.native_idle.map(|(_,_,start)|now.saturating_duration_since(start).as_millis()),"idle_start_ms":self.native_idle.map(|(_,_,start)|start.saturating_duration_since(self.native_started).as_millis()),"report_elapsed_ms":now.saturating_duration_since(self.native_started).as_millis(),"idle_window_complete":idle_complete,"timed_out":timed_out});
                let _ = std::fs::write(
                    out.join("native-result.json"),
                    serde_json::to_vec_pretty(&report).unwrap(),
                );
                println!("{report}");
                self.native_reported = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        vesper::timing::mark("first_ui");
        let ctx = ui.ctx().clone();
        let p = v2_ui::palette(&ctx);
        let opening = self.opens.checking();
        let watching = self.current.is_some() || opening;
        // While a video plays the chrome steps aside after a short idle; paused, loading or
        // being used, it stays. Playback already repaints every 250 ms, so hiding needs no timer.
        let idle = Duration::from_secs_f32(v2_ui::tokens::motion::OSD_IDLE_SECS);
        let visible = !watching
            || self.paused
            || self.loading
            || self.show_queue
            || self.last_input.elapsed() < idle
            || ctx.memory(|m| m.focused().is_some())
            || egui::Popup::is_any_open(&ctx);
        // Subtitles sit just above the controls while they show, at the bottom when they hide.
        let height = ctx.content_rect().height().max(1.0);
        let subtitle_position = if watching && visible {
            (100.0 * (1.0 - (BAR + 8.0) / height))
                .floor()
                .clamp(50.0, 100.0) as f64
        } else {
            100.0
        };
        if self.subtitle_position != Some(subtitle_position) {
            self.subtitle_position = Some(subtitle_position);
            if let Some(p) = self.player.clone() {
                self.apply(p.set_number("sub-pos", subtitle_position));
            }
        }
        if !watching {
            egui::Panel::top("header")
                .exact_size(v2_ui::tokens::size::HEADER)
                .frame(v2_ui::chrome::frame_header(&p))
                .show(ui, |ui| self.header(ui));
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(p.stage))
                .show(ui, |ui| {
                    let rect = ui.max_rect();
                    self.start(ui, rect);
                });
        } else {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
                .show(ui, |ui| {
                    let rect = ui.max_rect();
                    if let Some(v) = &self.video {
                        ui.painter().add(Video::callback(v.clone(), rect));
                    }
                    let response = ui.interact(rect, ui.id().with("video"), egui::Sense::click());
                    if response.double_clicked() {
                        self.fullscreen(&ctx);
                    } else if response.clicked() {
                        self.toggle();
                    }
                    if !visible && response.hovered() {
                        ctx.set_cursor_icon(egui::CursorIcon::None);
                    }
                    // Everything laid over the picture uses the dark style, in either theme.
                    ui.set_style(ctx.style_of(egui::Theme::Dark));
                    let mut cancel = false;
                    if self.loading || opening {
                        let label = self.capture_label.unwrap_or(if self.opens.starting() {
                            "Starting the video engine…"
                        } else {
                            "Opening video…"
                        });
                        let waited = self.loading_since.map_or(0.0, |t| t.elapsed().as_secs_f32());
                        let at = egui::Rect::from_center_size(rect.center(), egui::vec2(300.0, 44.0));
                        // A dark capsule keeps the label readable over a bright last frame.
                        ui.painter().rect_filled(
                            at,
                            v2_ui::tokens::radius::PILL,
                            egui::Color32::from_black_alpha(176),
                        );
                        ui.scope_builder(
                            egui::UiBuilder::new()
                                .max_rect(at.shrink2(egui::vec2(18.0, 4.0)))
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                            |ui| {
                                v2_ui::loading(ui, label);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if v2_ui::Button::ghost("Cancel")
                                            .small()
                                            .shortcut("Esc")
                                            .show(ui)
                                            .clicked()
                                        {
                                            cancel = true;
                                        }
                                    },
                                );
                            },
                        );
                        if waited > 5.0 {
                            // On its own dark pill so it reads over any picture.
                            let note = ui.painter().layout_no_wrap(
                                format!(
                                    "Still working after {} s. The first video after starting is the slowest.",
                                    waited as u32
                                ),
                                egui::FontId::proportional(13.0),
                                egui::Color32::from_white_alpha(230),
                            );
                            let pill = egui::Rect::from_center_size(
                                at.center_bottom() + egui::vec2(0.0, 28.0),
                                note.size() + egui::vec2(28.0, 12.0),
                            );
                            ui.painter().rect_filled(
                                pill,
                                v2_ui::tokens::radius::PILL,
                                egui::Color32::from_black_alpha(176),
                            );
                            ui.painter().galley(
                                pill.center() - note.size() / 2.0,
                                note,
                                egui::Color32::WHITE,
                            );
                        }
                    }
                    if cancel {
                        self.cancel_open();
                    }
                    if visible {
                        self.top_bar(ui, rect);
                        self.transport(ui, rect);
                    }
                });
            if self.show_queue {
                self.queue_sheet(&ctx);
            }
            if let Some(e) = self.error.clone() {
                egui::Area::new(egui::Id::new("vesper-error"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_TOP, [0.0, 68.0])
                    .fade_in(false)
                    .show(&ctx, |ui| {
                        ui.set_max_width((ctx.content_rect().width() - 48.0).min(640.0));
                        if v2_ui::Banner::error("Playback needs attention")
                            .detail(&e)
                            .dismissible()
                            .show(ui, |_| {})
                        {
                            self.error = None;
                        }
                    });
            }
        }
        self.search_window(&ctx);
        v2_ui::drop_overlay(&ctx, "Drop to play, or to add subtitles");
        self.toast_and_info(&ctx, watching);
        if self.help {
            egui::Window::new("Keyboard shortcuts")
                .collapsible(false)
                .resizable(false)
                .open(&mut self.help)
                .show(&ctx, |ui| {
                    v2_ui::shortcut_table(
                        ui,
                        &[
                            ("Space", "Play or pause"),
                            ("Left / Right", "Seek 5 seconds"),
                            ("Shift+Left / Right", "Seek 30 seconds"),
                            ("J / L", "Seek 10 seconds"),
                            ("Home", "Start over"),
                            ("Up / Down", "Volume"),
                            ("M", "Mute"),
                            ("[ / ]", "Slower or faster"),
                            ("Backspace", "Normal speed"),
                            (", / .", "Step one frame back or forward"),
                            ("S", "Next subtitle track"),
                            ("V", "Show or hide subtitles"),
                            ("Z / X", "Subtitles earlier or later"),
                            ("A", "Next audio track"),
                            ("Page Up / Down", "Previous or next video"),
                            ("I", "Video details"),
                            ("F / F11", "Full screen"),
                            ("Esc", "Leave full screen, or stop opening"),
                            ("Ctrl+O", "Open a video"),
                            ("Ctrl+F", "Find subtitles online"),
                            ("F1", "This list"),
                        ],
                    );
                    ui.add_space(6.0);
                    ui.label(v2_ui::caption(
                        "Click the picture to pause, double-click for full screen. Decoding uses the bundled libmpv, with hardware decoding when your GPU supports the codec.",
                    ));
                });
        }
    }
    fn on_exit(&mut self, gl: Option<&glow::Context>) {
        if let Some(p) = self.native_painter.as_mut() {
            p.destroy();
        }
        if self.native_out.is_none() {
            self.save_resume();
        }
        if let (Some(v), Some(gl)) = (&self.video, gl)
            && let Ok(mut v) = v.lock()
        {
            v.destroy(gl);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{decoder_options, idle_window_complete};
    use std::time::{Duration, Instant};

    #[test]
    fn native_idle_requires_five_actual_seconds_after_sampling() {
        let start = Instant::now();
        assert!(!idle_window_complete(start, start));
        assert!(!idle_window_complete(
            start,
            start + Duration::from_millis(4999)
        ));
        assert!(idle_window_complete(start, start + Duration::from_secs(5)));
    }

    fn option<'a>(options: &'a [(&str, String)], key: &str) -> &'a str {
        options
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no {key} option"))
    }

    #[test]
    fn a_muted_start_keeps_its_volume_so_unmuting_restores_sound() {
        let muted = decoder_options(70.0, true, 1.25, "auto-copy");
        assert_eq!(option(&muted, "mute"), "yes");
        assert_eq!(
            option(&muted, "volume"),
            "70",
            "muting must not zero the volume it will return to"
        );
        assert_eq!(option(&muted, "speed"), "1.25");
        let audible = decoder_options(70.0, false, 1.0, "auto-copy");
        assert_eq!(option(&audible, "mute"), "no");
        assert_eq!(option(&audible, "volume"), "70");
        // The decoder still starts paused and keeps the last frame, as before.
        assert_eq!(option(&audible, "pause"), "yes");
        assert_eq!(option(&audible, "keep-open"), "yes");
    }
}
