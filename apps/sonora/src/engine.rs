//! The playback engine: decoded tracks in, one continuous stream of device-format PCM out.
//!
//! Everything here is device-independent and runs on the audio worker thread. A [`Stream`] is one
//! track (decoder, channel conversion, sample-rate conversion). The [`Engine`] holds the playing
//! stream, at most two streams fading out and the prepared next stream, and mixes them into
//! chunks of output frames. Memory is bounded by the decoder packet size and a few thousand
//! frames per stream, never by the length of a song.

use crate::{
    decoder::{AudioDecoder, Metadata, StreamInfo},
    dsp::{
        fade_in_gain, fade_out_gain, seconds_to_frames, ChannelMap, PeakGuard, Role,
        StreamResampler, FEED_FRAMES, MAX_CHANNELS,
    },
};
use std::{path::PathBuf, sync::Arc};

/// Longest crossfade offered, in seconds.
pub const MAX_CROSSFADE_SECONDS: f32 = 12.0;
/// A user-chosen track change blends over the setting, but never longer than this: pressing
/// Next should not leave the old song audible for twelve seconds.
pub const MANUAL_FADE_SECONDS: f64 = 1.0;
/// Fades shorter than this are not worth a blend; the tracks simply run back to back.
pub const MIN_FADE_SECONDS: f64 = 0.02;
/// A track that is replaced while it is still fading out is finished off in this time.
const KILL_FADE_SECONDS: f64 = 0.010;
/// Fading-out streams that keep their full fade. A third one makes the oldest finish quickly.
const MAX_OUTGOING: usize = 2;
/// Most outgoing streams that can exist at once (the extra one is finishing quickly).
const HARD_OUTGOING: usize = MAX_OUTGOING + 1;
/// Device channel counts above this are not mapped.
pub const MAX_OUTPUT_CHANNELS: usize = 32;

/// Where decoded audio comes from. The player uses [`AudioDecoder`]; tests use synthetic sources.
pub trait PcmSource: Send {
    fn rate(&self) -> u32;
    fn layout(&self) -> Vec<Role>;
    /// Declared length in seconds, when the file states one.
    fn duration(&self) -> Option<f64>;
    /// Appends the next block (interleaved, source rate and layout). `Ok(false)` is end of file.
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String>;
    fn seek(&mut self, seconds: f64) -> Result<(), String>;
}

impl PcmSource for AudioDecoder {
    fn rate(&self) -> u32 {
        self.rate
    }
    fn layout(&self) -> Vec<Role> {
        self.layout.clone()
    }
    fn duration(&self) -> Option<f64> {
        (self.metadata.duration > 0.0).then_some(self.metadata.duration)
    }
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        self.read_into(out)
    }
    fn seek(&mut self, seconds: f64) -> Result<(), String> {
        AudioDecoder::seek(self, seconds)
    }
}

/// What the rest of the app knows about a stream: shown in the UI, never used for mixing.
#[derive(Clone, Debug, Default)]
pub struct Tag {
    pub path: PathBuf,
    pub info: StreamInfo,
    pub meta: Metadata,
    pub resampled: bool,
    pub channel_conversion: bool,
}

pub enum PullEnd {
    More,
    Eof,
}

/// One track converted to the device format.
pub struct Stream {
    src: Box<dyn PcmSource>,
    map: ChannelMap,
    map_first: bool,
    resampler: StreamResampler,
    out_rate: u32,
    out_channels: usize,
    src_channels: usize,
    ready: Vec<f32>,
    cursor: usize,
    scratch: Vec<f32>,
    mapped: Vec<f32>,
    converted: Vec<f32>,
    src_done: bool,
    error: Option<String>,
    delivered: u64,
    start_secs: f64,
    duration: Option<f64>,
    pub tag: Arc<Tag>,
}

impl Stream {
    pub fn new(
        src: Box<dyn PcmSource>,
        out_rate: u32,
        out_channels: usize,
        mut tag: Tag,
    ) -> Result<Self, String> {
        let layout = src.layout();
        let src_channels = layout.len().max(1);
        if src_channels > MAX_CHANNELS || out_channels == 0 || out_channels > MAX_OUTPUT_CHANNELS {
            return Err(format!(
                "Unsupported channel layout: {src_channels} channels to a {out_channels}-channel output"
            ));
        }
        let map = ChannelMap::new(&layout, out_channels);
        let map_first = out_channels < src_channels;
        let resampler = StreamResampler::new(
            src.rate(),
            out_rate,
            if map_first {
                out_channels
            } else {
                src_channels
            },
        )?;
        tag.resampled = !resampler.is_passthrough();
        tag.channel_conversion = !map.is_identity();
        Ok(Self {
            duration: src.duration(),
            src,
            map,
            map_first,
            resampler,
            out_rate,
            out_channels,
            src_channels,
            ready: Vec::new(),
            cursor: 0,
            scratch: Vec::new(),
            mapped: Vec::new(),
            converted: Vec::new(),
            src_done: false,
            error: None,
            delivered: 0,
            start_secs: 0.0,
            tag: Arc::new(tag),
        })
    }

