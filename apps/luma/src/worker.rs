use crate::{
    decode::{self, Cancel, Photo},
    model::{Album, CACHE_BUDGET, PhotoCache},
};
use eframe::egui;
use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

enum Request {
    Open(PathBuf),
    Navigate {
        path: PathBuf,
        next: Option<PathBuf>,
    },
}
struct Shared {
    pending: Option<(u64, Request)>,
    result: Option<Reply>,
    closed: bool,
}
pub struct Reply {
    pub generation: u64,
    pub album: Option<Album>,
    pub path: PathBuf,
    pub photo: Result<Option<Arc<Photo>>, String>,
}
pub struct Worker {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    generation: Arc<AtomicU64>,
}
impl Worker {
    pub fn new(ctx: egui::Context) -> Self {
        let shared = Arc::new((
            Mutex::new(Shared {
                pending: None,
                result: None,
                closed: false,
            }),
            Condvar::new(),
        ));
        let generation = Arc::new(AtomicU64::new(0));
        let thread_shared = shared.clone();
        let thread_generation = generation.clone();
        thread::Builder::new()
            .name("luma-decode".into())
            .spawn(move || {
                let mut cache = PhotoCache::new(CACHE_BUDGET);
                loop {
                    let (lock, wake) = &*thread_shared;
                    let (id, request) = {
                        let mut state = lock.lock().unwrap();
                        while state.pending.is_none() && !state.closed {
                            state = wake.wait(state).unwrap();
                        }
                        if state.closed {
                            break;
                        }
                        state.pending.take().unwrap()
                    };
                    let cancel = Cancel::new(thread_generation.clone(), id);
                    let (album, path, next) = match request {
                        Request::Open(path) => {
                            cache.clear();
                            match Album::open(&path) {
                                Ok(album) => {
                                    let current = album.current().cloned();
                                    let next = album.next_path();
                                    (Some(album), current.unwrap_or(path), next)
                                }
                                Err(error) => {
                                    if cancel.check().is_ok() {
                                        lock.lock().unwrap().result = Some(Reply {
                                            generation: id,
                                            album: None,
                                            path,
                                            photo: Err(error),
                                        });
                                        ctx.request_repaint();
                                    }
                                    continue;
                                }
                            }
                        }
                        Request::Navigate { path, next } => (None, path, next),
                    };
                    let result = if album.as_ref().is_some_and(|a| a.files.is_empty()) {
                        Ok(None)
                    } else {
                        if let Some(photo) = cache.get(&path) {
                            Ok(Some(photo))
                        } else {
                            // Catch parser panics as an error rather than losing the worker.
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    decode::decode(&path, &cancel)
                                }))
                                .unwrap_or_else(|_| {
                                    Err("Decoder rejected malformed image data.".into())
                                });
                            result.map(|p| {
                                let p = Arc::new(p);
                                cache.insert(path.clone(), p.clone());
                                Some(p)
                            })
                        }
                    };
                    if cancel.check().is_err() {
                        continue;
                    }
                    lock.lock().unwrap().result = Some(Reply {
                        generation: id,
                        album,
                        path,
                        photo: result,
                    });
                    ctx.request_repaint();
                    // One forward neighbour only, on this same worker. No extra job queue.
                    if let Some(next) = next
                        && cancel.check().is_ok()
                        && cache.get(&next).is_none()
                        && let Ok(Ok(photo)) =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                decode::decode(&next, &cancel)
                            }))
                        && cancel.check().is_ok()
                    {
                        cache.insert(next, Arc::new(photo));
                    }
                }
            })
            .expect("create bounded decode worker");
        Self { shared, generation }
    }
    fn request(&self, request: Request) -> u64 {
        let id = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (lock, wake) = &*self.shared;
        let mut state = lock.lock().unwrap();
        state.result = None;
        state.pending = Some((id, request));
        wake.notify_one();
        id
    }
    pub fn open(&self, path: PathBuf) -> u64 {
        self.request(Request::Open(path))
    }
    pub fn navigate(&self, path: PathBuf, next: Option<PathBuf>) -> u64 {
        self.request(Request::Navigate { path, next })
    }
    pub fn take(&self) -> Option<Reply> {
        self.shared.0.lock().unwrap().result.take()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let (lock, wake) = &*self.shared;
        lock.lock().unwrap().closed = true;
        wake.notify_one();
    }
}
