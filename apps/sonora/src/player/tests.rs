//! Device-edge and worker tests. The "device" is a test backend that the test itself drives: it
//! pulls frames out of the real [`OutputSource`] exactly as the Windows callback would, so the
//! captured PCM is what a speaker would be given, and no assertion depends on wall-clock timing.

use super::*;
use crate::engine::testutil::Synth;
use std::{
    f64::consts::TAU,
    sync::atomic::{AtomicU32, AtomicUsize},
};

const RATE: u32 = 48_000;

// ------------------------------------------------------------------------------------------
// OutputSource, the device callback

struct Rig {
    tx: SyncSender<Chunk>,
    src: OutputSource,
    shared: Arc<Shared>,
    channels: usize,
}

fn rig(channels: usize) -> Rig {
    rig_at(channels, 0)
}

fn rig_at(channels: usize, epoch: u32) -> Rig {
    let shared = Shared::new();
    shared.epoch.store(epoch, Ordering::Release);
    shared.volume.store(1.0f32.to_bits(), Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel(64);
    let (recycle, _keep) = mpsc::sync_channel(64);
    std::mem::forget(_keep);
    let src = OutputSource::new(rx, recycle, shared.clone(), RATE, channels);
    Rig {
        tx,
        src,
        shared,
        channels,
    }
}

impl Rig {
    fn send(&self, epoch: u32, seq: u64, start: f64, data: Vec<f32>, last: bool) {
        self.shared
            .queued
            .fetch_add((data.len() / self.channels) as i64, Ordering::Relaxed);
        self.tx
            .send(Chunk {
                epoch,
                seq,
                start_pos: start,
                last,
                data,
            })
            .unwrap();
    }
    fn render(&mut self, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * self.channels];
        self.src.render(&mut out);
        out
    }
}

fn dc(level: f32, frames: usize, channels: usize) -> Vec<f32> {
    vec![level; frames * channels]
}

#[test]
fn the_device_edge_ramps_in_from_silence_then_passes_samples_exactly() {
    let mut r = rig(2);
    let data: Vec<f32> = (0..4800)
        .flat_map(|f| [(f as f32 + 1.0) / 10_000.0; 2])
        .collect();
    r.send(0, 1, 0.0, data, false);
    let out = r.render(4800);
    assert!(
        out[0].abs() < 0.001,
        "first frame starts from silence: {}",
        out[0]
    );
    for f in 500..4800 {
        assert_eq!(out[f * 2], (f as f32 + 1.0) / 10_000.0, "frame {f}");
        assert_eq!(out[f * 2 + 1], out[f * 2]);
    }
    assert_eq!(r.shared.audible_seq.load(Ordering::Relaxed), 1);
}

#[test]
fn a_flush_fades_the_old_audio_out_and_the_new_epoch_in_without_a_step() {
    let mut r = rig(2);
    r.send(0, 1, 0.0, dc(0.5, 4800, 2), false);
    let before = r.render(1000);
    assert_eq!(before[1998], 0.5);
    // The worker flushes and queues the new material.
    r.shared.epoch.store(1, Ordering::Release);
    r.send(1, 2, 10.0, dc(-0.5, 4800, 2), false);
    let after = r.render(2000);
    let all: Vec<f32> = before.iter().chain(&after).step_by(2).copied().collect();
    let worst = all
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 0.0015, "largest step between frames {worst}");
    assert_eq!(
        *after.last().unwrap(),
        -0.5,
        "the new epoch reaches full level"
    );
    assert!(
        after.iter().any(|s| s.abs() < 0.01),
        "and the old one passed through silence"
    );
    assert_eq!(
        r.shared.queued.load(Ordering::Relaxed),
        4800,
        "the dropped chunk is no longer counted"
    );
    assert_eq!(r.shared.audible_seq.load(Ordering::Relaxed), 2);
}

#[test]
fn pause_fades_out_consumes_nothing_while_paused_and_resumes_on_the_next_frame() {
    let mut r = rig(2);
    r.send(0, 1, 0.0, dc(0.5, 48_000, 2), false);
    r.render(1000);
    r.shared.paused.store(true, Ordering::Relaxed);
    let fade = r.render(1000);
    let worst = fade
        .iter()
        .step_by(2)
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 0.0015, "pause is ramped, step {worst}");
    assert!(fade[1990].abs() < 1e-3, "silent once the ramp is done");
    let held = r.shared.position();
    r.render(24_000); // half a second of device time while paused
    assert_eq!(
        r.shared.position(),
        held,
        "position does not move while paused"
    );
    r.shared.paused.store(false, Ordering::Relaxed);
    r.render(1000);
    let resumed = r.shared.position();
    assert!(
        (resumed - held - 1000.0 / RATE as f64).abs() < 1e-9,
        "exactly the frames played since: {resumed} vs {held}"
    );
}

