use eframe::egui;
use egui_kittest::{
    kittest::{NodeT, Queryable},
    Harness,
};
use sonora::{
    decoder::StreamInfo,
    player::{DeviceInfo, State},
    queue::Track,
    settings::Settings,
    ui::Sonora,
};
use std::{path::PathBuf, sync::Arc};

#[test]
fn native_ui_snapshots_and_keyboard_controls() {
    for (name, theme, scale, width, height) in [
        ("empty-dark", egui::Theme::Dark, 1.0, 1040.0, 720.0),
        ("queue-light", egui::Theme::Light, 1.0, 1040.0, 720.0),
        ("queue-dark-150", egui::Theme::Dark, 1.5, 1040.0, 720.0),
        ("compact-dark", egui::Theme::Dark, 1.0, 700.0, 520.0),
        ("compact-light", egui::Theme::Light, 1.0, 600.0, 640.0),
        ("cover-dark", egui::Theme::Dark, 1.0, 1040.0, 720.0),
        ("cover-light-200", egui::Theme::Light, 2.0, 820.0, 560.0),
        ("error-light", egui::Theme::Light, 1.0, 1040.0, 720.0),
        ("loading-dark", egui::Theme::Dark, 1.0, 1040.0, 720.0),
    ] {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(width,height))
            .with_pixels_per_point(scale)
            .with_theme(theme)
            .with_max_steps(10)
            .build_ui_state(
                |ui, app: &mut Option<Sonora>| {
                    let context = ui.ctx().clone();
                    let ctx = &context;
                    if app.is_none() {
                        v2_ui::install_with(
                            ctx,
                            v2_ui::App::Sonora,
                            v2_ui::Options {
                                reduced_motion: Some(true),
                                text_scale: Some(1.0),
                                ..Default::default()
                            },
                        );
                        ctx.set_theme(theme);
                        let mut a = Sonora::with_settings(ctx.clone(), vec![], Settings::default(), None);
                        if name != "empty-dark" {
                            a.queue.tracks = vec![
                                Track {
                                    path: PathBuf::from("Evening walk.flac"),
                                    title: "Evening walk".into(),
                                },
                                Track {
                                    path: PathBuf::from("Pjesma č 音楽.opus"),
                                    title: "Pjesma č 音楽".into(),
                                },
                                Track {
                                    path: PathBuf::from("Quiet morning.mp3"),
                                    title: "Quiet morning".into(),
                                },
                            ];
                            a.metadata.title = "Evening walk".into();
                            a.metadata.artist = "Sonora fixture".into();
                            a.metadata.album = "Local listening".into();
                        }
                        if name.starts_with("cover") {
                            a.visual_artwork(ctx, [96, 96], &cover());
                        }
                        *app = Some(a);
                        // Font definitions become active at the next frame when installed during UI.
                        ctx.request_repaint();
                        return;
                    }
                    if name=="error-light"{app.as_mut().unwrap().visual_error("Unsupported codec in selected file; the rest of the queue remains available");}
                    if name=="loading-dark"{app.as_mut().unwrap().visual_loading();}
                    // kittest frames the UI with an 8 pt margin; the real window has none.
                    let full = ctx.content_rect();
                    ui.scope_builder(egui::UiBuilder::new().max_rect(full), |ui| {
                        app.as_mut().unwrap().draw(ui)
                    });
                },
                None,
            );
        harness.run();
        for label in ["Previous", "Play", "Next", "Stop"] {
            let button = harness.get_by_label(label);
            assert_eq!(
                button.accesskit_node().is_disabled(),
                name == "empty-dark",
                "{label} must reflect whether the queue has tracks in {name}"
            );
        }
        // The evidence is the state as opened: shuffle and repeat still off.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evidence");
        std::fs::create_dir_all(&dir).unwrap();
        harness
            .render()
            .unwrap()
            .save(dir.join(format!("{name}.png")))
            .unwrap();
        assert!(
            !harness.state().as_ref().unwrap().queue.shuffle,
            "shuffle defaults off"
        );
        assert_eq!(
            harness.state().as_ref().unwrap().queue.repeat,
            sonora::queue::Repeat::Off,
            "repeat defaults off"
        );
        // AccessKit node bounds are physical pixels; pointer input uses logical points.
        let point =
            harness.get_by_label("Shuffle").rect().center() / harness.ctx.pixels_per_point();
        harness.hover_at(point);
        harness.drag_at(point);
        harness.run_steps(1);
        harness.drop_at(point);
        harness.run();
        assert!(harness.state().as_ref().unwrap().queue.shuffle);
        harness.key_press(egui::Key::R);
        harness.run();
        assert_eq!(
            harness.state().as_ref().unwrap().queue.repeat,
            sonora::queue::Repeat::All
        );
        if name == "queue-light" {
            // A focused control owns Space: it toggles shuffle and must not also start playback.
            harness.get_by_label("Shuffle").focus();
            harness.run();
            harness.key_press(egui::Key::Space);
            harness.run();
            let app = harness.state().as_ref().unwrap();
            assert!(
                !app.queue.shuffle,
                "Space must activate the focused Shuffle button"
            );
            assert_eq!(
                app.queue.current, None,
                "Space on a focused button must not also start playback"
            );
        }
        if name == "compact-dark" {
            let point = egui::pos2(250.0, 270.0);
            harness.hover_at(point);
            harness.event(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                phase: egui::TouchPhase::Move,
                delta: egui::vec2(0.0, -200.0),
                modifiers: egui::Modifiers::NONE,
            });
            harness.run();
            let last = harness.get_by_label("Quiet morning").rect();
            let stop = harness.get_by_label("Stop").rect();
            assert!(
                last.max.y < stop.min.y,
                "last row must scroll fully above transport"
            );
        }
    }
}

