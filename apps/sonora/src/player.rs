//! The audio worker and the device edge of the engine.
//!
//! One persistent output stream is opened lazily on first play and kept until the player has been
//! idle for a while. The worker decodes, converts and mixes on its own thread (see
//! [`crate::engine`]) into small chunks; the device callback only applies volume/pause/flush
//! ramps and counts what it plays. Seeking never reopens the device, and the next track is
//! opened ahead of time on a prefetch thread, so a track change costs no device work at all.

use crate::{
    decoder::{AudioDecoder, Metadata, StreamInfo},
    engine::{Engine, Start, Stream, Tag, MANUAL_FADE_SECONDS},
};
use eframe::egui;
use rodio::{
    cpal::{
        traits::{DeviceTrait, HostTrait},
        SampleFormat,
    },
    OutputStream, OutputStreamBuilder, Source,
};
use std::{
    any::Any,
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

/// Output chunks last 20 ms.
const CHUNKS_PER_SECOND: u32 = 50;
/// Chunks queued ahead of the device (about 240 ms), cut down so a chunk queue never exceeds 2 MiB.
const QUEUE_CHUNKS: usize = 12;
const MAX_QUEUE_BYTES: usize = 2 * 1024 * 1024;
/// Volume, pause and flush changes ramp over this long, so no waveform is cut.
const RAMP_SECONDS: f32 = 0.008;
/// A stopped or finished player gives the device back after this.
const IDLE_CLOSE: Duration = Duration::from_secs(10);
const DEVICE_POLL: Duration = Duration::from_secs(2);
const HEARTBEAT_STALL: Duration = Duration::from_millis(2500);
const MAX_PENDING_EVENTS: usize = 16;

/// Frames a chunk counts for in the queue gauge. An empty end-of-stream marker counts as one,
/// so "something is waiting" stays true until the device has seen it.
fn frames_counted(samples: usize, channels: usize) -> i64 {
    ((samples / channels.max(1)) as i64).max(1)
}

pub fn chunk_frames(rate: u32) -> usize {
    rate.div_ceil(CHUNKS_PER_SECOND) as usize
}
/// Largest output format the player accepts: a chunk of it still fits the queue bound.
pub const MAX_DEVICE_RATE: u32 = 768_000;
pub const MAX_DEVICE_CHANNELS: usize = crate::engine::MAX_OUTPUT_CHANNELS;

/// Chunks queued ahead of the device. Always at least one, never more than 2 MiB in total.
pub fn queue_chunks(rate: u32, channels: usize) -> usize {
    let bytes = chunk_frames(rate) * channels.max(1) * std::mem::size_of::<f32>();
    (MAX_QUEUE_BYTES / bytes.max(1)).clamp(1, QUEUE_CHUNKS)
}

#[derive(Clone, Debug)]
pub enum Command {
    Play(PathBuf, f64, bool),
    Pause(bool),
    Stop,
    Seek(f64),
    Volume(f32),
    /// The track that should follow the current one, opened ahead of time. `None` for no track.
    SetNext(Option<PathBuf>),
    /// Crossfade length in seconds; 0 turns it off.
    SetCrossfade(f32),
}

pub enum Event {
    /// The track the user chose is open and playing; its tags and artwork.
    Loaded(Metadata),
    /// The queue moved on by itself (gapless or crossfade) and this track is now audible.
    Advanced {
        path: PathBuf,
        meta: Metadata,
    },
    Ended,
    /// The current track failed.
    Error(String),
    /// The track queued to follow could not be opened.
    NextFailed {
        path: PathBuf,
        error: String,
    },
    /// Something worth telling the user that is not a track failure.
    Notice(String),
}

/// What the device gives us.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    pub rate: u32,
    pub channels: usize,
    pub format: String,
}

#[derive(Clone, Default)]
pub struct State {
    pub generation: u64,
    pub playing: bool,
    pub loading: bool,
    pub duration: f64,
    /// Source format of the audible track (0 when nothing is loaded).
    pub rate: u32,
    pub channels: u16,
    pub info: Option<StreamInfo>,
    pub resampled: bool,
    pub channel_conversion: bool,
    pub output: Option<Arc<DeviceInfo>>,
    /// Seconds of crossfade still to play, while one is running.
    pub fading: Option<f64>,
    /// The next track is open and ready to start without delay.
    pub next_ready: bool,
}

// ------------------------------------------------------------------------------------------
// Events from worker to UI

#[derive(Default)]
struct Mailbox {
    loaded: Option<(u64, Event)>,
    terminal: Option<(u64, Event)>,
    /// Queue advances and prefetch failures: tagged with the track generation (a new Play
    /// supersedes them) and kept in order, bounded.
    transitions: VecDeque<(u64, Event)>,
    notices: VecDeque<String>,
}
impl Mailbox {
    fn push(&mut self, generation: u64, track_generation: u64, event: Event) {
        match event {
            Event::Loaded(_) => {
                self.loaded = Some((generation, event));
                if self
                    .terminal
                    .as_ref()
                    .is_some_and(|(g, _)| *g <= generation)
                {
                    self.terminal = None;
                }
            }
            Event::Advanced { .. } | Event::NextFailed { .. } => {
                if self.transitions.len() == MAX_PENDING_EVENTS {
                    self.transitions.pop_front();
                }
                self.transitions.push_back((track_generation, event));
            }
            Event::Notice(text) => {
                if self.notices.len() == 4 {
                    self.notices.pop_front();
                }
                self.notices.push_back(text);
            }
            _ => self.terminal = Some((generation, event)),
        }
    }
    fn drain(&mut self, generation: u64, track_generation: u64) -> Vec<Event> {
        let mut events: Vec<Event> = Vec::new();
        events.extend(
            self.loaded
                .take()
                .filter(|(g, _)| *g == generation)
                .map(|(_, e)| e),
        );
        events.extend(
            self.transitions
                .drain(..)
                .filter(|(g, _)| *g == track_generation)
                .map(|(_, e)| e),
        );
        events.extend(
            self.terminal
                .take()
                .filter(|(g, _)| *g == generation)
                .map(|(_, e)| e),
        );
        events.extend(self.notices.drain(..).map(Event::Notice));
        events
    }
}

// ------------------------------------------------------------------------------------------
// The device edge

