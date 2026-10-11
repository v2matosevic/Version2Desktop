use crate::dsp::{default_layout, Role};
use std::{fs::File, io::Cursor, path::Path};
use symphonia::core::{
    audio::{Channels, SampleBuffer},
    codecs::{CodecRegistry, Decoder, DecoderOptions},
    errors::Error,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::{MetadataOptions, StandardTagKey},
    probe::Hint,
    units::Time,
};

pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const MAX_PACKET_SAMPLES: usize = 1_048_576;
/// Damaged packets in a row that are skipped before the file is declared unplayable. One bad
/// MP3 frame is common in real libraries and should cost a click, not the whole track.
pub const MAX_CONSECUTIVE_DAMAGED: u32 = 32;

#[derive(Clone, Debug, Default)]
pub struct Metadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    pub artwork: Option<(usize, usize, Vec<u8>)>,
}

/// What the file really is, as reported by its container and decoder. Nothing here is guessed
/// from the extension.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamInfo {
    /// Short display name: MP3, FLAC, AAC, Vorbis, Opus, ALAC, PCM.
    pub codec: &'static str,
    pub lossless: bool,
    pub bits: Option<u32>,
    pub float: bool,
    pub rate: u32,
    pub channels: u16,
    /// Seconds, 0 when the container does not declare a length.
    pub duration: f64,
}

pub struct AudioDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track: u32,
    pub rate: u32,
    pub channels: u16,
    pub layout: Vec<Role>,
    pub metadata: Metadata,
    pub codec: String,
    pub info: StreamInfo,
    /// Packets skipped because they could not be decoded.
    pub damaged_packets: u32,
    skip_until: Option<f64>,
    time_base: Option<symphonia::core::units::TimeBase>,
    scratch: Option<SampleBuffer<f32>>,
    /// Frame index of the next frame this decoder will return (the seek target after a seek).
    frame: u64,
    /// For streams whose declared length is exact, the length the file ends at.
    exact_frames: Option<u64>,
}

fn seconds(t: Time) -> f64 {
    t.seconds as f64 + t.frac
}

fn roles(channels: Option<Channels>, count: usize) -> Vec<Role> {
    let Some(set) = channels else {
        return default_layout(count);
    };
    let layout: Vec<Role> = set
        .iter()
        .map(|c| match c {
            c if c == Channels::FRONT_LEFT => Role::FrontLeft,
            c if c == Channels::FRONT_RIGHT => Role::FrontRight,
            c if c == Channels::FRONT_CENTRE => Role::FrontCenter,
            c if c == Channels::LFE1 => Role::Lfe,
            c if c == Channels::REAR_LEFT => Role::BackLeft,
            c if c == Channels::REAR_RIGHT => Role::BackRight,
            c if c == Channels::FRONT_LEFT_CENTRE => Role::FrontLeftCenter,
            c if c == Channels::FRONT_RIGHT_CENTRE => Role::FrontRightCenter,
            c if c == Channels::REAR_CENTRE => Role::BackCenter,
            c if c == Channels::SIDE_LEFT => Role::SideLeft,
            c if c == Channels::SIDE_RIGHT => Role::SideRight,
            _ => Role::Other,
        })
        .collect();
    if layout.len() == count {
        layout
    } else {
        default_layout(count)
    }
}

