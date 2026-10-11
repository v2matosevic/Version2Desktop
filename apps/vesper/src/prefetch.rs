//! Sequential read-ahead of the decoder library and the edges of a chosen video.
//!
//! The decoder DLL is about 121 MiB. Mapped lazily, a first launch pulls it from a spinning disk
//! as thousands of scattered 4 KiB page faults, each waiting on a seek. Reading the same file once,
//! front to back, turns that into one sequential transfer and leaves the pages in the system file
//! cache (not in this process's memory). It starts only on a sign of intent (a file named on the
//! command line, the Open picker, a drag over the window) and never reads more than the limits
//! below. It reads only local drive paths, only the files named here, and keeps no data.
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Hard ceiling for any one warm-up read.
pub const MAX_LIBRARY_BYTES: u64 = 512 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;
const HEAD_BYTES: u64 = 8 * 1024 * 1024;
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

static LIBRARY_STARTED: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
fn open_sequential(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)
}
#[cfg(not(windows))]
fn open_sequential(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Read `len` bytes from `offset` in 1 MiB steps and discard them. Returns the bytes read.
/// `keep_going` is asked before every chunk so a superseded request stops within one chunk.
fn read_range(
    file: &mut std::fs::File,
    offset: u64,
    len: u64,
    keep_going: &dyn Fn() -> bool,
) -> std::io::Result<u64> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = vec![0_u8; CHUNK];
    let mut total = 0_u64;
    while total < len && keep_going() {
        let want = (len - total).min(CHUNK as u64) as usize;
        let n = file.read(&mut buffer[..want])?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    Ok(total)
}

/// Warm one local file front to back, up to `limit` bytes. Network, device and reparse paths are
/// refused by the same check the player uses before opening anything.
pub fn warm_file(path: &Path, limit: u64) -> Result<u64, String> {
    let path = crate::media::local_input(path)?;
    let mut file = open_sequential(&path).map_err(|e| e.to_string())?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    read_range(
        &mut file,
        0,
        size.min(limit.min(MAX_LIBRARY_BYTES)),
        &|| true,
    )
    .map_err(|e| e.to_string())
}

/// Warm the start and the end of a video: containers keep their index at one or the other, and the
/// demuxer reads both before the first picture. Ranges overlap harmlessly on a small file.
pub fn warm_media_edges(path: &Path) -> Result<u64, String> {
    warm_media_edges_while(path, &|| true)
}

fn warm_media_edges_while(path: &Path, keep_going: &dyn Fn() -> bool) -> Result<u64, String> {
    let path = crate::media::local_input(path)?;
    let mut file = open_sequential(&path).map_err(|e| e.to_string())?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let head =
        read_range(&mut file, 0, HEAD_BYTES.min(size), keep_going).map_err(|e| e.to_string())?;
    let tail_from = size.saturating_sub(TAIL_BYTES).max(head);
    let tail = read_range(&mut file, tail_from, size - tail_from, keep_going)
        .map_err(|e| e.to_string())?;
    Ok(head + tail)
}

/// Warm-ups can be switched off for an A/B measurement (`--no-prefetch` in the benchmark).
static DISABLED: AtomicBool = AtomicBool::new(false);
pub fn disable() {
    DISABLED.store(true, Ordering::SeqCst);
}

/// Start warming the decoder library once per process, on its own thread. A second call does
/// nothing. The result is discarded: warming is an optimisation and never an error to show.
pub fn start_library(path: PathBuf) -> bool {
    if DISABLED.load(Ordering::SeqCst) || LIBRARY_STARTED.swap(true, Ordering::SeqCst) {
        return false;
    }
    std::thread::Builder::new()
        .name("vesper-prefetch-decoder".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let bytes = warm_file(&path, MAX_LIBRARY_BYTES).unwrap_or(0);
            if bytes > 0 {
                crate::timing::mark_value(
                    "decoder_prefetch_ms",
                    started.elapsed().as_secs_f64() * 1000.0,
                );
            }
        })
        .is_ok()
}

