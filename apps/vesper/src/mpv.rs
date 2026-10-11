//! Minimal audited subset of the bundled mpv client/render C ABI. Headers are in vendor/mpv/include.
use libloading::Library;
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString, c_char, c_void},
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, Mutex, OnceLock},
};

pub type Handle = *mut c_void;

pub struct PollBatch {
    pub events: Vec<(i32, Option<String>)>,
    /// The bounded pass did not observe NONE; schedule another UI logic pass.
    pub needs_drain: bool,
}

fn drain_events(mut consume_one: impl FnMut() -> bool) -> bool {
    for _ in 0..128 {
        if !consume_one() {
            return false;
        }
    }
    true
}
#[repr(C)]
pub struct Param {
    pub kind: i32,
    pub data: *mut c_void,
}
impl Param {
    pub fn new<T>(kind: i32, data: &mut T) -> Self {
        Self {
            kind,
            data: (data as *mut T).cast(),
        }
    }
    pub fn end() -> Self {
        Self {
            kind: 0,
            data: ptr::null_mut(),
        }
    }
}
#[repr(C)]
pub struct Event {
    pub id: i32,
    pub error: i32,
    pub userdata: u64,
    pub data: *mut c_void,
}
#[repr(C)]
pub struct EndFile {
    pub reason: i32,
    pub error: i32,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: i32,
}
#[repr(C)]
struct StartFile {
    playlist_entry_id: i64,
}
#[repr(C)]
union NodeValue {
    integer: i64,
    list: *mut NodeList,
}
#[repr(C)]
struct Node {
    value: NodeValue,
    format: i32,
}
#[repr(C)]
struct NodeList {
    count: i32,
    values: *mut Node,
    keys: *mut *mut c_char,
}
#[repr(C)]
struct CommandResult {
    result: Node,
}

// These cookies cannot overlap the observed-property categories (1..=6).
const LOAD_COOKIE: u64 = 0x80 << 56;
const CONTROL_COOKIE: u64 = 0x81 << 56;
const COOKIE_KIND: u64 = 0xff << 56;
const COOKIE_GENERATION: u64 = !COOKIE_KIND;
const PENDING_FILE_EVENTS: usize = 128;

/// Copy the result scalar while the borrowed event is valid, before wait_event again.
/// Event-owned nodes must never be passed to mpv_free_node_contents.
unsafe fn load_entry_id(data: *mut c_void) -> Option<i64> {
    let command = unsafe { data.cast::<CommandResult>().as_ref() }?;
    if command.result.format != 8 {
        return None;
    }
    let list = unsafe { command.result.value.list.as_ref() }?;
    if !(1..=32).contains(&list.count) || list.keys.is_null() || list.values.is_null() {
        return None;
    }
    for index in 0..list.count as usize {
        let key = unsafe { *list.keys.add(index) };
        if key.is_null() {
            return None;
        }
        if unsafe { CStr::from_ptr(key) }.to_bytes() == b"playlist_entry_id" {
            let value = unsafe { &*list.values.add(index) };
            if value.format != 4 {
                return None;
            }
            let entry = unsafe { value.value.integer };
            return (entry >= 0).then_some(entry);
        }
    }
    None
}

