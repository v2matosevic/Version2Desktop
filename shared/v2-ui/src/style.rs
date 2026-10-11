//! Maps the palette and tokens onto egui's own `Style`, so stock egui widgets (menus, text
//! edits, scroll areas, combo boxes) already look like the system before any v2 widget is used.

use egui::style::{Selection, WidgetVisuals, Widgets};
use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, Style, TextStyle, Vec2};

use crate::palette::Palette;
use crate::tokens::{radius, size, space, text};

/// Named text styles beyond egui's five. Use through [`crate::title`] / [`crate::display`].
pub const TITLE: &str = "v2-title";
pub const DISPLAY: &str = "v2-display";

pub fn build(p: &Palette, text_scale: f32, reduced_motion: bool) -> Style {
    let mut style = Style::default();
    let s = |pt: f32| pt * text_scale;
    let semibold = crate::fonts::semibold();

    style.text_styles = [
        (TextStyle::Small, FontId::new(s(text::CAPTION), FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(s(text::BODY), FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(s(text::BODY), FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(s(text::MONO), FontFamily::Monospace)),
        (TextStyle::Heading, FontId::new(s(text::HEADING), semibold.clone())),
        (TextStyle::Name(TITLE.into()), FontId::new(s(text::TITLE), semibold.clone())),
        (TextStyle::Name(DISPLAY.into()), FontId::new(s(text::DISPLAY), semibold)),
    ]
    .into();

    let sp = &mut style.spacing;
    sp.item_spacing = Vec2::new(space::SM, space::SM);
    sp.button_padding = Vec2::new(space::MD, space::XS + 1.0);
    sp.interact_size = Vec2::new(size::CONTROL, size::CONTROL_SM);
    sp.window_margin = Margin::same(space::MD as i8);
    sp.menu_margin = Margin::same(space::XS as i8 + 2);
    sp.menu_spacing = space::XXS;
    sp.slider_rail_height = size::RAIL;
    sp.icon_width = 16.0;
    sp.icon_width_inner = 8.0;
    sp.icon_spacing = space::SM;
    sp.tooltip_width = 320.0;
    sp.combo_height = 280.0;
    sp.scroll = egui::style::ScrollStyle::floating();

    style.visuals = visuals(p);
    style.animation_time = if reduced_motion { 0.0 } else { crate::tokens::motion::FAST };
    style.scroll_animation = if reduced_motion {
        egui::style::ScrollAnimation::none()
    } else {
        egui::style::ScrollAnimation::default()
    };
    style.interaction.tooltip_delay = 0.45;
    style.interaction.selectable_labels = false;
    style.url_in_tooltip = true;
    style
}

fn wv(fill: Color32, stroke: Stroke, fg: Color32, r: u8) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill: fill,
        weak_bg_fill: fill,
        bg_stroke: stroke,
        corner_radius: CornerRadius::same(r),
        fg_stroke: Stroke::new(1.5, fg),
        expansion: 0.0,
    }
}

pub fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let hair = |c| Stroke::new(1.0, c);

    // Interactive controls (stock buttons, text fields, checkboxes, radios, combo boxes) take their
    // outline from these strokes, and the outline is what identifies them, so it must reach 3:1
    // against the ground: `border_strong`, never the hairline `border`. Hover steps up to `muted`
    // so the change stays visible. `noninteractive` (frames, separators) stays a hairline.
    v.widgets = Widgets {
        noninteractive: wv(p.surface, hair(p.border), p.text, radius::MD),
        inactive: wv(p.raised, hair(p.border_strong), p.text, radius::MD),
        hovered: wv(p.hover, hair(p.muted), p.text, radius::MD),
        active: wv(p.pressed, hair(p.focus), p.text, radius::MD),
        open: wv(p.hover, hair(p.muted), p.text, radius::MD),
    };
    v.selection = Selection {
        bg_fill: p.accent.gamma_multiply(if p.dark { 0.38 } else { 0.22 }),
        stroke: Stroke::new(1.5, p.focus),
    };
    v.override_text_color = None;
    v.weak_text_color = Some(p.muted);
    v.hyperlink_color = p.accent_text;
    v.faint_bg_color = if p.dark { p.surface } else { p.hover };
    v.extreme_bg_color = if p.dark { p.bg } else { p.raised };
    v.text_edit_bg_color = Some(if p.dark { p.bg } else { p.raised });
    v.code_bg_color = p.raised;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.window_corner_radius = CornerRadius::same(radius::LG);
    v.menu_corner_radius = CornerRadius::same(radius::MD + 2);
    v.window_fill = p.raised;
    v.window_stroke = hair(p.border);
    v.panel_fill = p.bg;
    let shade = if p.dark { 110 } else { 34 };
    v.window_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(shade) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(shade) };
    v.text_cursor.stroke = Stroke::new(2.0, p.accent_text);
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.striped = false;
    v.indent_has_left_vline = false;
    v.collapsing_header_frame = false;
    // Windows controls keep the arrow; the hand is for links only.
    v.interact_cursor = None;
    v.image_loading_spinners = false;
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::contrast;
    use crate::App;

    const APPS: [App; 4] = [App::Sonora, App::Luma, App::Vesper, App::WinImprover];

    /// A control outline is readable when it reaches 3:1 against every ground a control sits on.
    fn outline_ok(c: Color32, p: &Palette) -> Result<(), String> {
        for (ground, g) in [("bg", p.bg), ("surface", p.surface)] {
            let r = contrast(c, g);
            if r < 3.0 {
                return Err(format!("{r:.2}:1 on {ground}"));
            }
        }
        Ok(())
    }

    #[test]
    fn interactive_control_outlines_reach_3_to_1() {
        // The strokes egui itself draws around stock buttons, text fields, checkboxes and
        // radios, read from the Visuals the apps actually install.
        let mut failures = Vec::new();
        for app in APPS {
            for p in [Palette::dark(app), Palette::light(app)] {
                let w = visuals(&p).widgets;
                for (state, s) in [
                    ("inactive", w.inactive.bg_stroke),
                    ("hovered", w.hovered.bg_stroke),
                    ("active", w.active.bg_stroke),
                    ("open", w.open.bg_stroke),
                ] {
                    if let Err(e) = outline_ok(s.color, &p) {
                        failures.push(format!("{app:?} dark={} {state}: {e}", p.dark));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "control outlines too faint:\n{}", failures.join("\n"));
    }

    #[test]
    fn hover_outline_differs_from_rest() {
        for app in APPS {
            for p in [Palette::dark(app), Palette::light(app)] {
                let w = visuals(&p).widgets;
                assert_ne!(w.inactive.bg_stroke.color, w.hovered.bg_stroke.color, "{app:?}");
            }
        }
    }

    #[test]
    fn the_shipped_hairline_would_have_failed() {
        // The token the outlines used until 2026-10-10 measured 1.20-1.34:1 in Sonora's
        // screenshots. The check must reject it, or it guards nothing.
        for app in APPS {
            for p in [Palette::dark(app), Palette::light(app)] {
                assert!(outline_ok(p.border, &p).is_err(), "{app:?} dark={}", p.dark);
            }
        }
    }
}