/// A run of mixed output frames travelling from the worker to the device callback.
pub struct Chunk {
    epoch: u32,
    seq: u64,
    start_pos: f64,
    last: bool,
    data: Vec<f32>,
}

/// State the worker, the device callback and the UI thread share. All lock-free.
pub struct Shared {
    /// Bumped by the worker to flush: the callback fades out what it is playing, drops every
    /// older chunk and continues with the new epoch.
    epoch: AtomicU32,
    paused: AtomicBool,
    volume: AtomicU32,
    /// Position (seconds) of the audible frame, as `f64` bits.
    pos: AtomicU64,
    audible_seq: AtomicU64,
    /// `epoch + 1` once the callback has played the last chunk of that epoch.
    ended_epoch: AtomicU64,
    /// Frames sent to the callback and not yet played or dropped.
    queued: AtomicI64,
    heartbeat: AtomicU64,
    underruns: AtomicU32,
}
impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            epoch: AtomicU32::new(0),
            paused: AtomicBool::new(false),
            volume: AtomicU32::new(0.7f32.to_bits()),
            pos: AtomicU64::new(0f64.to_bits()),
            audible_seq: AtomicU64::new(0),
            ended_epoch: AtomicU64::new(0),
            queued: AtomicI64::new(0),
            heartbeat: AtomicU64::new(0),
            underruns: AtomicU32::new(0),
        })
    }
    fn position(&self) -> f64 {
        f64::from_bits(self.pos.load(Ordering::Relaxed))
    }
}

/// The never-ending source the device plays. Everything it does is per frame and allocation-free.
pub struct OutputSource {
    rx: Receiver<Chunk>,
    recycle: SyncSender<Vec<f32>>,
    shared: Arc<Shared>,
    rate: u32,
    channels: usize,
    cur: Option<Chunk>,
    idx: usize,
    stash: Option<Chunk>,
    epoch: u32,
    gain: f32,
    step: f32,
    started: bool,
    frame: Vec<f32>,
    frame_at: usize,
    ticks: u32,
}

impl OutputSource {
    pub fn new(
        rx: Receiver<Chunk>,
        recycle: SyncSender<Vec<f32>>,
        shared: Arc<Shared>,
        rate: u32,
        channels: usize,
    ) -> Self {
        let epoch = shared.epoch.load(Ordering::Acquire);
        Self {
            rx,
            recycle,
            rate,
            channels,
            cur: None,
            idx: 0,
            stash: None,
            epoch,
            gain: 0.0,
            step: 1.0 / (RAMP_SECONDS * rate as f32),
            started: false,
            frame: vec![0.0; channels],
            frame_at: channels,
            ticks: 0,
            shared,
        }
    }

    fn retire(&mut self, chunk: Chunk) {
        self.shared.queued.fetch_sub(
            frames_counted(chunk.data.len(), self.channels),
            Ordering::Relaxed,
        );
        let mut data = chunk.data;
        data.clear();
        let _ = self.recycle.try_send(data);
    }

    /// The next chunk of the current epoch, discarding older ones.
    fn pull_chunk(&mut self) -> Option<Chunk> {
        loop {
            let chunk = match self.stash.take() {
                Some(c) => c,
                None => match self.rx.try_recv() {
                    Ok(c) => c,
                    Err(_) => return None,
                },
            };
            if chunk.epoch == self.epoch {
                return Some(chunk);
            }
            if chunk.epoch.wrapping_sub(self.epoch) < u32::MAX / 2 {
                // Newer than what we are playing: the epoch change is about to be seen.
                self.stash = Some(chunk);
                return None;
            }
            self.retire(chunk);
        }
    }

    fn next_frame(&mut self) {
        let shared = &self.shared;
        let want_epoch = shared.epoch.load(Ordering::Acquire);
        let paused = shared.paused.load(Ordering::Relaxed);
        let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
        let mismatch = want_epoch != self.epoch;
        let target = if mismatch || paused { 0.0 } else { volume };
        if self.gain < target {
            self.gain = (self.gain + self.step).min(target);
        } else if self.gain > target {
            self.gain = (self.gain - self.step).max(target);
        }
        if mismatch && self.gain <= 1e-4 {
            // What was playing has faded out: drop it and everything queued behind it.
            if let Some(c) = self.cur.take() {
                self.retire(c);
            }
            self.epoch = want_epoch;
            self.started = false;
        }
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.is_multiple_of(256) {
            self.shared.heartbeat.fetch_add(1, Ordering::Relaxed);
        }
        let silent = paused && self.gain <= 1e-4;
        if silent {
            self.frame.iter_mut().for_each(|s| *s = 0.0);
            return;
        }
        // Skip end-of-stream markers that carry no frames, reporting the end they stand for.
        while self.cur.is_none() {
            self.cur = self.pull_chunk();
            self.idx = 0;
            match &self.cur {
                Some(c) if c.data.is_empty() => {
                    let last = c.last;
                    let done = self.cur.take().expect("chunk");
                    self.retire(done);
                    if last {
                        self.started = false;
                        self.shared
                            .ended_epoch
                            .store(u64::from(self.epoch) + 1, Ordering::Release);
                    }
                }
                Some(c) => {
                    self.shared.audible_seq.store(c.seq, Ordering::Relaxed);
                    self.started = true;
                }
                None => break,
            }
        }
        let Some(chunk) = self.cur.as_ref() else {
            // Nothing to play. While the worker has promised audio this is an underrun.
            if self.started && self.shared.queued.load(Ordering::Relaxed) <= 0 && !paused {
                self.shared.underruns.fetch_add(1, Ordering::Relaxed);
                self.started = false;
            }
            self.frame.iter_mut().for_each(|s| *s = 0.0);
            return;
        };
        let at = self.idx;
        let (g, ch) = (self.gain, self.channels);
        for (o, s) in self.frame.iter_mut().zip(&chunk.data[at..at + ch]) {
            let v = *s * g;
            *o = if v.is_finite() {
                v.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        self.idx += ch;
        let frames_in = (self.idx / ch) as f64;
        let pos = chunk.start_pos + frames_in / self.rate as f64;
        if self.epoch == want_epoch {
            // A flushed track is still fading out: the worker already reset the position.
            self.shared.pos.store(pos.to_bits(), Ordering::Relaxed);
        }
        if self.idx >= chunk.data.len() {
            let last = chunk.last;
            let done = self.cur.take().expect("chunk");
            self.retire(done);
            if last {
                self.started = false;
                self.shared
                    .ended_epoch
                    .store(u64::from(self.epoch) + 1, Ordering::Release);
            }
        }
    }

    /// True while a chunk is playing or queued (used by the test device to wait for the worker).
    pub fn has_audio(&self) -> bool {
        self.cur.is_some() || self.stash.is_some() || self.shared.queued.load(Ordering::Relaxed) > 0
    }

    /// Fills `out` (whole frames) the way the device callback does.
    pub fn render(&mut self, out: &mut [f32]) {
        for sample in out.iter_mut() {
            *sample = self.next().unwrap_or(0.0);
        }
    }
}

impl Iterator for OutputSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.frame_at >= self.channels {
            self.next_frame();
            self.frame_at = 0;
        }
        let s = self.frame[self.frame_at];
        self.frame_at += 1;
        Some(s)
    }
}