struct FileNotice {
    id: i32,
    entry: Option<i64>,
    error: Option<String>,
}
#[derive(Default)]
struct FileEvents {
    generation: u64,
    selected_entry: Option<i64>,
    /// Cursor for the actual decoder stream, including superseded START_FILE events.
    active_entry: Option<i64>,
    awaiting_reply: bool,
    pending: VecDeque<FileNotice>,
}
impl FileEvents {
    fn begin(&mut self, generation: u64) {
        self.generation = generation;
        self.selected_entry = None;
        self.awaiting_reply = true;
        self.pending.clear();
        // Keep the actual stream cursor until the decoder emits its next START_FILE.
    }
    fn invalidate(&mut self, message: &str) -> Vec<(i32, Option<String>)> {
        self.selected_entry = None;
        self.active_entry = None;
        self.awaiting_reply = false;
        self.pending.clear();
        vec![(5, Some(message.into()))]
    }
    fn acknowledge(
        &mut self,
        generation: u64,
        entry: Result<i64, String>,
    ) -> Vec<(i32, Option<String>)> {
        if generation != self.generation || !self.awaiting_reply {
            return Vec::new();
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return self.invalidate(&error),
        };
        self.selected_entry = Some(entry);
        self.awaiting_reply = false;
        self.pending
            .drain(..)
            .filter(|event| event.entry == Some(entry))
            .map(|event| (event.id, event.error))
            .collect()
    }
    fn notice(&mut self, event: FileNotice) -> Vec<(i32, Option<String>)> {
        if let Some(selected) = self.selected_entry {
            return if event.entry == Some(selected) {
                vec![(event.id, event.error)]
            } else {
                Vec::new()
            };
        }
        if !self.awaiting_reply {
            return Vec::new();
        }
        if self.pending.len() == PENDING_FILE_EVENTS {
            return self.invalidate("Playback event queue overflowed. Reopen the file.");
        }
        self.pending.push_back(event);
        Vec::new()
    }
}
#[repr(C)]
struct LogMessage {
    prefix: *const c_char,
    level: *const c_char,
    text: *const c_char,
    log_level: i32,
}
#[repr(C)]
struct Property {
    name: *const c_char,
    format: i32,
    data: *mut c_void,
}
#[derive(Default)]
struct Properties {
    strings: HashMap<String, String>,
    numbers: HashMap<String, f64>,
    pending: VecDeque<(String, i32)>,
    inflight: usize,
    sequence: u32,
    generation: u16,
    tracks_dirty: bool,
}
#[repr(C)]
pub struct GlInit {
    pub get_proc_address: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    pub context: *mut c_void,
}
#[repr(C)]
pub struct GlFbo {
    pub fbo: i32,
    pub width: i32,
    pub height: i32,
    pub internal_format: i32,
}
pub type Callback = Option<unsafe extern "C" fn(*mut c_void)>;