/// A 96 px stand-in cover: a warm diagonal gradient with a soft disc, so the ambient tint and
/// artwork framing can be judged without shipping a real album cover.
fn cover() -> Vec<u8> {
    let mut px = Vec::with_capacity(96 * 96 * 4);
    for y in 0..96 {
        for x in 0..96 {
            let t = (x + y) as f32 / 190.0;
            let (dx, dy) = (x as f32 - 60.0, y as f32 - 38.0);
            let disc = (1.0 - (dx * dx + dy * dy).sqrt() / 30.0).clamp(0.0, 1.0);
            let r = 214.0 - 120.0 * t + 40.0 * disc;
            let g = 92.0 - 40.0 * t + 120.0 * disc;
            let b = 72.0 + 70.0 * t + 60.0 * disc;
            px.extend([
                r.min(255.0) as u8,
                g.min(255.0) as u8,
                b.min(255.0) as u8,
                255,
            ]);
        }
    }
    px
}

fn tracks() -> Vec<Track> {
    [
        ("Evening walk", "Evening walk.flac"),
        ("Pjesma č 音楽", "Pjesma č 音楽.opus"),
        ("Quiet morning", "Quiet morning.mp3"),
        ("Harbour lights", "Harbour lights.m4a"),
    ]
    .into_iter()
    .map(|(title, file)| Track {
        path: PathBuf::from(file),
        title: title.into(),
    })
    .collect()
}

fn playing_state() -> State {
    State {
        generation: 0,
        playing: true,
        duration: 214.0,
        rate: 44_100,
        channels: 2,
        info: Some(StreamInfo {
            codec: "FLAC",
            lossless: true,
            bits: Some(16),
            rate: 44_100,
            channels: 2,
            duration: 214.0,
            ..Default::default()
        }),
        resampled: true,
        output: Some(Arc::new(DeviceInfo {
            name: "Speakers (Realtek Audio)".into(),
            rate: 48_000,
            channels: 2,
            format: "32-bit float".into(),
        })),
        next_ready: true,
        ..Default::default()
    }
}