impl Source for OutputSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        self.channels as u16
    }
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// An opened output device, as the worker sees it.
pub struct OpenDevice {
    pub info: DeviceInfo,
    tx: SyncSender<Chunk>,
    recycle: Receiver<Vec<f32>>,
    lost: Arc<AtomicBool>,
    _guard: Box<dyn Any>,
}

pub trait Backend: Send {
    fn open(&mut self, shared: Arc<Shared>) -> Result<OpenDevice, String>;
    /// Name of the device Windows currently routes sound to, for change detection.
    fn default_device_name(&mut self) -> Option<String>;
}

/// The Windows output (WASAPI through cpal, via rodio's stream).
pub struct SystemBackend;

fn sample_format_name(f: SampleFormat) -> String {
    match f {
        SampleFormat::F32 => "32-bit float".into(),
        SampleFormat::F64 => "64-bit float".into(),
        SampleFormat::I16 => "16-bit".into(),
        SampleFormat::I32 => "32-bit".into(),
        other => format!("{other:?}"),
    }
}

impl Backend for SystemBackend {
    fn open(&mut self, shared: Arc<Shared>) -> Result<OpenDevice, String> {
        let fail = |e: &dyn std::fmt::Display| format!("Cannot open Windows audio output: {e}");
        let name = self
            .default_device_name()
            .unwrap_or_else(|| "Default output".into());
        let lost = Arc::new(AtomicBool::new(false));
        let flag = lost.clone();
        let mut stream: OutputStream = OutputStreamBuilder::from_default_device()
            .map_err(|e| fail(&e))?
            .with_error_callback(move |_| flag.store(true, Ordering::Release))
            .open_stream()
            .or_else(|_| OutputStreamBuilder::open_default_stream())
            .map_err(|e| fail(&e))?;
        stream.log_on_drop(false);
        let config = *stream.config();
        let (rate, channels) = (config.sample_rate(), config.channel_count() as usize);
        let cap = queue_chunks(rate, channels);
        // Room for twice the chunk count: partial chunks at track boundaries are shorter.
        let (tx, rx) = mpsc::sync_channel(cap * 2);
        let (recycle_tx, recycle) = mpsc::sync_channel(cap * 2 + 2);
        stream
            .mixer()
            .add(OutputSource::new(rx, recycle_tx, shared, rate, channels));
        Ok(OpenDevice {
            info: DeviceInfo {
                name,
                rate,
                channels,
                format: sample_format_name(config.sample_format()),
            },
            tx,
            recycle,
            lost,
            _guard: Box::new(stream),
        })
    }

    fn default_device_name(&mut self) -> Option<String> {
        rodio::cpal::default_host()
            .default_output_device()
            .and_then(|d| d.name().ok())
    }
}

// ------------------------------------------------------------------------------------------
// Opening tracks off the audio thread

/// Opens a track in the device's format. The player uses [`FileOpener`]; tests supply synthetic ones.
pub trait Opener: Send + Sync + 'static {
    fn open(&self, path: &Path, start: f64, rate: u32, channels: usize) -> Result<Stream, String>;
}

pub struct FileOpener;
impl Opener for FileOpener {
    fn open(&self, path: &Path, start: f64, rate: u32, channels: usize) -> Result<Stream, String> {
        let mut decoder = AudioDecoder::open(path, true)?;
        let artwork = decoder.metadata.artwork.take();
        let mut meta = decoder.metadata.clone();
        meta.artwork = artwork;
        let tag = Tag {
            path: path.to_path_buf(),
            info: decoder.info.clone(),
            meta,
            ..Default::default()
        };
        let mut stream = Stream::new(Box::new(decoder), rate, channels, tag)?;
        if start > 0.0 {
            stream.seek(start)?;
        }
        Ok(stream)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Manual,
    Next,
}

struct Request {
    id: u64,
    kind: Kind,
    path: PathBuf,
    start: f64,
    rate: u32,
    channels: usize,
}

struct Prepared {
    id: u64,
    kind: Kind,
    path: PathBuf,
    rate: u32,
    channels: usize,
    result: Result<Stream, String>,
}

enum Msg {
    Cmd {
        generation: u64,
        track_generation: u64,
        command: Command,
    },
    Prepared(Box<Prepared>),
    /// The player was dropped.
    Shutdown,
}

#[derive(Default)]
struct Slots {
    manual: Option<Request>,
    next: Option<Request>,
    shutdown: bool,
}

/// One bounded worker that opens the track the user chose, or else the one queued next, with
/// only the latest request of each kind kept: a burst of Next presses never queues up threads.
struct Prefetcher {
    slots: Arc<(Mutex<Slots>, Condvar)>,
}

impl Prefetcher {
    fn spawn(opener: Arc<dyn Opener>, tx: SyncSender<Msg>) -> Self {
        let slots: Arc<(Mutex<Slots>, Condvar)> = Arc::default();
        let shared = slots.clone();
        std::thread::Builder::new()
            .name("sonora-open".into())
            .spawn(move || loop {
                let request = {
                    let (lock, wake) = &*shared;
                    let mut guard = lock.lock().unwrap_or_else(|p| p.into_inner());
                    loop {
                        if guard.shutdown {
                            return;
                        }
                        if let Some(r) = guard.manual.take().or_else(|| guard.next.take()) {
                            break r;
                        }
                        guard = wake.wait(guard).unwrap_or_else(|p| p.into_inner());
                    }
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    opener.open(&request.path, request.start, request.rate, request.channels)
                }))
                .unwrap_or_else(|_| Err("Decoder rejected malformed media".into()));
                let prepared = Prepared {
                    id: request.id,
                    kind: request.kind,
                    path: request.path,
                    rate: request.rate,
                    channels: request.channels,
                    result,
                };
                if tx.send(Msg::Prepared(Box::new(prepared))).is_err() {
                    return;
                }
            })
            .expect("open worker");
        Self { slots }
    }
    fn request(&self, r: Request) {
        let (lock, wake) = &*self.slots;
        let mut g = lock.lock().unwrap_or_else(|p| p.into_inner());
        match r.kind {
            Kind::Manual => g.manual = Some(r),
            Kind::Next => g.next = Some(r),
        }
        wake.notify_one();
    }
    fn cancel_next(&self) {
        self.slots.0.lock().unwrap_or_else(|p| p.into_inner()).next = None;
    }
    fn shutdown(&self) {
        let (lock, wake) = &*self.slots;
        lock.lock().unwrap_or_else(|p| p.into_inner()).shutdown = true;
        wake.notify_all();
    }
}

