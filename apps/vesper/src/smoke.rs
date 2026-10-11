use crate::mpv::{Api, Player, SoftwareRenderer};
use serde::Serialize;
use std::{
    path::Path,
    time::{Duration, Instant},
};

#[derive(Serialize, Default)]
pub struct SmokeResult {
    pub ok: bool,
    pub file: String,
    pub decoder: String,
    pub ffmpeg: String,
    pub video_codec: String,
    pub audio_codec: String,
    pub container: String,
    pub frames_rendered: usize,
    pub distinct_frames: usize,
    pub frame_checksum: u64,
    pub width: usize,
    pub height: usize,
    pub audio_samples: usize,
    pub audio_peak: u16,
    pub audio_output: String,
    pub hwdec: String,
    pub elapsed_ms: u128,
    pub initialization_ms: u128,
    pub dll_load_ms: u128,
    pub player_init_ms: u128,
    pub renderer_init_ms: u128,
    pub load_command_ms: u128,
    pub file_started_ms: Option<u128>,
    pub file_loaded_ms: Option<u128>,
    pub first_decoded_frame_ms: Option<u128>,
    pub last_position_seconds: Option<f64>,
    pub paused: Option<String>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

/// Auto-ending decoder/output evidence; deliberately different from a perceptual speaker check.
pub fn run(file: &Path, dll: &Path, out: &Path, device: bool) -> SmokeResult {
    let started = Instant::now();
    let mut result = SmokeResult {
        file: file.display().to_string(),
        ..Default::default()
    };
    let run = (|| -> Result<(), String> {
        std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
        let pcm = out
            .join("decoded.wav")
            .canonicalize()
            .unwrap_or_else(|_| out.join("decoded.wav"));
        let pcm_path = if pcm.is_absolute() {
            pcm
        } else {
            std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(pcm)
        };
        let audio = pcm_path.to_str().ok_or("Invalid PCM output path.")?;
        let opts = if device {
            vec![
                ("ao", "wasapi"),
                ("volume", "0"),
                ("hwdec", "no"),
                ("keep-open", "no"),
            ]
        } else {
            vec![
                ("ao", "pcm"),
                ("ao-pcm-file", audio),
                ("ao-pcm-waveheader", "yes"),
                ("audio-format", "s16"),
                ("audio-samplerate", "48000"),
                ("hwdec", "no"),
                ("keep-open", "no"),
            ]
        };
        let phase = Instant::now();
        let api = Api::load(dll)?;
        result.dll_load_ms = phase.elapsed().as_millis();
        let phase = Instant::now();
        let player = Player::new(api, &opts)?;
        result.player_init_ms = phase.elapsed().as_millis();
        result.decoder = player.string("mpv-version").unwrap_or_default();
        result.ffmpeg = player.string("ffmpeg-version").unwrap_or_default();
        let phase = Instant::now();
        let mut renderer = SoftwareRenderer::new(player.clone(), 320, 180)?;
        result.renderer_init_ms = phase.elapsed().as_millis();
        let phase = Instant::now();
        player.open(file, 0.0)?;
        result.load_command_ms = phase.elapsed().as_millis();
        result.initialization_ms = started.elapsed().as_millis();
        // Loading the DLL/context on a busy disk is outside the bounded fixture
        // playback window. Report that cost rather than silently skipping decode.
        let decode_started = Instant::now();
        let mut loaded = false;
        let mut done = false;
        let mut previous = 0;
        while decode_started.elapsed() < Duration::from_secs(15) {
            for (id, error) in player.poll() {
                if let Some(e) = error {
                    return Err(e);
                }
                if id == 8 {
                    loaded = true;
                    result.file_loaded_ms = Some(decode_started.elapsed().as_millis());
                }
                if id == 6 {
                    result.file_started_ms = Some(decode_started.elapsed().as_millis());
                }
                if id == 7 {
                    done = true;
                }
            }
            if loaded {
                if result.video_codec.is_empty() {
                    result.video_codec = player.string("video-codec").unwrap_or_default();
                }
                if result.audio_codec.is_empty() {
                    result.audio_codec = player.string("audio-codec").unwrap_or_default();
                }
                if result.container.is_empty() {
                    result.container = player.string("file-format").unwrap_or_default();
                }
                if result.width == 0 {
                    result.width = player.number("video-params/w").unwrap_or(0.0) as usize;
                    result.height = player.number("video-params/h").unwrap_or(0.0) as usize;
                }
                if result.audio_output.is_empty() {
                    result.audio_output = player.string("current-ao").unwrap_or_default();
                }
                result.hwdec = player.string("hwdec-current").unwrap_or_default();
                result.last_position_seconds = player.number("time-pos");
                result.paused = player.string("pause");
            }
            if renderer.draw()? && loaded {
                if result.first_decoded_frame_ms.is_none() {
                    result.first_decoded_frame_ms = Some(decode_started.elapsed().as_millis());
                }
                let checksum = renderer.pixels.iter().fold(0xcbf29ce484222325_u64, |h, b| {
                    (h ^ *b as u64).wrapping_mul(0x100000001b3)
                });
                if checksum != previous {
                    result.distinct_frames += 1;
                    previous = checksum;
                }
                result.frame_checksum = checksum;
                result.frames_rendered += 1;
                if result.frames_rendered == 3 {
                    write_png(&out.join("frame.png"), &renderer.pixels, 320, 180)?;
                }
            }
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        result.warnings = player.messages();
        if !loaded {
            return Err("The decoder did not load the file.".into());
        }
        if !done {
            return Err("Smoke check reached the 15 second bound; use a short fixture.".into());
        }
        drop(renderer);
        drop(player); // Finish the WAV header before inspecting samples.
        if !device && !result.audio_codec.is_empty() {
            let (samples, peak) =
                pcm_evidence(&std::fs::read(&pcm_path).map_err(|e| e.to_string())?)?;
            result.audio_samples = samples;
            result.audio_peak = peak;
            if !result.audio_codec.is_empty() && peak == 0 {
                return Err("Audio decoded to silence.".into());
            }
        } else if device && result.audio_output != "wasapi" {
            return Err("WASAPI device output did not initialize.".into());
        }
        if result.frames_rendered < 2 || result.distinct_frames < 2 {
            return Err("No changing displayed frame pixels were proven.".into());
        }
        Ok(())
    })();
    result.elapsed_ms = started.elapsed().as_millis();
    result.ok = run.is_ok();
    result.error = run.err();
    result
}

pub fn write_png(path: &Path, bgr: &[u8], width: u32, height: u32) -> Result<(), String> {
    let rgb: Vec<u8> = bgr
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0]])
        .collect();
    let mut enc = png::Encoder::new(
        std::fs::File::create(path).map_err(|e| e.to_string())?,
        width,
        height,
    );
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .map_err(|e| e.to_string())?
        .write_image_data(&rgb)
        .map_err(|e| e.to_string())
}

