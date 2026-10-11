//! A self-ending headless benchmark: `luma --bench <folder> [--think-ms N] [--out file.json]`.
//!
//! It measures what Luma itself controls, using the real decode worker and the real decoders on
//! the files in a folder: how long each file takes to decode, how quickly a step through the
//! folder is answered when the neighbour was prefetched, and how that changes with the prefetch
//! policy. It opens no window, writes nothing but the optional JSON, and never touches the files.
//!
//! What it does not measure: GPU texture upload, painting, window creation, process start-up
//! from the shell, or anything about the monitor. Numbers apply to the files and machine they
//! were taken on, with the operating system's file cache in whatever state it was in.
use crate::{
    decode::{self, Cancel},
    model::Album,
    worker::Worker,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Which neighbour the worker is asked to read ahead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// The 0.1.0 behaviour: always the next photo, whichever way the person is going.
    ForwardOnly,
    /// 0.2.0: the neighbour in the direction of travel.
    DirectionAware,
}
impl Policy {
    fn name(self) -> &'static str {
        match self {
            Policy::ForwardOnly => "forward-only (0.1.0 policy)",
            Policy::DirectionAware => "direction-aware (0.2.0 policy)",
        }
    }
    fn neighbour(self, album: &Album, direction: isize) -> Option<PathBuf> {
        match self {
            Policy::ForwardOnly => album.next_path(),
            Policy::DirectionAware => album.neighbour_path(direction),
        }
    }
}

fn wait_for(worker: &Worker, generation: u64, limit: Duration) -> Option<crate::worker::Reply> {
    let start = Instant::now();
    loop {
        if let Some(reply) = worker.take()
            && reply.generation == generation
        {
            return Some(reply);
        }
        if start.elapsed() > limit {
            return None;
        }
        std::thread::yield_now();
    }
}

fn summary(mut ms: Vec<f64>) -> Value {
    ms.sort_by(|a, b| a.total_cmp(b));
    if ms.is_empty() {
        return json!({"steps": 0});
    }
    let pick = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
    json!({
        "steps": ms.len(),
        "min_ms": ms[0],
        "median_ms": pick(0.5),
        "p95_ms": pick(0.95),
        "max_ms": ms[ms.len() - 1],
        "all_ms": ms,
    })
}

/// Step through the folder `steps` times in `direction`, waiting `think` between steps (a person
/// looking at the photo, during which the read-ahead runs), and time each answer.
fn walk(files: &Path, policy: Policy, direction: isize, steps: usize, think: Duration) -> Value {
    let worker = Worker::new(eframe::egui::Context::default());
    let mut album = Album::open(files).expect("album");
    if direction < 0 {
        album.index = album.files.len() - 1;
    }
    let first = album.current().cloned().expect("a photo");
    let generation = worker.navigate(first, policy.neighbour(&album, direction));
    let opened = wait_for(&worker, generation, Duration::from_secs(60));
    assert!(
        opened.is_some_and(|r| r.photo.is_ok()),
        "the first photo decodes"
    );
    std::thread::sleep(think);
    let mut latencies = Vec::new();
    let mut failures = 0;
    for _ in 0..steps {
        album.step(direction);
        let path = album.current().cloned().expect("a photo");
        let started = Instant::now();
        let generation = worker.navigate(path, policy.neighbour(&album, direction));
        match wait_for(&worker, generation, Duration::from_secs(60)) {
            Some(reply) if reply.photo.is_ok() => {
                latencies.push(started.elapsed().as_secs_f64() * 1000.0)
            }
            _ => failures += 1,
        }
        std::thread::sleep(think);
    }
    let mut value = summary(latencies);
    value["failures"] = json!(failures);
    value["policy"] = json!(policy.name());
    value["direction"] = json!(if direction > 0 { "forward" } else { "backward" });
    value["think_ms"] = json!(think.as_millis());
    value
}

/// Hold the arrow key: many requests with no pause. Only the last may be answered.
fn rapid(files: &Path, count: usize) -> Value {
    let worker = Worker::new(eframe::egui::Context::default());
    let mut album = Album::open(files).expect("album");
    let started = Instant::now();
    let mut last = 0;
    for _ in 0..count {
        album.step(1);
        let path = album.current().cloned().expect("a photo");
        last = worker.navigate(path, album.next_path());
    }
    let issued = started.elapsed().as_secs_f64() * 1000.0;
    let reply = wait_for(&worker, last, Duration::from_secs(120));
    let done = started.elapsed().as_secs_f64() * 1000.0;
    // Anything still queued would arrive now; only the latest request may have an answer.
    std::thread::sleep(Duration::from_millis(200));
    let stale = worker.take().is_some_and(|r| r.generation != last);
    json!({
        "requests": count,
        "all_issued_in_ms": issued,
        "last_answer_after_ms": done,
        "last_answered": reply.is_some_and(|r| r.photo.is_ok()),
        "stale_answer_delivered": stale,
    })
}

#[cfg(windows)]
fn memory() -> Value {
    use windows::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: a valid pseudo-handle and a correctly sized, writable struct.
    let ok =
        unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) }.is_ok();
    if !ok {
        return json!({"measured": false});
    }
    json!({
        "measured": true,
        "peak_working_set_mb": counters.PeakWorkingSetSize as f64 / 1_048_576.0,
        "working_set_mb_at_end": counters.WorkingSetSize as f64 / 1_048_576.0,
        "peak_pagefile_mb": counters.PeakPagefileUsage as f64 / 1_048_576.0,
    })
}

pub fn run(folder: &Path, think_ms: u64) -> Result<Value, String> {
    let album = Album::open(folder)?;
    if album.files.len() < 3 {
        return Err("The benchmark folder needs at least three photos.".into());
    }
    let total: u64 = album
        .files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let mut decodes = Vec::new();
    for path in &album.files {
        let started = Instant::now();
        let result = decode::decode(path, &Cancel::never());
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        decodes.push(match result {
            Ok(p) => json!({
                "file": path.file_name().map(|n| n.to_string_lossy().into_owned()),
                "decode_ms": ms, "width": p.width, "height": p.height,
                "format": p.format, "file_bytes": p.file_bytes, "rgba_bytes": p.byte_len(),
            }),
            Err(e) => json!({"file": path.file_name().map(|n| n.to_string_lossy().into_owned()), "error": e}),
        });
    }
    let think = Duration::from_millis(think_ms);
    let steps = (album.files.len() - 1).min(10);
    let mut walks = Vec::new();
    for policy in [Policy::ForwardOnly, Policy::DirectionAware] {
        for direction in [1, -1] {
            walks.push(walk(folder, policy, direction, steps, think));
        }
    }
    Ok(json!({
        "workload": "headless decode worker over a folder; no window, no GPU upload, no painting",
        "folder": folder.display().to_string(),
        "files": album.files.len(),
        "total_file_bytes": total,
        "decode_each_file_once": decodes,
        "walks": walks,
        "rapid_stepping": rapid(folder, 40),
        "process_memory": memory(),
    }))
}
