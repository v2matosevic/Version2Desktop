//! Pure signal processing for the playback engine: channel-layout conversion, sample-rate
//! conversion, fade gains and the crossfade peak guard. No I/O, no device, no threads, so every
//! behaviour here is proven by ordinary unit tests on synthetic signals.

use rubato::{
    audioadapter_buffers::direct::InterleavedSlice, Async, Fft, FixedAsync, FixedSync, Indexing,
    Resampler, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Most channels any layout here carries (decoder and device).
pub const MAX_CHANNELS: usize = 8;
/// Longest run of input frames handed to the resampler in one call. It bounds the memory a
/// single decoded packet can expand into when the output rate is much higher than the source.
pub const FEED_FRAMES: usize = 4096;

// ------------------------------------------------------------------------------------------
// Channel layout

/// What a channel is for. Decoders name their layout, devices follow Windows' speaker order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    FrontLeft,
    FrontRight,
    FrontCenter,
    Lfe,
    BackLeft,
    BackRight,
    FrontLeftCenter,
    FrontRightCenter,
    BackCenter,
    SideLeft,
    SideRight,
    Other,
}

/// Channel order a file with `channels` channels carries when it does not say (WAVE order).
pub fn default_layout(channels: usize) -> Vec<Role> {
    use Role::*;
    match channels {
        1 => vec![FrontCenter],
        2 => vec![FrontLeft, FrontRight],
        3 => vec![FrontLeft, FrontRight, FrontCenter],
        4 => vec![FrontLeft, FrontRight, BackLeft, BackRight],
        5 => vec![FrontLeft, FrontRight, FrontCenter, BackLeft, BackRight],
        6 => vec![FrontLeft, FrontRight, FrontCenter, Lfe, BackLeft, BackRight],
        7 => vec![
            FrontLeft,
            FrontRight,
            FrontCenter,
            Lfe,
            BackCenter,
            SideLeft,
            SideRight,
        ],
        8 => vec![
            FrontLeft,
            FrontRight,
            FrontCenter,
            Lfe,
            BackLeft,
            BackRight,
            SideLeft,
            SideRight,
        ],
        n => {
            let mut v = default_layout(8);
            v.resize(n, Other);
            v
        }
    }
}

/// Speaker order of an output device with `channels` channels (Windows channel-mask order).
pub fn device_layout(channels: usize) -> Vec<Role> {
    default_layout(channels)
}

/// Conversion between two channel layouts as a matrix. Never raises a signal above full scale:
/// if any output row could sum above unity for full-scale input, the whole matrix is scaled down
/// by that sum (the same clipping-safe rule FFmpeg and VLC follow), so a 5.1 file plays quieter
/// than its stereo mix would, not distorted.
#[derive(Clone, Debug)]
pub struct ChannelMap {
    input: usize,
    output: usize,
    identity: bool,
    /// `output` rows of `input` coefficients.
    matrix: Vec<f32>,
}

const MINUS_3_DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// How one source channel contributes to a left/right pair when its own speaker is missing.
fn stereo_fold(role: Role) -> (f32, f32) {
    use Role::*;
    match role {
        FrontLeft => (1.0, 0.0),
        FrontRight => (0.0, 1.0),
        FrontCenter => (MINUS_3_DB, MINUS_3_DB),
        BackLeft | SideLeft | FrontLeftCenter => (MINUS_3_DB, 0.0),
        BackRight | SideRight | FrontRightCenter => (0.0, MINUS_3_DB),
        BackCenter => (0.5, 0.5),
        // Low-frequency effects are not folded into a stereo or mono mix.
        Lfe | Other => (0.0, 0.0),
    }
}

