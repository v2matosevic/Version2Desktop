//! Window chrome shared by the media apps: the header strip, the one "More" menu with the common
//! theme choice, the floating dock, and static backdrops (ambient tint, scrims, artwork shadow).
//!
//! Everything here paints only when the frame is drawn for another reason. Nothing animates,
//! nothing schedules a repaint.

use egui::epaint::{Mesh, Shadow};
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Painter, Pos2, Rect,
    Response, Sense, Stroke, TextureHandle, ThemePreference, Ui, Vec2, WidgetInfo, WidgetType,
};

use crate::icons::Icon;
use crate::palette::Palette;
use crate::tokens::{radius, size, space, text};
use crate::widgets::{focus_ring, IconButton, Tone};
use crate::App;

// ---------------------------------------------------------------------------------------------
// Header

/// The app's own mark (its 64 px window icon), drawn at `side` points. Loaded once per context.
pub fn app_mark(ui: &mut Ui, app: App, side: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    let id = Id::new(("v2-app-mark", app.id()));
    let cached: Option<TextureHandle> = ui.ctx().data(|d| d.get_temp(id));
    let texture = cached.or_else(|| {
        let rgba = crate::mark_rgba(app)?;
        let image = egui::ColorImage::from_rgba_unmultiplied([64, 64], rgba);
        let t = ui.ctx().load_texture("v2-app-mark", image, egui::TextureOptions::LINEAR);
        ui.ctx().data_mut(|d| d.insert_temp(id, t.clone()));
        Some(t)
    });
    if let Some(t) = texture {
        ui.painter().image(t.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    }
    response
}

/// Lay out a header strip: `left` from the leading edge, `right` from the trailing edge.
/// Put it in `egui::Panel::top(..).exact_size(size::HEADER).frame(frame_header(&p))`.
pub fn header(ui: &mut Ui, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    ui.horizontal_centered(|ui| {
        ui.add_space(space::LG);
        ui.spacing_mut().item_spacing.x = space::SM;
        left(ui);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(space::MD);
            ui.spacing_mut().item_spacing.x = space::SM;
            right(ui);
        });
    });
}

/// Header surface: the window base, no hairline. Separation comes from the content below it.
pub fn frame_header(p: &Palette) -> Frame {
    Frame::new().fill(p.bg).inner_margin(Margin::ZERO)
}

/// The app name next to its mark: one quiet wordmark, not a title.
pub fn wordmark(ui: &mut Ui, app: App) {
    let p = crate::palette_of(ui);
    app_mark(ui, app, 24.0);
    ui.label(egui::RichText::new(app.name()).font(FontId::new(15.0, crate::fonts::semibold())).color(p.text));
}

/// A short vertical rule between groups in a header or a dock.
pub fn divider(ui: &mut Ui) {
    let p = crate::palette_of(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(space::MD + 1.0, 20.0), Sense::hover());
    ui.painter().vline(rect.center().x, rect.y_range(), Stroke::new(1.0, if p.dark { p.border_strong.gamma_multiply(0.6) } else { p.border }));
}

// ---------------------------------------------------------------------------------------------
// The one menu

/// The "More" button and its menu. Secondary actions, the theme choice and keyboard shortcuts
/// live here in every app, so no app grows its own settings chrome.
pub fn more_menu(ui: &mut Ui, add: impl FnOnce(&mut Ui)) -> Response {
    let button = IconButton::new(Icon::More, "More").show(ui);
    egui::Popup::menu(&button)
        .align(egui::RectAlign::BOTTOM_END)
        .gap(space::XS)
        .show(|ui| {
            ui.set_min_width(240.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui);
        });
    button
}

/// One row of a menu: optional icon, label, optional shortcut. Closes the menu when clicked.
pub fn menu_item(ui: &mut Ui, icon: Option<Icon>, label: &str, shortcut: Option<&str>, enabled: bool) -> Response {
    row(ui, icon.map_or(Glyph::None, Glyph::Icon), label, shortcut, enabled, None)
}

/// A checkable row (radio semantics inside a group). Closes the menu when clicked.
pub fn menu_check(ui: &mut Ui, checked: bool, label: &str) -> Response {
    row(ui, if checked { Glyph::Icon(Icon::Check) } else { Glyph::None }, label, None, true, Some(checked))
}

pub fn menu_separator(ui: &mut Ui) {
    let p = crate::palette_of(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::hover());
    let y = rect.center().y;
    ui.painter().hline(rect.x_range().shrink(space::SM), y, Stroke::new(1.0, p.border));
}

