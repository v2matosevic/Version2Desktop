//! Startup/first-file timeline and in-process resource counters for the auto-ending
//! benchmark modes. Nothing here runs in normal playback beyond a few `Instant` reads and a
//! bounded in-memory list; no value is written anywhere unless a benchmark flag asks for it.
use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};

static START: OnceLock<Instant> = OnceLock::new();
static MARKS: Mutex<Vec<(&'static str, f64)>> = Mutex::new(Vec::new());
const MAX_MARKS: usize = 64;

/// Call first thing in `main`. Later calls keep the original origin.
pub fn init() {
    START.get_or_init(Instant::now);
}

/// Milliseconds since `init`, or 0 before it.
pub fn since_start_ms() -> f64 {
    START
        .get()
        .map(|s| s.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Record the first time each label is reached; repeats and overflow are ignored.
pub fn mark(label: &'static str) {
    let at = since_start_ms();
    if let Ok(mut marks) = MARKS.lock()
        && marks.len() < MAX_MARKS
        && !marks.iter().any(|(l, _)| *l == label)
    {
        marks.push((label, at));
    }
}

/// Record a measured duration (not a point in time) under a label, once.
pub fn mark_value(label: &'static str, value_ms: f64) {
    if let Ok(mut marks) = MARKS.lock()
        && marks.len() < MAX_MARKS
        && !marks.iter().any(|(l, _)| *l == label)
    {
        marks.push((label, value_ms));
    }
}

pub fn mark_time(label: &str) -> Option<f64> {
    MARKS
        .lock()
        .ok()?
        .iter()
        .find(|(l, _)| *l == label)
        .map(|(_, t)| *t)
}

/// Marks in the order reached, rounded to 0.1 ms, as a JSON object.
pub fn timeline() -> serde_json::Value {
    let marks = MARKS.lock().map(|m| m.clone()).unwrap_or_default();
    serde_json::Value::Object(
        marks
            .into_iter()
            .map(|(label, ms)| {
                (
                    label.to_string(),
                    serde_json::json!((ms * 10.0).round() / 10.0),
                )
            })
            .collect(),
    )
}

#[derive(Clone, Copy, Default, serde::Serialize)]
pub struct Usage {
    pub cpu_seconds: f64,
    pub working_set_bytes: u64,
    pub peak_working_set_bytes: u64,
    pub private_bytes: u64,
}

#[cfg(windows)]
pub fn usage() -> Usage {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
        private_usage: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            created: *mut FileTime,
            exited: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut Counters, size: u32)
        -> i32;
    }
    let seconds = |t: &FileTime| (((t.high as u64) << 32) | t.low as u64) as f64 / 10_000_000.0;
    let mut usage = Usage::default();
    unsafe {
        let process = GetCurrentProcess();
        let (mut c, mut e, mut k, mut u) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        if GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u) != 0 {
            usage.cpu_seconds = seconds(&k) + seconds(&u);
        }
        let mut m = Counters {
            cb: std::mem::size_of::<Counters>() as u32,
            page_fault_count: 0,
            peak_working_set: 0,
            working_set: 0,
            quota_peak_paged: 0,
            quota_paged: 0,
            quota_peak_nonpaged: 0,
            quota_nonpaged: 0,
            pagefile: 0,
            peak_pagefile: 0,
            private_usage: 0,
        };
        if K32GetProcessMemoryInfo(process, &mut m, m.cb) != 0 {
            usage.working_set_bytes = m.working_set as u64;
            usage.peak_working_set_bytes = m.peak_working_set as u64;
            usage.private_bytes = m.private_usage as u64;
        }
    }
    usage
}

#[cfg(not(windows))]
pub fn usage() -> Usage {
    Usage::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_keep_the_first_occurrence_and_order() {
        init();
        mark("test-a");
        mark("test-b");
        let first = mark_time("test-a").unwrap();
        mark("test-a");
        assert_eq!(mark_time("test-a"), Some(first));
        assert!(mark_time("test-b").unwrap() >= first);
        assert!(timeline().get("test-b").is_some());
    }

    #[cfg(windows)]
    #[test]
    fn usage_reports_nonzero_memory() {
        let usage = usage();
        assert!(usage.working_set_bytes > 0 && usage.private_bytes > 0);
    }
}