fn pcm_evidence(data: &[u8]) -> Result<(usize, u16), String> {
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("Invalid decoded WAV output.".into());
    }
    let mut i = 12;
    let mut format_ok = false;
    while i + 8 <= data.len() {
        let n = u32::from_le_bytes(data[i + 4..i + 8].try_into().unwrap()) as usize;
        let end = (i + 8).checked_add(n).ok_or("Oversized WAV chunk.")?;
        if end > data.len() {
            return Err("Truncated decoded WAV.".into());
        }
        if &data[i..i + 4] == b"fmt " && n >= 16 {
            let fmt = u16::from_le_bytes(data[i + 8..i + 10].try_into().unwrap());
            let bits = u16::from_le_bytes(data[i + 22..i + 24].try_into().unwrap());
            format_ok = (fmt == 1 || fmt == 65534) && bits == 16;
        }
        if &data[i..i + 4] == b"data" {
            if !format_ok {
                return Err("Smoke audio output is not PCM16.".into());
            }
            let peak = data[i + 8..end]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
                .max()
                .unwrap_or(0);
            return Ok((n / 2, peak));
        }
        i = end + n % 2;
    }
    Err("Decoded WAV has no audio samples.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncated_pcm_is_rejected() {
        assert!(pcm_evidence(b"RIFF\0\0\0\0WAVEdata\xff\xff\xff\xff").is_err());
    }
}
