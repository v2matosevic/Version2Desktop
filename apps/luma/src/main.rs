#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::path::{Path, PathBuf};
fn main() -> eframe::Result {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|s| s == "--smoke") {
        let Some(path) = args.get(1) else {
            eprintln!("Usage: luma --smoke <image-or-folder> --json");
            std::process::exit(2)
        };
        let path = Path::new(path);
        let paths = if path.is_dir() {
            match luma::model::Album::open(path) {
                Ok(a) => a.files,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2)
                }
            }
        } else {
            vec![path.to_owned()]
        };
        let results: Vec<_> = paths.iter().map(|p| luma::decode::smoke(p)).collect();
        let failed = results.iter().any(|r| r.error.is_some());
        // Write to explicit file if requested; Windows GUI binaries may have no attached console.
        let json = serde_json::to_string_pretty(&results).unwrap();
        if let Some(i) = args.iter().position(|s| s == "--out")
            && let Some(out) = args.get(i + 1)
        {
            std::fs::write(out, &json).expect("write smoke report");
        }
        println!("{json}");
        std::process::exit(if failed { 1 } else { 0 });
    }
    if args.first().is_some_and(|s| s == "--bench") {
        // Self-ending decode and navigation benchmark; see src/bench.rs for what it measures.
        let Some(folder) = args.get(1) else {
            eprintln!("Usage: luma --bench <folder> [--think-ms N] [--out report.json]");
            std::process::exit(2)
        };
        let think = args
            .iter()
            .position(|s| s == "--think-ms")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.to_str()?.parse().ok())
            .unwrap_or(400);
        match luma::bench::run(Path::new(folder), think) {
            Ok(report) => {
                let json = serde_json::to_string_pretty(&report).unwrap();
                if let Some(i) = args.iter().position(|s| s == "--out")
                    && let Some(out) = args.get(i + 1)
                {
                    std::fs::write(out, &json).expect("write benchmark report");
                }
                println!("{json}");
                return Ok(());
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2)
            }
        }
    }
    if args.first().is_some_and(|s| s == "--codecs") {
        // Self-ending: which Windows codecs exist and which actually decoded their sample.
        let report = luma::wic::report();
        let json = serde_json::to_string_pretty(&report).unwrap();
        if let Some(i) = args.iter().position(|s| s == "--out")
            && let Some(out) = args.get(i + 1)
        {
            std::fs::write(out, &json).expect("write codec report");
        }
        println!("{json}");
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "--visual-smoke") {
        let output = args
            .get(1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("evidence/visual"));
        luma::visual::run(&output).expect("headless visual fixtures");
        return Ok(());
    }
    let mut options = v2_ui::native_options(v2_ui::App::Luma);
    options.viewport = options
        .viewport
        // Small enough to snap beside another window on a laptop screen; the header, dock and
        // details card have compact layouts down to this size (see the narrow-* captures).
        .with_min_inner_size([640.0, 440.0])
        .with_drag_and_drop(true);
    let initial = args.first().map(PathBuf::from);
    eframe::run_native(
        "Luma",
        options,
        Box::new(|cc| {
            luma::ui::install(&cc.egui_ctx);
            Ok(Box::new(luma::ui::Luma::new(&cc.egui_ctx, initial)))
        }),
    )
}
