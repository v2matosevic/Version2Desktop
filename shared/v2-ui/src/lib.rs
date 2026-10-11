//! Version2 desktop design system for the egui apps (Sonora, Luma, Vesper).
//!
//! ```ignore
//! fn main() -> eframe::Result {
//!     eframe::run_native("Sonora", v2_ui::native_options(v2_ui::App::Sonora), Box::new(|cc| {
//!         let installed = v2_ui::install(&cc.egui_ctx, v2_ui::App::Sonora);
//!         Ok(Box::new(MyApp::new(installed)))
//!     }))
//! }
//! ```
//!
//! Then per frame: `let p = v2_ui::palette(ctx);` for colours, the widgets for controls.
//! The rules behind the values live in `Version2Desktop/docs/DESIGN.md`.

pub mod chrome;
pub mod fonts;
pub mod icons;
pub mod palette;
pub mod style;
pub mod system;
pub mod tokens;
pub mod widgets;

pub use fonts::{semibold, FontReport};
pub use icons::Icon;
pub use palette::{contrast, Palette};
pub use widgets::{
    auto_hide, caption, chip, display, drop_overlay, focus_ring, format_duration, frame, kbd,
    loading, search_field, section_label, shortcut_table, slider, title, volume_slider, Banner, Button, EmptyState,
    IconButton, Kind, ListRow, Seek, Seekbar, Tone,
};

use egui::{Context, Id, Theme, ThemePreference};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum App {
    Sonora,
    Luma,
    Vesper,
    WinImprover,
}

impl App {
    pub const fn name(self) -> &'static str {
        match self {
            App::Sonora => "Sonora",
            App::Luma => "Luma",
            App::Vesper => "Vesper",
            App::WinImprover => "WinImprover",
        }
    }

    /// Stable id for the window class / taskbar grouping.
    pub const fn id(self) -> &'static str {
        match self {
            App::Sonora => "version2.sonora",
            App::Luma => "version2.luma",
            App::Vesper => "version2.vesper",
            App::WinImprover => "version2.winimprover",
        }
    }

    pub const fn default_size(self) -> [f32; 2] {
        match self {
            App::Sonora => [1040.0, 680.0],
            App::Luma => [1200.0, 800.0],
            App::Vesper => [1280.0, 760.0],
            App::WinImprover => [1180.0, 760.0],
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Fall back to Microsoft YaHei UI and Malgun Gothic for CJK text. Memory-mapped, so it costs
    /// address space, not RAM, until such text is drawn. Default on.
    pub cjk_fallback: bool,
    /// `None` reads Windows' "Animation effects" setting.
    pub reduced_motion: Option<bool>,
    /// `None` reads Windows' "Text size" setting.
    pub text_scale: Option<f32>,
}

impl Default for Options {
    fn default() -> Self {
        Self { cjk_fallback: true, reduced_motion: None, text_scale: None }
    }
}

/// What [`install`] found. Log it once at startup; it is the first thing to read when text
/// looks wrong on someone else's machine.
#[derive(Clone, Debug)]
pub struct Installed {
    pub app: App,
    pub fonts: FontReport,
    pub reduced_motion: bool,
    pub text_scale: f32,
}

#[derive(Clone, Copy)]
struct State {
    app: App,
    reduced_motion: bool,
}

fn state_id() -> Id {
    Id::new("v2-ui-state")
}

/// Fonts, both themes, follow-the-OS theme preference, reduced motion. Call once, at creation.
pub fn install(ctx: &Context, app: App) -> Installed {
    install_with(ctx, app, Options::default())
}

pub fn install_with(ctx: &Context, app: App, opts: Options) -> Installed {
    let (defs, report) = fonts::definitions(opts.cjk_fallback);
    ctx.set_fonts(defs);
    let reduced_motion = opts.reduced_motion.unwrap_or_else(|| !system::animations_enabled());
    let text_scale = opts.text_scale.unwrap_or_else(system::text_scale);
    ctx.set_style_of(Theme::Dark, style::build(&Palette::dark(app), text_scale, reduced_motion));
    ctx.set_style_of(Theme::Light, style::build(&Palette::light(app), text_scale, reduced_motion));
    ctx.options_mut(|o| o.theme_preference = ThemePreference::System);
    ctx.data_mut(|d| d.insert_temp(state_id(), State { app, reduced_motion }));
    Installed { app, fonts: report, reduced_motion, text_scale }
}

fn state(ctx: &Context) -> State {
    ctx.data(|d| d.get_temp(state_id()))
        .unwrap_or(State { app: App::WinImprover, reduced_motion: false })
}

/// The palette for the app and the theme currently showing.
pub fn palette(ctx: &Context) -> Palette {
    Palette::for_theme(state(ctx).app, ctx.theme())
}

/// The palette for what this `ui` draws on. Normally the app theme; a scope that switched to the
/// dark style (controls laid over video) gets the dark palette in either theme, so every widget
/// inside it follows.
pub fn palette_of(ui: &egui::Ui) -> Palette {
    Palette::for_theme(state(ui.ctx()).app, if ui.visuals().dark_mode { Theme::Dark } else { Theme::Light })
}

pub fn reduced_motion(ctx: &Context) -> bool {
    state(ctx).reduced_motion
}

/// Window defaults every app shares: glow renderer, title, taskbar id, size floor, icon.
#[cfg(feature = "eframe")]
pub fn native_options(app: App) -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(app.name())
        .with_app_id(app.id())
        .with_inner_size(app.default_size())
        .with_min_inner_size(tokens::size::MIN_WINDOW);
    if let Some(icon) = window_icon(app) {
        viewport = viewport.with_icon(icon);
    }
    eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        centered: true,
        ..Default::default()
    }
}