    /// Position in the track of the next frame this stream will deliver.
    pub fn position(&self) -> f64 {
        self.start_secs + self.delivered as f64 / self.out_rate as f64
    }
    pub fn duration(&self) -> Option<f64> {
        self.duration
    }
    /// Output frames left before the declared end, when the length is declared.
    pub fn remaining_frames(&self) -> Option<u64> {
        let total = seconds_to_frames(self.duration?, self.out_rate);
        let at = seconds_to_frames(self.start_secs, self.out_rate) + self.delivered;
        Some(total.saturating_sub(at))
    }
    /// Samples currently held (converted audio waiting plus resampler input), for bounds checks.
    pub fn buffered_samples(&self) -> usize {
        self.ready.len() - self.cursor + self.resampler.pending_samples()
    }

    pub fn seek(&mut self, seconds: f64) -> Result<(), String> {
        self.src.seek(seconds)?;
        self.resampler.reset();
        self.ready.clear();
        self.cursor = 0;
        self.src_done = false;
        self.error = None;
        self.delivered = 0;
        self.start_secs = seconds.max(0.0);
        Ok(())
    }

    fn fill_once(&mut self) -> Result<(), String> {
        self.scratch.clear();
        if !self.src.read(&mut self.scratch)? {
            self.converted.clear();
            self.resampler.finish(&mut self.converted)?;
            self.finish_tail();
            self.src_done = true;
            return Ok(());
        }
        let input = std::mem::take(&mut self.scratch);
        for piece in input.chunks(FEED_FRAMES * self.src_channels) {
            self.converted.clear();
            if self.map_first {
                self.mapped.clear();
                self.map.apply(piece, &mut self.mapped);
                self.resampler.push(&self.mapped, &mut self.converted)?;
            } else {
                self.resampler.push(piece, &mut self.converted)?;
            }
            self.finish_tail();
        }
        self.scratch = input;
        Ok(())
    }

    /// Moves `converted` (resampler output) into the ready queue, mapping channels if that
    /// was deferred until after resampling.
    fn finish_tail(&mut self) {
        if self.converted.is_empty() {
            return;
        }
        let from = self.ready.len();
        if self.map_first {
            self.ready.extend_from_slice(&self.converted);
        } else {
            self.map.apply(&self.converted, &mut self.ready);
        }
        self.converted.clear();
        // Nothing invalid may reach the mixer, the peak guard or the device.
        for s in &mut self.ready[from..] {
            if !s.is_finite() {
                *s = 0.0;
            }
        }
    }

    /// Appends up to `frames` frames to `out`. Fewer than asked for means the end of the file
    /// (`PullEnd::Eof`) or a block cut short ahead of a deferred error.
    pub fn pull(&mut self, frames: usize, out: &mut Vec<f32>) -> Result<(usize, PullEnd), String> {
        let ch = self.out_channels;
        let want = frames * ch;
        while self.ready.len() - self.cursor < want && !self.src_done {
            if let Err(e) = self.fill_once() {
                self.error = Some(e);
                self.src_done = true;
            }
        }
        let available = (self.ready.len() - self.cursor) / ch;
        let n = available.min(frames);
        if n == 0 {
            if let Some(e) = self.error.take() {
                return Err(e);
            }
            return Ok((0, PullEnd::Eof));
        }
        out.extend_from_slice(&self.ready[self.cursor..self.cursor + n * ch]);
        self.cursor += n * ch;
        self.delivered += n as u64;
        // Look one packet ahead when everything decoded has been handed out, so the end of the
        // file is known with the last frames and not one call later.
        if self.cursor == self.ready.len() && !self.src_done {
            if let Err(e) = self.fill_once() {
                self.error = Some(e);
                self.src_done = true;
            }
        }
        if self.cursor > 16_384 && self.cursor * 2 > self.ready.len() {
            self.ready.drain(..self.cursor);
            self.cursor = 0;
        }
        let eof = self.src_done && self.error.is_none() && self.cursor == self.ready.len();
        Ok((n, if eof { PullEnd::Eof } else { PullEnd::More }))
    }
}

// ------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Env {
    Steady,
    In { pos: u64, len: u64 },
    Out { from: f32, pos: u64, len: u64 },
}

impl Env {
    fn gain(&self, ahead: u64) -> f32 {
        match *self {
            Env::Steady => 1.0,
            Env::In { pos, len } => fade_in_gain((pos + ahead) as f32 / len as f32),
            Env::Out { from, pos, len } => from * fade_out_gain((pos + ahead) as f32 / len as f32),
        }
    }
    fn remaining(&self) -> Option<u64> {
        match *self {
            Env::Steady => None,
            Env::In { pos, len } | Env::Out { pos, len, .. } => Some(len.saturating_sub(pos)),
        }
    }
    fn advance(&mut self, frames: u64) {
        match self {
            Env::Steady => {}
            Env::In { pos, .. } | Env::Out { pos, .. } => *pos += frames,
        }
    }
    fn finished(&self) -> bool {
        self.remaining() == Some(0)
    }
}

