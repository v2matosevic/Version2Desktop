//! Colour tokens. Neutrals are shared by every app; each app owns exactly one accent hue.
//!
//! Contrast promises are tests, not prose (see the bottom of this file):
//! `text` and `muted` reach 4.5:1 on `bg`, `surface` and `raised`; `accent_text`, `danger`,
//! `warning` and `success` reach 4.5:1 on `bg` and `surface`; `on_accent` reaches 4.5:1 on
//! `accent` and `accent_hover`; `focus` and `border_strong`-level UI parts reach 3:1 on `bg`.
//! `faint` is for disabled and placeholder text only and promises nothing.

use egui::Color32;

use crate::App;

/// Every colour a v2 app paints with. `Copy`, so fetch it once per frame with
/// [`crate::palette`] and pass it around.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Window base and central area.
    pub bg: Color32,
    /// Side panels, bars and lists.
    pub surface: Color32,
    /// Cards, popovers, menus and buttons at rest.
    pub raised: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    /// Secondary text: metadata, durations, captions.
    pub muted: Color32,
    /// Disabled and placeholder text only.
    pub faint: Color32,
    /// Fills: primary button, seek progress, selected item, toggled icon background.
    pub accent: Color32,
    pub accent_hover: Color32,
    /// Text and icons drawn on `accent`.
    pub on_accent: Color32,
    /// The accent used as text or an icon on `bg`/`surface` (links, toggled icons, active row title).
    pub accent_text: Color32,
    /// Keyboard focus ring.
    pub focus: Color32,
    /// Neutral surround for photos. Never tinted by the accent.
    pub stage: Color32,
    pub danger: Color32,
    pub warning: Color32,
    pub success: Color32,
    pub danger_bg: Color32,
    pub warning_bg: Color32,
    pub success_bg: Color32,
    /// Scrim behind on-video controls. Identical in both themes, because video is always dark.
    pub overlay: Color32,
    pub on_overlay: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// One app's accent, per theme: `[accent, accent_hover, on_accent, accent_text]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accent {
    pub dark: [u32; 4],
    pub light: [u32; 4],
}

impl App {
    pub const fn accent(self) -> Accent {
        match self {
            // Ember coral: warmth of a played record. Dark text on the bright dark-theme fill.
            App::Sonora => Accent {
                dark: [0xFF7A59, 0xFF9174, 0x1C0D07, 0xFF8D70],
                light: [0xC4391F, 0xAD2F17, 0xFFFFFF, 0xB5341B],
            },
            // Clear teal: daylight on water. Kept away from blue so it never reads as a link.
            App::Luma => Accent {
                dark: [0x38D1C0, 0x5CDCCD, 0x00201C, 0x4AD8C8],
                light: [0x00786D, 0x00665D, 0xFFFFFF, 0x00756A],
            },
            // Dusk violet: the evening star.
            App::Vesper => Accent {
                dark: [0xA48BFF, 0xB7A2FF, 0x150A33, 0xB4A0FF],
                light: [0x6440D8, 0x5532C4, 0xFFFFFF, 0x5B37CF],
            },
            // WinImprover keeps the blue it already shipped with.
            App::WinImprover => Accent {
                dark: [0x60A5FA, 0x7DB6FB, 0x06162B, 0x7AB4FB],
                light: [0x1D63D8, 0x1853B8, 0xFFFFFF, 0x1A5BC7],
            },
        }
    }
}

impl Palette {
    pub fn dark(app: App) -> Self {
        let [accent, accent_hover, on_accent, accent_text] = app.accent().dark.map(hex);
        Self {
            dark: true,
            bg: hex(0x111215),
            surface: hex(0x17181C),
            raised: hex(0x1E2025),
            hover: hex(0x272930),
            pressed: hex(0x2F323A),
            border: hex(0x2A2C33),
            border_strong: hex(0x63676F),
            text: hex(0xECEDF0),
            muted: hex(0xA2A5AE),
            faint: hex(0x6B6E77),
            accent,
            accent_hover,
            on_accent,
            accent_text,
            focus: accent_text,
            stage: hex(0x0B0B0D),
            danger: hex(0xFF8080),
            warning: hex(0xF0B54A),
            success: hex(0x4CCB8F),
            danger_bg: hex(0x3A1D21),
            warning_bg: hex(0x372C17),
            success_bg: hex(0x15302A),
            overlay: Color32::from_rgba_unmultiplied(10, 10, 12, 199),
            on_overlay: hex(0xF2F2F4),
        }
    }

