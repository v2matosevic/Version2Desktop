//! Windows System Media Transport Controls (SMTC) for Sonora's own audio engine.
//!
//! Microsoft's automatic integration belongs to `MediaPlayer`; a custom engine has to drive the
//! controls itself. This session is attached to Sonora's own window, so the hardware media keys,
//! the volume flyout and the lock screen talk to Sonora only while Sonora is the app that is
//! playing. It registers no global hotkey and installs nothing: a key press arrives here only when
//! Windows routes it to this session, and when nothing is loaded the session is disabled so the
//! keys go to whichever other player is active.
//!
//! All Windows calls run on one small thread (the artwork hand-off waits for an async store). What
//! the UI wants shown is a single "latest wins" slot, never a queue, so a stalled call can never
//! pile up work or hold copies of old artwork; requests from Windows come back through a small
//! bounded channel that the Windows callback thread fills without ever blocking.

use std::{
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

/// A request from the Windows media controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MediaCommand {
    Play,
    Pause,
    Next,
    Previous,
    Stop,
    /// Position in seconds from the flyout's scrubber.
    SeekTo(f64),
}

/// Whether the Windows side accepted the session. Starting never blocks the window.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaStatus {
    Connecting,
    Connected,
    Unavailable(String),
}

impl MediaStatus {
    /// The sentence the details sheet shows.
    pub fn describe(&self) -> String {
        match self {
            MediaStatus::Connecting => "Connecting to Windows".into(),
            MediaStatus::Connected => {
                "Connected to Windows media controls (Windows chooses which player gets the keys)"
                    .into()
            }
            MediaStatus::Unavailable(why) => format!("Unavailable: {why}"),
        }
    }
}

/// Requests held for the UI thread at most; further ones are dropped, never waited for.
pub const COMMAND_QUEUE: usize = 16;

pub fn channel() -> (SyncSender<MediaCommand>, Receiver<MediaCommand>) {
    mpsc::sync_channel(COMMAND_QUEUE)
}

/// Cover art for the flyout: tightly packed RGBA.
#[derive(Clone, Debug, PartialEq)]
pub struct Artwork {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Everything Windows is told about what is playing. Sent only when it changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaSnapshot {
    /// A track is loaded (playing or paused). When false the session is disabled.
    pub active: bool,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    pub can_next: bool,
    pub can_previous: bool,
    /// Changes when the artwork does, so identical bytes are not re-encoded.
    pub art_serial: u64,
}

/// What the media thread should do with the flyout's cover image for one snapshot.
#[derive(Debug, PartialEq)]
pub enum ArtUpdate<'a> {
    Keep,
    Clear,
    Set(&'a Arc<Artwork>),
}

/// Compares against the cover already shown. `shown` is the serial last applied.
pub fn art_update<'a>(
    shown: &mut Option<u64>,
    snapshot: &MediaSnapshot,
    art: Option<&'a Arc<Artwork>>,
) -> ArtUpdate<'a> {
    if !snapshot.active {
        *shown = None;
        return ArtUpdate::Clear;
    }
    if *shown == Some(snapshot.art_serial) {
        return ArtUpdate::Keep;
    }
    *shown = Some(snapshot.art_serial);
    match art {
        Some(art) => ArtUpdate::Set(art),
        None => ArtUpdate::Clear,
    }
}

/// The latest thing the UI wants shown. Overwritten, not queued.
#[derive(Default)]
pub struct Slot {
    pending: Mutex<Pending>,
    wake: Condvar,
}

#[derive(Default)]
pub struct Pending {
    pub snapshot: Option<(MediaSnapshot, Option<Arc<Artwork>>)>,
    pub position: Option<(f64, f64)>,
    pub shutdown: bool,
}

impl Slot {
    pub fn set_snapshot(&self, snapshot: MediaSnapshot, art: Option<Arc<Artwork>>) {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        p.snapshot = Some((snapshot, art));
        p.position = None; // the snapshot sets the position too
        self.wake.notify_one();
    }
    pub fn set_position(&self, position: f64, duration: f64) {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        p.position = Some((position, duration));
        self.wake.notify_one();
    }
    pub fn shut_down(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown = true;
        self.wake.notify_one();
    }
    /// Waits for something to do and takes all of it at once.
    pub fn take(&self) -> Pending {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        while p.snapshot.is_none() && p.position.is_none() && !p.shutdown {
            p = self.wake.wait(p).unwrap_or_else(|e| e.into_inner());
        }
        std::mem::take(&mut *p)
    }
}