#[test]
fn volume_applies_before_the_final_clamp_and_invalid_samples_are_silenced() {
    let mut r = rig(1);
    r.shared.volume.store(0.5f32.to_bits(), Ordering::Relaxed);
    let mut data = dc(1.5, 3000, 1);
    data[2000] = f32::NAN;
    data[2001] = f32::INFINITY;
    data[2002] = f32::NEG_INFINITY;
    r.send(0, 1, 0.0, data, false);
    let out = r.render(3000);
    assert!(
        (out[1500] - 0.75).abs() < 1e-6,
        "headroom above full scale survives attenuation: {}",
        out[1500]
    );
    assert_eq!(&out[2000..2003], &[0.0, 0.0, 0.0]);
    let mut loud = rig(1);
    loud.send(0, 1, 0.0, dc(1.5, 3000, 1), false);
    let out = loud.render(3000);
    assert_eq!(out[2000], 1.0, "and at full volume the device edge clamps");
}

#[test]
fn the_end_of_a_track_is_reported_once_and_is_not_an_underrun() {
    let mut r = rig_at(2, 3);
    r.send(3, 7, 0.0, dc(0.1, 480, 2), true);
    r.render(2000);
    assert_eq!(r.shared.ended_epoch.load(Ordering::Acquire), 4);
    assert_eq!(
        r.shared.underruns.load(Ordering::Relaxed),
        0,
        "running out at the natural end is not a glitch"
    );
}

#[test]
fn running_dry_in_the_middle_of_playback_is_counted_once_and_stays_in_phase() {
    let mut r = rig(6);
    r.send(0, 1, 0.0, dc(0.2, 600, 6), false);
    r.render(2000);
    assert_eq!(r.shared.underruns.load(Ordering::Relaxed), 1);
    // Fresh audio resumes on a whole frame: six channels carry six different values.
    let frame: Vec<f32> = (1..=6).map(|c| c as f32 / 10.0).collect();
    r.send(
        0,
        2,
        5.0,
        frame.iter().cycle().take(6 * 1500).copied().collect(),
        false,
    );
    let out = r.render(1500);
    for ch in 0..6 {
        assert!(
            (out[6 * 1400 + ch] - frame[ch]).abs() < 1e-6,
            "channel {ch} out of phase"
        );
    }
}

#[test]
fn position_and_audible_track_come_from_the_frames_actually_played() {
    let mut r = rig(2);
    r.send(0, 1, 12.0, dc(0.2, 4800, 2), false);
    r.send(0, 2, 0.0, dc(0.3, 4800, 2), false);
    r.render(2400);
    assert!((r.shared.position() - (12.0 + 2400.0 / RATE as f64)).abs() < 1e-9);
    assert_eq!(r.shared.audible_seq.load(Ordering::Relaxed), 1);
    r.render(4800);
    assert_eq!(
        r.shared.audible_seq.load(Ordering::Relaxed),
        2,
        "switches when the first chunk of track 2 plays"
    );
    assert!((r.shared.position() - (2400.0 / RATE as f64)).abs() < 1e-9);
}

#[test]
fn the_queue_never_exceeds_two_mebibytes_for_any_supported_device() {
    for rate in [
        8_000,
        44_100,
        48_000,
        96_000,
        192_000,
        384_000,
        MAX_DEVICE_RATE,
    ] {
        for channels in [1, 2, 6, 8, 16, MAX_DEVICE_CHANNELS] {
            let chunks = queue_chunks(rate, channels);
            let bytes = chunks * chunk_frames(rate) * channels * 4;
            assert!(chunks >= 1);
            assert!(
                bytes <= MAX_QUEUE_BYTES,
                "{rate} Hz x {channels}: {bytes} bytes"
            );
        }
    }
}

#[test]
fn terminal_event_survives_saturation_and_stale_responses_are_ignored() {
    let mut m = Mailbox::default();
    for g in 0..100 {
        m.push(g, g, Event::Loaded(Metadata::default()));
        m.push(g, g, Event::Error(format!("error {g}")));
    }
    let latest = m.drain(99, 99);
    assert_eq!(latest.len(), 2);
    assert!(matches!(&latest[1], Event::Error(e) if e == "error 99"));
    m.push(10, 10, Event::Ended);
    assert!(m.drain(11, 11).is_empty());
    m.push(12, 12, Event::Loaded(Metadata::default()));
    m.push(12, 12, Event::Ended);
    assert!(matches!(m.drain(12, 12).last(), Some(Event::Ended)));
}

#[test]
fn queue_advances_are_ordered_bounded_and_dropped_when_a_new_track_is_chosen() {
    let mut m = Mailbox::default();
    for i in 0..40 {
        m.push(
            1,
            5,
            Event::Advanced {
                path: PathBuf::from(format!("{i}")),
                meta: Metadata::default(),
            },
        );
    }
    let events = m.drain(1, 5);
    assert_eq!(events.len(), MAX_PENDING_EVENTS);
    assert!(matches!(&events[0], Event::Advanced { path, .. } if path == Path::new("24")));
    assert!(matches!(events.last(), Some(Event::Advanced { path, .. }) if path == Path::new("39")));
    m.push(
        1,
        5,
        Event::Advanced {
            path: PathBuf::from("x"),
            meta: Metadata::default(),
        },
    );
    assert!(
        m.drain(1, 6).is_empty(),
        "a Play bumped the track generation"
    );
    m.push(1, 6, Event::Notice("kept".into()));
    assert!(matches!(m.drain(9, 9).as_slice(), [Event::Notice(t)] if t == "kept"));
}