impl AudioDecoder {
    pub fn open(path: &Path, artwork: bool) -> Result<Self, String> {
        crate::paths::local(path)?;
        let file = File::open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
        let size = file.metadata().map_err(|e| e.to_string())?;
        if !size.is_file() || size.len() == 0 || size.len() > MAX_FILE_BYTES {
            return Err("Choose a nonempty regular file up to 16 GiB".into());
        }
        let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let mut probe = symphonia::default::get_probe()
            .format(
                &hint,
                stream,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions {
                    limit_metadata_bytes: symphonia::core::meta::Limit::Maximum(1024 * 1024),
                    limit_visual_bytes: symphonia::core::meta::Limit::Maximum(8 * 1024 * 1024),
                },
            )
            .map_err(|e| format!("Unrecognized or unsupported audio: {e}"))?;
        let mut meta = Metadata {
            title: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            ..Default::default()
        };
        if let Some(m) = probe.metadata.get().as_ref().and_then(|m| m.current()) {
            read_meta(m, &mut meta, artwork);
        }
        if let Some(m) = probe.format.metadata().current() {
            read_meta(m, &mut meta, artwork);
        }
        let track = probe
            .format
            .default_track()
            .filter(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .or_else(|| {
                probe
                    .format
                    .tracks()
                    .iter()
                    .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            })
            .ok_or("No playable audio track")?;
        let params = track.codec_params.clone();
        let track_id = track.id;
        let rate = params.sample_rate.unwrap_or(48000);
        let channels = params.channels.map(|c| c.count()).unwrap_or(2);
        if !(8000..=384000).contains(&rate) || !(1..=8).contains(&channels) {
            return Err("Unsupported sample rate or channel layout".into());
        }
        meta.duration = params
            .time_base
            .zip(params.n_frames)
            .map(|(tb, n)| seconds(tb.calc_time(n)))
            .unwrap_or(0.0);
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_all::<symphonia_adapter_libopus::OpusDecoder>();
        let decoder = registry
            .make(&params, &DecoderOptions::default())
            .map_err(|e| format!("Unsupported codec: {e}"))?;
        let short = registry
            .get_codec(params.codec)
            .map(|d| d.short_name)
            .unwrap_or("unknown");
        let (name, lossless, float) = match short {
            "mp3" => ("MP3", false, false),
            "aac" => ("AAC", false, false),
            "vorbis" => ("Vorbis", false, false),
            "opus" => ("Opus", false, false),
            "flac" => ("FLAC", true, false),
            "alac" => ("ALAC", true, false),
            s if s.starts_with("pcm") => ("PCM", true, s.contains("f32") || s.contains("f64")),
            _ => ("Audio", false, false),
        };
        // Formats whose declared length is the exact length of the audio (the demuxer reads it
        // from the stream header or the last Ogg page, not from a bitrate estimate). Anything
        // the decoder emits beyond it is encoder padding, which gapless playback must not play.
        let exact_frames = matches!(name, "FLAC" | "PCM" | "Vorbis" | "ALAC")
            .then(|| {
                params
                    .time_base
                    .zip(params.n_frames)
                    .map(|(tb, n)| (seconds(tb.calc_time(n)) * rate as f64).round() as u64)
            })
            .flatten()
            .filter(|n| *n > 0);
        let info = StreamInfo {
            codec: name,
            lossless,
            bits: params.bits_per_sample.or(params.bits_per_coded_sample),
            float,
            rate,
            channels: channels as u16,
            duration: meta.duration,
        };
        Ok(Self {
            format: probe.format,
            decoder,
            track: track_id,
            rate,
            channels: channels as u16,
            layout: roles(params.channels, channels),
            metadata: meta,
            codec: format!("{:?}", params.codec),
            info,
            damaged_packets: 0,
            skip_until: None,
            time_base: params.time_base,
            scratch: None,
            frame: 0,
            exact_frames,
        })
    }

    pub fn seek(&mut self, position: f64) -> Result<(), String> {
        if !position.is_finite() {
            return Err("Seek position must be finite".into());
        }
        let position = if self.metadata.duration > 0.0 {
            position.clamp(0.0, self.metadata.duration)
        } else {
            position.max(0.0)
        };
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::from(position),
                    track_id: Some(self.track),
                },
            )
            .map_err(|e| format!("This file cannot seek: {e}"))?;
        self.decoder.reset();
        self.skip_until = Some(position);
        self.frame = (position * self.rate as f64).round() as u64;
        Ok(())
    }

    /// Next decoded block as interleaved `f32`, or `None` at the end of the file.
    pub fn next_packet(&mut self) -> Result<Option<Vec<f32>>, String> {
        let mut out = Vec::new();
        Ok(self.read_into(&mut out)?.then_some(out))
    }

    /// Appends the next decoded block (interleaved, finite values) to `out`. `Ok(false)` means
    /// the end of the file; nothing was appended.
    pub fn read_into(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        if self.exact_frames.is_some_and(|limit| self.frame >= limit) {
            return Ok(false);
        }
        // A bounded packet budget also prevents non-audio/malformed streams from monopolizing a worker.
        let mut damaged_run = 0;
        for _ in 0..4096 {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(false)
                }
                Err(e) => return Err(format!("Damaged audio stream: {e}")),
            };
            if packet.track_id() != self.track {
                continue;
            }
            let timestamp = self
                .time_base
                .map(|tb| seconds(tb.calc_time(packet.ts())))
                .unwrap_or(0.0);
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(Error::DecodeError(_)) => {
                    damaged_run += 1;
                    self.damaged_packets += 1;
                    if damaged_run > MAX_CONSECUTIVE_DAMAGED {
                        return Err("Audio decode failed: too many damaged packets in a row".into());
                    }
                    continue;
                }
                Err(e) => return Err(format!("Audio decode failed: {e}")),
            };
            damaged_run = 0;
            let spec = *decoded.spec();
            if spec.rate != self.rate || spec.channels.count() != self.channels as usize {
                return Err("Midstream audio layout change is unsupported".into());
            }
            let wanted = decoded.capacity().saturating_mul(self.channels as usize);
            if wanted > MAX_PACKET_SAMPLES {
                return Err("Audio packet exceeds the safety limit".into());
            }
            if self.scratch.as_ref().is_none_or(|s| s.capacity() < wanted) {
                self.scratch = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
            }
            let scratch = self.scratch.as_mut().expect("scratch buffer");
            scratch.copy_interleaved_ref(decoded);
            let samples = scratch.samples();
            let channels = self.channels as usize;
            let skip = self
                .skip_until
                .map(|target| {
                    (((target - timestamp).max(0.0) * self.rate as f64).round() as usize)
                        .saturating_mul(channels)
                })
                .unwrap_or(0)
                .min(samples.len());
            if skip == samples.len() {
                continue;
            }
            self.skip_until = None;
            let mut end = samples.len();
            if let Some(limit) = self.exact_frames {
                let left = limit.saturating_sub(self.frame) as usize;
                end = end.min(skip + left * channels);
            }
            let kept = &samples[skip..end];
            // Float files may carry finite values above full scale; volume can bring them back
            // into range, so only NaN and infinity are removed here. The device edge clamps last.
            out.extend(kept.iter().map(|s| if s.is_finite() { *s } else { 0.0 }));
            self.frame += (kept.len() / channels) as u64;
            return Ok(true);
        }
        Err("Too many packets without usable audio".into())
    }
}