// ------------------------------------------------------------------------------------------
// The UI-facing handle

/// Knobs that tests turn; the app uses the defaults.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// How long a stopped or finished player keeps the output device open.
    pub idle_close: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            idle_close: IDLE_CLOSE,
        }
    }
}

pub struct Player {
    tx: SyncSender<Msg>,
    /// Set when the player is dropped; the worker sees it even if its queue is full.
    closing: Arc<AtomicBool>,
    mailbox: Arc<Mutex<Mailbox>>,
    generation: Arc<AtomicU64>,
    track_generation: Arc<AtomicU64>,
    state: Arc<Mutex<State>>,
    shared: Arc<Shared>,
}

fn command_for_generation(g: u64, c: Command, latest: u64, active: &mut u64) -> Option<Command> {
    // Settings and pause do not start a new request, so a newer one never cancels them.
    if matches!(
        c,
        Command::Volume(_) | Command::SetNext(_) | Command::SetCrossfade(_) | Command::Pause(_)
    ) {
        return Some(c);
    }
    if g < latest {
        None
    } else {
        *active = g;
        Some(c)
    }
}

impl Player {
    pub fn new(ctx: egui::Context) -> Self {
        Self::with_backend(ctx, Box::new(SystemBackend), Arc::new(FileOpener))
    }

    pub fn with_backend(
        ctx: egui::Context,
        backend: Box<dyn Backend>,
        opener: Arc<dyn Opener>,
    ) -> Self {
        Self::with_options(ctx, backend, opener, Options::default())
    }

    pub fn with_options(
        ctx: egui::Context,
        backend: Box<dyn Backend>,
        opener: Arc<dyn Opener>,
        options: Options,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel(32);
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let generation = Arc::new(AtomicU64::new(0));
        let track_generation = Arc::new(AtomicU64::new(0));
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Shared::new();
        let prefetch = Prefetcher::spawn(opener, tx.clone());
        let (w_shared, w_mailbox, w_state, w_generation) = (
            shared.clone(),
            mailbox.clone(),
            state.clone(),
            generation.clone(),
        );
        let closing = Arc::new(AtomicBool::new(false));
        let w_closing = closing.clone();
        std::thread::Builder::new()
            .name("sonora-audio".into())
            // Built on the thread: the open device is not `Send`.
            .spawn(move || {
                let mut worker = Worker::new(
                    rx,
                    prefetch,
                    backend,
                    w_shared,
                    w_mailbox,
                    w_state,
                    w_generation,
                    ctx,
                );
                worker.idle_close = options.idle_close;
                worker.closing = w_closing;
                worker.run()
            })
            .expect("audio worker");
        Self {
            tx,
            closing,
            mailbox,
            generation,
            track_generation,
            state,
            shared,
        }
    }

    pub fn send(&self, command: Command) -> Result<(), String> {
        if let Command::Volume(v) = command {
            // Volume is immediate and needs no worker round trip.
            self.shared
                .volume
                .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
            return Ok(());
        }
        let current = self.generation.load(Ordering::Acquire);
        let bumps = matches!(
            command,
            Command::Play(..) | Command::Seek(..) | Command::Stop
        );
        let next = if bumps { current + 1 } else { current };
        let track_current = self.track_generation.load(Ordering::Acquire);
        let track_next = if matches!(command, Command::Play(..) | Command::Stop) {
            track_current + 1
        } else {
            track_current
        };
        self.tx
            .try_send(Msg::Cmd {
                generation: next,
                track_generation: track_next,
                command,
            })
            .map_err(|_| "Playback is busy; try again".to_string())?;
        self.generation.store(next, Ordering::Release);
        self.track_generation.store(track_next, Ordering::Release);
        Ok(())
    }
    pub fn events(&self) -> Vec<Event> {
        self.mailbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(
                self.generation.load(Ordering::Acquire),
                self.track_generation.load(Ordering::Acquire),
            )
    }
    pub fn state(&self) -> State {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if s.generation == self.generation.load(Ordering::Acquire) {
            s
        } else {
            State {
                loading: true,
                output: s.output,
                ..Default::default()
            }
        }
    }
    /// Position of the audible frame, counted from the frames the device actually consumed, so
    /// pausing and resuming never shift it.
    pub fn position(&self) -> f64 {
        self.shared.position()
    }
    /// Times the device ran dry while playing (a diagnostic for the self-ending smoke runs).
    pub fn underruns(&self) -> u32 {
        self.shared.underruns.load(Ordering::Relaxed)
    }
}

impl Drop for Player {
    /// Stops both worker threads and releases the output device. Nothing waits: a full command
    /// queue is not an obstacle because the worker also watches the flag.
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        let _ = self.tx.try_send(Msg::Shutdown);
    }
}

// ------------------------------------------------------------------------------------------
// The worker

struct PendingPlay {
    id: u64,
    path: PathBuf,
    start: f64,
    paused: bool,
    generation: u64,
    /// Reopening the same track after a device change: no Loaded event.
    resume: bool,
}

struct Announce {
    seq: u64,
    tag: Arc<Tag>,
    automatic: bool,
}