struct Live {
    stream: Stream,
    seq: u64,
    env: Env,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Start {
    /// Replace everything now; the caller flushes whatever was already queued for the device.
    Cut,
    /// Blend the current track into the new one over this many seconds.
    Fade(f64),
}

#[derive(Debug)]
enum Transition {
    Fade(u64),
    Gapless,
}

#[derive(Default, Debug)]
pub struct Rendered {
    /// Frames appended to the output.
    pub frames: usize,
    /// Sequence number of the primary track those frames belong to.
    pub seq: u64,
    /// Position in that track (seconds) of the first appended frame.
    pub start_pos: f64,
    /// Set when this call began by switching the primary track.
    pub promoted: Option<(u64, Arc<Tag>)>,
    /// These were the last frames: nothing is playing or prepared any more.
    pub ended: bool,
    /// The playing track failed. Frames before the failure are still returned.
    pub error: Option<(Arc<Tag>, String)>,
    /// Seconds of crossfade still to go.
    pub fading: Option<f64>,
    /// A fading-out track failed and was dropped; the blend carried on without it.
    pub dropped_outgoing: u32,
}

pub struct Engine {
    out_rate: u32,
    out_channels: usize,
    crossfade_secs: f64,
    primary: Option<Live>,
    outgoing: Vec<Live>,
    next: Option<Stream>,
    pending: Option<Transition>,
    guard: PeakGuard,
    seq: u64,
    /// One block of pulled frames per live stream: the primary first, then the outgoing ones.
    bufs: Vec<Vec<f32>>,
}

impl Engine {
    pub fn new(out_rate: u32, out_channels: usize) -> Self {
        Self {
            out_rate,
            out_channels,
            crossfade_secs: 0.0,
            primary: None,
            outgoing: Vec::new(),
            next: None,
            pending: None,
            guard: PeakGuard::new(out_rate),
            seq: 0,
            bufs: vec![Vec::new(); 1 + HARD_OUTGOING],
        }
    }

    pub fn rate(&self) -> u32 {
        self.out_rate
    }
    pub fn channels(&self) -> usize {
        self.out_channels
    }
    pub fn set_crossfade(&mut self, seconds: f32) {
        self.crossfade_secs = f64::from(seconds.clamp(0.0, MAX_CROSSFADE_SECONDS));
    }
    pub fn crossfade(&self) -> f32 {
        self.crossfade_secs as f32
    }
    pub fn is_active(&self) -> bool {
        self.primary.is_some()
    }
    pub fn primary_tag(&self) -> Option<Arc<Tag>> {
        self.primary.as_ref().map(|p| p.stream.tag.clone())
    }
    pub fn primary_position(&self) -> Option<f64> {
        self.primary.as_ref().map(|p| p.stream.position())
    }
    pub fn primary_duration(&self) -> Option<f64> {
        self.primary.as_ref().and_then(|p| p.stream.duration())
    }
    pub fn next_path(&self) -> Option<PathBuf> {
        self.next.as_ref().map(|n| n.tag.path.clone())
    }
    pub fn outgoing_count(&self) -> usize {
        self.outgoing.len()
    }
    /// Samples currently held across all streams' converted-audio queues: the memory bound.
    pub fn buffered_samples(&self) -> usize {
        self.primary
            .iter()
            .map(|l| l.stream.buffered_samples())
            .sum::<usize>()
            + self
                .outgoing
                .iter()
                .map(|l| l.stream.buffered_samples())
                .sum::<usize>()
            + self.next.as_ref().map_or(0, Stream::buffered_samples)
    }

    pub fn set_next(&mut self, stream: Option<Stream>) {
        self.next = stream;
    }

    pub fn stop(&mut self) {
        self.primary = None;
        self.outgoing.clear();
        self.next = None;
        self.pending = None;
        self.guard.reset();
    }

    /// Starts `stream` as the playing track. With [`Start::Fade`] the current track keeps
    /// playing underneath and fades out. Returns the new track's sequence number.
    pub fn start(&mut self, stream: Stream, how: Start) -> u64 {
        self.seq += 1;
        let seq = self.seq;
        self.pending = None;
        self.next = None;
        let fade = match how {
            Start::Fade(secs) if self.primary.is_some() => self.frames(secs),
            _ => 0,
        };
        let env = if fade > 0 {
            Env::In { pos: 0, len: fade }
        } else {
            Env::Steady
        };
        match self.primary.replace(Live { stream, seq, env }) {
            Some(old) if fade > 0 => self.fade_out(old, fade),
            _ => {
                self.outgoing.clear();
                self.guard.reset();
            }
        }
        seq
    }

    fn frames(&self, secs: f64) -> u64 {
        seconds_to_frames(secs, self.out_rate)
    }