pub struct Api {
    _library: Library,
    pub create: unsafe extern "C" fn() -> Handle,
    pub initialize: unsafe extern "C" fn(Handle) -> i32,
    pub destroy: unsafe extern "C" fn(Handle),
    pub option: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> i32,
    pub command_async: unsafe extern "C" fn(Handle, u64, *const *const c_char) -> i32,
    pub get: unsafe extern "C" fn(Handle, *const c_char, i32, *mut c_void) -> i32,
    pub free: unsafe extern "C" fn(*mut c_void),
    pub wait: unsafe extern "C" fn(Handle, f64) -> *mut Event,
    pub error: unsafe extern "C" fn(i32) -> *const c_char,
    pub wakeup: unsafe extern "C" fn(Handle, Callback, *mut c_void),
    pub request_logs: unsafe extern "C" fn(Handle, *const c_char) -> i32,
    pub observe: unsafe extern "C" fn(Handle, u64, *const c_char, i32) -> i32,
    pub get_async: unsafe extern "C" fn(Handle, u64, *const c_char, i32) -> i32,
    pub render_create: unsafe extern "C" fn(*mut Handle, Handle, *mut Param) -> i32,
    pub render_free: unsafe extern "C" fn(Handle),
    pub render: unsafe extern "C" fn(Handle, *mut Param) -> i32,
    pub render_update: unsafe extern "C" fn(Handle) -> u64,
    pub render_callback: unsafe extern "C" fn(Handle, Callback, *mut c_void),
    pub report_swap: unsafe extern "C" fn(Handle),
}
struct LoadedApi {
    path: PathBuf,
    api: Arc<Api>,
}
impl Api {
    pub fn load(path: &Path) -> Result<Arc<Self>, String> {
        if !path.is_absolute() {
            return Err("The decoder DLL path must be absolute.".into());
        }
        // Native dependencies can install process-wide runtime hooks. Unloading and reloading
        // their DLLs leaves those hooks dangling (the verified MSYS2 GGML build aborts on reload).
        // Keep the chosen decoder alive for the process and reuse it across player instances.
        static CACHE: OnceLock<Mutex<Option<LoadedApi>>> = OnceLock::new();
        let path = path
            .canonicalize()
            .map_err(|e| format!("Bundled decoder unavailable: {e}"))?;
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| "Decoder initialization lock failed".to_string())?;
        if let Some(loaded) = cache.as_ref() {
            if loaded.path != path {
                return Err("A different decoder is already loaded. Restart to change it.".into());
            }
            return Ok(loaded.api.clone());
        }
        let api = Self::load_once(&path)?;
        *cache = Some(LoadedApi {
            path,
            api: api.clone(),
        });
        Ok(api)
    }

    fn load_once(path: &Path) -> Result<Arc<Self>, String> {
        // Only load the explicitly chosen/bundled absolute DLL, never PATH/current-directory lookup.
        #[cfg(windows)]
        let lib: Library = unsafe {
            // Restrict dependencies to the DLL's directory and Windows default safe directories.
            // Neither current directory nor PATH participates. Flags are documented Win32 values.
            libloading::os::windows::Library::load_with_flags(path, 0x00000100 | 0x00001000)
        }
        .map_err(|e| format!("Bundled decoder unavailable: {e}"))?
        .into();
        #[cfg(not(windows))]
        let lib = unsafe { Library::new(path) }
            .map_err(|e| format!("Bundled decoder unavailable: {e}"))?;
        macro_rules! sym {
            ($name:literal) => {
                unsafe {
                    *lib.get(concat!($name, "\0").as_bytes())
                        .map_err(|e| e.to_string())?
                }
            };
        }
        Ok(Arc::new(Self {
            create: sym!("mpv_create"),
            initialize: sym!("mpv_initialize"),
            destroy: sym!("mpv_terminate_destroy"),
            option: sym!("mpv_set_option_string"),
            command_async: sym!("mpv_command_async"),
            get: sym!("mpv_get_property"),
            free: sym!("mpv_free"),
            wait: sym!("mpv_wait_event"),
            error: sym!("mpv_error_string"),
            wakeup: sym!("mpv_set_wakeup_callback"),
            request_logs: sym!("mpv_request_log_messages"),
            observe: sym!("mpv_observe_property"),
            get_async: sym!("mpv_get_property_async"),
            render_create: sym!("mpv_render_context_create"),
            render_free: sym!("mpv_render_context_free"),
            render: sym!("mpv_render_context_render"),
            render_update: sym!("mpv_render_context_update"),
            render_callback: sym!("mpv_render_context_set_update_callback"),
            report_swap: sym!("mpv_render_context_report_swap"),
            _library: lib,
        }))
    }
    pub fn check(&self, value: i32) -> Result<(), String> {
        if value < 0 {
            Err(unsafe { CStr::from_ptr((self.error)(value)) }
                .to_string_lossy()
                .into_owned())
        } else {
            Ok(())
        }
    }
}

pub fn bundled_dll() -> PathBuf {
    std::env::current_exe()
        .unwrap_or_default()
        .parent()
        .unwrap_or(Path::new("."))
        .join("libmpv-2.dll")
}
fn c(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "A path or value contains a NUL character.".into())
}