/// Draws the app with fixture state and returns the harness for further interaction.
fn fixture(
    theme: egui::Theme,
    size: egui::Vec2,
    setup: impl Fn(&mut Sonora, &egui::Context) + 'static,
) -> Harness<'static, Option<Sonora>> {
    let mut harness = Harness::builder()
        .with_size(size)
        .with_theme(theme)
        .with_max_steps(12)
        .build_ui_state(
            move |ui, app: &mut Option<Sonora>| {
                let context = ui.ctx().clone();
                let ctx = &context;
                if app.is_none() {
                    v2_ui::install_with(
                        ctx,
                        v2_ui::App::Sonora,
                        v2_ui::Options {
                            reduced_motion: Some(true),
                            text_scale: Some(1.0),
                            ..Default::default()
                        },
                    );
                    ctx.set_theme(theme);
                    let mut a =
                        Sonora::with_settings(ctx.clone(), vec![], Settings::default(), None);
                    a.queue.tracks = tracks();
                    a.queue.current = Some(0);
                    a.metadata.title = "Evening walk".into();
                    a.metadata.artist = "Sonora fixture".into();
                    a.metadata.album = "Local listening".into();
                    setup(&mut a, ctx);
                    *app = Some(a);
                    ctx.request_repaint();
                    return;
                }
                let full = ctx.content_rect();
                ui.scope_builder(egui::UiBuilder::new().max_rect(full), |ui| {
                    app.as_mut().unwrap().draw(ui)
                });
            },
            None,
        );
    // A playing fixture asks for a repaint every 250 ms, so step a fixed number of frames.
    harness.run_steps(4);
    harness
}

fn save(harness: &mut Harness<'static, Option<Sonora>>, name: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evidence");
    std::fs::create_dir_all(&dir).unwrap();
    harness
        .render()
        .unwrap()
        .save(dir.join(format!("{name}.png")))
        .unwrap();
}

fn two_up(a: &mut Sonora, _: &egui::Context) {
    a.visual_state(playing_state(), 188.0);
    a.visual_crossfade(6.0);
    a.visual_next(Some(PathBuf::from("Pjesma č 音楽.opus")));
}

#[test]
fn crossfade_controls_details_and_queue_status_are_drawn_and_reachable() {
    // Playing, crossfade set, the next song prepared: chip, seek-bar zone and the "Up next" tag.
    let mut h = fixture(egui::Theme::Dark, egui::vec2(1040.0, 720.0), two_up);
    h.get_by_label_contains("Crossfade 6 s");
    h.get_by_label("Playback details");
    save(&mut h, "playing-crossfade-dark");

    // A blend in progress: the chip says so and the old track's row is tagged.
    let mut h = fixture(egui::Theme::Light, egui::vec2(1040.0, 720.0), |a, _| {
        let mut s = playing_state();
        s.fading = Some(3.2);
        a.visual_state(s, 3.0);
        a.visual_crossfade(6.0);
        a.queue.current = Some(1);
        a.metadata.title = "Pjesma č 音楽".into();
        a.visual_fading_from(Some(0));
        a.visual_next(Some(PathBuf::from("Quiet morning.mp3")));
    });
    h.get_by_label_contains("Crossfading");
    save(&mut h, "crossfading-light");

    // The crossfade panel.
    let mut h = fixture(egui::Theme::Dark, egui::vec2(1040.0, 720.0), |a, c| {
        two_up(a, c);
        a.visual_open_crossfade();
    });
    h.get_by_label("Crossfade length");
    save(&mut h, "crossfade-panel-dark");

    // Details open, on the light theme.
    let mut h = fixture(egui::Theme::Light, egui::vec2(1040.0, 720.0), |a, c| {
        two_up(a, c);
        a.visual_open_details();
    });
    save(&mut h, "details-light");

    // The narrow layout keeps both controls.
    let mut h = fixture(egui::Theme::Dark, egui::vec2(560.0, 700.0), two_up);
    h.get_by_label_contains("Crossfade 6 s");
    save(&mut h, "compact-crossfade-dark");

    // A skipped track is shown, not swallowed.
    let mut h = fixture(egui::Theme::Dark, egui::vec2(1040.0, 720.0), |a, c| {
        two_up(a, c);
        a.visual_warning("Harbour lights.m4a: Unsupported codec. Sonora will skip it.");
    });
    h.get_by_label_contains("Skipped a track");
    save(&mut h, "skipped-track-dark");
}