struct Worker {
    rx: Receiver<Msg>,
    prefetch: Prefetcher,
    backend: Box<dyn Backend>,
    shared: Arc<Shared>,
    mailbox: Arc<Mutex<Mailbox>>,
    state: Arc<Mutex<State>>,
    latest_generation: Arc<AtomicU64>,
    ctx: egui::Context,
    device: Option<OpenDevice>,
    engine: Option<Engine>,
    crossfade: f32,
    generation: u64,
    track_generation: u64,
    epoch: u32,
    next_id: u64,
    pending_play: Option<PendingPlay>,
    want_next: Option<PathBuf>,
    next_requested: Option<(u64, PathBuf)>,
    announce: VecDeque<Announce>,
    held: Option<Chunk>,
    idle_since: Option<Instant>,
    ended_reported: bool,
    last_poll: Instant,
    heartbeat: (u64, Instant),
    ignored_default: Option<String>,
    fading_shown: Option<f64>,
    idle_close: Duration,
    closing: Arc<AtomicBool>,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        rx: Receiver<Msg>,
        prefetch: Prefetcher,
        backend: Box<dyn Backend>,
        shared: Arc<Shared>,
        mailbox: Arc<Mutex<Mailbox>>,
        state: Arc<Mutex<State>>,
        latest_generation: Arc<AtomicU64>,
        ctx: egui::Context,
    ) -> Self {
        Self {
            rx,
            prefetch,
            backend,
            shared,
            mailbox,
            state,
            latest_generation,
            ctx,
            device: None,
            engine: None,
            crossfade: 0.0,
            generation: 0,
            track_generation: 0,
            epoch: 0,
            next_id: 0,
            pending_play: None,
            want_next: None,
            next_requested: None,
            announce: VecDeque::new(),
            held: None,
            idle_since: None,
            ended_reported: true,
            last_poll: Instant::now(),
            heartbeat: (0, Instant::now()),
            ignored_default: None,
            fading_shown: None,
            idle_close: IDLE_CLOSE,
            closing: Arc::new(AtomicBool::new(false)),
        }
    }

    fn emit(&self, generation: u64, event: Event) {
        self.mailbox.lock().unwrap_or_else(|p| p.into_inner()).push(
            generation,
            self.track_generation,
            event,
        );
        self.ctx.request_repaint();
    }
    fn with_state(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap_or_else(|p| p.into_inner()));
        self.ctx.request_repaint();
    }
    fn engine_active(&self) -> bool {
        self.engine.as_ref().is_some_and(Engine::is_active)
    }
    fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }
    fn queued_ms(&self) -> i64 {
        let rate = self
            .device
            .as_ref()
            .map_or(48_000, |d| i64::from(d.info.rate));
        self.shared.queued.load(Ordering::Relaxed).max(0) * 1000 / rate.max(1)
    }

    fn run(mut self) {
        loop {
            if self.closing.load(Ordering::Acquire) {
                break;
            }
            let msg = match self.timeout() {
                Some(t) => match self.rx.recv_timeout(t) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match self.rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => break,
                },
            };
            if let Some(m) = msg {
                self.handle(m);
                while let Ok(m) = self.rx.try_recv() {
                    self.handle(m);
                }
            }
            self.service();
        }
        self.prefetch.shutdown();
    }

    /// How long to wait for a message before the next housekeeping pass.
    fn timeout(&self) -> Option<Duration> {
        self.device.as_ref()?;
        let playing = self.engine_active() && !self.paused();
        if playing {
            // Wake when the queue is about to drop below ~100 ms, not on a fixed fast tick.
            return Some(Duration::from_millis(
                (self.queued_ms() - 100).clamp(4, 60) as u64
            ));
        }
        if !self.ended_reported {
            // The end of the last queued chunk has to be noticed promptly.
            return Some(Duration::from_millis(15));
        }
        self.idle_since.map(|since| {
            self.idle_close
                .saturating_sub(since.elapsed())
                .max(Duration::from_millis(50))
        })
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Cmd {
                generation,
                track_generation,
                command,
            } => {
                let latest = self.latest_generation.load(Ordering::Acquire);
                let mut active = self.generation;
                let Some(command) =
                    command_for_generation(generation, command, latest, &mut active)
                else {
                    return;
                };
                if !matches!(
                    command,
                    Command::SetNext(_)
                        | Command::SetCrossfade(_)
                        | Command::Volume(_)
                        | Command::Pause(_)
                ) {
                    self.generation = active;
                    self.track_generation = self.track_generation.max(track_generation);
                }
                self.command(command);
            }
            Msg::Prepared(p) => self.prepared(*p),
            Msg::Shutdown => self.closing.store(true, Ordering::Release),
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Play(path, start, paused) => self.play(path, start, paused),
            Command::Pause(paused) => {
                if self.engine_active() {
                    self.shared.paused.store(paused, Ordering::Relaxed);
                    self.with_state(|s| s.playing = !paused);
                    self.idle_since = None;
                }
            }
            Command::Stop => {
                self.stop();
            }
            Command::Seek(position) => self.seek(position),
            Command::Volume(v) => self
                .shared
                .volume
                .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed),
            Command::SetNext(path) => {
                self.want_next = path;
                self.sync_next();
            }
            Command::SetCrossfade(seconds) => {
                self.crossfade = seconds;
                if let Some(e) = self.engine.as_mut() {
                    e.set_crossfade(seconds);
                }
            }
        }
    }

    /// Opens the device if it is not open. The output format is known only after this.
    fn ensure_device(&mut self) -> Result<(), String> {
        if self.device.is_some() && self.engine.is_some() {
            return Ok(());
        }
        self.shared.queued.store(0, Ordering::Relaxed);
        let device = self.backend.open(self.shared.clone())?;
        let mut engine = Engine::new(device.info.rate, device.info.channels);
        engine.set_crossfade(self.crossfade);
        let info = Arc::new(device.info.clone());
        self.with_state(|s| s.output = Some(info));
        self.device = Some(device);
        self.engine = Some(engine);
        self.heartbeat = (
            self.shared.heartbeat.load(Ordering::Relaxed),
            Instant::now(),
        );
        Ok(())
    }

    fn format(&self) -> Option<(u32, usize)> {
        self.device.as_ref().map(|d| (d.info.rate, d.info.channels))
    }

    /// Flushes everything already sent to the device: it fades out over a few milliseconds.
    fn flush(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.shared.epoch.store(self.epoch, Ordering::Release);
        self.held = None;
        self.announce.clear();
        self.shared.ended_epoch.store(0, Ordering::Relaxed);
    }

    fn play(&mut self, path: PathBuf, start: f64, paused: bool) {
        if let Err(e) = self.ensure_device() {
            self.pending_play = None;
            self.emit(self.generation, Event::Error(e));
            return;
        }
        let (rate, channels) = self.format().expect("device");
        self.next_id += 1;
        let id = self.next_id;
        self.pending_play = Some(PendingPlay {
            id,
            path: path.clone(),
            start,
            paused,
            generation: self.generation,
            resume: false,
        });
        // The UI re-sends the track that follows the new one.
        self.want_next = None;
        self.next_requested = None;
        self.prefetch.cancel_next();
        if let Some(e) = self.engine.as_mut() {
            e.set_next(None);
        }
        self.prefetch.request(Request {
            id,
            kind: Kind::Manual,
            path,
            start,
            rate,
            channels,
        });
        let generation = self.generation;
        self.with_state(|s| {
            s.generation = generation;
            s.loading = true;
        });
    }

    fn stop(&mut self) {
        if let Some(e) = self.engine.as_mut() {
            e.stop();
        }
        self.flush();
        self.pending_play = None;
        self.want_next = None;
        self.next_requested = None;
        self.prefetch.cancel_next();
        self.shared.paused.store(false, Ordering::Relaxed);
        self.shared.pos.store(0f64.to_bits(), Ordering::Relaxed);
        let (generation, output) = (
            self.generation,
            self.state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .output
                .clone(),
        );
        self.with_state(|s| {
            *s = State {
                generation,
                output,
                ..Default::default()
            }
        });
        self.ended_reported = true;
        self.fading_shown = None;
        self.idle_since = Some(Instant::now());
    }

    fn seek(&mut self, position: f64) {
        let generation = self.generation;
        if self.pending_play.is_some() || !self.engine_active() {
            self.with_state(|s| s.generation = generation);
            return;
        }
        let result = self.engine.as_mut().expect("engine").seek(position);
        match result {
            Ok(()) => {
                self.announce_now();
                self.flush();
                self.shared
                    .pos
                    .store(position.max(0.0).to_bits(), Ordering::Relaxed);
                self.ended_reported = false;
            }
            Err(e) => self.emit(generation, Event::Notice(e)),
        }
        self.with_state(|s| s.generation = generation);
    }

    /// Asks for the hinted next track to be opened, if one is wanted and nothing is in the way.
    fn sync_next(&mut self) {
        if self.pending_play.is_some() || !self.engine_active() {
            return;
        }
        let Some((rate, channels)) = self.format() else {
            return;
        };
        match self.want_next.clone() {
            None => {
                self.engine.as_mut().expect("engine").set_next(None);
                self.prefetch.cancel_next();
                self.next_requested = None;
                self.with_state(|s| s.next_ready = false);
            }
            Some(path) => {
                if self.engine.as_ref().and_then(Engine::next_path).as_ref() == Some(&path)
                    || self
                        .next_requested
                        .as_ref()
                        .is_some_and(|(_, p)| *p == path)
                {
                    return;
                }
                self.engine.as_mut().expect("engine").set_next(None);
                self.next_id += 1;
                let id = self.next_id;
                self.next_requested = Some((id, path.clone()));
                self.prefetch.request(Request {
                    id,
                    kind: Kind::Next,
                    path,
                    start: 0.0,
                    rate,
                    channels,
                });
                self.with_state(|s| s.next_ready = false);
            }
        }
    }

    fn prepared(&mut self, p: Prepared) {
        match p.kind {
            Kind::Manual => {
                let Some(pending) = self.pending_play.take_if(|pp| pp.id == p.id) else {
                    return; // superseded
                };
                if self.format() != Some((p.rate, p.channels)) {
                    // The device changed while this was opening: open it again for the new format.
                    self.pending_play = Some(pending);
                    self.replay_pending();
                    return;
                }
                match p.result {
                    Ok(stream) => self.begin(stream, pending),
                    Err(e) => {
                        if let Some(engine) = self.engine.as_mut() {
                            engine.stop();
                        }
                        self.flush();
                        let generation = pending.generation;
                        self.with_state(|s| {
                            *s = State {
                                generation,
                                output: s.output.clone(),
                                ..Default::default()
                            }
                        });
                        self.emit(pending.generation, Event::Error(e));
                        self.idle_since = Some(Instant::now());
                    }
                }
            }
            Kind::Next => {
                if self.next_requested.as_ref().map(|(id, _)| *id) != Some(p.id) {
                    return;
                }
                self.next_requested = None;
                let wanted = self.want_next.as_ref() == Some(&p.path);
                match p.result {
                    Ok(stream) => {
                        if wanted && self.format() == Some((p.rate, p.channels)) {
                            if let Some(e) = self.engine.as_mut() {
                                e.set_next(Some(stream));
                            }
                            self.with_state(|s| s.next_ready = true);
                        }
                    }
                    Err(error) if wanted => {
                        let generation = self.generation;
                        self.emit(
                            generation,
                            Event::NextFailed {
                                path: p.path,
                                error,
                            },
                        );
                    }
                    Err(_) => {}
                }
            }
        }
    }

    fn replay_pending(&mut self) {
        let Some(p) = self.pending_play.as_ref() else {
            return;
        };
        let Some((rate, channels)) = self.format() else {
            return;
        };
        self.next_id += 1;
        let id = self.next_id;
        let (path, start) = (p.path.clone(), p.start);
        self.pending_play.as_mut().expect("pending").id = id;
        self.prefetch.request(Request {
            id,
            kind: Kind::Manual,
            path,
            start,
            rate,
            channels,
        });
    }

    /// The chosen track is open: start it, blending from the current one when that is audible.
    fn begin(&mut self, stream: Stream, pending: PendingPlay) {
        let tag = stream.tag.clone();
        let duration = stream.duration().unwrap_or(0.0);
        let audible = self.engine_active()
            && !self.paused()
            && self.shared.queued.load(Ordering::Relaxed) > 0;
        let how = if self.crossfade > 0.0 && audible && !pending.paused && !pending.resume {
            Start::Fade(f64::from(self.crossfade).min(MANUAL_FADE_SECONDS))
        } else {
            Start::Cut
        };
        let seq = self.engine.as_mut().expect("engine").start(stream, how);
        self.shared.paused.store(pending.paused, Ordering::Relaxed);
        self.idle_since = None;
        self.ended_reported = false;
        let generation = pending.generation;
        match how {
            Start::Cut => {
                self.flush();
                self.shared
                    .pos
                    .store(pending.start.to_bits(), Ordering::Relaxed);
                self.shared.audible_seq.store(seq, Ordering::Relaxed);
                self.publish(&tag, duration, !pending.paused, generation);
            }
            Start::Fade(_) => {
                self.announce.push_back(Announce {
                    seq,
                    tag: tag.clone(),
                    automatic: false,
                });
                self.with_state(|s| {
                    s.generation = generation;
                    s.loading = false;
                });
            }
        }
        if !pending.resume {
            let mut meta = tag.meta.clone();
            meta.duration = duration;
            self.emit(generation, Event::Loaded(meta));
        }
        self.sync_next();
    }

    fn publish(&self, tag: &Tag, duration: f64, playing: bool, generation: u64) {
        let (info, resampled, converted) =
            (tag.info.clone(), tag.resampled, tag.channel_conversion);
        self.with_state(|s| {
            s.generation = generation;
            s.playing = playing;
            s.loading = false;
            s.duration = duration;
            s.rate = info.rate;
            s.channels = info.channels;
            s.info = Some(info);
            s.resampled = resampled;
            s.channel_conversion = converted;
        });
    }

    // ---- housekeeping

    fn service(&mut self) {
        if self.device.is_some() {
            self.watch_device();
        }
        self.produce();
        self.announce_audible();
        self.check_ended();
        self.idle_close();
    }

    /// Publishes the crossfade progress, but only when it changes visibly.
    fn note_fading(&mut self, fading: Option<f64>) {
        let changed = match (self.fading_shown, fading) {
            (None, None) => false,
            (Some(a), Some(b)) => (a - b).abs() >= 0.25,
            _ => true,
        };
        if changed {
            self.fading_shown = fading;
            self.with_state(|s| s.fading = fading);
        }
    }

    fn watch_device(&mut self) {
        let lost = self
            .device
            .as_ref()
            .is_some_and(|d| d.lost.swap(false, Ordering::AcqRel));
        if lost {
            self.recover("The audio device was removed");
            return;
        }
        let playing = self.engine_active() && !self.paused();
        if !playing {
            self.heartbeat = (
                self.shared.heartbeat.load(Ordering::Relaxed),
                Instant::now(),
            );
            return;
        }
        let beats = self.shared.heartbeat.load(Ordering::Relaxed);
        if beats != self.heartbeat.0 {
            self.heartbeat = (beats, Instant::now());
        } else if self.heartbeat.1.elapsed() > HEARTBEAT_STALL {
            self.recover("The audio device stopped responding");
            return;
        }
        if self.last_poll.elapsed() >= DEVICE_POLL {
            self.last_poll = Instant::now();
            let current = self.device.as_ref().map(|d| d.info.name.clone());
            if let (Some(default), Some(current)) = (self.backend.default_device_name(), current) {
                if default != current && self.ignored_default.as_ref() != Some(&default) {
                    self.ignored_default = Some(default);
                    self.recover("Windows switched the default audio output");
                }
            }
        }
    }

    /// Reopens the output (possibly a different device and format) and carries on from the
    /// audible position.
    fn recover(&mut self, why: &str) {
        let resume = self
            .engine
            .as_ref()
            .and_then(Engine::primary_tag)
            .map(|tag| (tag.path.clone(), self.shared.position(), !self.paused()));
        self.announce_now();
        self.engine = None;
        self.device = None;
        self.held = None;
        self.announce.clear();
        self.next_requested = None;
        let generation = self.generation;
        match self.ensure_device() {
            Err(e) => {
                self.with_state(|s| s.playing = false);
                self.ended_reported = true;
                self.idle_since = Some(Instant::now());
                self.emit(generation, Event::Error(e));
            }
            Ok(()) => {
                let name = self
                    .device
                    .as_ref()
                    .map(|d| d.info.name.clone())
                    .unwrap_or_default();
                self.emit(
                    generation,
                    Event::Notice(format!("{why}. Playing on {name}.")),
                );
                if let Some((path, position, playing)) = resume {
                    let (rate, channels) = self.format().expect("device");
                    self.next_id += 1;
                    let id = self.next_id;
                    self.pending_play = Some(PendingPlay {
                        id,
                        path: path.clone(),
                        start: position,
                        paused: !playing,
                        generation,
                        resume: true,
                    });
                    self.prefetch.request(Request {
                        id,
                        kind: Kind::Manual,
                        path,
                        start: position,
                        rate,
                        channels,
                    });
                }
            }
        }
    }

    /// Mixes more audio while the device queue has room.
    fn produce(&mut self) {
        let Some(device) = self.device.as_ref() else {
            return;
        };
        let rate = device.info.rate;
        let frames = chunk_frames(rate);
        let room = (queue_chunks(rate, device.info.channels) * frames) as i64;
        loop {
            if let Some(chunk) = self.held.take() {
                if let Err(c) = self.send(chunk) {
                    self.held = Some(c);
                    return;
                }
            }
            if self.shared.queued.load(Ordering::Relaxed) + frames as i64 > room {
                return;
            }
            if !self.engine_active() {
                return;
            }
            let mut data = self
                .device
                .as_ref()
                .and_then(|d| d.recycle.try_recv().ok())
                .unwrap_or_default();
            data.clear();
            let rendered = self
                .engine
                .as_mut()
                .expect("engine")
                .render(frames, &mut data);
            self.note_fading(rendered.fading);
            if let Some((seq, tag)) = rendered.promoted.clone() {
                self.announce.push_back(Announce {
                    seq,
                    tag,
                    automatic: true,
                });
            }
            let finished = rendered.ended || rendered.error.is_some();
            if !data.is_empty() || rendered.ended {
                let chunk = Chunk {
                    epoch: self.epoch,
                    seq: rendered.seq,
                    start_pos: rendered.start_pos,
                    last: rendered.ended,
                    data,
                };
                if let Err(c) = self.send(chunk) {
                    self.held = Some(c);
                }
            }
            if let Some((tag, error)) = rendered.error {
                self.fail_current(&tag, error);
                return;
            }
            if finished {
                self.idle_since = Some(Instant::now());
                return;
            }
            if rendered.frames == 0 && rendered.promoted.is_none() {
                return;
            }
            if self.held.is_some() {
                return;
            }
        }
    }

    fn send(&mut self, chunk: Chunk) -> Result<(), Chunk> {
        let Some(device) = self.device.as_ref() else {
            return Ok(());
        };
        let frames = frames_counted(chunk.data.len(), device.info.channels);
        match device.tx.try_send(chunk) {
            Ok(()) => {
                self.shared.queued.fetch_add(frames, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(c)) => Err(c),
            Err(TrySendError::Disconnected(_)) => Ok(()),
        }
    }

    /// The playing track failed mid-stream. Audio already queued plays out, then the UI moves on.
    fn fail_current(&mut self, tag: &Arc<Tag>, error: String) {
        let generation = self.generation;
        self.with_state(|s| s.playing = false);
        self.ended_reported = true;
        self.idle_since = Some(Instant::now());
        let name = tag
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        self.emit(generation, Event::Error(format!("{name}: {error}")));
    }

    /// Switches the UI to a track the moment it becomes audible, not when it was mixed.
    fn announce_audible(&mut self) {
        self.announce_up_to(self.shared.audible_seq.load(Ordering::Relaxed));
    }

    /// Publishes every pending track change now. Used when an action (seek, device change)
    /// would otherwise flush the announcements of a track the engine already switched to.
    fn announce_now(&mut self) {
        self.announce_up_to(u64::MAX);
    }

    fn announce_up_to(&mut self, heard: u64) {
        while self.announce.front().is_some_and(|a| a.seq <= heard) {
            let a = self.announce.pop_front().expect("announced");
            let duration = a.tag.meta.duration.max(a.tag.info.duration);
            let playing = !self.paused();
            let generation = self.generation;
            self.publish(&a.tag, duration, playing, generation);
            if a.automatic {
                let mut meta = a.tag.meta.clone();
                meta.duration = duration;
                self.want_next = None;
                self.with_state(|s| s.next_ready = false);
                self.emit(
                    generation,
                    Event::Advanced {
                        path: a.tag.path.clone(),
                        meta,
                    },
                );
            }
        }
    }

    fn check_ended(&mut self) {
        if self.ended_reported {
            return;
        }

        if self.shared.ended_epoch.load(Ordering::Acquire) == u64::from(self.epoch) + 1 {
            self.ended_reported = true;
            self.shared.ended_epoch.store(0, Ordering::Relaxed);
            self.idle_since = Some(Instant::now());
            let generation = self.generation;
            self.with_state(|s| s.playing = false);
            self.emit(generation, Event::Ended);
        }
    }

    fn idle_close(&mut self) {
        if self.engine_active() || self.pending_play.is_some() {
            self.idle_since = None;
            return;
        }
        if self.shared.queued.load(Ordering::Relaxed) > 0 || !self.ended_reported {
            return;
        }
        if self
            .idle_since
            .is_some_and(|t| t.elapsed() >= self.idle_close)
        {
            self.device = None;
            self.engine = None;
            self.idle_since = None;
            self.shared.queued.store(0, Ordering::Relaxed);
        }
    }
}