/// Decides which updates are worth sending: the full snapshot when anything but the position
/// changed, the position about once a second while playing, and straight away after a jump.
#[derive(Debug, Default)]
pub struct Throttle {
    last: Option<MediaSnapshot>,
    last_position: Option<(f64, std::time::Instant)>,
}

#[derive(Debug, PartialEq)]
pub enum Due {
    Snapshot,
    Position,
    Nothing,
}

impl Throttle {
    pub fn decide(
        &mut self,
        snapshot: &MediaSnapshot,
        position: f64,
        now: std::time::Instant,
    ) -> Due {
        if self.last.as_ref() != Some(snapshot) {
            self.last = Some(snapshot.clone());
            self.last_position = Some((position, now));
            return Due::Snapshot;
        }
        if !snapshot.active {
            return Due::Nothing;
        }
        let Some((previous, at)) = self.last_position else {
            self.last_position = Some((position, now));
            return Due::Position;
        };
        let elapsed = now.duration_since(at);
        let expected = previous
            + if snapshot.playing {
                elapsed.as_secs_f64()
            } else {
                0.0
            };
        let jumped = (position - expected).abs() > 1.5;
        let due = elapsed >= Duration::from_millis(1000);
        if jumped || (snapshot.playing && due) {
            self.last_position = Some((position, now));
            return Due::Position;
        }
        Due::Nothing
    }
}