/// The newest media request. One worker at most reads for it; a newer request makes the worker
/// stop within one chunk and read the newer file instead, so rapid opens never pile up reads.
struct MediaRequest {
    latest: Option<PathBuf>,
    generation: u64,
    running: bool,
}
static MEDIA: Mutex<MediaRequest> = Mutex::new(MediaRequest {
    latest: None,
    generation: 0,
    running: false,
});

fn media_generation() -> u64 {
    MEDIA.lock().map(|m| m.generation).unwrap_or(0)
}

/// Warm the head and tail of the newest chosen video. Replaces any request still waiting.
pub fn start_media(path: PathBuf) {
    if DISABLED.load(Ordering::SeqCst) {
        return;
    }
    let Ok(mut request) = MEDIA.lock() else {
        return;
    };
    request.latest = Some(path);
    request.generation += 1;
    if request.running {
        return;
    }
    request.running = true;
    drop(request);
    let spawned = std::thread::Builder::new()
        .name("vesper-prefetch-media".into())
        .spawn(|| {
            loop {
                let (path, generation) = {
                    let Ok(mut request) = MEDIA.lock() else {
                        return;
                    };
                    match request.latest.take() {
                        Some(path) => (path, request.generation),
                        None => {
                            request.running = false;
                            return;
                        }
                    }
                };
                let _ = warm_media_edges_while(&path, &|| media_generation() == generation);
            }
        });
    if spawned.is_err()
        && let Ok(mut request) = MEDIA.lock()
    {
        request.running = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, bytes: usize) -> PathBuf {
        let p = std::env::temp_dir().join(format!("vesper-prefetch-{}-{name}", std::process::id()));
        std::fs::write(&p, vec![7_u8; bytes]).unwrap();
        p
    }

    #[test]
    fn warms_exactly_the_file_up_to_the_limit() {
        let p = temp("whole.bin", 3 * 1024 * 1024 + 123);
        assert_eq!(warm_file(&p, u64::MAX).unwrap(), 3 * 1024 * 1024 + 123);
        assert_eq!(warm_file(&p, 1000).unwrap(), 1000);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn edges_of_a_large_file_are_head_plus_tail_not_the_middle() {
        let size = 40 * 1024 * 1024;
        let p = temp("big.bin", size);
        assert_eq!(warm_media_edges(&p).unwrap(), HEAD_BYTES + TAIL_BYTES);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn edges_of_a_small_file_read_it_once() {
        let p = temp("small.bin", 1_000_000);
        assert_eq!(warm_media_edges(&p).unwrap(), 1_000_000);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn a_superseded_read_stops_within_a_chunk() {
        let p = temp("superseded.bin", 20 * 1024 * 1024);
        let calls = std::cell::Cell::new(0);
        let keep_going = || {
            calls.set(calls.get() + 1);
            calls.get() <= 3
        };
        let read = warm_media_edges_while(&p, &keep_going).unwrap();
        assert_eq!(
            read,
            3 * CHUNK as u64,
            "three chunks, then the request was replaced"
        );
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn rapid_requests_keep_only_the_newest_and_one_worker() {
        let a = temp("rapid-a.bin", 30 * 1024 * 1024);
        let b = temp("rapid-b.bin", 1_000_000);
        for _ in 0..50 {
            start_media(a.clone());
        }
        start_media(b.clone());
        // However the worker interleaves, the generation advanced once per call and nothing queued
        // beyond the newest path.
        let request = MEDIA.lock().unwrap();
        assert!(request.generation >= 51);
        assert!(request.latest.as_ref().is_none_or(|p| *p == b));
        drop(request);
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn network_and_missing_paths_are_refused() {
        assert!(warm_file(Path::new("\\\\server\\share\\libmpv-2.dll"), 10).is_err());
        assert!(warm_media_edges(Path::new("\\\\server\\share\\movie.mkv")).is_err());
        assert!(warm_file(&std::env::temp_dir().join("vesper-prefetch-absent.bin"), 10).is_err());
    }
}