pub fn menu_heading(ui: &mut Ui, label: &str) {
    let p = crate::palette_of(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left() + space::MD, rect.center().y + 2.0),
        Align2::LEFT_CENTER,
        label,
        FontId::new(text::CAPTION, crate::fonts::semibold()),
        p.muted,
    );
}

/// The common theme choice. Changes egui's theme and the native title bar together.
pub fn theme_menu(ui: &mut Ui) {
    menu_heading(ui, "Theme");
    let current = ui.ctx().options(|o| o.theme_preference);
    for (pref, label) in [
        (ThemePreference::System, "Follow Windows"),
        (ThemePreference::Light, "Light"),
        (ThemePreference::Dark, "Dark"),
    ] {
        if menu_check(ui, current == pref, label).clicked() {
            set_theme(ui.ctx(), pref);
        }
    }
}

/// Apply a theme to the content and the native title bar.
pub fn set_theme(ctx: &egui::Context, pref: ThemePreference) {
    ctx.set_theme(pref);
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(match pref {
        ThemePreference::System => egui::SystemTheme::SystemDefault,
        ThemePreference::Light => egui::SystemTheme::Light,
        ThemePreference::Dark => egui::SystemTheme::Dark,
    }));
}

enum Glyph {
    None,
    Icon(Icon),
}

fn row(ui: &mut Ui, glyph: Glyph, label: &str, shortcut: Option<&str>, enabled: bool, checked: Option<bool>) -> Response {
    let p = crate::palette_of(ui);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width().max(220.0), size::CONTROL), sense);
    let owned = label.to_owned();
    response.widget_info(|| match checked {
        Some(c) => WidgetInfo::selected(WidgetType::RadioButton, enabled, c, &owned),
        None => WidgetInfo::labeled(WidgetType::Button, enabled, &owned),
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if enabled && response.hovered() {
            painter.rect_filled(rect.shrink2(Vec2::new(space::XS, 1.0)), radius::MD, p.hover);
        }
        let fg = if enabled { p.text } else { p.faint };
        if let Glyph::Icon(icon) = glyph {
            let g = Rect::from_center_size(Pos2::new(rect.left() + 22.0, rect.center().y), Vec2::splat(size::ICON_SM));
            icon.paint(painter, g, if checked == Some(true) { p.accent_text } else { fg });
        }
        painter.text(Pos2::new(rect.left() + 40.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(text::BODY), fg);
        if let Some(keys) = shortcut {
            painter.text(Pos2::new(rect.right() - space::MD, rect.center().y), Align2::RIGHT_CENTER, keys, FontId::proportional(text::CAPTION), p.muted);
        }
        focus_ring(ui, rect.shrink2(Vec2::new(space::XS, 1.0)), radius::MD, &response);
    }
    if response.clicked() {
        ui.close();
    }
    response
}

// ---------------------------------------------------------------------------------------------
// Surfaces

/// The floating control capsule used by every app's transport or view controls.
pub fn frame_dock(p: &Palette) -> Frame {
    Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, if p.dark { Color32::from_white_alpha(14) } else { p.border }))
        .corner_radius(radius::DOCK)
        .inner_margin(Margin::symmetric(space::LG as i8, space::SM as i8 + 2))
        .shadow(Shadow {
            offset: [0, 10],
            blur: 30,
            spread: 0,
            color: Color32::from_black_alpha(if p.dark { 110 } else { 28 }),
        })
}

/// A large rounded panel inside the window (the queue, an info sheet).
pub fn frame_sheet(p: &Palette) -> Frame {
    Frame::new()
        .fill(if p.dark { p.surface } else { p.raised })
        .stroke(Stroke::new(1.0, if p.dark { Color32::from_white_alpha(10) } else { p.border }))
        .corner_radius(radius::XL)
        .inner_margin(Margin::same(space::LG as i8))
}

/// Soft shadow under artwork or a floating card. Paint it before the thing it belongs to.
pub fn soft_shadow(painter: &Painter, rect: Rect, corner: u8, dark: bool) {
    let s = Shadow {
        offset: [0, 14],
        blur: 40,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 140 } else { 46 }),
    };
    painter.add(s.as_shape(rect, CornerRadius::same(corner)));
}