impl ChannelMap {
    pub fn new(source: &[Role], output: usize) -> Self {
        let input = source.len().max(1);
        let output = output.max(1);
        let dest = device_layout(output);
        let unknown = source.iter().all(|r| *r == Role::Other);
        if input == output && (source == dest.as_slice() || unknown || source.is_empty()) {
            return Self {
                input,
                output,
                identity: true,
                matrix: Vec::new(),
            };
        }
        let mut m = vec![0.0f32; input * output];
        let source: Vec<Role> = if source.len() == input {
            source.to_vec()
        } else {
            default_layout(input)
        };
        if input == 1 {
            // A mono file is not a centre channel: both ears get it at full level.
            for (o, role) in dest.iter().enumerate() {
                if matches!(role, Role::FrontLeft | Role::FrontRight) || output == 1 {
                    m[o * input] = 1.0;
                }
            }
        } else if output == 1 {
            for (i, role) in source.iter().enumerate() {
                let (l, r) = stereo_fold(*role);
                m[i] = 0.5 * (l + r);
            }
        } else {
            for (i, role) in source.iter().enumerate() {
                match dest.iter().position(|d| d == role && *role != Role::Other) {
                    Some(o) => m[o * input + i] = 1.0,
                    None => {
                        let (l, r) = stereo_fold(*role);
                        if let Some(o) = dest.iter().position(|d| *d == Role::FrontLeft) {
                            m[o * input + i] += l;
                        }
                        if let Some(o) = dest.iter().position(|d| *d == Role::FrontRight) {
                            m[o * input + i] += r;
                        }
                    }
                }
            }
        }
        let peak = (0..output)
            .map(|o| {
                m[o * input..(o + 1) * input]
                    .iter()
                    .map(|c| c.abs())
                    .sum::<f32>()
            })
            .fold(0.0, f32::max);
        if peak > 1.0 {
            m.iter_mut().for_each(|c| *c /= peak);
        }
        Self {
            input,
            output,
            identity: false,
            matrix: m,
        }
    }

    pub fn is_identity(&self) -> bool {
        self.identity
    }
    pub fn input_channels(&self) -> usize {
        self.input
    }
    pub fn output_channels(&self) -> usize {
        self.output
    }
    /// Coefficient from source channel `i` to output channel `o` (identity maps channel to itself).
    pub fn coefficient(&self, o: usize, i: usize) -> f32 {
        if self.identity {
            f32::from(o == i)
        } else {
            self.matrix[o * self.input + i]
        }
    }