#[test]
fn the_crossfade_slider_works_from_the_keyboard_and_is_off_by_default() {
    let mut h = fixture(egui::Theme::Dark, egui::vec2(1040.0, 720.0), |a, _| {
        a.visual_state(playing_state(), 20.0);
        a.visual_open_crossfade();
    });
    assert_eq!(
        h.state().as_ref().unwrap().crossfade_seconds(),
        0.0,
        "a fresh install has crossfade off"
    );
    h.get_by_label_contains("Crossfade off");
    h.get_by_label("Crossfade length").focus();
    h.run_steps(3);
    for _ in 0..4 {
        h.key_press(egui::Key::ArrowRight);
        h.run_steps(3);
    }
    assert_eq!(
        h.state().as_ref().unwrap().crossfade_seconds(),
        2.0,
        "four half-second steps"
    );
    h.key_press(egui::Key::ArrowLeft);
    h.run_steps(3);
    assert_eq!(h.state().as_ref().unwrap().crossfade_seconds(), 1.5);
}

#[test]
fn the_c_key_toggles_crossfade_and_the_choice_is_remembered() {
    let dir = std::env::temp_dir().join(format!("sonora-ui-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let remembered = dir.clone();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1040.0, 720.0))
        .with_max_steps(12)
        .build_ui_state(
            move |ui, app: &mut Option<Sonora>| {
                let context = ui.ctx().clone();
                let ctx = &context;
                if app.is_none() {
                    v2_ui::install_with(
                        ctx,
                        v2_ui::App::Sonora,
                        v2_ui::Options {
                            reduced_motion: Some(true),
                            text_scale: Some(1.0),
                            ..Default::default()
                        },
                    );
                    let mut a = Sonora::with_settings(
                        ctx.clone(),
                        vec![],
                        Settings::default(),
                        Some(remembered.clone()),
                    );
                    a.queue.tracks = tracks();
                    *app = Some(a);
                    ctx.request_repaint();
                    return;
                }
                let full = ctx.content_rect();
                ui.scope_builder(egui::UiBuilder::new().max_rect(full), |ui| {
                    app.as_mut().unwrap().draw(ui)
                });
            },
            None,
        );
    harness.run();
    harness.key_press(egui::Key::C);
    harness.run();
    assert_eq!(
        harness.state().as_ref().unwrap().crossfade_seconds(),
        6.0,
        "C turns it on at the default length"
    );
    std::thread::sleep(std::time::Duration::from_millis(900));
    harness.run_steps(3);
    let on_disk = sonora::settings::load_from(&dir);
    assert_eq!(
        on_disk.crossfade_seconds, 6.0,
        "saved shortly after the change"
    );
    harness.key_press(egui::Key::C);
    harness.run();
    assert_eq!(harness.state().as_ref().unwrap().crossfade_seconds(), 0.0);
    std::thread::sleep(std::time::Duration::from_millis(900));
    harness.run_steps(3);
    assert_eq!(
        sonora::settings::load_from(&dir).crossfade_seconds,
        0.0,
        "off is remembered too"
    );
    // A new session starts from what was saved.
    let again = Sonora::with_settings(
        egui::Context::default(),
        vec![],
        sonora::settings::load_from(&dir),
        Some(dir.clone()),
    );
    assert_eq!(again.crossfade_seconds(), 0.0);
    std::fs::remove_dir_all(&dir).unwrap();
}