    /// Turns a track into a fading-out one. A track that has fallen behind the newest two is
    /// finished quickly rather than dropped, so replacing tracks rapidly never cuts a waveform.
    fn fade_out(&mut self, mut old: Live, len: u64) {
        let from = old.env.gain(0);
        old.env = Env::Out {
            from,
            pos: 0,
            len: len.max(1),
        };
        self.outgoing.push(old);
        let kill = self.frames(KILL_FADE_SECONDS).max(1);
        let excess = self.outgoing.len().saturating_sub(MAX_OUTGOING);
        for live in self.outgoing.iter_mut().take(excess) {
            let left = live.env.remaining().unwrap_or(kill).min(kill).max(1);
            live.env = Env::Out {
                from: live.env.gain(0),
                pos: 0,
                len: left,
            };
        }
    }

    /// Seeks the playing track in place. Cancels any blend: the old track is dropped.
    pub fn seek(&mut self, seconds: f64) -> Result<(), String> {
        let Some(p) = self.primary.as_mut() else {
            return Ok(());
        };
        p.stream.seek(seconds)?;
        p.env = Env::Steady;
        self.outgoing.clear();
        self.pending = None;
        self.guard.reset();
        Ok(())
    }

    /// If the playing track is about to end and a next track is ready, how far ahead of the end
    /// the blend starts. `None` when no blend applies and the tracks run back to back.
    fn auto_fade_window(&self) -> Option<u64> {
        let (p, next) = (self.primary.as_ref()?, self.next.as_ref()?);
        if self.crossfade_secs <= 0.0 || p.stream.tag.path == next.tag.path {
            return None; // repeat-one loops gaplessly
        }
        let a = p.stream.duration()?;
        let mut limit = self.crossfade_secs.min(a / 2.0);
        if let Some(b) = next.duration() {
            limit = limit.min(b / 2.0);
        }
        (limit >= MIN_FADE_SECONDS).then(|| self.frames(limit))
    }

    /// Renders up to `max_frames` frames, appending them to `out`. Returns early at a track
    /// boundary so every call's frames belong to a single playing track.
    pub fn render(&mut self, max_frames: usize, out: &mut Vec<f32>) -> Rendered {
        let mut r = Rendered::default();
        if let Some(t) = self.pending.take() {
            r.promoted = self.execute(t);
        }
        let Some(primary) = self.primary.as_ref() else {
            r.ended = true;
            return r;
        };
        r.seq = primary.seq;
        r.start_pos = primary.stream.position();
        while r.frames < max_frames {
            let mut n = max_frames - r.frames;
            let (env, remaining) = {
                let p = self.primary.as_ref().expect("primary");
                (p.env, p.stream.remaining_frames())
            };
            // Block boundaries land exactly on the frame where something changes.
            if env == Env::Steady && self.outgoing.is_empty() {
                if let (Some(window), Some(left)) = (self.auto_fade_window(), remaining) {
                    if left > window {
                        n = n.min((left - window) as usize);
                    } else if left >= self.frames(MIN_FADE_SECONDS) {
                        self.pending = Some(Transition::Fade(left));
                        break;
                    }
                    // else: too close to the end to blend; the tracks run back to back
                }
            }
            let ends = std::iter::once(env.remaining())
                .chain(self.outgoing.iter().map(|o| o.env.remaining()))
                .flatten();
            for left in ends {
                n = n.min(left as usize);
            }
            if n == 0 {
                self.settle();
                continue;
            }

            for b in &mut self.bufs {
                b.clear();
            }
            let result = self
                .primary
                .as_mut()
                .expect("primary")
                .stream
                .pull(n, &mut self.bufs[0]);
            let (got, eof) = match result {
                Ok((got, end)) => (got, matches!(end, PullEnd::Eof)),
                Err(e) => {
                    let tag = self.primary.as_ref().expect("primary").stream.tag.clone();
                    r.error = Some((tag, e));
                    self.stop();
                    break;
                }
            };
            // A fading-out track that fails or runs out early simply stops contributing.
            let mut alive = vec![true; self.outgoing.len()];
            for (i, o) in self.outgoing.iter_mut().enumerate() {
                match o.stream.pull(got, &mut self.bufs[i + 1]) {
                    Ok((g, _)) if g > 0 => {}
                    Ok(_) => alive[i] = false,
                    Err(_) => {
                        alive[i] = false;
                        r.dropped_outgoing += 1;
                    }
                }
            }
            self.mix_block(got, out);
            r.frames += got;
            for o in &mut self.outgoing {
                o.env.advance(got as u64);
            }
            self.primary
                .as_mut()
                .expect("primary")
                .env
                .advance(got as u64);
            let mut i = 0;
            self.outgoing.retain(|o| {
                i += 1;
                alive[i - 1] && !o.env.finished()
            });
            self.settle();

            if eof {
                if self.next.is_some() {
                    self.pending = Some(Transition::Gapless);
                } else {
                    r.ended = true;
                    self.stop();
                }
                break;
            }
        }
        r.fading = self.fade_remaining();
        r
    }