    /// Appends the converted frames of `input` (whole frames only) to `out`.
    pub fn apply(&self, input: &[f32], out: &mut Vec<f32>) {
        if self.identity {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / self.input;
        out.reserve(frames * self.output);
        for frame in input.chunks_exact(self.input) {
            for o in 0..self.output {
                let row = &self.matrix[o * self.input..(o + 1) * self.input];
                out.push(row.iter().zip(frame).map(|(c, s)| c * s).sum());
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// Sample-rate conversion

/// Streaming sample-rate converter around rubato. Same-rate audio is passed through untouched.
/// Output length is exactly `ceil(input frames * out / in)` once the stream is finished, with the
/// resampler's start-up delay removed, so back-to-back tracks stay sample-aligned.
pub struct StreamResampler {
    channels: usize,
    in_rate: u32,
    out_rate: u32,
    inner: Option<Inner>,
}

struct Inner {
    rs: Box<dyn Resampler<f32>>,
    pending: Vec<f32>,
    block: Vec<f32>,
    delay_left: usize,
    fed: u64,
    produced: u64,
}

impl StreamResampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Result<Self, String> {
        if in_rate == out_rate {
            return Ok(Self {
                channels,
                in_rate,
                out_rate,
                inner: None,
            });
        }
        let rs = Self::build(in_rate, out_rate, channels)?;
        let delay = rs.output_delay();
        let block = vec![0.0; rs.output_frames_max() * channels];
        Ok(Self {
            channels,
            in_rate,
            out_rate,
            inner: Some(Inner {
                rs,
                pending: Vec::new(),
                block,
                delay_left: delay,
                fed: 0,
                produced: 0,
            }),
        })
    }

    fn build(
        in_rate: u32,
        out_rate: u32,
        channels: usize,
    ) -> Result<Box<dyn Resampler<f32>>, String> {
        let (i, o) = (in_rate as usize, out_rate as usize);
        // FFT resampling is exact for rational ratios, which every common rate pair is. Its block
        // size is a multiple of in/gcd and out/gcd, so an odd source rate can force huge blocks:
        // those go to the (slower, any-ratio) sinc resampler instead.
        let gcd = gcd(i, o);
        if (i / gcd).max(o / gcd) <= 1024 {
            // About 1,024-frame FFT blocks: a flat passband to roughly 0.9 of the lower Nyquist
            // for a ~10 ms start-up delay that the glue below removes.
            let sub_chunks = 4;
            if let Ok(fft) = Fft::<f32>::new_custom(
                i,
                o,
                1024 * sub_chunks,
                sub_chunks,
                channels,
                WindowFunction::BlackmanHarris2,
                FixedSync::Both,
            ) {
                return Ok(Box::new(fft));
            }
        }
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: None,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Cubic,
            window: WindowFunction::BlackmanHarris2,
        };
        Async::<f32>::new_sinc(
            o as f64 / i as f64,
            1.0,
            &params,
            1024,
            channels,
            FixedAsync::Input,
        )
        .map(|r| Box::new(r) as Box<dyn Resampler<f32>>)
        .map_err(|e| format!("Cannot convert {in_rate} Hz audio to {out_rate} Hz: {e}"))
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }
    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }
    pub fn is_passthrough(&self) -> bool {
        self.inner.is_none()
    }
    /// Frames of source audio held back (not yet converted), a bound for memory accounting.
    pub fn pending_samples(&self) -> usize {
        self.inner.as_ref().map_or(0, |i| i.pending.len())
    }

    /// Appends the converted audio for `input` (interleaved) to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        let Some(inner) = self.inner.as_mut() else {
            out.extend_from_slice(input);
            return Ok(());
        };
        let ch = self.channels;
        inner.pending.extend_from_slice(input);
        inner.fed += (input.len() / ch) as u64;
        loop {
            let need = inner.rs.input_frames_next();
            if inner.pending.len() / ch < need {
                return Ok(());
            }
            inner.run(need, None, ch, out, u64::MAX)?;
        }
    }

    /// Drains the resampler at end of stream, appending the tail so the total length is exact.
    pub fn finish(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
        let (in_rate, out_rate, ch) = (self.in_rate as u64, self.out_rate as u64, self.channels);
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        let expected = (inner.fed * out_rate).div_ceil(in_rate);
        if !inner.pending.is_empty() {
            let frames = inner.pending.len() / ch;
            inner.run(frames, Some(frames), ch, out, expected)?;
        }
        let mut guard = 0;
        while inner.produced < expected {
            inner.run(0, Some(0), ch, out, expected)?;
            guard += 1;
            if guard > 64 {
                return Err("Sample-rate converter did not drain".into());
            }
        }
        Ok(())
    }

    /// Forget all stream history (after a seek) so the next input starts a fresh stream.
    pub fn reset(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.rs.reset();
            inner.pending.clear();
            inner.delay_left = inner.rs.output_delay();
            inner.fed = 0;
            inner.produced = 0;
        }
    }
}