fn read_meta(rev: &symphonia::core::meta::MetadataRevision, meta: &mut Metadata, artwork: bool) {
    for tag in rev.tags() {
        let target = match tag.std_key {
            Some(StandardTagKey::TrackTitle) => &mut meta.title,
            Some(StandardTagKey::Artist) => &mut meta.artist,
            Some(StandardTagKey::Album) => &mut meta.album,
            _ => continue,
        };
        *target = tag.value.to_string().chars().take(1024).collect();
    }
    if artwork && meta.artwork.is_none() {
        if let Some(visual) = rev
            .visuals()
            .iter()
            .find(|v| v.data.len() <= 8 * 1024 * 1024)
        {
            if let Ok(mut reader) =
                image::ImageReader::new(Cursor::new(&visual.data)).with_guessed_format()
            {
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(8192);
                limits.max_image_height = Some(8192);
                limits.max_alloc = Some(64 * 1024 * 1024);
                reader.limits(limits);
                if let Ok(img) = reader.decode() {
                    let img = if img.width() > 512 || img.height() > 512 {
                        img.thumbnail(512, 512)
                    } else {
                        img
                    }
                    .into_rgba8();
                    meta.artwork =
                        Some((img.width() as usize, img.height() as usize, img.into_raw()));
                }
            }
        }
    }
}

#[derive(serde::Serialize)]
pub struct DecodeReport {
    pub path: String,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub duration_seconds: f64,
    pub peak: f32,
    pub title: String,
    pub decode_elapsed_ms: f64,
}
pub fn smoke(path: &Path) -> Result<DecodeReport, String> {
    let started = std::time::Instant::now();
    let mut d = AudioDecoder::open(path, true)?;
    let mut frames = 0;
    let mut peak = 0.0_f32;
    while let Some(samples) = d.next_packet()? {
        frames += (samples.len() / d.channels as usize) as u64;
        for s in samples {
            peak = peak.max(s.abs());
        }
    }
    if frames == 0 {
        return Err("No decoded audio frames".into());
    }
    Ok(DecodeReport {
        path: path.to_string_lossy().into(),
        codec: d.codec,
        sample_rate: d.rate,
        channels: d.channels,
        frames,
        duration_seconds: frames as f64 / d.rate as f64,
        peak,
        title: d.metadata.title,
        decode_elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}