    /// Retires finished fades.
    fn settle(&mut self) {
        self.outgoing.retain(|o| !o.env.finished());
        if let Some(p) = self.primary.as_mut() {
            if matches!(p.env, Env::In { .. }) && p.env.finished() {
                p.env = Env::Steady;
            }
        }
    }

    fn fade_remaining(&self) -> Option<f64> {
        let mut left = self.outgoing.iter().filter_map(|o| o.env.remaining()).max();
        if let Some(p) = self.primary.as_ref() {
            left = left.max(p.env.remaining());
        }
        left.filter(|l| *l > 0)
            .map(|l| l as f64 / self.out_rate as f64)
    }

    fn execute(&mut self, t: Transition) -> Option<(u64, Arc<Tag>)> {
        let next = self.next.take()?;
        self.seq += 1;
        let seq = self.seq;
        let tag = next.tag.clone();
        match t {
            Transition::Fade(len) => {
                let old = self.primary.take();
                self.primary = Some(Live {
                    stream: next,
                    seq,
                    env: Env::In { pos: 0, len },
                });
                if let Some(old) = old {
                    self.fade_out(old, len);
                }
            }
            Transition::Gapless => {
                self.primary = Some(Live {
                    stream: next,
                    seq,
                    env: Env::Steady,
                });
                self.outgoing.clear();
            }
        }
        Some((seq, tag))
    }

    /// Mixes the pulled blocks into `out`. A single steady track is copied untouched.
    fn mix_block(&mut self, frames: usize, out: &mut Vec<f32>) {
        let ch = self.out_channels;
        if frames == 0 {
            return;
        }
        let primary = self.primary.as_ref().expect("primary");
        let fading = !self.outgoing.is_empty() || matches!(primary.env, Env::In { .. });
        if !fading && !self.guard.engaged() {
            out.extend_from_slice(&self.bufs[0][..frames * ch]);
            return;
        }
        let start = out.len();
        out.resize(start + frames * ch, 0.0);
        for f in 0..frames {
            let frame = &mut out[start + f * ch..start + (f + 1) * ch];
            let gain = primary.env.gain(f as u64);
            for (o, s) in frame.iter_mut().zip(&self.bufs[0][f * ch..(f + 1) * ch]) {
                *o = s * gain;
            }
            for (i, live) in self.outgoing.iter().enumerate() {
                let buf = &self.bufs[i + 1];
                if (f + 1) * ch > buf.len() {
                    continue; // this track ended inside the block
                }
                let gain = live.env.gain(f as u64);
                for (o, s) in frame.iter_mut().zip(&buf[f * ch..(f + 1) * ch]) {
                    *o += s * gain;
                }
            }
            self.guard.process(frame);
        }
    }
}

// ------------------------------------------------------------------------------------------

#[cfg(test)]
pub mod testutil {
    use super::*;

    /// A synthetic track: `signal(frame, channel)` for `frames` frames at `rate`, delivered in
    /// packets of `packet` frames. Optionally fails after `fail_at` frames.
    pub struct Synth {
        pub rate: u32,
        pub layout: Vec<Role>,
        pub frames: u64,
        pub packet: u64,
        pub pos: u64,
        pub declared: bool,
        pub fail_at: Option<u64>,
        pub signal: Box<dyn Fn(u64, usize) -> f32 + Send>,
        pub reads: u64,
    }

    impl Synth {
        pub fn new(
            rate: u32,
            channels: usize,
            seconds: f64,
            signal: impl Fn(u64, usize) -> f32 + Send + 'static,
        ) -> Self {
            Self {
                rate,
                layout: crate::dsp::default_layout(channels),
                frames: (seconds * rate as f64).round() as u64,
                packet: 1152,
                pos: 0,
                declared: true,
                fail_at: None,
                signal: Box::new(signal),
                reads: 0,
            }
        }
        pub fn boxed(self) -> Box<dyn PcmSource> {
            Box::new(self)
        }
    }

    impl PcmSource for Synth {
        fn rate(&self) -> u32 {
            self.rate
        }
        fn layout(&self) -> Vec<Role> {
            self.layout.clone()
        }
        fn duration(&self) -> Option<f64> {
            self.declared.then(|| self.frames as f64 / self.rate as f64)
        }
        fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
            if self.pos >= self.frames {
                return Ok(false);
            }
            if self.fail_at.is_some_and(|f| self.pos >= f) {
                return Err("synthetic decode failure".into());
            }
            let n = self.packet.min(self.frames - self.pos);
            for f in self.pos..self.pos + n {
                for c in 0..self.layout.len() {
                    out.push((self.signal)(f, c));
                }
            }
            self.pos += n;
            self.reads += 1;
            Ok(true)
        }
        fn seek(&mut self, seconds: f64) -> Result<(), String> {
            self.pos = ((seconds * self.rate as f64).round() as u64).min(self.frames);
            Ok(())
        }
    }

    pub fn tag(name: &str) -> Tag {
        Tag {
            path: PathBuf::from(name),
            ..Default::default()
        }
    }