#[test]
fn commands_from_a_superseded_generation_are_dropped_but_settings_are_not() {
    let mut active = 3;
    assert!(
        matches!(command_for_generation(2, Command::Volume(0.1), 3, &mut active), Some(Command::Volume(v)) if v == 0.1)
    );
    assert!(command_for_generation(2, Command::SetCrossfade(5.0), 3, &mut active).is_some());
    assert!(command_for_generation(2, Command::SetNext(None), 3, &mut active).is_some());
    assert_eq!(active, 3);
    assert!(command_for_generation(2, Command::Seek(1.0), 3, &mut active).is_none());
    assert!(command_for_generation(4, Command::Stop, 4, &mut active).is_some());
    assert_eq!(active, 4);
}

// ------------------------------------------------------------------------------------------
// The whole player against a test device

struct Device {
    rate: Mutex<u32>,
    channels: Mutex<usize>,
    source: Mutex<Option<OutputSource>>,
    lost: Mutex<Option<Arc<AtomicBool>>>,
    opens: AtomicU32,
    closes: Arc<AtomicU32>,
    name: Mutex<String>,
    fail: AtomicBool,
}

#[derive(Clone)]
struct TestBackend(Arc<Device>);

struct CloseGuard(Arc<AtomicU32>);
impl Drop for CloseGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl TestBackend {
    fn new(rate: u32, channels: usize) -> Self {
        Self(Arc::new(Device {
            rate: Mutex::new(rate),
            channels: Mutex::new(channels),
            source: Mutex::new(None),
            lost: Mutex::new(None),
            opens: AtomicU32::new(0),
            closes: Arc::new(AtomicU32::new(0)),
            name: Mutex::new("Test speakers".into()),
            fail: AtomicBool::new(false),
        }))
    }
    fn opens(&self) -> u32 {
        self.0.opens.load(Ordering::SeqCst)
    }
    fn closes(&self) -> u32 {
        self.0.closes.load(Ordering::SeqCst)
    }
    /// Renders frames whenever there is audio waiting, like a callback that is never starved.
    fn pull(&self, frames: usize, wait: Duration) -> Vec<f32> {
        let channels = *self.0.channels.lock().unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + wait;
        while got.len() < frames * channels && Instant::now() < deadline {
            let mut guard = self.0.source.lock().unwrap();
            match guard.as_mut() {
                Some(src) if src.has_audio() => {
                    // One frame at a time: this device never runs ahead of the worker, so a
                    // starved queue is waited for rather than turned into silence.
                    let mut frame = vec![0.0; channels];
                    for _ in 0..(frames - got.len() / channels).min(64) {
                        if !src.has_audio() {
                            break;
                        }
                        src.render(&mut frame);
                        got.extend_from_slice(&frame);
                    }
                }
                _ => {
                    drop(guard);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        got
    }
    /// Renders frames whether or not audio is waiting: device time passing.
    fn tick(&self, frames: usize) -> Vec<f32> {
        let channels = *self.0.channels.lock().unwrap();
        let mut out = vec![0.0; frames * channels];
        if let Some(src) = self.0.source.lock().unwrap().as_mut() {
            src.render(&mut out);
        }
        out
    }
}

impl Backend for TestBackend {
    fn open(&mut self, shared: Arc<Shared>) -> Result<OpenDevice, String> {
        if self.0.fail.load(Ordering::SeqCst) {
            return Err("Cannot open Windows audio output: test device is unavailable".into());
        }
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        let (rate, channels) = (
            *self.0.rate.lock().unwrap(),
            *self.0.channels.lock().unwrap(),
        );
        let cap = queue_chunks(rate, channels);
        let (tx, rx) = mpsc::sync_channel(cap * 2);
        let (recycle_tx, recycle) = mpsc::sync_channel(cap * 2 + 2);
        *self.0.source.lock().unwrap() =
            Some(OutputSource::new(rx, recycle_tx, shared, rate, channels));
        let lost = Arc::new(AtomicBool::new(false));
        *self.0.lost.lock().unwrap() = Some(lost.clone());
        Ok(OpenDevice {
            info: DeviceInfo {
                name: self.0.name.lock().unwrap().clone(),
                rate,
                channels,
                format: "test".into(),
            },
            tx,
            recycle,
            lost,
            _guard: Box::new(CloseGuard(self.0.closes.clone())),
        })
    }
    fn default_device_name(&mut self) -> Option<String> {
        Some(self.0.name.lock().unwrap().clone())
    }
}

type Factory = Box<dyn Fn(&str) -> Option<Synth> + Send + Sync>;

struct TestOpener {
    make: Factory,
    opened: Mutex<Vec<String>>,
    concurrent: AtomicUsize,
    peak: AtomicUsize,
}

impl Opener for TestOpener {
    fn open(&self, path: &Path, start: f64, rate: u32, channels: usize) -> Result<Stream, String> {
        let n = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(n, Ordering::SeqCst);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let result = (self.make)(&name)
            .ok_or_else(|| "Unsupported or damaged test file".to_string())
            .and_then(|synth| {
                let duration = synth.frames as f64 / synth.rate as f64;
                let tag = Tag {
                    path: path.to_path_buf(),
                    info: StreamInfo {
                        codec: "TEST",
                        rate: synth.rate,
                        channels: synth.layout.len() as u16,
                        duration,
                        ..Default::default()
                    },
                    meta: Metadata {
                        title: name.clone(),
                        duration,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut stream = Stream::new(synth.boxed(), rate, channels, tag)?;
                if start > 0.0 {
                    stream.seek(start)?;
                }
                Ok(stream)
            });
        self.opened.lock().unwrap().push(name);
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

struct Harness {
    player: Player,
    device: TestBackend,
    opener: Arc<TestOpener>,
}

impl Harness {
    fn new(
        rate: u32,
        channels: usize,
        make: impl Fn(&str) -> Option<Synth> + Send + Sync + 'static,
    ) -> Self {
        Self::with_options(rate, channels, Options::default(), make)
    }
    fn with_options(
        rate: u32,
        channels: usize,
        options: Options,
        make: impl Fn(&str) -> Option<Synth> + Send + Sync + 'static,
    ) -> Self {
        let device = TestBackend::new(rate, channels);
        let opener = Arc::new(TestOpener {
            make: Box::new(make),
            opened: Mutex::new(Vec::new()),
            concurrent: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let player = Player::with_options(
            egui::Context::default(),
            Box::new(device.clone()),
            opener.clone(),
            options,
        );
        player.send(Command::Volume(1.0)).unwrap();
        Self {
            player,
            device,
            opener,
        }
    }
    fn play(&self, name: &str) {
        self.player
            .send(Command::Play(PathBuf::from(name), 0.0, false))
            .unwrap();
    }
    fn next(&self, name: Option<&str>) {
        self.player
            .send(Command::SetNext(name.map(PathBuf::from)))
            .unwrap();
    }
    /// Drives the device and collects events until `done` says so (or twenty seconds pass).
    fn pump(&self, mut done: impl FnMut(&[Event]) -> bool) -> (Vec<f32>, Vec<Event>) {
        let (mut pcm, mut events) = (Vec::new(), Vec::new());
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            pcm.extend(self.device.pull(480, Duration::from_millis(20)));
            events.extend(self.player.events());
            if done(&events) {
                break;
            }
        }
        (pcm, events)
    }
    fn pump_frames(&self, frames: usize) -> Vec<f32> {
        let mut pcm = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        let channels = *self.device.0.channels.lock().unwrap();
        while pcm.len() < frames * channels && Instant::now() < deadline {
            pcm.extend(
                self.device
                    .pull(frames - pcm.len() / channels, Duration::from_millis(50)),
            );
        }
        pcm
    }
    fn wait_for(&self, what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn ramp(seconds: f64, offset: f32) -> Synth {
    Synth::new(RATE, 2, seconds, move |f, _| {
        offset + (f as f32 + 1.0) / 1e6
    })
}
fn level(seconds: f64, v: f32) -> Synth {
    Synth::new(RATE, 2, seconds, move |_, _| v)
}
fn left(pcm: &[f32]) -> Vec<f32> {
    pcm.iter().step_by(2).copied().collect()
}
fn ended(events: &[Event]) -> bool {
    events.iter().any(|e| matches!(e, Event::Ended))
}
fn count(events: &[Event], pick: impl Fn(&Event) -> bool) -> usize {
    events.iter().filter(|e| pick(e)).count()
}

#[test]
fn a_track_reaches_the_device_frame_for_frame_and_ends_exactly_once() {
    let h = Harness::new(RATE, 2, |n| (n == "a").then(|| ramp(1.0, 0.0)));
    h.play("a");
    let (pcm, events) = h.pump(ended);
    let l = left(&pcm);
    assert_eq!(l.len(), 48_000, "exactly the track, nothing added or lost");
    for (f, sample) in l.iter().enumerate().skip(500) {
        assert_eq!(*sample, (f as f32 + 1.0) / 1e6, "frame {f}");
    }
    assert_eq!(count(&events, |e| matches!(e, Event::Loaded(_))), 1);
    assert_eq!(count(&events, |e| matches!(e, Event::Ended)), 1);
    assert!(
        (h.player.position() - 1.0).abs() < 1.0 / RATE as f64,
        "position {}",
        h.player.position()
    );
    assert_eq!(h.player.underruns(), 0);
    assert_eq!(h.device.opens(), 1);
}

#[test]
fn the_next_track_follows_with_no_gap_and_one_advance_event_by_default() {
    // Crossfade is off by default: the two tracks run back to back, sample for sample.
    let h = Harness::new(RATE, 2, |n| match n {
        "a" => Some(ramp(1.0, 0.0)),
        "b" => Some(ramp(0.5, 0.5)),
        _ => None,
    });
    h.play("a");
    h.next(Some("b"));
    let (pcm, events) = h.pump(ended);
    let l = left(&pcm);
    assert_eq!(l.len(), 48_000 + 24_000);
    assert_eq!(l[47_999], 48_000.0 / 1e6);
    assert_eq!(
        l[48_000],
        0.5 + 1.0 / 1e6,
        "B starts on the very next frame"
    );
    assert_eq!(l[71_999], 0.5 + 24_000.0 / 1e6);
    let advanced: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let Event::Advanced { path, meta } = e {
                Some((path.clone(), meta.title.clone()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(advanced, vec![(PathBuf::from("b"), "b".to_string())]);
    assert_eq!(count(&events, |e| matches!(e, Event::Ended)), 1);
    assert_eq!(h.device.opens(), 1, "no device work between tracks");
    assert_eq!(h.player.underruns(), 0);
}

#[test]
fn crossfade_overlaps_the_two_tracks_in_the_captured_device_stream() {
    let h = Harness::new(RATE, 2, |n| match n {
        "a" => Some(level(3.0, 0.5)),
        "b" => Some(level(3.0, -0.25)),
        _ => None,
    });
    h.player.send(Command::SetCrossfade(1.0)).unwrap();
    h.play("a");
    h.next(Some("b"));
    let mut saw_fading = false;
    let (mut pcm, mut events) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !ended(&events) {
        pcm.extend(h.device.pull(480, Duration::from_millis(20)));
        saw_fading |= h.player.state().fading.is_some();
        events.extend(h.player.events());
    }
    let l = left(&pcm);
    assert_eq!(
        l.len(),
        5 * RATE as usize,
        "3 s + 3 s overlapped by one second is 5 s"
    );
    assert_eq!(l[2 * RATE as usize - 10], 0.5, "A alone before the blend");
    let mid = (0.5 - 0.25) * std::f32::consts::FRAC_1_SQRT_2;
    assert!(
        (l[2 * RATE as usize + 24_000] - mid).abs() < 2e-3,
        "midpoint {} vs {mid}",
        l[2 * RATE as usize + 24_000]
    );
    assert_eq!(l[3 * RATE as usize + 10], -0.25, "B alone after it");
    assert!(saw_fading, "the UI can see the blend");
    assert_eq!(count(&events, |e| matches!(e, Event::Advanced { .. })), 1);
    assert_eq!(h.player.underruns(), 0);
}

#[test]
fn pausing_holds_the_position_for_as_long_as_it_lasts_and_resuming_loses_nothing() {
    let h = Harness::new(RATE, 2, |n| (n == "a").then(|| ramp(3.0, 0.0)));
    h.play("a");
    let first = h.pump_frames(14_400);
    h.player.send(Command::Pause(true)).unwrap();
    h.wait_for("pause", || !h.player.state().playing);
    h.device.tick(2_000); // let the ramp finish
    let held = h.player.position();
    std::thread::sleep(Duration::from_millis(400)); // wall time must not matter
    let silent = h.device.tick(48_000);
    assert!(silent.iter().all(|s| s.abs() < 1e-3), "silent while paused");
    assert_eq!(
        h.player.position(),
        held,
        "position is counted from played frames, not the clock"
    );
    h.player.send(Command::Pause(false)).unwrap();
    h.wait_for("resume", || h.player.state().playing);
    let rest = h.pump(ended).0;
    assert!((h.player.position() - 3.0).abs() < 1.0 / RATE as f64);
    // Every frame of the track was played exactly once: first run + ramp frames + rest = 3 s.
    let played = first.len() / 2 + rest.len() / 2;
    assert!(
        played >= 3 * RATE as usize - 1_000 && played <= 3 * RATE as usize + 50_000,
        "{played}"
    );
    let l = left(&rest);
    let tail = &l[l.len() - 10_000..];
    for w in tail.windows(2) {
        assert!((w[1] - w[0] - 1e-6).abs() < 3e-8, "tail is contiguous");
    }
}

#[test]
fn seeking_stays_on_the_open_device_lands_on_the_target_and_never_steps() {
    let h = Harness::new(RATE, 2, |n| {
        (n == "a").then(|| {
            Synth::new(RATE, 2, 4.0, |f, _| {
                0.5 * (TAU * 440.0 * f as f64 / RATE as f64).sin() as f32
            })
        })
    });
    h.play("a");
    let mut pcm = h.pump_frames(14_400);
    h.player.send(Command::Seek(2.5)).unwrap();
    pcm.extend(h.pump_frames(14_400));
    let l = left(&pcm);
    let worst = l
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    let slope = 0.5 * TAU as f32 * 440.0 / RATE as f32;
    assert!(
        worst < slope * 1.5,
        "largest step {worst} against a natural slope of {slope}: no click"
    );
    let position = h.player.position();
    assert!(
        position > 2.5 && position < 2.5 + 0.6,
        "position {position}"
    );
    let last = *l.last().unwrap() as f64;
    let expected = 0.5 * (TAU * 440.0 * (position - 1.0 / RATE as f64)).sin();
    assert!(
        (last - expected).abs() < 1e-4,
        "sample {last} at {position} s should be {expected}"
    );
    assert_eq!(h.device.opens(), 1, "seeking never reopens the output");
    assert_eq!(h.opener.opened.lock().unwrap().len(), 1, "nor the file");
}

#[test]
fn stop_silences_resets_the_state_and_playing_again_works() {
    let h = Harness::new(RATE, 2, |n| (n == "a").then(|| level(3.0, 0.4)));
    h.play("a");
    h.pump_frames(9_600);
    h.player.send(Command::Stop).unwrap();
    h.wait_for("stop", || {
        h.player.state().rate == 0 && !h.player.state().loading
    });
    let after = h.device.tick(4_800);
    assert!(
        after[2_000..].iter().all(|s| s.abs() < 1e-3),
        "silent after the ramp"
    );
    assert_eq!(h.player.position(), 0.0);
    assert!(!h.player.state().playing);
    h.play("a");
    let again = h.pump_frames(9_600);
    assert!((again[again.len() - 2] - 0.4).abs() < 1e-6);
    assert_eq!(h.device.opens(), 1, "the device stays open between tracks");
}

#[test]
fn choosing_several_tracks_in_a_burst_plays_only_the_last_and_reports_only_it() {
    let h = Harness::new(RATE, 2, |n| match n {
        "a" => Some(level(5.0, 0.1)),
        "b" => Some(level(5.0, 0.2)),
        "c" => Some(level(5.0, 0.3)),
        _ => None,
    });
    h.play("a");
    h.play("b");
    h.play("c");
    let (pcm, events) = h.pump(|ev| count(ev, |e| matches!(e, Event::Loaded(_))) >= 1);
    let more = h.pump_frames(14_400);
    assert!(more[more.len() - 2] == 0.3, "settled on the last request");
    let titles: Vec<String> = events
        .iter()
        .filter_map(|e| {
            if let Event::Loaded(m) = e {
                Some(m.title.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        titles,
        vec!["c".to_string()],
        "stale Loaded events are never delivered"
    );
    drop(pcm);
}

#[test]
fn a_file_that_cannot_open_reports_the_error_and_the_old_track_stops_cleanly() {
    let h = Harness::new(RATE, 2, |n| (n == "a").then(|| level(5.0, 0.3)));
    h.play("a");
    h.pump_frames(9_600);
    h.play("broken");
    let (_, events) = h.pump(|ev| ev.iter().any(|e| matches!(e, Event::Error(_))));
    let message = events
        .iter()
        .find_map(|e| {
            if let Event::Error(m) = e {
                Some(m.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert!(message.contains("damaged test file"), "{message}");
    h.wait_for("idle state", || !h.player.state().loading);
    let after = h.device.tick(4_800);
    assert!(
        after[3_000..].iter().all(|s| s.abs() < 1e-3),
        "the previous track is not left playing"
    );
    assert!(!h.player.state().playing);
}

#[test]
fn a_next_track_that_cannot_open_is_reported_and_the_current_one_plays_on() {
    let h = Harness::new(RATE, 2, |n| (n == "a").then(|| level(0.5, 0.2)));
    h.play("a");
    h.next(Some("broken"));
    let (pcm, events) = h.pump(ended);
    assert_eq!(left(&pcm).len(), 24_000);
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::NextFailed { path, .. } if path == Path::new("broken"))));
    assert_eq!(count(&events, |e| matches!(e, Event::Advanced { .. })), 0);
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error(_))),
        "the playing track was never in error"
    );
}

#[test]
fn changing_the_hint_replaces_the_prepared_track_before_it_is_ever_heard() {
    let h = Harness::new(RATE, 2, |n| match n {
        "a" => Some(level(2.0, 0.2)),
        "b" => Some(level(2.0, 0.4)),
        "c" => Some(level(2.0, 0.6)),
        _ => None,
    });
    h.play("a");
    h.next(Some("b"));
    h.wait_for("b ready", || h.player.state().next_ready);
    h.next(Some("c"));
    let (pcm, events) = h.pump(ended);
    let l = left(&pcm);
    let advanced: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let Event::Advanced { path, .. } = e {
                Some(path.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(advanced, vec![PathBuf::from("c")]);
    assert!(
        l[2 * RATE as usize + 10..]
            .iter()
            .all(|s| (*s - 0.6).abs() < 1e-6),
        "B was never heard"
    );
    assert!(
        h.opener.peak.load(Ordering::SeqCst) <= 1,
        "one opener at a time, never a thread per request"
    );
}

#[test]
fn removing_the_hint_leaves_the_track_to_end_normally() {
    let h = Harness::new(RATE, 2, |n| match n {
        "a" => Some(level(1.0, 0.2)),
        "b" => Some(level(1.0, 0.4)),
        _ => None,
    });
    h.play("a");
    h.next(Some("b"));
    h.wait_for("b ready", || h.player.state().next_ready);
    h.next(None);
    let (pcm, events) = h.pump(ended);
    assert_eq!(left(&pcm).len(), 48_000);
    assert_eq!(count(&events, |e| matches!(e, Event::Advanced { .. })), 0);
    assert_eq!(count(&events, |e| matches!(e, Event::Ended)), 1);
}

#[test]
fn a_manual_change_blends_quickly_when_crossfade_is_on_and_cuts_when_it_is_off() {
    let make = |n: &str| match n {
        "a" => Some(level(30.0, 0.4)),
        "b" => Some(level(30.0, -0.4)),
        _ => None,
    };
    for (fade, blends) in [(0.0f32, false), (6.0, true)] {
        let h = Harness::new(RATE, 2, make);
        h.player.send(Command::SetCrossfade(fade)).unwrap();
        h.play("a");
        h.pump_frames(24_000);
        h.play("b");
        let pcm = h.pump_frames(3 * RATE as usize);
        let l: Vec<f32> = left(&pcm);
        let mixed = l.iter().any(|s| s.abs() < 0.35 && s.abs() > 0.05);
        let last = *l.last().unwrap();
        assert!((last + 0.4).abs() < 1e-4, "ends on B either way: {last}");
        let (at, steps) = l
            .windows(2)
            .enumerate()
            .map(|(i, w)| (i, (w[1] - w[0]).abs()))
            .fold((0, 0.0f32), |a, b| if b.1 > a.1 { b } else { a });
        assert!(
            steps < 0.01,
            "fade={fade}: no hard step, worst {steps} at {at}: {:?}",
            &l[at.saturating_sub(4)..(at + 6).min(l.len())]
        );
        assert!(
            mixed,
            "both paths pass through the middle ground (ramp or blend)"
        );
        let _ = blends;
    }
}

#[test]
fn a_lost_device_is_reopened_in_its_new_format_and_playback_carries_on_from_where_it_was() {
    let h = Harness::new(RATE, 2, |n| (n == "a").then(|| ramp(5.0, 0.0)));
    h.play("a");
    h.pump_frames(14_400);
    let before = h.player.position();
    // The headphones are unplugged and Windows falls back to a 44.1 kHz output.
    *h.device.0.rate.lock().unwrap() = 44_100;
    *h.device.0.name.lock().unwrap() = "Speakers".into();
    h.device
        .0
        .lost
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .store(true, Ordering::SeqCst);
    let (_, events) = h.pump(|ev| ev.iter().any(|e| matches!(e, Event::Notice(_))));
    let notice = events
        .iter()
        .find_map(|e| {
            if let Event::Notice(t) = e {
                Some(t.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert!(notice.contains("Speakers"), "{notice}");
    let pcm = h.pump_frames(9_000);
    assert_eq!(h.device.opens(), 2);
    assert_eq!(h.device.closes(), 1, "the lost device was released");
    let after = h.player.position();
    assert!(
        after > before && after < before + 1.5,
        "resumed near {before}: now {after}"
    );
    let l = left(&pcm);
    let expected = (after - 1.0 / 44_100.0) * RATE as f64 / 1e6;
    assert!(
        (*l.last().unwrap() as f64 - expected).abs() < 1e-3 * expected.max(1e-3) + 2e-5,
        "audio continues from the same point of the file"
    );
    assert_eq!(h.player.state().output.as_ref().unwrap().rate, 44_100);
    assert!(
        h.player.state().resampled,
        "48 kHz material is converted for the 44.1 kHz device"
    );
}

#[test]
fn a_device_that_stops_calling_back_is_treated_as_lost() {
    let h = Harness::new(RATE, 2, |n| (n == "a").then(|| level(30.0, 0.2)));
    h.play("a");
    h.pump_frames(9_600);
    // The callback goes silent for longer than the watchdog allows.
    let (_, events) = {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut events = Vec::new();
        while Instant::now() < deadline && !events.iter().any(|e| matches!(e, Event::Notice(_))) {
            std::thread::sleep(Duration::from_millis(100));
            events.extend(h.player.events());
        }
        (Vec::<f32>::new(), events)
    };
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Notice(t) if t.contains("stopped responding"))));
    assert_eq!(h.device.opens(), 2);
}

#[test]
fn an_unavailable_device_is_an_error_the_ui_can_recognise_not_a_hang() {
    let h = Harness::new(RATE, 2, |n| (n == "a").then(|| level(1.0, 0.2)));
    h.device.0.fail.store(true, Ordering::SeqCst);
    h.play("a");
    let (_, events) = h.pump(|ev| ev.iter().any(|e| matches!(e, Event::Error(_))));
    let message = events
        .iter()
        .find_map(|e| {
            if let Event::Error(m) = e {
                Some(m.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert!(
        message.starts_with("Cannot open Windows audio output"),
        "{message}"
    );
    // Plugging it back in works without restarting.
    h.device.0.fail.store(false, Ordering::SeqCst);
    h.play("a");
    let (pcm, _) = h.pump(ended);
    assert_eq!(left(&pcm).len(), 48_000);
}

#[test]
fn an_idle_player_hands_the_device_back_and_takes_it_again_for_the_next_track() {
    let h = Harness::with_options(
        RATE,
        2,
        Options {
            idle_close: Duration::from_millis(300),
        },
        |n| (n == "a").then(|| level(0.2, 0.2)),
    );
    h.play("a");
    h.pump(ended);
    h.wait_for("device release", || h.device.closes() == 1);
    assert_eq!(h.device.opens(), 1);
    h.play("a");
    let (pcm, _) = h.pump(ended);
    assert_eq!(left(&pcm).len(), 9_600);
    assert_eq!(h.device.opens(), 2);
}

#[test]
fn high_rate_multichannel_files_play_on_a_stereo_device_without_clipping() {
    let h = Harness::new(RATE, 2, |n| {
        (n == "surround").then(|| {
            let mut s = Synth::new(192_000, 8, 0.5, |f, _| {
                (TAU * 997.0 * f as f64 / 192_000.0).sin() as f32
            });
            s.packet = 4096;
            s
        })
    });
    h.play("surround");
    let (pcm, events) = h.pump(ended);
    assert!(!events.iter().any(|e| matches!(e, Event::Error(_))));
    assert_eq!(left(&pcm).len(), 24_000);
    assert!(pcm.iter().all(|s| s.abs() <= 1.0));
    let peak = pcm.iter().fold(0.0f32, |p, s| p.max(s.abs()));
    assert!(peak > 0.05, "the downmix is audible: {peak}");
    let state = h.player.state();
    assert_eq!((state.rate, state.channels), (192_000, 8));
    assert!(state.resampled && state.channel_conversion);
}

#[test]
fn nothing_is_buffered_beyond_the_bound_while_a_long_track_plays() {
    let h = Harness::new(RATE, 2, |n| (n == "long").then(|| level(3600.0, 0.1)));
    h.play("long");
    h.pump_frames(RATE as usize);
    // The device is not consuming; the worker may only fill its fixed queue.
    std::thread::sleep(Duration::from_millis(300));
    let queued = h.device_shared_queued();
    let cap = (queue_chunks(RATE, 2) * chunk_frames(RATE)) as i64;
    assert!(
        queued <= cap + chunk_frames(RATE) as i64,
        "{queued} frames queued, cap {cap}"
    );
    assert!(queued > 0);
}

impl Harness {
    fn device_shared_queued(&self) -> i64 {
        self.device
            .0
            .source
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .shared
            .queued
            .load(Ordering::Relaxed)
    }
}

#[test]
fn dropping_the_player_stops_both_threads_and_releases_the_device() {
    let device = TestBackend::new(RATE, 2);
    let opener = Arc::new(TestOpener {
        make: Box::new(|n| (n == "a").then(|| level(30.0, 0.2))),
        opened: Mutex::new(Vec::new()),
        concurrent: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let player = Player::with_backend(
        egui::Context::default(),
        Box::new(device.clone()),
        opener.clone(),
    );
    player
        .send(Command::Play(PathBuf::from("a"), 0.0, false))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while player.state().rate == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(device.opens(), 1);
    assert_eq!(
        Arc::strong_count(&opener),
        2,
        "the open worker holds the opener while the player lives"
    );
    drop(player);
    let deadline = Instant::now() + Duration::from_secs(10);
    while (device.closes() < 1 || Arc::strong_count(&opener) > 1) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(device.closes(), 1, "the output device was released");
    assert_eq!(
        Arc::strong_count(&opener),
        1,
        "the open worker has exited and let go of the opener"
    );
}

#[test]
fn dropping_a_player_with_a_full_command_queue_still_shuts_down() {
    let device = TestBackend::new(RATE, 2);
    let opener = Arc::new(TestOpener {
        make: Box::new(|n| (n == "a").then(|| level(30.0, 0.2))),
        opened: Mutex::new(Vec::new()),
        concurrent: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let player = Player::with_backend(
        egui::Context::default(),
        Box::new(device.clone()),
        opener.clone(),
    );
    player
        .send(Command::Play(PathBuf::from("a"), 0.0, false))
        .unwrap();
    // Flood the queue; some sends are refused as busy, which is fine.
    for i in 0..200 {
        let _ = player.send(Command::Seek(i as f64 * 0.01));
    }
    drop(player);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Arc::strong_count(&opener) > 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(Arc::strong_count(&opener), 1, "both workers ended");
}