impl Inner {
    /// One resampler call. `frames` is how many pending frames it reads (a whole chunk, or the
    /// short final one with `partial` set). Output beyond `limit` total frames is discarded.
    fn run(
        &mut self,
        frames: usize,
        partial: Option<usize>,
        ch: usize,
        out: &mut Vec<f32>,
        limit: u64,
    ) -> Result<(), String> {
        let capacity = self.rs.output_frames_max();
        if self.block.len() < capacity * ch {
            self.block.resize(capacity * ch, 0.0);
        }
        let input = InterleavedSlice::new(&self.pending[..frames * ch], ch, frames)
            .map_err(|e| format!("Resampler input: {e}"))?;
        let mut output = InterleavedSlice::new_mut(&mut self.block[..capacity * ch], ch, capacity)
            .map_err(|e| format!("Resampler output: {e}"))?;
        let indexing = partial.map(|n| Indexing::new().partial_len(n));
        let (consumed, produced) = self
            .rs
            .process_into_buffer(&input, &mut output, indexing.as_ref())
            .map_err(|e| format!("Sample-rate conversion failed: {e}"))?;
        self.pending.drain(..consumed.min(frames) * ch);
        let skip = self.delay_left.min(produced);
        self.delay_left -= skip;
        let keep = ((produced - skip) as u64).min(limit.saturating_sub(self.produced)) as usize;
        out.extend_from_slice(&self.block[skip * ch..(skip + keep) * ch]);
        self.produced += keep as u64;
        Ok(())
    }
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

// ------------------------------------------------------------------------------------------
// Fades and the peak guard

/// Gain of the track fading in, `t` = 0 (start) to 1 (end). Equal power: `in² + out² = 1`.
pub fn fade_in_gain(t: f32) -> f32 {
    (t.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2).sin()
}

/// Gain of the track fading out.
pub fn fade_out_gain(t: f32) -> f32 {
    (t.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2).cos()
}

/// Keeps a mix of two tracks inside full scale. Only a sample that would exceed ±1.0 changes the
/// gain: it drops at once to the value that brings that frame to exactly full scale and recovers
/// linearly over [`PeakGuard::RELEASE_SECONDS`]. Equal-power fades of uncorrelated music stay
/// below full scale and pass untouched; a fade between identical full-scale material (which sums
/// to +3 dB at the midpoint) is held at full scale instead of clipping.
#[derive(Clone, Debug)]
pub struct PeakGuard {
    gain: f32,
    release: f32,
}

impl PeakGuard {
    pub const RELEASE_SECONDS: f32 = 0.060;

