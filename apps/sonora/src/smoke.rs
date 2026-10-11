//! Self-ending checks of the engine on real files, with no window and no audio device. They run
//! the same code the player runs (decoder, channel mapping, resampler, crossfade mixer) and report
//! what came out, so a release build can be checked without listening to it.

use crate::{
    engine::{Engine, Start, Stream, Tag},
    player::FileOpener,
    player::Opener,
};
use std::path::PathBuf;

const CHUNK_DIVISOR: u32 = 50;

/// CPU time this process has used and its peak memory, for the self-ending workloads. Not a
/// measurement of the GUI: only of what the named workload did.
pub fn process_stats() -> serde_json::Value {
    #[cfg(windows)]
    {
        use windows::Win32::{
            Foundation::FILETIME,
            System::{
                ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
                Threading::{GetCurrentProcess, GetProcessTimes},
            },
        };
        let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        unsafe {
            let process = GetCurrentProcess();
            let (mut created, mut exited, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            let times = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
            let mut memory = PROCESS_MEMORY_COUNTERS {
                cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            let counters = GetProcessMemoryInfo(process, &mut memory, memory.cb);
            serde_json::json!({
                "cpu_ms": times.is_ok().then(|| (ticks(kernel) + ticks(user)) as f64 / 10_000.0),
                "peak_working_set_bytes": counters.is_ok().then_some(memory.PeakWorkingSetSize),
                "working_set_bytes": counters.is_ok().then_some(memory.WorkingSetSize),
            })
        }
    }
    #[cfg(not(windows))]
    serde_json::json!({})
}

fn open(path: &std::path::Path, rate: u32, channels: usize) -> Result<Stream, String> {
    FileOpener.open(path, 0.0, rate, channels)
}

/// Frames a track yields on its own at the device format.
fn solo_frames(path: &std::path::Path, rate: u32, channels: usize) -> Result<(u64, Tag), String> {
    let mut stream = open(path, rate, channels)?;
    let mut buf = Vec::new();
    let mut frames = 0u64;
    loop {
        buf.clear();
        let (n, end) = stream.pull(4096, &mut buf)?;
        frames += n as u64;
        if n == 0 || matches!(end, crate::engine::PullEnd::Eof) {
            break;
        }
    }
    Ok((frames, (*stream.tag).clone()))
}

fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
}

/// Plays `paths` in order through the engine into memory-free statistics: the programme length
/// against the sum of the tracks, where each hand-off happened, the overlap, level continuity and
/// the memory the engine held at its peak.
pub fn mix_smoke(
    paths: &[PathBuf],
    crossfade: f32,
    rate: u32,
    channels: usize,
) -> Result<serde_json::Value, String> {
    if paths.len() < 2 {
        return Err("Give at least two audio files".into());
    }
    let started = std::time::Instant::now();
    let mut solo = Vec::new();
    for p in paths {
        solo.push(solo_frames(p, rate, channels)?);
    }
    let sum: u64 = solo.iter().map(|(f, _)| *f).sum();

    let mut engine = Engine::new(rate, channels);
    engine.set_crossfade(crossfade);
    engine.start(open(&paths[0], rate, channels)?, Start::Cut);
    let mut next = 1;
    engine.set_next(Some(open(&paths[next], rate, channels)?));

    let chunk = (rate / CHUNK_DIVISOR) as usize;
    let mut out = Vec::new();
    let mut total_frames = 0u64;
    let mut promotions = Vec::new();
    let mut peak = 0.0f32;
    let mut over_full_scale = 0u64;
    let mut max_step = 0.0f32;
    let mut last: Option<f32> = None;
    let mut worst_buffer = 0usize;
    let mut fade_frames_seen = 0u64;
    let mut fading_now = false;
    let mut level_windows = Vec::new();
    loop {
        out.clear();
        let r = engine.render(chunk, &mut out);
        worst_buffer = worst_buffer.max(engine.buffered_samples());
        if let Some((_, tag)) = &r.promoted {
            promotions.push(serde_json::json!({
                "track": tag.path.file_name().map(|n| n.to_string_lossy().into_owned()),
                "at_frame": total_frames,
                "at_seconds": total_frames as f64 / rate as f64,
            }));
            next += 1;
            engine.set_next(
                paths
                    .get(next)
                    .map(|p| open(p, rate, channels))
                    .transpose()?,
            );
        }
        if r.fading.is_some() || fading_now {
            fade_frames_seen += r.frames as u64;
        }
        fading_now = r.fading.is_some();
        for (i, s) in out.iter().enumerate() {
            peak = peak.max(s.abs());
            if s.abs() > 1.0 {
                over_full_scale += 1;
            }
            // Channel 0 only: the step between consecutive frames of one channel.
            if i % channels == 0 {
                if let Some(prev) = last {
                    max_step = max_step.max((s - prev).abs());
                }
                last = Some(*s);
            }
        }
        if !out.is_empty() {
            level_windows.push((total_frames, rms(&out), r.fading.is_some()));
        }
        total_frames += r.frames as u64;
        if let Some((tag, error)) = r.error {
            return Err(format!("{}: {error}", tag.path.display()));
        }
        if r.ended {
            break;
        }
    }
    let overlap = sum.saturating_sub(total_frames);
    // Level inside the blend against the tracks' own level just outside it.
    let blended: Vec<f64> = level_windows.iter().filter(|w| w.2).map(|w| w.1).collect();
    let outside: Vec<f64> = level_windows.iter().filter(|w| !w.2).map(|w| w.1).collect();
    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    Ok(serde_json::json!({
        "device_format": {"rate": rate, "channels": channels},
        "crossfade_seconds": crossfade,
        "tracks": solo.iter().zip(paths).map(|((frames, tag), path)| serde_json::json!({
            "file": path.file_name().map(|n| n.to_string_lossy().into_owned()),
            "codec": tag.info.codec,
            "source_rate": tag.info.rate,
            "source_channels": tag.info.channels,
            "resampled": tag.resampled,
            "channel_conversion": tag.channel_conversion,
            "frames_at_device_format": frames,
        })).collect::<Vec<_>>(),
        "sum_of_tracks_frames": sum,
        "programme_frames": total_frames,
        "overlap_frames": overlap,
        "overlap_seconds": overlap as f64 / rate as f64,
        "frames_rendered_while_fading": fade_frames_seen,
        "hand_offs": promotions,
        "peak": peak,
        "samples_above_full_scale": over_full_scale,
        "max_step_channel0": max_step,
        "mean_rms_during_blend": mean(&blended),
        "mean_rms_outside_blend": mean(&outside),
        "peak_engine_buffer_samples": worst_buffer,
        "peak_engine_buffer_bytes": worst_buffer * 4,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "audio_seconds_per_wall_second": (sum as f64 + total_frames as f64) / rate as f64 / started.elapsed().as_secs_f64(),
        "process": process_stats(),
    }))
}