pub struct Player {
    pub api: Arc<Api>,
    pub handle: Handle,
    logs: Mutex<VecDeque<String>>,
    properties: Mutex<Properties>,
    file_events: Mutex<FileEvents>,
}
// The mpv client API is thread safe. Render context remains UI-thread-only.
unsafe impl Send for Player {}
unsafe impl Sync for Player {}
impl Player {
    pub fn new(api: Arc<Api>, options: &[(&str, &str)]) -> Result<Arc<Self>, String> {
        let handle = unsafe { (api.create)() };
        if handle.is_null() {
            return Err("Cannot create decoder.".into());
        }
        let p = Arc::new(Self {
            api,
            handle,
            logs: Mutex::new(VecDeque::new()),
            properties: Mutex::new(Properties::default()),
            file_events: Mutex::new(FileEvents::default()),
        });
        p.api
            .check(unsafe { (p.api.request_logs)(p.handle, c"warn".as_ptr()) })?;
        for (k, v) in [
            ("config", "no"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            ("input-default-bindings", "no"),
            ("input-terminal", "no"),
            ("terminal", "no"),
            ("osc", "no"),
            ("access-references", "no"),
            ("demuxer-lavf-o", "protocol_whitelist=file"),
            ("demuxer-max-bytes", "64MiB"),
            ("demuxer-max-back-bytes", "16MiB"),
            ("cache", "no"),
            ("audio-buffer", "0.2"),
            ("vd-lavc-threads", "2"),
            ("vd-lavc-o", "max_pixels=33177600"),
            ("vo", "libmpv"),
        ]
        .into_iter()
        .chain(options.iter().copied())
        {
            p.option(k, v)?;
        }
        p.api.check(unsafe { (p.api.initialize)(handle) })?;
        // The only synchronous reads occur here, before any render context
        // exists. During playback/rendering, all property reads use this cache.
        for key in ["mpv-version", "ffmpeg-version"] {
            if let Some(v) = p.initial_string(key) {
                p.properties.lock().unwrap().strings.insert(key.into(), v);
            }
        }
        let strings = [
            "pause",
            "idle-active",
            "eof-reached",
            "mute",
            "video-codec",
            "audio-codec",
            "file-format",
            "current-ao",
            "hwdec-current",
            "sid",
            "sub-text",
        ];
        let numbers = [
            "time-pos",
            "duration",
            "video-params/w",
            "video-params/h",
            "volume",
            "speed",
            "aid",
            "sid",
            "track-list/count",
            "sub-pos",
            "sub-delay",
            "sub-scale",
        ];
        for (kind, keys) in [(1, strings.as_slice()), (5, numbers.as_slice())] {
            for (i, key) in keys.iter().enumerate() {
                p.api.check(unsafe {
                    (p.api.observe)(
                        p.handle,
                        ((kind as u64) << 56) | i as u64,
                        c(key)?.as_ptr(),
                        kind,
                    )
                })?;
            }
        }
        p.api
            .check(unsafe { (p.api.observe)(p.handle, 999, c"track-list".as_ptr(), 0) })?;
        Ok(p)
    }
    /// Observe additional numeric properties (benchmarks only; each change posts an event).
    pub fn observe_numbers(&self, keys: &[&str]) -> Result<(), String> {
        for (i, key) in keys.iter().enumerate().take(16) {
            self.api.check(unsafe {
                (self.api.observe)(
                    self.handle,
                    (5u64 << 56) | (100 + i as u64),
                    c(key)?.as_ptr(),
                    5,
                )
            })?;
        }
        Ok(())
    }
    fn option(&self, key: &str, val: &str) -> Result<(), String> {
        self.api
            .check(unsafe { (self.api.option)(self.handle, c(key)?.as_ptr(), c(val)?.as_ptr()) })
    }
    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        self.command_impl(args)
    }
    pub fn command_async(&self, args: &[&str]) -> Result<(), String> {
        self.command_impl(args)
    }
    fn command_impl(&self, args: &[&str]) -> Result<(), String> {
        let files = self
            .file_events
            .lock()
            .map_err(|_| "Playback identity lock failed.")?;
        self.command_with_cookie(args, CONTROL_COOKIE | files.generation)
    }
    fn command_with_cookie(&self, args: &[&str], cookie: u64) -> Result<(), String> {
        let strings: Vec<CString> = args.iter().map(|s| c(s)).collect::<Result<_, _>>()?;
        let pointers: Vec<*const c_char> = strings
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(ptr::null()))
            .collect();
        self.api
            .check(unsafe { (self.api.command_async)(self.handle, cookie, pointers.as_ptr()) })
    }
    pub fn open(&self, path: &Path, seconds: f64) -> Result<PathBuf, String> {
        let validated = crate::media::validate(path)?;
        self.open_validated(&validated, seconds)
    }
    /// Start a file whose filesystem checks already ran. Everything here is memory and one
    /// non-blocking command; the request/generation identity handling is unchanged.
    pub fn open_validated(
        &self,
        file: &crate::media::Validated,
        seconds: f64,
    ) -> Result<PathBuf, String> {
        let path = file.path().to_path_buf();
        let raw = path.to_str().ok_or("File path is not valid Unicode.")?;
        // All arguments are separate C strings, no shell/command-string interpretation.
        let mut files = self
            .file_events
            .lock()
            .map_err(|_| "Playback identity lock failed.")?;
        let generation = files
            .generation
            .checked_add(1)
            .filter(|value| *value <= COOKIE_GENERATION)
            .ok_or("Playback request identifiers exhausted.")?;
        self.command_with_cookie(
            &[
                "loadfile",
                raw,
                "replace",
                "-1",
                &format!("start={}", seconds.max(0.0)),
            ],
            LOAD_COOKIE | generation,
        )?;
        // A failed enqueue leaves attribution for the previous selection intact.
        files.begin(generation);
        Ok(path)
    }
    pub fn number(&self, key: &str) -> Option<f64> {
        self.properties.lock().ok()?.numbers.get(key).copied()
    }
    pub fn string(&self, key: &str) -> Option<String> {
        self.properties.lock().ok()?.strings.get(key).cloned()
    }
    fn initial_string(&self, key: &str) -> Option<String> {
        let mut p: *mut c_char = ptr::null_mut();
        let key = c(key).ok()?;
        if unsafe {
            (self.api.get)(
                self.handle,
                key.as_ptr(),
                1,
                (&mut p as *mut *mut c_char).cast(),
            )
        } < 0
            || p.is_null()
        {
            return None;
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { (self.api.free)(p.cast()) };
        Some(s)
    }
    pub fn set_number(&self, key: &str, v: f64) -> Result<(), String> {
        if !v.is_finite() {
            return Err("Non-finite control value.".into());
        }
        self.command_async(&["set", key, &v.to_string()])
    }
    pub fn set_string(&self, key: &str, val: &str) -> Result<(), String> {
        self.command_async(&["set", key, val])
    }
    pub fn poll(&self) -> Vec<(i32, Option<String>)> {
        self.poll_batch().events
    }
    pub fn poll_batch(&self) -> PollBatch {
        let mut events = Vec::new();
        let needs_drain = drain_events(|| {
            // Never let malformed/event-heavy media monopolize the UI.
            let e = unsafe { &*(self.api.wait)(self.handle, 0.0) };
            if e.id == 0 {
                return false;
            }
            if e.id == 24 {
                if let Ok(mut files) = self.file_events.lock() {
                    events.extend(
                        files.invalidate("Decoder event queue overflowed. Reopen the file."),
                    );
                }
                return true;
            }
            if e.id == 5 && e.userdata & COOKIE_KIND == LOAD_COOKIE {
                if let Ok(mut files) = self.file_events.lock()
                    && e.userdata & COOKIE_GENERATION == files.generation
                {
                    let entry = self.api.check(e.error).and_then(|_| {
                        unsafe { load_entry_id(e.data) }.ok_or_else(|| {
                            "Decoder did not identify the selected file. Reopen it.".into()
                        })
                    });
                    events.extend(files.acknowledge(e.userdata & COOKIE_GENERATION, entry));
                }
                return true;
            }
            if e.id == 5
                && e.userdata & COOKIE_KIND == CONTROL_COOKIE
                && self
                    .file_events
                    .lock()
                    .is_ok_and(|files| e.userdata & COOKIE_GENERATION != files.generation)
            {
                return true;
            }
            if e.id == 3 || e.id == 22 {
                if e.id == 3
                    && let Ok(mut p) = self.properties.lock()
                {
                    p.inflight = p.inflight.saturating_sub(1);
                    if ((e.userdata >> 32) & 0xffff) as u16 != p.generation {
                        return true;
                    }
                }
                if e.userdata == 999 {
                    if let Ok(mut p) = self.properties.lock() {
                        p.tracks_dirty = true;
                    }
                } else if !e.data.is_null() {
                    self.property_event(unsafe { &*e.data.cast::<Property>() }, e.userdata);
                }
                return true;
            }
            if e.id == 2 && !e.data.is_null() {
                let log = unsafe { &*e.data.cast::<LogMessage>() };
                if !log.prefix.is_null() && !log.level.is_null() && !log.text.is_null() {
                    let prefix = unsafe { CStr::from_ptr(log.prefix) }.to_string_lossy();
                    let level = unsafe { CStr::from_ptr(log.level) }.to_string_lossy();
                    let body = unsafe { CStr::from_ptr(log.text) }.to_string_lossy();
                    let text = format!("[{prefix}] {level}: {body}")
                        .chars()
                        .take(2000)
                        .collect();
                    if let Ok(mut logs) = self.logs.lock() {
                        logs.push_back(text);
                        if logs.len() > 64 {
                            logs.pop_front();
                        }
                    }
                }
            }
            let mut error = if e.error < 0 {
                self.api.check(e.error).err()
            } else {
                None
            };
            if e.id == 7 && !e.data.is_null() {
                let end = unsafe { &*e.data.cast::<EndFile>() };
                if end.error < 0 {
                    error = self.api.check(end.error).err();
                }
            }
            if e.id == 6
                && let Ok(mut p) = self.properties.lock()
            {
                p.generation = p.generation.wrapping_add(1);
                p.pending.clear();
                p.strings.retain(|k, _| !k.starts_with("track-list/"));
                p.numbers.retain(|k, _| !k.starts_with("track-list/"));
            }
            if e.id == 8
                && let Ok(mut p) = self.properties.lock()
            {
                p.tracks_dirty = true;
            }
            if matches!(e.id, 6..=8) {
                if let Ok(mut files) = self.file_events.lock() {
                    let entry = match e.id {
                        6 => {
                            files.active_entry = unsafe { e.data.cast::<StartFile>().as_ref() }
                                .map(|start| start.playlist_entry_id);
                            files.active_entry
                        }
                        7 => unsafe { e.data.cast::<EndFile>().as_ref() }
                            .map(|end| end.playlist_entry_id),
                        _ => files.active_entry,
                    };
                    events.extend(files.notice(FileNotice {
                        id: e.id,
                        entry,
                        error,
                    }));
                }
            } else {
                events.push((e.id, error));
            }
            true
        });
        self.flush_queries();
        PollBatch {
            events,
            needs_drain,
        }
    }
    fn property_event(&self, prop: &Property, userdata: u64) {
        if prop.name.is_null() {
            return;
        }
        let name = unsafe { CStr::from_ptr(prop.name) }
            .to_string_lossy()
            .into_owned();
        let Ok(mut p) = self.properties.lock() else {
            return;
        };
        if prop.format == 1 && !prop.data.is_null() {
            let text = unsafe { *prop.data.cast::<*const c_char>() };
            if !text.is_null() {
                let bytes = unsafe { CStr::from_ptr(text) }.to_bytes();
                p.strings.insert(
                    name.clone(),
                    String::from_utf8_lossy(&bytes[..bytes.len().min(8192)]).into_owned(),
                );
            }
        } else if prop.format == 5 && !prop.data.is_null() {
            let value = unsafe { *prop.data.cast::<f64>() };
            if value.is_finite() {
                p.numbers.insert(name.clone(), value);
            }
        } else if prop.format == 0 {
            match userdata >> 56 {
                1 => {
                    p.strings.remove(&name);
                }
                5 => {
                    p.numbers.remove(&name);
                }
                _ => {}
            }
        }
        if name == "track-list/count" {
            p.tracks_dirty = true;
        }
    }
    fn flush_queries(&self) {
        let mut batch = Vec::new();
        if let Ok(mut p) = self.properties.lock() {
            if p.tracks_dirty {
                p.tracks_dirty = false;
                p.pending.clear();
                let count = p
                    .numbers
                    .get("track-list/count")
                    .copied()
                    .unwrap_or(0.0)
                    .clamp(0.0, 256.0) as usize;
                for i in 0..count {
                    for (key, kind) in [
                        ("id", 5),
                        ("type", 1),
                        ("lang", 1),
                        ("title", 1),
                        ("selected", 1),
                    ] {
                        p.pending.push_back((format!("track-list/{i}/{key}"), kind));
                    }
                }
            }
            while batch.len() < 16 && p.inflight < 32 {
                let Some((key, kind)) = p.pending.pop_front() else {
                    break;
                };
                p.sequence = p.sequence.wrapping_add(1);
                let id = ((kind as u64) << 56) | ((p.generation as u64) << 32) | p.sequence as u64;
                p.inflight += 1;
                batch.push((key, kind, id));
            }
        }
        for (key, kind, id) in batch {
            if let Ok(key_c) = c(&key) {
                let rc = unsafe { (self.api.get_async)(self.handle, id, key_c.as_ptr(), kind) };
                if rc < 0
                    && let Ok(mut p) = self.properties.lock()
                {
                    p.inflight = p.inflight.saturating_sub(1);
                    p.pending.push_front((key, kind));
                }
            }
        }
    }
    pub fn messages(&self) -> Vec<String> {
        self.logs
            .lock()
            .map(|m| m.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod event_drain_tests {
    use super::drain_events;
    use std::collections::VecDeque;

    #[test]
    fn filtered_property_burst_still_schedules_terminal_continuation() {
        let mut queue = VecDeque::from(vec![22; 128]);
        queue.extend([7, 0]);
        let mut visible = Vec::new();
        {
            let mut pass = || {
                let id = queue.pop_front().unwrap_or(0);
                if id == 0 {
                    return false;
                }
                if id != 22 {
                    visible.push(id);
                }
                true
            };
            assert!(drain_events(&mut pass));
        }
        assert_eq!(queue.len(), 2);
        assert!(visible.is_empty());
        assert!(!drain_events(|| {
            let id = queue.pop_front().unwrap_or(0);
            if id == 0 {
                return false;
            }
            if id != 22 {
                visible.push(id);
            }
            true
        }));
        assert_eq!(visible, vec![7]);
        assert!(queue.is_empty());
    }

    #[test]
    fn empty_queue_stops_without_continuation() {
        let mut calls = 0;
        assert!(!drain_events(|| {
            calls += 1;
            false
        }));
        assert_eq!(calls, 1);
    }

    #[test]
    fn exact_budget_needs_only_one_empty_continuation() {
        let mut remaining = 128;
        let mut consume = || {
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
            true
        };
        assert!(drain_events(&mut consume));
        assert!(!drain_events(&mut consume));
    }
}

#[cfg(test)]
mod file_identity_tests {
    use super::*;
    fn notice(id: i32, entry: i64, error: Option<&str>) -> FileNotice {
        FileNotice {
            id,
            entry: Some(entry),
            error: error.map(str::to_owned),
        }
    }
    #[test]
    fn pending_events_keep_ingress_identity_and_fifo_across_three_selections() {
        let mut files = FileEvents::default();
        files.begin(1);
        files.active_entry = Some(10);
        files.begin(2);
        assert_eq!(files.active_entry, Some(10));
        files.begin(3);
        assert!(files.notice(notice(8, 10, None)).is_empty());
        assert!(files.notice(notice(7, 10, Some("old error"))).is_empty());
        assert!(files.notice(notice(6, 30, None)).is_empty());
        assert!(files.notice(notice(8, 30, None)).is_empty());
        assert!(
            files
                .notice(notice(7, 30, Some("current error")))
                .is_empty()
        );
        assert!(files.acknowledge(1, Ok(10)).is_empty());
        assert!(
            files
                .acknowledge(2, Err("old load failed".into()))
                .is_empty()
        );
        assert_eq!(
            files.acknowledge(3, Ok(30)),
            vec![(6, None), (8, None), (7, Some("current error".into()))]
        );
        assert!(files.pending.is_empty());
        assert!(
            files
                .notice(notice(7, 10, Some("late old error")))
                .is_empty()
        );
        assert_eq!(
            files.notice(notice(7, 30, Some("late current error"))),
            vec![(7, Some("late current error".into()))]
        );
    }
    #[test]
    fn pending_bound_fails_visibly_instead_of_losing_unknown_current_events() {
        let mut files = FileEvents::default();
        files.begin(1);
        for entry in 0..PENDING_FILE_EVENTS as i64 {
            assert!(files.notice(notice(6, entry, None)).is_empty());
        }
        assert_eq!(files.pending.len(), PENDING_FILE_EVENTS);
        let report = files.notice(notice(7, 0, Some("unknown terminal")));
        assert!(report[0].1.as_deref().unwrap().contains("overflowed"));
        assert!(files.pending.is_empty());
        assert!(!files.awaiting_reply);
        assert!(files.notice(notice(8, 0, None)).is_empty());
    }
    #[test]
    fn current_reply_failure_invalidates_cursor_and_reports_error() {
        let mut files = FileEvents {
            active_entry: Some(8),
            ..FileEvents::default()
        };
        files.begin(1);
        assert_eq!(
            files.acknowledge(1, Err("missing entry id".into())),
            vec![(5, Some("missing entry id".into()))]
        );
        assert!(files.active_entry.is_none());
        assert!(files.selected_entry.is_none());
        assert!(files.pending.is_empty());
    }
    #[test]
    fn late_acknowledgement_cannot_revive_invalidated_selection() {
        let mut files = FileEvents::default();
        files.begin(1);
        assert!(
            files.invalidate("Decoder event queue overflowed.")[0]
                .1
                .is_some()
        );
        assert!(files.acknowledge(1, Ok(8)).is_empty());
        assert!(files.selected_entry.is_none());
        assert!(files.notice(notice(8, 8, None)).is_empty());
        files.begin(2);
        assert!(files.acknowledge(2, Ok(9)).is_empty());
        assert_eq!(files.notice(notice(8, 9, None)), vec![(8, None)]);
    }
    #[test]
    fn command_result_parser_guards_abi_and_copies_only_the_integer() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<Node>(), 16);
            assert_eq!(std::mem::offset_of!(Node, format), 8);
            assert_eq!(std::mem::size_of::<NodeList>(), 24);
            assert_eq!(std::mem::size_of::<CommandResult>(), 16);
        }
        fn parse(key: &CStr, count: i32, value_format: i32, node_format: i32) -> Option<i64> {
            let mut keys = [key.as_ptr().cast_mut()];
            let mut values = [Node {
                value: NodeValue { integer: 27 },
                format: value_format,
            }];
            let mut list = NodeList {
                count,
                values: values.as_mut_ptr(),
                keys: keys.as_mut_ptr(),
            };
            let mut result = CommandResult {
                result: Node {
                    value: NodeValue { list: &mut list },
                    format: node_format,
                },
            };
            unsafe { load_entry_id((&mut result as *mut CommandResult).cast()) }
        }
        assert_eq!(parse(c"playlist_entry_id", 1, 4, 8), Some(27));
        assert_eq!(unsafe { load_entry_id(ptr::null_mut()) }, None);
        assert_eq!(parse(c"playlist_entry_id", 1, 1, 8), None);
        assert_eq!(parse(c"other", 1, 4, 8), None);
        assert_eq!(parse(c"playlist_entry_id", 33, 4, 8), None);
        assert_eq!(parse(c"playlist_entry_id", 1, 4, 0), None);
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        unsafe {
            (self.api.wakeup)(self.handle, None, ptr::null_mut());
            (self.api.destroy)(self.handle);
        }
    }
}