    pub fn new(rate: u32) -> Self {
        Self {
            gain: 1.0,
            release: 1.0 / (Self::RELEASE_SECONDS * rate as f32),
        }
    }
    /// True while the guard is still recovering from a reduction and must keep running.
    pub fn engaged(&self) -> bool {
        self.gain < 1.0
    }
    pub fn reset(&mut self) {
        self.gain = 1.0;
    }
    /// Applies the guard to one interleaved frame in place.
    pub fn process(&mut self, frame: &mut [f32]) {
        let peak = frame.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        self.gain = (self.gain + self.release).min(1.0);
        if peak * self.gain > 1.0 {
            self.gain = 1.0 / peak;
        }
        if self.gain < 1.0 {
            for s in frame.iter_mut() {
                *s = (*s * self.gain).clamp(-1.0, 1.0);
            }
        }
    }
}

pub fn seconds_to_frames(seconds: f64, rate: u32) -> u64 {
    (seconds.max(0.0) * rate as f64).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn sine(freq: f64, rate: u32, frames: usize, amp: f32) -> Vec<f32> {
        (0..frames)
            .map(|n| (amp as f64 * (TAU * freq * n as f64 / rate as f64).sin()) as f32)
            .collect()
    }
    fn interleave(channels: &[Vec<f32>]) -> Vec<f32> {
        let n = channels[0].len();
        (0..n)
            .flat_map(|i| channels.iter().map(move |c| c[i]))
            .collect()
    }
    /// Least-squares sine fit over `[from, to)`: (amplitude, residual RMS relative to the amplitude).
    #[allow(clippy::needless_range_loop)]
    fn fit(signal: &[f32], freq: f64, rate: u32, from: usize, to: usize) -> (f64, f64) {
        let (mut ss, mut cc, mut sc, mut sy, mut cy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for n in from..to {
            let p = TAU * freq * n as f64 / rate as f64;
            let (s, c, y) = (p.sin(), p.cos(), signal[n] as f64);
            ss += s * s;
            cc += c * c;
            sc += s * c;
            sy += s * y;
            cy += c * y;
        }
        let det = ss * cc - sc * sc;
        let (a, b) = ((sy * cc - cy * sc) / det, (cy * ss - sy * sc) / det);
        let mut residual = 0.0;
        for n in from..to {
            let p = TAU * freq * n as f64 / rate as f64;
            let e = signal[n] as f64 - (a * p.sin() + b * p.cos());
            residual += e * e;
        }
        let amplitude = (a * a + b * b).sqrt();
        (
            amplitude,
            (residual / (to - from) as f64).sqrt() / (amplitude / 2f64.sqrt()),
        )
    }
    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }
    fn resample_all(
        input: &[f32],
        in_rate: u32,
        out_rate: u32,
        channels: usize,
        split: usize,
    ) -> Vec<f32> {
        let mut r = StreamResampler::new(in_rate, out_rate, channels).unwrap();
        let mut out = Vec::new();
        for piece in input.chunks(split * channels) {
            r.push(piece, &mut out).unwrap();
        }
        r.finish(&mut out).unwrap();
        out
    }

    #[test]
    fn same_rate_is_bit_exact_passthrough() {
        let input = sine(440.0, 48_000, 5_000, 0.5);
        let out = resample_all(&input, 48_000, 48_000, 1, 777);
        assert_eq!(out, input);
        assert!(StreamResampler::new(48_000, 48_000, 2)
            .unwrap()
            .is_passthrough());
    }

    #[test]
    fn output_length_is_exact_for_common_and_awkward_rates() {
        for (i, o) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (96_000, 48_000),
            (192_000, 44_100),
            (8_000, 48_000),
            (22_050, 96_000),
            (44_100, 192_000),
            (88_200, 48_000),
        ] {
            for frames in [1usize, 100, 4_095, 4_096, 50_000, 123_457] {
                let input = vec![0.1f32; frames * 2];
                let out = resample_all(&input, i, o, 2, 3_001);
                let expected = (frames as u64 * o as u64).div_ceil(i as u64) as usize;
                assert_eq!(out.len(), expected * 2, "{i}->{o} {frames} frames");
            }
        }
    }

    #[test]
    fn splitting_the_input_differently_gives_identical_output() {
        let input = interleave(&[
            sine(1_000.0, 44_100, 30_000, 0.8),
            sine(3_000.0, 44_100, 30_000, 0.4),
        ]);
        let a = resample_all(&input, 44_100, 48_000, 2, 17);
        let b = resample_all(&input, 44_100, 48_000, 2, 4_096);
        let c = resample_all(&input, 44_100, 48_000, 2, 30_000);
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn mid_band_sine_is_converted_with_low_distortion() {
        for (i, o) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (96_000, 48_000),
            (44_100, 96_000),
            (192_000, 48_000),
        ] {
            let input = sine(1_000.0, i, i as usize, 0.5);
            let out = resample_all(&input, i, o, 1, 4_096);
            let n = out.len();
            let (amp, rel) = fit(&out, 1_000.0, o, n / 5, n * 4 / 5);
            println!("{i}->{o}: amplitude {:.5}, THD+N {:.1} dB", amp, db(rel));
            assert!(
                (amp - 0.5).abs() < 0.0005,
                "{i}->{o} gain error {}",
                db(amp / 0.5)
            );
            assert!(db(rel) < -80.0, "{i}->{o} residual {:.1} dB", db(rel));
        }
    }

    #[test]
    fn passband_stays_flat_for_cd_to_48k() {
        for hz in [100.0, 1_000.0, 5_000.0, 10_000.0, 15_000.0, 18_000.0] {
            let input = sine(hz, 44_100, 44_100, 0.5);
            let out = resample_all(&input, 44_100, 48_000, 1, 4_096);
            let n = out.len();
            let (amp, _) = fit(&out, hz, 48_000, n / 5, n * 4 / 5);
            println!("{hz} Hz: {:+.3} dB", db(amp / 0.5));
            assert!(
                db(amp / 0.5).abs() < 0.1,
                "{hz} Hz changed by {:.3} dB",
                db(amp / 0.5)
            );
        }
    }