#[cfg(windows)]
pub use platform::{smoke, MediaSession};

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use windows::{
        core::{factory, HSTRING},
        Foundation::{TimeSpan, TypedEventHandler},
        Media::{
            MediaPlaybackStatus, MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
            SystemMediaTransportControls, SystemMediaTransportControlsButton,
            SystemMediaTransportControlsButtonPressedEventArgs,
            SystemMediaTransportControlsTimelineProperties,
        },
        Storage::Streams::{DataWriter, InMemoryRandomAccessStream, RandomAccessStreamReference},
        Win32::{
            Foundation::HWND,
            System::WinRT::{
                ISystemMediaTransportControlsInterop, RoInitialize, RO_INIT_MULTITHREADED,
            },
        },
    };

    /// The live connection to the Windows media controls.
    pub struct MediaSession {
        slot: Arc<Slot>,
        status: Arc<Mutex<MediaStatus>>,
    }

    fn span(seconds: f64) -> TimeSpan {
        TimeSpan {
            Duration: (seconds.max(0.0) * 10_000_000.0) as i64,
        }
    }

    fn button_command(button: SystemMediaTransportControlsButton) -> Option<MediaCommand> {
        match button {
            SystemMediaTransportControlsButton::Play => Some(MediaCommand::Play),
            SystemMediaTransportControlsButton::Pause => Some(MediaCommand::Pause),
            SystemMediaTransportControlsButton::Stop => Some(MediaCommand::Stop),
            SystemMediaTransportControlsButton::Next => Some(MediaCommand::Next),
            SystemMediaTransportControlsButton::Previous => Some(MediaCommand::Previous),
            _ => None,
        }
    }

    fn png(art: &Artwork) -> Option<Vec<u8>> {
        use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(
                &art.rgba,
                art.width as u32,
                art.height as u32,
                ExtendedColorType::Rgba8,
            )
            .ok()?;
        Some(bytes)
    }

    fn thumbnail(art: &Artwork) -> Result<RandomAccessStreamReference, String> {
        let bytes = png(art).ok_or("Cover art could not be encoded")?;
        let stream = InMemoryRandomAccessStream::new().map_err(|e| e.message())?;
        let writer = DataWriter::CreateDataWriter(&stream).map_err(|e| e.message())?;
        writer.WriteBytes(&bytes).map_err(|e| e.message())?;
        writer
            .StoreAsync()
            .map_err(|e| e.message())?
            .join()
            .map_err(|e| e.message())?;
        writer.DetachStream().map_err(|e| e.message())?;
        stream.Seek(0).map_err(|e| e.message())?;
        RandomAccessStreamReference::CreateFromStream(&stream).map_err(|e| e.message())
    }

    fn control_for(hwnd: isize) -> Result<SystemMediaTransportControls, String> {
        let interop =
            factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()
                .map_err(|e| format!("Windows media controls are unavailable: {}", e.message()))?;
        unsafe { interop.GetForWindow::<SystemMediaTransportControls>(HWND(hwnd as *mut c_void)) }
            .map_err(|e| format!("Windows media controls are unavailable: {}", e.message()))
    }

    /// `enable` is false only in the readback check, which must not make the session a candidate
    /// for the media keys.
    fn apply(
        smtc: &SystemMediaTransportControls,
        snapshot: &MediaSnapshot,
        art: ArtUpdate,
        enable: bool,
    ) -> Result<(), String> {
        let e = |e: windows::core::Error| e.message();
        let updater = smtc.DisplayUpdater().map_err(e)?;
        if !snapshot.active {
            smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed)
                .map_err(e)?;
            updater.ClearAll().map_err(e)?;
            updater.Update().map_err(e)?;
            // Disabled, so the hardware keys go to whichever other player is active.
            smtc.SetIsEnabled(false).map_err(e)?;
            return Ok(());
        }
        smtc.SetIsEnabled(enable).map_err(e)?;
        smtc.SetIsPlayEnabled(true).map_err(e)?;
        smtc.SetIsPauseEnabled(true).map_err(e)?;
        smtc.SetIsStopEnabled(true).map_err(e)?;
        smtc.SetIsNextEnabled(snapshot.can_next).map_err(e)?;
        smtc.SetIsPreviousEnabled(snapshot.can_previous)
            .map_err(e)?;
        updater.SetType(MediaPlaybackType::Music).map_err(e)?;
        let music = updater.MusicProperties().map_err(e)?;
        music
            .SetTitle(&HSTRING::from(snapshot.title.as_str()))
            .map_err(e)?;
        music
            .SetArtist(&HSTRING::from(snapshot.artist.as_str()))
            .map_err(e)?;
        music
            .SetAlbumTitle(&HSTRING::from(snapshot.album.as_str()))
            .map_err(e)?;
        match art {
            ArtUpdate::Keep => {}
            ArtUpdate::Clear => updater
                .SetThumbnail(None::<&RandomAccessStreamReference>)
                .map_err(e)?,
            ArtUpdate::Set(art) => {
                if let Ok(reference) = thumbnail(art) {
                    updater.SetThumbnail(&reference).map_err(e)?;
                }
            }
        }
        updater.Update().map_err(e)?;
        smtc.SetPlaybackStatus(if snapshot.playing {
            MediaPlaybackStatus::Playing
        } else {
            MediaPlaybackStatus::Paused
        })
        .map_err(e)?;
        Ok(())
    }

    fn timeline(
        smtc: &SystemMediaTransportControls,
        position: f64,
        duration: f64,
    ) -> Result<(), String> {
        let e = |e: windows::core::Error| e.message();
        let t = SystemMediaTransportControlsTimelineProperties::new().map_err(e)?;
        t.SetStartTime(span(0.0)).map_err(e)?;
        t.SetEndTime(span(duration)).map_err(e)?;
        t.SetMinSeekTime(span(0.0)).map_err(e)?;
        t.SetMaxSeekTime(span(duration)).map_err(e)?;
        t.SetPosition(span(position.min(duration))).map_err(e)?;
        smtc.UpdateTimelineProperties(&t).map_err(e)
    }

    impl MediaSession {
        /// Attaches to the window `hwnd` (a top-level window of this process). Returns at once;
        /// the Windows side answers on its own thread and [`MediaSession::status`] says how it went.
        pub fn start(
            hwnd: isize,
            commands: SyncSender<MediaCommand>,
            wake: impl Fn() + Send + Sync + 'static,
        ) -> Result<Self, String> {
            let slot = Arc::new(Slot::default());
            let status = Arc::new(Mutex::new(MediaStatus::Connecting));
            let (worker_slot, worker_status) = (slot.clone(), status.clone());
            let wake = Arc::new(wake);
            let publish =
                move |s: MediaStatus| *worker_status.lock().unwrap_or_else(|e| e.into_inner()) = s;
            std::thread::Builder::new()
                .name("sonora-media-keys".into())
                .spawn(move || {
                    // A second initialisation on this thread is harmless; a different apartment
                    // type means something else set the thread up, which also works for SMTC.
                    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
                    let smtc = match control_for(hwnd) {
                        Ok(s) => s,
                        Err(e) => {
                            publish(MediaStatus::Unavailable(e));
                            return;
                        }
                    };
                    // Windows calls these on its own threads: they only try_send and wake the UI.
                    let (tx, wake_buttons) = (commands.clone(), wake.clone());
                    let buttons =
                        smtc.ButtonPressed(&TypedEventHandler::<
                            SystemMediaTransportControls,
                            SystemMediaTransportControlsButtonPressedEventArgs,
                        >::new(move |_, args| {
                            if let Some(command) = args.ok()?.Button().ok().and_then(button_command)
                            {
                                let _ = tx.try_send(command);
                                wake_buttons();
                            }
                            Ok(())
                        }));
                    let (tx, wake_seeks) = (commands.clone(), wake.clone());
                    let seeks = smtc.PlaybackPositionChangeRequested(&TypedEventHandler::<
                        SystemMediaTransportControls,
                        PlaybackPositionChangeRequestedEventArgs,
                    >::new(
                        move |_, args| {
                            if let Ok(at) = args.ok()?.RequestedPlaybackPosition() {
                                let _ = tx.try_send(MediaCommand::SeekTo(
                                    at.Duration as f64 / 10_000_000.0,
                                ));
                                wake_seeks();
                            }
                            Ok(())
                        },
                    ));
                    // Both handlers or neither: a half-registered session would show controls that do nothing.
                    let remove =
                        |buttons: &windows::core::Result<i64>,
                         seeks: &windows::core::Result<i64>| {
                            if let Ok(token) = buttons {
                                let _ = smtc.RemoveButtonPressed(*token);
                            }
                            if let Ok(token) = seeks {
                                let _ = smtc.RemovePlaybackPositionChangeRequested(*token);
                            }
                        };
                    if let Err(e) = buttons.as_ref().and(seeks.as_ref()) {
                        let why =
                            format!("Windows refused the media key handlers: {}", e.message());
                        remove(&buttons, &seeks);
                        let _ = apply(&smtc, &MediaSnapshot::default(), ArtUpdate::Clear, true);
                        publish(MediaStatus::Unavailable(why));
                        return;
                    }
                    publish(MediaStatus::Connected);
                    let mut shown_art = None;
                    loop {
                        let work = worker_slot.take();
                        if let Some((snapshot, art)) = work.snapshot {
                            let update = art_update(&mut shown_art, &snapshot, art.as_ref());
                            let _ = apply(&smtc, &snapshot, update, true);
                            if snapshot.active && work.position.is_none() {
                                let _ = timeline(&smtc, 0.0, snapshot.duration);
                            }
                        }
                        if let Some((position, duration)) = work.position {
                            let _ = timeline(&smtc, position, duration);
                        }
                        if work.shutdown {
                            break;
                        }
                    }
                    // Leave nothing behind: disabled, empty, handlers removed.
                    let _ = apply(&smtc, &MediaSnapshot::default(), ArtUpdate::Clear, true);
                    remove(&buttons, &seeks);
                })
                .map_err(|e| e.to_string())?;
            Ok(Self { slot, status })
        }

        pub fn status(&self) -> MediaStatus {
            self.status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        pub fn update(&self, snapshot: MediaSnapshot, art: Option<Arc<Artwork>>) {
            self.slot.set_snapshot(snapshot, art);
        }

        pub fn position(&self, position: f64, duration: f64) {
            self.slot.set_position(position, duration);
        }
    }

    impl Drop for MediaSession {
        fn drop(&mut self) {
            self.slot.shut_down();
        }
    }

    /// Self-ending check against the real Windows API. Attaches to a hidden window, writes what a
    /// track would write while the session stays disabled, reads it back from Windows, then
    /// starts and closes a real session to prove its handlers register and are removed. No key
    /// press of any kind is sent; which player receives the hardware keys is decided by Windows
    /// and remains an owner check.
    pub fn smoke() -> Result<serde_json::Value, String> {
        use windows::Win32::{
            Foundation::HINSTANCE,
            UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP},
        };
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0x80), // tool window: never in the taskbar
                windows::core::w!("STATIC"),
                windows::core::w!("Sonora media check"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(HINSTANCE::default()),
                None,
            )
        }
        .map_err(|e| format!("Cannot create the check window: {}", e.message()))?;
        let result = (|| {
            let e = |e: windows::core::Error| e.message();
            let smtc = control_for(hwnd.0 as isize)?;
            let art = Arc::new(Artwork {
                width: 4,
                height: 4,
                rgba: [255, 122, 89, 255].repeat(16),
            });
            let snapshot = MediaSnapshot {
                active: true,
                playing: false,
                title: "Sonora check".into(),
                artist: "Version2".into(),
                album: "Offline tests".into(),
                duration: 60.0,
                can_next: true,
                can_previous: true,
                art_serial: 1,
            };
            apply(&smtc, &snapshot, ArtUpdate::Set(&art), false)?;
            timeline(&smtc, 12.0, 60.0)?;
            let updater = smtc.DisplayUpdater().map_err(e)?;
            let music = updater.MusicProperties().map_err(e)?;
            let mut report = serde_json::json!({
                "attached_to_own_window": true,
                "title_read_back": music.Title().map_err(e)?.to_string(),
                "artist_read_back": music.Artist().map_err(e)?.to_string(),
                "album_read_back": music.AlbumTitle().map_err(e)?.to_string(),
                "type_music": updater.Type().map_err(e)? == MediaPlaybackType::Music,
                "enabled_while_checking": smtc.IsEnabled().map_err(e)?,
                "next_enabled": smtc.IsNextEnabled().map_err(e)?,
                "previous_enabled": smtc.IsPreviousEnabled().map_err(e)?,
                "status_read_back_is_paused": smtc.PlaybackStatus().map_err(e)? == MediaPlaybackStatus::Paused,
                "thumbnail_set": updater.Thumbnail().is_ok(),
            });
            apply(&smtc, &MediaSnapshot::default(), ArtUpdate::Clear, false)?;
            // A real session: handlers must register (status Connected) and be removed on close.
            let (tx, _rx) = channel();
            let session = MediaSession::start(hwnd.0 as isize, tx, || {})?;
            let waited = std::time::Instant::now();
            while session.status() == MediaStatus::Connecting
                && waited.elapsed() < Duration::from_secs(5)
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            report["session_status"] = serde_json::json!(session.status().describe());
            let connected = session.status() == MediaStatus::Connected;
            drop(session);
            std::thread::sleep(Duration::from_millis(300));
            report["session_connected"] = serde_json::json!(connected);
            report["disabled_after_close"] = serde_json::json!(!smtc.IsEnabled().map_err(e)?);
            report["key_delivery"] = serde_json::json!("not exercised: no hardware or synthetic key was sent; routing between players is an owner check");
            Ok::<_, String>(report)
        })();
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn playing(title: &str) -> MediaSnapshot {
        MediaSnapshot {
            active: true,
            playing: true,
            title: title.into(),
            duration: 200.0,
            ..Default::default()
        }
    }

    #[test]
    fn changes_are_sent_at_once_and_the_position_about_every_second() {
        let mut t = Throttle::default();
        let start = Instant::now();
        let snap = playing("One");
        assert_eq!(t.decide(&snap, 0.0, start), Due::Snapshot);
        assert_eq!(
            t.decide(&snap, 0.25, start + Duration::from_millis(250)),
            Due::Nothing
        );
        assert_eq!(
            t.decide(&snap, 1.05, start + Duration::from_millis(1050)),
            Due::Position
        );
        assert_eq!(
            t.decide(&snap, 1.5, start + Duration::from_millis(1500)),
            Due::Nothing
        );
        assert_eq!(
            t.decide(&snap, 2.1, start + Duration::from_millis(2100)),
            Due::Position
        );
        let mut paused = snap.clone();
        paused.playing = false;
        assert_eq!(
            t.decide(&paused, 2.2, start + Duration::from_millis(2200)),
            Due::Snapshot,
            "pausing is a state change"
        );
        assert_eq!(
            t.decide(&paused, 2.2, start + Duration::from_secs(10)),
            Due::Nothing,
            "a paused position is not repeated"
        );
    }

    #[test]
    fn a_seek_is_sent_immediately_and_a_new_track_resets_everything() {
        let mut t = Throttle::default();
        let start = Instant::now();
        let snap = playing("One");
        t.decide(&snap, 0.0, start);
        assert_eq!(
            t.decide(&snap, 90.0, start + Duration::from_millis(100)),
            Due::Position,
            "a jump does not wait for the second"
        );
        assert_eq!(
            t.decide(&playing("Two"), 0.0, start + Duration::from_millis(200)),
            Due::Snapshot
        );
    }

    #[test]
    fn nothing_loaded_sends_one_clearing_snapshot_then_stays_quiet() {
        let mut t = Throttle::default();
        let now = Instant::now();
        let idle = MediaSnapshot::default();
        assert_eq!(t.decide(&idle, 0.0, now), Due::Snapshot);
        assert_eq!(
            t.decide(&idle, 0.0, now + Duration::from_secs(5)),
            Due::Nothing
        );
        assert_eq!(
            t.decide(&playing("One"), 0.0, now + Duration::from_secs(6)),
            Due::Snapshot
        );
    }

    #[test]
    fn the_slot_keeps_only_the_latest_request_and_wakes_the_worker_once() {
        let slot = Slot::default();
        for i in 0..1000 {
            slot.set_snapshot(
                playing(&format!("track {i}")),
                Some(Arc::new(Artwork {
                    width: 1,
                    height: 1,
                    rgba: vec![0; 4],
                })),
            );
            slot.set_position(i as f64, 100.0);
        }
        slot.set_snapshot(playing("last"), None);
        slot.set_position(7.0, 100.0);
        let work = slot.take();
        assert_eq!(
            work.snapshot.as_ref().map(|(s, _)| s.title.as_str()),
            Some("last")
        );
        assert_eq!(work.position, Some((7.0, 100.0)));
        assert!(!work.shutdown);
        slot.shut_down();
        assert!(
            slot.take().shutdown,
            "shutdown is never lost behind pending work"
        );
    }

    #[test]
    fn cover_art_is_encoded_once_per_track_and_cleared_when_nothing_is_loaded() {
        let art = Arc::new(Artwork {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        });
        let mut shown = None;
        let mut one = playing("One");
        one.art_serial = 1;
        assert_eq!(
            art_update(&mut shown, &one, Some(&art)),
            ArtUpdate::Set(&art)
        );
        assert_eq!(
            art_update(&mut shown, &one, Some(&art)),
            ArtUpdate::Keep,
            "same cover: nothing to re-send"
        );
        let mut two = playing("Two");
        two.art_serial = 2;
        assert_eq!(
            art_update(&mut shown, &two, None),
            ArtUpdate::Clear,
            "a track without cover removes the old one"
        );
        assert_eq!(
            art_update(&mut shown, &MediaSnapshot::default(), None),
            ArtUpdate::Clear
        );
        assert_eq!(
            art_update(&mut shown, &one, Some(&art)),
            ArtUpdate::Set(&art),
            "after idling the cover is set again"
        );
    }

    #[test]
    fn requests_from_windows_are_bounded_and_never_block_its_thread() {
        let (tx, rx) = channel();
        for _ in 0..(COMMAND_QUEUE * 4) {
            let _ = tx.try_send(MediaCommand::Next);
        }
        assert_eq!(rx.try_iter().count(), COMMAND_QUEUE);
    }
}
