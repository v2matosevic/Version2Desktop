#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#[cfg(feature = "desktop")]
mod ui;
#[cfg(feature = "desktop")]
mod video;
use std::path::PathBuf;

fn main() {
    vesper::timing::init();
    let args: Vec<String> = std::env::args().collect();
    let value = |key: &str| {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
    };
    if let Some(file) = value("--controller-smoke") {
        let dll = value("--dll").unwrap_or_else(vesper::mpv::bundled_dll);
        let fixture = file.parent().unwrap_or(std::path::Path::new("."));
        match vesper::controller::check(
            &file,
            &dll,
            &fixture.join("caption.srt"),
            &fixture.join("caption.ass"),
        ) {
            Ok(v) => println!("{v}"),
            Err(e) => {
                println!("{}", serde_json::json!({"ok":false,"error":e}));
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(file) = value("--smoke").or_else(|| value("--audio-smoke")) {
        let dll = value("--dll").unwrap_or_else(vesper::mpv::bundled_dll);
        let out = value("--out").unwrap_or_else(|| {
            std::env::temp_dir().join(format!("vesper-smoke-{}", std::process::id()))
        });
        let result =
            vesper::smoke::run(&file, &dll, &out, args.iter().any(|a| a == "--audio-smoke"));
        println!("{}", serde_json::to_string(&result).unwrap());
        if !result.ok {
            std::process::exit(1);
        }
        return;
    }
    #[cfg(feature = "desktop")]
    {
        let file = args
            .get(1)
            .filter(|a| !a.starts_with("--"))
            .map(PathBuf::from);
        if let Err(e) = ui::run(file) {
            eprintln!("Vesper: {e}");
        }
    }
    #[cfg(not(feature = "desktop"))]
    eprintln!(
        "This build contains headless decode checks only. Build the default desktop feature for the player."
    );
}