    #[test]
    fn content_above_the_new_nyquist_is_rejected_not_folded_back() {
        // 30 kHz at a 96 kHz source folds to 14.1 kHz at 44.1 kHz unless it is filtered first.
        let input = sine(30_000.0, 96_000, 96_000, 0.5);
        let out = resample_all(&input, 96_000, 44_100, 1, 4_096);
        let n = out.len();
        let (amp, _) = fit(&out, 14_100.0, 44_100, n / 5, n * 4 / 5);
        println!("alias at 14.1 kHz: {:.1} dB re source", db(amp / 0.5));
        assert!(
            db(amp / 0.5) < -60.0,
            "alias only {:.1} dB down",
            db(amp / 0.5)
        );
    }

    #[test]
    fn reset_starts_a_fresh_stream() {
        let input = sine(500.0, 44_100, 20_000, 0.6);
        let mut r = StreamResampler::new(44_100, 48_000, 1).unwrap();
        let mut first = Vec::new();
        r.push(&input[..9_000], &mut first).unwrap();
        r.reset();
        let mut second = Vec::new();
        r.push(&input, &mut second).unwrap();
        r.finish(&mut second).unwrap();
        assert_eq!(second, resample_all(&input, 44_100, 48_000, 1, 20_000));
    }

    #[test]
    fn expansion_stays_bounded_for_extreme_ratios() {
        let mut r = StreamResampler::new(8_000, 384_000, 8).unwrap();
        let mut out = Vec::new();
        let packet = vec![0.1f32; FEED_FRAMES * 8];
        for _ in 0..8 {
            r.push(&packet, &mut out).unwrap();
        }
        assert!(r.pending_samples() < (FEED_FRAMES + 4_096) * 8);
        // 8 kHz -> 384 kHz is 48x: 8 packets of 4,096 frames cannot exceed 48x their length.
        assert!(out.len() <= 8 * FEED_FRAMES * 8 * 48);
    }

    #[test]
    fn identity_map_when_layouts_match() {
        let m = ChannelMap::new(&default_layout(2), 2);
        assert!(m.is_identity());
        let mut out = Vec::new();
        m.apply(&[0.1, 0.2, 0.3, 0.4], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);
        assert!(ChannelMap::new(&default_layout(6), 6).is_identity());
    }

    #[test]
    fn mono_goes_to_both_ears_at_full_level_and_stereo_to_mono_averages() {
        let up = ChannelMap::new(&default_layout(1), 2);
        let mut out = Vec::new();
        up.apply(&[0.5, -0.25], &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.25, -0.25]);
        let down = ChannelMap::new(&default_layout(2), 1);
        out.clear();
        down.apply(&[1.0, 0.0, 0.5, 0.5], &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
        // A mono file on a 5.1 device plays from front left and right only.
        let wide = ChannelMap::new(&default_layout(1), 6);
        assert_eq!(wide.coefficient(0, 0), 1.0);
        assert_eq!(wide.coefficient(1, 0), 1.0);
        for o in 2..6 {
            assert_eq!(wide.coefficient(o, 0), 0.0);
        }
    }

    #[test]
    fn surround_downmix_cannot_exceed_full_scale_and_keeps_every_speaker_audible() {
        for source in [3, 4, 5, 6, 7, 8] {
            let layout = default_layout(source);
            let m = ChannelMap::new(&layout, 2);
            let mut out = Vec::new();
            // Worst case for each ear: every contributing channel at full scale, in phase.
            let frame: Vec<f32> = (0..source).map(|_| 1.0).collect();
            m.apply(&frame, &mut out);
            assert!(
                out.iter().all(|s| *s <= 1.0 + 1e-6),
                "{source}ch peaks {out:?}"
            );
            for (i, role) in layout.iter().enumerate() {
                if *role != Role::Lfe {
                    assert!(
                        m.coefficient(0, i) + m.coefficient(1, i) > 0.0,
                        "{source}ch {role:?} dropped"
                    );
                }
            }
            // The left front speaker never leaks into the right ear.
            assert_eq!(m.coefficient(1, 0), 0.0, "{source}ch");
        }
        // 5.1: LFE is not folded into the stereo mix.
        let m = ChannelMap::new(&default_layout(6), 2);
        assert_eq!(m.coefficient(0, 3), 0.0);
        assert_eq!(m.coefficient(1, 3), 0.0);
    }