/// The app's mark as raw 64×64 RGBA, embedded at build time. `None` for WinImprover.
pub fn mark_rgba(app: App) -> Option<&'static [u8]> {
    let rgba: &'static [u8] = match app {
        App::Sonora => include_bytes!("../../assets/sonora/icon-64.rgba"),
        App::Luma => include_bytes!("../../assets/luma/icon-64.rgba"),
        App::Vesper => include_bytes!("../../assets/vesper/icon-64.rgba"),
        App::WinImprover => return None,
    };
    (rgba.len() == 64 * 64 * 4).then_some(rgba)
}

/// The app's mark as a 64×64 window icon. The .ico for the executable lives beside it in
/// `shared/assets/<app>/` and is the packaging step's job.
#[cfg(feature = "eframe")]
pub fn window_icon(app: App) -> Option<egui::IconData> {
    mark_rgba(app).map(|rgba| egui::IconData { rgba: rgba.to_vec(), width: 64, height: 64 })
}

#[cfg(all(test, feature = "eframe"))]
mod tests {
    use super::*;

    #[test]
    fn every_media_app_ships_a_real_window_icon() {
        for app in [App::Sonora, App::Luma, App::Vesper] {
            let icon = window_icon(app).unwrap_or_else(|| panic!("{app:?} icon-64.rgba is missing or not 64x64 RGBA"));
            // The marks are objects on transparency, not tiles, so "mostly opaque" no longer
            // applies. What must hold: a solid body (an all-transparent or speck-sized buffer would
            // pass the length check and show nothing) that spans the canvas, so it reads at 16 px.
            let opaque: Vec<(usize, usize)> = icon
                .rgba
                .chunks(4)
                .enumerate()
                .filter(|(_, px)| px[3] > 200)
                .map(|(i, _)| (i % 64, i / 64))
                .collect();
            assert!(opaque.len() > 64 * 64 / 4, "{app:?} icon has too little solid area ({} opaque px)", opaque.len());
            let span = |v: Vec<usize>| v.iter().max().unwrap() - v.iter().min().unwrap() + 1;
            let (w, h) = (span(opaque.iter().map(|p| p.0).collect()), span(opaque.iter().map(|p| p.1).collect()));
            assert!(w >= 48 && h >= 48, "{app:?} icon body is only {w}x{h} of 64x64");
        }
        assert!(window_icon(App::WinImprover).is_none(), "WinImprover keeps its Tauri icon");
    }
}