/// A static ambient wash: two soft glows of `tint` over the base. Used behind Sonora's stage,
/// tinted by the cover art. It is a handful of vertices, painted only when a frame is drawn.
pub fn paint_ambient(painter: &Painter, rect: Rect, tint: Color32, p: &Palette) {
    painter.rect_filled(rect, 0, p.bg);
    let strength = if p.dark { 0.34 } else { 0.22 };
    let reach = rect.width().max(rect.height());
    glow(painter, Pos2::new(rect.left() + rect.width() * 0.28, rect.top() + rect.height() * 0.30), reach * 0.70, tint.gamma_multiply(strength), rect);
    glow(painter, Pos2::new(rect.right() - rect.width() * 0.15, rect.bottom()), reach * 0.55, tint.gamma_multiply(strength * 0.45), rect);
}

fn glow(painter: &Painter, center: Pos2, radius: f32, color: Color32, clip: Rect) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(center, color);
    let n = 48;
    for i in 0..=n {
        let t = i as f32 / n as f32 * std::f32::consts::TAU;
        mesh.colored_vertex(center + Vec2::new(t.cos(), t.sin()) * radius, Color32::TRANSPARENT);
    }
    for i in 1..=n as u32 {
        mesh.add_triangle(0, i, i + 1);
    }
    painter.with_clip_rect(clip).add(mesh);
}

/// Vertical fade to black at the top or bottom edge of `rect`: the video player's scrims.
pub fn paint_scrim(painter: &Painter, rect: Rect, from_top: bool, alpha: u8) {
    let dark = Color32::from_black_alpha(alpha);
    let (top, bottom) = if from_top { (dark, Color32::TRANSPARENT) } else { (Color32::TRANSPARENT, dark) };
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(mesh);
}

/// Placeholder artwork: a rounded tile in the tint with a large soft glyph, so "no cover" still
/// looks deliberate.
pub fn paint_placeholder_art(painter: &Painter, rect: Rect, tint: Color32, icon: Icon, p: &Palette) {
    let base = if p.dark { blend(tint, Color32::from_rgb(20, 20, 24), 0.55) } else { blend(tint, Color32::WHITE, 0.45) };
    let corner = if rect.width() > 100.0 { radius::XL } else { radius::LG };
    painter.rect_filled(rect, corner, base);
    // A soft lift from the centre. It stays inside the inscribed circle, so it needs no clip and
    // never shows a square edge against the rounded corners.
    let lift = Color32::from_white_alpha(if p.dark { 20 } else { 64 });
    glow(painter, rect.center(), rect.width().min(rect.height()) * 0.5, lift, rect);
    let g = rect.width() * 0.34;
    icon.paint(painter, Rect::from_center_size(rect.center(), Vec2::splat(g)), Color32::from_white_alpha(if p.dark { 150 } else { 220 }));
}

fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

// ---------------------------------------------------------------------------------------------
// Start and failure card

/// The card an app shows before it has content, or when content failed to open: icon disc,
/// title, body, a centred action row and an optional footnote. One composition for all three
/// apps, so "nothing here yet" and "that didn't work" look like the same family.
#[must_use = "call .show(ui, area, actions)"]
pub struct Welcome<'a> {
    icon: Icon,
    title: &'a str,
    body: Option<&'a str>,
    foot: Option<&'a str>,
    tone: Option<Tone>,
}