    pub fn light(app: App) -> Self {
        let [accent, accent_hover, on_accent, accent_text] = app.accent().light.map(hex);
        Self {
            dark: false,
            bg: hex(0xF4F4F6),
            surface: hex(0xFAFAFB),
            raised: hex(0xFFFFFF),
            hover: hex(0xEBECEF),
            pressed: hex(0xE0E2E6),
            border: hex(0xDDDFE4),
            border_strong: hex(0x858993),
            text: hex(0x15161A),
            muted: hex(0x565963),
            faint: hex(0x9A9DA5),
            accent,
            accent_hover,
            on_accent,
            accent_text,
            focus: accent_text,
            stage: hex(0xE6E6E9),
            danger: hex(0xBF3227),
            warning: hex(0x895800),
            success: hex(0x1A7848),
            danger_bg: hex(0xFBE8E6),
            warning_bg: hex(0xFAF0DA),
            success_bg: hex(0xE2F3E9),
            overlay: Color32::from_rgba_unmultiplied(10, 10, 12, 199),
            on_overlay: hex(0xF2F2F4),
        }
    }

    pub fn for_theme(app: App, theme: egui::Theme) -> Self {
        match theme {
            egui::Theme::Dark => Self::dark(app),
            egui::Theme::Light => Self::light(app),
        }
    }
}

/// WCAG 2.x relative luminance of an opaque colour.
pub fn luminance(c: Color32) -> f32 {
    fn lin(v: u8) -> f32 {
        let s = v as f32 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio, 1.0..=21.0.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPS: [App; 4] = [App::Sonora, App::Luma, App::Vesper, App::WinImprover];

    fn every_palette() -> impl Iterator<Item = (String, Palette)> {
        APPS.into_iter().flat_map(|app| {
            [
                (format!("{app:?}/dark"), Palette::dark(app)),
                (format!("{app:?}/light"), Palette::light(app)),
            ]
        })
    }

    fn at_least(min: f32, what: &str, fg: Color32, bg: Color32, failures: &mut Vec<String>) {
        let c = contrast(fg, bg);
        if c < min {
            failures.push(format!("{what}: {c:.2} < {min}"));
        }
    }

    #[test]
    fn text_tokens_meet_wcag_aa() {
        let mut failures = Vec::new();
        for (name, p) in every_palette() {
            for (ground, g) in [("bg", p.bg), ("surface", p.surface), ("raised", p.raised)] {
                at_least(4.5, &format!("{name} text on {ground}"), p.text, g, &mut failures);
                at_least(4.5, &format!("{name} muted on {ground}"), p.muted, g, &mut failures);
            }
            for (ground, g) in [("bg", p.bg), ("surface", p.surface)] {
                for (role, c) in [
                    ("accent_text", p.accent_text),
                    ("danger", p.danger),
                    ("warning", p.warning),
                    ("success", p.success),
                ] {
                    at_least(4.5, &format!("{name} {role} on {ground}"), c, g, &mut failures);
                }
            }
            at_least(4.5, &format!("{name} on_accent"), p.on_accent, p.accent, &mut failures);
            at_least(4.5, &format!("{name} on_accent hover"), p.on_accent, p.accent_hover, &mut failures);
            at_least(4.5, &format!("{name} danger on danger_bg"), p.danger, p.danger_bg, &mut failures);
            at_least(4.5, &format!("{name} warning on warning_bg"), p.warning, p.warning_bg, &mut failures);
            at_least(4.5, &format!("{name} success on success_bg"), p.success, p.success_bg, &mut failures);
            at_least(4.5, &format!("{name} text on danger_bg"), p.text, p.danger_bg, &mut failures);
        }
        assert!(failures.is_empty(), "contrast failures:\n{}", failures.join("\n"));
    }

    #[test]
    fn non_text_parts_meet_3_to_1() {
        let mut failures = Vec::new();
        for (name, p) in every_palette() {
            for (ground, g) in [("bg", p.bg), ("surface", p.surface)] {
                at_least(3.0, &format!("{name} focus on {ground}"), p.focus, g, &mut failures);
                at_least(3.0, &format!("{name} accent fill on {ground}"), p.accent, g, &mut failures);
                at_least(3.0, &format!("{name} border_strong on {ground}"), p.border_strong, g, &mut failures);
            }
        }
        assert!(failures.is_empty(), "contrast failures:\n{}", failures.join("\n"));
    }

    #[test]
    fn overlay_text_is_readable_over_white_video() {
        // The scrim is translucent; its worst case is a white frame behind it.
        let a = 199.0 / 255.0;
        let mix = |c: u8| (c as f32 * a + 255.0 * (1.0 - a)).round() as u8;
        let worst = Color32::from_rgb(mix(10), mix(10), mix(12));
        let p = Palette::dark(App::Vesper);
        assert!(contrast(p.on_overlay, worst) >= 4.5);
    }

    #[test]
    fn accents_are_distinct_per_app() {
        let fills: Vec<u32> = APPS.iter().map(|a| a.accent().dark[0]).collect();
        for (i, a) in fills.iter().enumerate() {
            assert!(!fills[i + 1..].contains(a), "two apps share accent {a:06X}");
        }
    }

    #[test]
    fn contrast_rejects_a_low_pair() {
        // The checker itself must be able to fail.
        assert!(contrast(hex(0x777777), hex(0x888888)) < 1.5);
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
    }
}