    #[test]
    fn stereo_content_on_a_surround_device_stays_front_left_and_right() {
        let m = ChannelMap::new(&default_layout(2), 8);
        assert_eq!(m.coefficient(0, 0), 1.0);
        assert_eq!(m.coefficient(1, 1), 1.0);
        for o in 2..8 {
            assert_eq!(m.coefficient(o, 0) + m.coefficient(o, 1), 0.0);
        }
    }

    #[test]
    fn named_layouts_map_by_speaker_not_by_position() {
        use Role::*;
        // A quad file on a 5.1 device: its back speakers go to the 5.1 back speakers.
        let m = ChannelMap::new(&[FrontLeft, FrontRight, BackLeft, BackRight], 6);
        assert_eq!(m.coefficient(4, 2), 1.0);
        assert_eq!(m.coefficient(5, 3), 1.0);
        assert_eq!(m.coefficient(2, 2), 0.0);
    }

    #[test]
    fn equal_power_fades_sum_to_constant_power() {
        for step in 0..=100 {
            let t = step as f32 / 100.0;
            let p = fade_in_gain(t).powi(2) + fade_out_gain(t).powi(2);
            assert!((p - 1.0).abs() < 1e-6, "t={t} power {p}");
        }
        assert_eq!(fade_in_gain(0.0), 0.0);
        assert!((fade_out_gain(1.0)).abs() < 1e-6);
    }

    #[test]
    fn peak_guard_is_transparent_until_a_frame_would_clip() {
        let mut guard = PeakGuard::new(48_000);
        let mut frame = [0.99, -0.99];
        guard.process(&mut frame);
        assert_eq!(frame, [0.99, -0.99]);
        assert!(!guard.engaged());
    }

    #[test]
    fn peak_guard_holds_identical_full_scale_signals_at_full_scale() {
        // The worst case: two identical full-scale sines under an equal-power fade sum to
        // cos+sin = up to 1.414 at the midpoint.
        let rate = 48_000u32;
        let frames = rate as usize * 2;
        let mut guard = PeakGuard::new(rate);
        let (mut peak, mut peak_unguarded) = (0.0f32, 0.0f32);
        for n in 0..frames {
            let x = (TAU * 440.0 * n as f64 / rate as f64).sin() as f32;
            let t = n as f32 / frames as f32;
            let mixed = x * fade_out_gain(t) + x * fade_in_gain(t);
            peak_unguarded = peak_unguarded.max(mixed.abs());
            let mut frame = [mixed, mixed];
            guard.process(&mut frame);
            peak = peak.max(frame[0].abs());
        }
        assert!(
            peak_unguarded > 1.3,
            "worst case really exceeds full scale: {peak_unguarded}"
        );
        assert!(peak <= 1.0, "guarded peak {peak}");
    }

    #[test]
    fn peak_guard_recovers_to_unity_after_the_overshoot() {
        let mut guard = PeakGuard::new(48_000);
        let mut frame = [1.4];
        guard.process(&mut frame);
        assert!((frame[0] - 1.0).abs() < 1e-6);
        assert!(guard.engaged());
        for _ in 0..(0.07 * 48_000.0) as usize {
            let mut f = [0.2];
            guard.process(&mut f);
        }
        assert!(!guard.engaged());
        let mut f = [0.2];
        guard.process(&mut f);
        assert_eq!(f, [0.2]);
    }
}