impl<'a> Welcome<'a> {
    pub fn new(icon: Icon, title: &'a str) -> Self {
        Self { icon, title, body: None, foot: None, tone: None }
    }
    pub fn body(mut self, body: &'a str) -> Self {
        self.body = Some(body);
        self
    }
    /// One quiet line under the actions: supported formats, a hint.
    pub fn foot(mut self, foot: &'a str) -> Self {
        self.foot = Some(foot);
        self
    }
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = Some(tone);
        self
    }

    /// Centre the card in `area`. Returns the card's rect.
    pub fn show(self, ui: &mut Ui, area: Rect, actions: impl FnOnce(&mut Ui)) -> Rect {
        let p = crate::palette_of(ui);
        let width = (area.width() - 2.0 * space::XL).clamp(240.0, 460.0);
        let inner_w = width - 2.0 * space::XXL;
        let body_h = self.body.map_or(0.0, |b| {
            let job = egui::text::LayoutJob::simple(b.to_owned(), FontId::proportional(text::BODY), p.muted, inner_w);
            space::SM + ui.fonts_mut(|f| f.layout_job(job)).size().y
        });
        let foot_h = if self.foot.is_some() { space::LG + 16.0 } else { 0.0 };
        let height = space::XXL + 72.0 + space::LG + 28.0 + body_h + space::XL + size::CONTROL + foot_h + space::XXL;
        let top = (area.center().y - height / 2.0).max(area.top() + space::LG);
        let card = Rect::from_min_size(Pos2::new(area.center().x - width / 2.0, top), Vec2::new(width, height));

        let painter = ui.painter();
        if !p.dark {
            soft_shadow(painter, card, radius::XL, false);
        }
        painter.rect(
            card,
            radius::XL,
            if p.dark { Color32::from_white_alpha(9) } else { p.raised },
            Stroke::new(1.0, if p.dark { Color32::from_white_alpha(22) } else { p.border }),
            egui::StrokeKind::Inside,
        );
        let (disc, glyph) = match self.tone {
            Some(Tone::Danger) => (p.danger_bg, p.danger),
            Some(Tone::Warning) => (p.warning_bg, p.warning),
            _ => (p.accent.gamma_multiply(if p.dark { 0.22 } else { 0.14 }), p.accent_text),
        };
        let centre = Pos2::new(card.center().x, card.top() + space::XXL + 36.0);
        painter.circle_filled(centre, 36.0, disc);
        self.icon.paint(painter, Rect::from_center_size(centre, Vec2::splat(32.0)), glyph);

        let content = Rect::from_min_max(
            Pos2::new(card.left() + space::XXL, card.top() + space::XXL + 72.0 + space::LG),
            Pos2::new(card.right() - space::XXL, card.bottom() - space::XXL),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(content).layout(Layout::top_down(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(egui::RichText::new(self.title).font(FontId::new(text::HEADING + 2.0, crate::fonts::semibold())).color(p.text));
            if let Some(body) = self.body {
                ui.add_space(space::SM);
                ui.label(egui::RichText::new(body).color(p.muted));
            }
            ui.add_space(space::XL);
            centred_row(ui, Id::new(("v2-welcome-actions", self.title)), actions);
            if let Some(foot) = self.foot {
                ui.add_space(space::LG);
                ui.label(crate::widgets::caption(foot));
            }
        });
        card
    }
}

/// A horizontal row centred in the available width. The row is measured the first frame it
/// appears and the frame is discarded once, so it never jumps and never animates.
pub fn centred_row(ui: &mut Ui, id: Id, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = space::SM;
        let w: f32 = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(0.0);
        ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
        let start = ui.cursor().left();
        add(ui);
        let used = ui.min_rect().right() - start;
        if (used - w).abs() > 0.5 {
            ui.ctx().data_mut(|d| d.insert_temp(id, used));
            ui.ctx().request_discard("v2 centred row measured");
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Colour from artwork

/// A representative, pleasant colour from RGBA pixels (cover art), for the ambient tint.
/// Samples at most ~4096 pixels, ignores greys and near-black/near-white, and returns `None` for
/// monochrome art so the caller falls back to the app accent.
pub fn ambient_color(rgba: &[u8]) -> Option<Color32> {
    let pixels = rgba.len() / 4;
    if pixels == 0 {
        return None;
    }
    let step = (pixels / 4096).max(1);
    let (mut r, mut g, mut b, mut w) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for i in (0..pixels).step_by(step) {
        let px = &rgba[i * 4..i * 4 + 4];
        if px[3] < 128 {
            continue;
        }
        let (fr, fg, fb) = (px[0] as f32 / 255.0, px[1] as f32 / 255.0, px[2] as f32 / 255.0);
        let max = fr.max(fg).max(fb);
        let min = fr.min(fg).min(fb);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        if sat < 0.25 || !(0.18..=0.97).contains(&max) {
            continue;
        }
        r += fr * sat;
        g += fg * sat;
        b += fb * sat;
        w += sat;
    }
    if w < 1.0 {
        return None;
    }
    let (r, g, b) = (r / w, g / w, b / w);
    // Normalise brightness so dark covers still tint and bright covers do not glare.
    let max = r.max(g).max(b).max(1e-3);
    let k = 0.78 / max;
    Some(Color32::from_rgb((r * k * 255.0) as u8, (g * k * 255.0) as u8, (b * k * 255.0) as u8))
}

#[cfg(test)]
mod tests {
    use super::ambient_color;

    fn solid(r: u8, g: u8, b: u8) -> Vec<u8> {
        std::iter::repeat_n([r, g, b, 255], 64 * 64).flatten().collect()
    }

    #[test]
    fn colourful_art_gives_its_hue() {
        let c = ambient_color(&solid(200, 40, 30)).unwrap();
        assert!(c.r() > c.g() && c.r() > c.b(), "{c:?}");
    }

    #[test]
    fn grey_or_empty_art_gives_none() {
        assert!(ambient_color(&solid(128, 128, 128)).is_none());
        assert!(ambient_color(&[]).is_none());
        assert!(ambient_color(&solid(5, 0, 0)).is_none(), "near-black is not a tint");
    }
}