/// Used only by self-ending headless decode checks. No GL/window/device input needed.
pub struct SoftwareRenderer {
    player: Arc<Player>,
    handle: Handle,
    pub pixels: Vec<u8>,
    size: [i32; 2],
}
impl SoftwareRenderer {
    pub fn new(player: Arc<Player>, width: i32, height: i32) -> Result<Self, String> {
        if !(1..=1920).contains(&width) || !(1..=1080).contains(&height) {
            return Err("Unsafe smoke frame size.".into());
        }
        let mut handle = ptr::null_mut();
        let mut params = [
            Param {
                kind: 1,
                data: c"sw".as_ptr().cast_mut().cast(),
            },
            Param::end(),
        ];
        player.api.check(unsafe {
            (player.api.render_create)(&mut handle, player.handle, params.as_mut_ptr())
        })?;
        Ok(Self {
            player,
            handle,
            pixels: vec![0; (width * height * 4) as usize],
            size: [width, height],
        })
    }
    pub fn draw(&mut self) -> Result<bool, String> {
        if unsafe { (self.player.api.render_update)(self.handle) } & 1 == 0 {
            return Ok(false);
        }
        let mut stride = (self.size[0] * 4) as usize;
        let mut block = 0_i32;
        let mut params = [
            Param::new(17, &mut self.size),
            Param {
                kind: 18,
                data: c"bgr0".as_ptr().cast_mut().cast(),
            },
            Param::new(19, &mut stride),
            Param {
                kind: 20,
                data: self.pixels.as_mut_ptr().cast(),
            },
            Param::new(12, &mut block),
            Param::end(),
        ];
        self.player
            .api
            .check(unsafe { (self.player.api.render)(self.handle, params.as_mut_ptr()) })?;
        Ok(true)
    }
}
impl Drop for SoftwareRenderer {
    fn drop(&mut self) {
        unsafe { (self.player.api.render_free)(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_relative_dll() {
        assert!(Api::load(Path::new("libmpv-2.dll")).is_err());
    }
    #[test]
    fn missing_absolute_dll_is_error() {
        assert!(Api::load(&std::env::temp_dir().join("vesper-absent-decoder-641c.dll")).is_err());
    }
}
