#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
use std::path::PathBuf;

/// Self-ending command-line checks used by the release verification scripts. None of them opens
/// a window, changes a file or leaves anything running.
fn smoke(args: &[std::ffi::OsString]) -> Result<serde_json::Value, String> {
    let mode = args[0].to_string_lossy().into_owned();
    let number = |i: usize, what: &str| -> Result<f64, String> {
        args.get(i)
            .and_then(|a| a.to_str())
            .and_then(|a| a.parse::<f64>().ok())
            .filter(|n| n.is_finite())
            .ok_or_else(|| format!("{mode}: expected {what}"))
    };
    let files = |from: usize| {
        args.iter()
            .skip(from)
            .map(PathBuf::from)
            .filter(|p| p.to_str() != Some("--json"))
            .collect::<Vec<_>>()
    };
    match mode.as_str() {
        "--smoke" => {
            let path = args.get(1).map(PathBuf::from).unwrap_or_default();
            std::panic::catch_unwind(|| sonora::decoder::smoke(&path))
                .unwrap_or_else(|_| Err("Malformed media caused a decoder panic".into()))
                .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
        }
        "--device-smoke" => {
            let path = args.get(1).map(PathBuf::from).unwrap_or_default();
            sonora::player::device_smoke(path).map(
                |_| serde_json::json!({"device_output":"completed","audible_quality":"unverified"}),
            )
        }
        // --playback-smoke <crossfade seconds> <volume 0..1> <file> <file>...
        "--playback-smoke" => {
            let (crossfade, volume) = (
                number(1, "crossfade seconds")? as f32,
                number(2, "volume")? as f32,
            );
            sonora::player::playback_smoke(&files(3), crossfade, volume.clamp(0.0, 0.5))
        }
        // --mix-smoke <crossfade seconds> <device rate> <device channels> <file> <file>...
        "--mix-smoke" => {
            let (crossfade, rate, channels) = (
                number(1, "crossfade seconds")? as f32,
                number(2, "device rate")? as u32,
                number(3, "device channels")? as usize,
            );
            sonora::smoke::mix_smoke(&files(4), crossfade, rate, channels)
        }
        // --media-smoke: attaches to a hidden window, sets and reads back track details, removes them.
        #[cfg(windows)]
        "--media-smoke" => sonora::media_session::smoke(),
        other => Err(format!("Unknown option {other}")),
    }
}

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a.to_string_lossy().ends_with("-smoke") || a == "--smoke")
    {
        match smoke(&args) {
            Ok(report) => println!("{report}"),
            Err(e) => {
                eprintln!("{}", serde_json::json!({"error":e}));
                std::process::exit(1);
            }
        }
        return;
    }
    let paths = args.into_iter().map(PathBuf::from).collect();
    let result = eframe::run_native(
        "Sonora",
        v2_ui::native_options(v2_ui::App::Sonora),
        Box::new(move |cc| {
            v2_ui::install(&cc.egui_ctx, v2_ui::App::Sonora);
            #[allow(unused_mut)]
            let mut app = sonora::ui::Sonora::new(cc.egui_ctx.clone(), paths);
            #[cfg(windows)]
            if let Some(hwnd) = window_handle(cc) {
                app.attach_media(hwnd, cc.egui_ctx.clone());
            }
            Ok(Box::new(app))
        }),
    );
    if let Err(e) = result {
        eprintln!("Sonora: {e}");
    }
}

/// The Win32 window handle of the main window, for the media controls.
#[cfg(windows)]
fn window_handle(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}