    pub fn stream(src: Synth, name: &str, rate: u32, channels: usize) -> Stream {
        Stream::new(src.boxed(), rate, channels, tag(name)).expect("stream")
    }

    /// Renders everything the engine will produce, in `chunk` frame calls.
    pub fn drain(engine: &mut Engine, chunk: usize) -> (Vec<f32>, Vec<Rendered>) {
        let mut out = Vec::new();
        let mut calls = Vec::new();
        for _ in 0..1_000_000 {
            let before = out.len();
            let r = engine.render(chunk, &mut out);
            let ended = r.ended;
            let failed = r.error.is_some();
            let progressed = out.len() > before || r.promoted.is_some();
            calls.push(Rendered {
                frames: (out.len() - before) / engine.channels(),
                ..r
            });
            if ended || failed || !progressed && !engine.is_active() {
                break;
            }
        }
        (out, calls)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::f64::consts::TAU;

    const RATE: u32 = 48_000;

    fn dc(level: f32) -> impl Fn(u64, usize) -> f32 + Send + 'static {
        move |_, _| level
    }
    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
    }
    fn engine_with(a: Synth, b: Synth, fade: f32) -> Engine {
        let mut e = Engine::new(RATE, 2);
        e.set_crossfade(fade);
        e.start(stream(a, "a.wav", RATE, 2), Start::Cut);
        e.set_next(Some(stream(b, "b.wav", RATE, 2)));
        e
    }

    #[test]
    fn gapless_hand_off_loses_and_repeats_nothing() {
        // Ramps make every frame unique, so a gap, an overlap or a repeat shows immediately.
        let a = Synth::new(RATE, 2, 1.0, |f, _| f as f32 / 100_000.0);
        let b = Synth::new(RATE, 2, 0.5, |f, _| 1.0 + f as f32 / 100_000.0);
        let mut e = engine_with(a, b, 0.0);
        let (out, calls) = drain(&mut e, 960);
        assert_eq!(out.len(), (48_000 + 24_000) * 2);
        for f in 0..48_000usize {
            assert_eq!(out[f * 2], f as f32 / 100_000.0, "A frame {f}");
        }
        for f in 0..24_000usize {
            assert_eq!(
                out[(48_000 + f) * 2],
                1.0 + f as f32 / 100_000.0,
                "B frame {f}"
            );
        }
        assert!(calls.last().unwrap().ended);
        // The hand-off is announced exactly once, on the call that starts the new track.
        assert_eq!(calls.iter().filter(|c| c.promoted.is_some()).count(), 1);
    }

    #[test]
    fn crossfade_overlaps_the_tail_of_one_track_with_the_head_of_the_next() {
        let fade = 2.0;
        let a = Synth::new(RATE, 2, 10.0, dc(0.5));
        let b = Synth::new(RATE, 2, 10.0, dc(-0.25));
        let mut e = engine_with(a, b, fade);
        let (out, _) = drain(&mut e, 960);
        let total = (10.0 + 10.0 - fade as f64) * RATE as f64;
        assert_eq!(
            out.len(),
            total as usize * 2,
            "overlap shortens the programme by exactly the fade"
        );
        let at = |s: f64| out[(s * RATE as f64) as usize * 2];
        assert_eq!(at(7.9), 0.5, "A plays alone until the fade starts");
        assert!((at(8.0) - 0.5).abs() < 1e-3, "fade starts at 8.0 s");
        // Mid-fade the two contributions are equal power: 0.5*cos(45 deg) + -0.25*sin(45 deg).
        let mid = 0.5 * std::f32::consts::FRAC_1_SQRT_2 - 0.25 * std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (at(9.0) - mid).abs() < 2e-3,
            "mid-fade value {} vs {}",
            at(9.0),
            mid
        );
        assert!((at(10.0) + 0.25).abs() < 2e-3, "fade ends on B alone");
        assert_eq!(at(10.5), -0.25);
    }

    #[test]
    fn equal_power_keeps_uncorrelated_loudness_constant_through_the_fade() {
        // Two different tones at the same level: RMS must stay within 0.5 dB of one track alone.
        let a = Synth::new(RATE, 2, 12.0, |f, _| {
            0.6 * (TAU * 440.0 * f as f64 / RATE as f64).sin() as f32
        });
        let b = Synth::new(RATE, 2, 12.0, |f, _| {
            0.6 * (TAU * 997.0 * f as f64 / RATE as f64).sin() as f32
        });
        let mut e = engine_with(a, b, 6.0);
        let (out, _) = drain(&mut e, 960);
        let alone = 0.6 / 2f64.sqrt();
        let start = 6 * RATE as usize;
        for second in 0..6usize {
            let from = (start + second * RATE as usize) * 2;
            let level = rms(&out[from..from + RATE as usize * 2]);
            let change = 20.0 * (level / alone).log10();
            assert!(
                change.abs() < 0.5,
                "second {second} of the fade is {change:+.2} dB"
            );
        }
    }

    #[test]
    fn worst_case_identical_full_scale_material_never_clips() {
        let tone = |f: u64, _| (TAU * 220.0 * f as f64 / RATE as f64).sin() as f32;
        let mut e = engine_with(
            Synth::new(RATE, 2, 4.0, tone),
            Synth::new(RATE, 2, 4.0, tone),
            2.0,
        );
        let (out, _) = drain(&mut e, 960);
        let peak = out.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak <= 1.0, "peak {peak}");
        assert!(
            peak > 0.99,
            "and the programme is still at full scale: {peak}"
        );
    }

    #[test]
    fn promotion_is_announced_when_the_fade_starts() {
        let mut e = engine_with(
            Synth::new(RATE, 2, 5.0, dc(0.2)),
            Synth::new(RATE, 2, 5.0, dc(0.3)),
            2.0,
        );
        let (_, calls) = drain(&mut e, 480);
        let mut played = 0usize;
        let mut promoted_at = None;
        for c in &calls {
            if let Some((_, tag)) = &c.promoted {
                promoted_at = Some(played);
                assert_eq!(tag.path, PathBuf::from("b.wav"));
            }
            played += c.frames;
        }
        assert_eq!(
            promoted_at,
            Some(3 * RATE as usize),
            "A has 3 s alone, then B becomes the playing track"
        );
        // Chunks never mix primary identities: sequence numbers only change at a promotion.
        let mut seq = calls[0].seq;
        for c in &calls {
            if c.frames == 0 {
                continue;
            }
            if c.seq != seq {
                assert!(c.promoted.is_some());
                seq = c.seq;
            }
        }
    }

    #[test]
    fn fade_is_shortened_for_short_tracks_and_skipped_for_unknown_length() {
        // 3 s tracks with a 12 s setting: the blend is half the shorter track, 1.5 s.
        let mut e = engine_with(
            Synth::new(RATE, 2, 3.0, dc(0.5)),
            Synth::new(RATE, 2, 3.0, dc(0.25)),
            12.0,
        );
        let (out, _) = drain(&mut e, 960);
        assert_eq!(out.len(), ((3.0 + 3.0 - 1.5) * RATE as f64) as usize * 2);
        // Unknown length: no way to know when the end is near, so the tracks run back to back.
        let mut a = Synth::new(RATE, 2, 3.0, dc(0.5));
        a.declared = false;
        let mut e = engine_with(a, Synth::new(RATE, 2, 3.0, dc(0.25)), 6.0);
        let (out, _) = drain(&mut e, 960);
        assert_eq!(out.len(), 6 * RATE as usize * 2);
        assert_eq!(out[3 * RATE as usize * 2 - 2], 0.5);
        assert_eq!(out[3 * RATE as usize * 2], 0.25);
    }

    #[test]
    fn repeat_one_never_crossfades_a_track_into_itself() {
        let mut e = Engine::new(RATE, 2);
        e.set_crossfade(4.0);
        e.start(
            stream(
                Synth::new(RATE, 2, 6.0, |f, _| f as f32 / 1e6),
                "same.flac",
                RATE,
                2,
            ),
            Start::Cut,
        );
        e.set_next(Some(stream(
            Synth::new(RATE, 2, 6.0, |f, _| f as f32 / 1e6),
            "same.flac",
            RATE,
            2,
        )));
        let (out, _) = drain(&mut e, 960);
        assert_eq!(out.len(), 12 * RATE as usize * 2);
        assert_eq!(
            out[6 * RATE as usize * 2],
            0.0,
            "the second pass starts from the top, not from a blend"
        );
    }

    #[test]
    fn mixed_rates_and_channel_counts_arrive_in_the_device_format() {
        // 44.1 kHz mono into a 96 kHz stereo device, then 48 kHz 5.1 into the same.
        let mut e = Engine::new(96_000, 2);
        e.start(
            stream(Synth::new(44_100, 1, 1.0, dc(0.4)), "mono.wav", 96_000, 2),
            Start::Cut,
        );
        e.set_next(Some(stream(
            Synth::new(48_000, 6, 1.0, |_, c| if c == 2 { 0.5 } else { 0.0 }),
            "surround.wav",
            96_000,
            2,
        )));
        let (out, _) = drain(&mut e, 1920);
        assert_eq!(out.len(), 2 * 96_000 * 2);
        let mono = &out[2 * 20_000..2 * 76_000];
        assert!(
            mono.iter().all(|s| (s - 0.4).abs() < 0.01),
            "mono reaches both ears at full level"
        );
        // Centre-only 5.1 content: 0.5 at -3 dB into each ear, then the clipping-safe matrix scale.
        let tail = &out[96_000 * 2 + 40_000..96_000 * 4 - 40_000];
        let left: Vec<f32> = tail.iter().step_by(2).copied().collect();
        let right: Vec<f32> = tail.iter().skip(1).step_by(2).copied().collect();
        assert!(
            left.iter().zip(&right).all(|(l, r)| (l - r).abs() < 1e-5),
            "centre is in both ears equally"
        );
        assert!(left.iter().all(|s| *s > 0.05 && *s <= 0.5));
    }

    #[test]
    fn seek_moves_in_place_cancels_the_blend_and_keeps_the_prepared_next() {
        let mut e = engine_with(
            Synth::new(RATE, 2, 10.0, |f, _| f as f32 / 1e6),
            Synth::new(RATE, 2, 10.0, dc(-0.5)),
            4.0,
        );
        let mut out = Vec::new();
        // Play 7 s: 1 s into the blend.
        for _ in 0..(7 * RATE as usize / 960) {
            e.render(960, &mut out);
        }
        assert_eq!(e.outgoing_count(), 1);
        // Seven seconds in, B is already the playing track (the blend began at 6 s).
        e.seek(2.0).unwrap();
        assert_eq!(e.outgoing_count(), 0, "seeking drops the fading-out track");
        out.clear();
        let r = e.render(960, &mut out);
        assert!(
            (r.start_pos - 2.0).abs() < 1e-9,
            "playing from the seek target"
        );
        assert_eq!(
            out[0], -0.5,
            "B continues from 2 s at full level, A is gone"
        );
        assert!(r.fading.is_none());
    }

    #[test]
    fn a_new_track_started_during_a_blend_finishes_the_old_ones_quickly_without_a_click() {
        let mut e = engine_with(
            Synth::new(RATE, 2, 10.0, dc(0.4)),
            Synth::new(RATE, 2, 10.0, dc(0.4)),
            4.0,
        );
        let mut out = Vec::new();
        for _ in 0..(8 * RATE as usize / 960) {
            e.render(960, &mut out);
        }
        assert_eq!(e.outgoing_count(), 1);
        // Two more manual skips while the first blend is still running.
        e.start(
            stream(Synth::new(RATE, 2, 10.0, dc(0.4)), "c.wav", RATE, 2),
            Start::Fade(1.0),
        );
        e.start(
            stream(Synth::new(RATE, 2, 10.0, dc(0.4)), "d.wav", RATE, 2),
            Start::Fade(1.0),
        );
        assert!(e.outgoing_count() <= HARD_OUTGOING);
        out.clear();
        for _ in 0..(RATE as usize / 960 * 2) {
            e.render(960, &mut out);
        }
        let step = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            step < 0.01,
            "largest sample-to-sample step {step}: no hard cut"
        );
        assert_eq!(e.outgoing_count(), 0);
    }

    #[test]
    fn a_failing_incoming_track_is_reported_and_a_failing_outgoing_track_is_not_fatal() {
        // Outgoing track dies in the middle of its fade: the programme carries on with B.
        let mut a = Synth::new(RATE, 2, 10.0, dc(0.3));
        a.fail_at = Some(9 * RATE as u64);
        let mut e = engine_with(a, Synth::new(RATE, 2, 10.0, dc(0.3)), 4.0);
        let (out, calls) = drain(&mut e, 960);
        assert!(calls.iter().all(|c| c.error.is_none()));
        assert!(
            out.len() >= 12 * RATE as usize * 2,
            "B still plays to the end"
        );
        // Playing track fails: frames before the failure are delivered, then the error.
        let mut a = Synth::new(RATE, 2, 10.0, dc(0.3));
        a.fail_at = Some(RATE as u64);
        let mut e = Engine::new(RATE, 2);
        e.start(stream(a, "bad.mp3", RATE, 2), Start::Cut);
        let (out, calls) = drain(&mut e, 960);
        let err = calls
            .iter()
            .find_map(|c| c.error.as_ref())
            .expect("error reported");
        assert!(err.1.contains("synthetic"));
        assert_eq!(err.0.path, PathBuf::from("bad.mp3"));
        assert!(out.len() >= RATE as usize * 2 && out.len() < 3 * RATE as usize * 2);
        assert!(!e.is_active());
    }

    #[test]
    fn memory_stays_bounded_over_a_long_track() {
        // One hour of audio, produced lazily: the engine never holds more than a few packets.
        let long = Synth::new(RATE, 2, 3600.0, |f, _| ((f % 100) as f32) / 100.0);
        let mut e = Engine::new(44_100, 2);
        e.start(stream(long, "long.flac", 44_100, 2), Start::Cut);
        let mut out = Vec::new();
        let mut worst = 0;
        for _ in 0..3000 {
            out.clear();
            e.render(960, &mut out);
            worst = worst.max(e.buffered_samples());
        }
        assert!(worst < 40_000, "{worst} samples buffered");
    }

    #[test]
    fn stop_and_pause_free_everything() {
        let mut e = engine_with(
            Synth::new(RATE, 2, 5.0, dc(0.1)),
            Synth::new(RATE, 2, 5.0, dc(0.1)),
            2.0,
        );
        let mut out = Vec::new();
        e.render(960, &mut out);
        e.stop();
        assert!(!e.is_active());
        assert_eq!(e.buffered_samples(), 0);
        assert!(e.next_path().is_none());
        assert!(e.render(960, &mut out).ended);
    }
}