/// Self-ending device smoke: plays real files through the real Windows output at low volume,
/// with the configured crossfade, and reports what the engine and device did.
pub fn playback_smoke(
    paths: &[PathBuf],
    crossfade: f32,
    volume: f32,
) -> Result<serde_json::Value, String> {
    if paths.is_empty() {
        return Err("Give at least one audio file".into());
    }
    let ctx = egui::Context::default();
    let player = Player::new(ctx);
    player.send(Command::Volume(volume))?;
    player.send(Command::SetCrossfade(crossfade))?;
    let started = Instant::now();
    let mut index = 0;
    let mut timeline = Vec::new();
    player.send(Command::Play(paths[0].clone(), 0.0, false))?;
    player.send(Command::SetNext(paths.get(1).cloned()))?;
    let deadline = started + Duration::from_secs(120);
    let mut finished = false;
    while Instant::now() < deadline && !finished {
        std::thread::sleep(Duration::from_millis(10));
        for event in player.events() {
            let at = started.elapsed().as_secs_f64();
            match event {
                Event::Loaded(m) => timeline
                    .push(serde_json::json!({"at": at, "event": "loaded", "title": m.title})),
                Event::Advanced { path, .. } => {
                    index += 1;
                    timeline.push(serde_json::json!({"at": at, "event": "advanced", "path": path}));
                    player.send(Command::SetNext(paths.get(index + 1).cloned()))?;
                }
                Event::Ended => {
                    timeline.push(serde_json::json!({"at": at, "event": "ended"}));
                    finished = true;
                }
                Event::Error(e) => return Err(e),
                Event::NextFailed { error, .. } => return Err(error),
                Event::Notice(n) => {
                    timeline.push(serde_json::json!({"at": at, "event": "notice", "text": n}))
                }
            }
        }
    }
    if !finished {
        return Err("Playback did not finish within two minutes".into());
    }
    let state = player.state();
    Ok(serde_json::json!({
        "device_output": "completed",
        "audible_quality": "unverified",
        "tracks": paths.len(),
        "crossfade_seconds": crossfade,
        "wall_seconds": started.elapsed().as_secs_f64(),
        "underruns": player.underruns(),
        "process": crate::smoke::process_stats(),
        "device": state.output.as_deref().map(|d| serde_json::json!({
            "name": d.name, "rate": d.rate, "channels": d.channels, "format": d.format,
        })),
        "timeline": timeline,
    }))
}

/// Single-file device smoke kept from the first release: plays one file to the end.
pub fn device_smoke(path: PathBuf) -> Result<(), String> {
    let report = playback_smoke(&[path], 0.0, 0.15)?;
    if report["wall_seconds"].as_f64().is_some_and(|s| s > 15.0) {
        return Err("Device smoke is limited to files below 15 seconds".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
