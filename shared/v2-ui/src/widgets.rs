//! The shared controls. Each one: keyboard-focusable where it acts, a visible focus ring, an
//! AccessKit label, a tooltip on icon-only buttons, and no repaint unless something changed.

use egui::{
    Align2, Color32, CornerRadius, FontId, Frame, Id, Key, Margin, Modifiers, Rect, Response,
    RichText, Sense, Stroke, StrokeKind, TextStyle, Ui, Vec2, WidgetInfo, WidgetText, WidgetType,
};

use crate::icons::Icon;
use crate::palette::Palette;
use crate::tokens::{radius, size, space, text};

fn pal(ui: &Ui) -> Palette {
    crate::palette_of(ui)
}

/// 2 px ring outside `rect` while the widget has keyboard focus.
pub fn focus_ring(ui: &Ui, rect: Rect, corner: impl Into<CornerRadius>, response: &Response) {
    if response.has_focus() {
        let mut r: CornerRadius = corner.into();
        r = CornerRadius {
            nw: r.nw.saturating_add(2),
            ne: r.ne.saturating_add(2),
            sw: r.sw.saturating_add(2),
            se: r.se.saturating_add(2),
        };
        ui.painter().rect_stroke(rect.expand(2.0), r, Stroke::new(2.0, pal(ui).focus), StrokeKind::Outside);
    }
}

/// `3:07`, `1:02:07`. Negative, NaN and infinite read as `0:00`.
pub fn format_duration(secs: f64) -> String {
    let s = if secs.is_finite() && secs > 0.0 { secs.floor() as u64 } else { 0 };
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

// ---------------------------------------------------------------------------------------------
// Text helpers

/// 16 pt semibold: panel titles, track title in a now-playing bar.
pub fn title(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(TextStyle::Name(crate::style::TITLE.into()))
}

/// 28 pt semibold: the one big line of an empty state or hero readout.
pub fn display(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(TextStyle::Name(crate::style::DISPLAY.into()))
}

/// 12 pt secondary text.
pub fn caption(text: impl Into<String>) -> RichText {
    RichText::new(text).small().weak()
}

/// Small semibold muted label above a group ("Up next", "Details"). Sentence case, never caps.
pub fn section_label(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    ui.label(RichText::new(text).font(FontId::new(text::CAPTION, crate::fonts::semibold())).color(p.muted))
}

// ---------------------------------------------------------------------------------------------
// Buttons

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One per view: the action the screen exists for.
    Primary,
    Secondary,
    /// Toolbar and inline actions.
    Ghost,
    /// Destructive or irreversible: the button says what it destroys.
    Danger,
}

#[must_use = "call .show(ui)"]
pub struct Button {
    text: String,
    icon: Option<Icon>,
    kind: Kind,
    small: bool,
    pill: bool,
    enabled: bool,
    shortcut: Option<&'static str>,
}

impl Button {
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self { text: text.into(), icon: None, kind, small: false, pill: false, enabled: true, shortcut: None }
    }
    /// Fully rounded ends: the header's Open action and other free-standing calls to action.
    pub fn pill(mut self) -> Self {
        self.pill = true;
        self
    }
    pub fn primary(text: impl Into<String>) -> Self {
        Self::new(Kind::Primary, text)
    }
    pub fn secondary(text: impl Into<String>) -> Self {
        Self::new(Kind::Secondary, text)
    }
    pub fn ghost(text: impl Into<String>) -> Self {
        Self::new(Kind::Ghost, text)
    }
    pub fn danger(text: impl Into<String>) -> Self {
        Self::new(Kind::Danger, text)
    }
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    /// Shown in the tooltip, e.g. `"Ctrl+O"`. The app still owns the key handling.
    pub fn shortcut(mut self, keys: &'static str) -> Self {
        self.shortcut = Some(keys);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let p = pal(ui);
        let enabled = self.enabled && ui.is_enabled();
        let h = if self.small { size::CONTROL_SM } else { size::CONTROL };
        let pad = if self.small { space::SM + 2.0 } else { space::MD + 2.0 };
        let icon_w = if self.icon.is_some() { size::ICON_SM + space::SM - 2.0 } else { 0.0 };
        let galley = WidgetText::from(self.text.as_str()).into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            TextStyle::Button,
        );
        let width = (pad * 2.0 + icon_w + galley.size().x).max(h * 2.0);
        let sense = if enabled { Sense::click() } else { Sense::hover() };
        let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, h), sense);
        let text = self.text.clone();
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &text));

        if ui.is_rect_visible(rect) {
            let hot = enabled && response.hovered();
            let down = enabled && response.is_pointer_button_down_on();
            let (fill, border, fg) = button_colors(self.kind, &p, hot, down);
            // Disabled reads as neutral, never as a faded accent: a dimmed coral looks like a
            // different, muddier colour rather than "unavailable".
            let (fill, border, fg) = if enabled {
                (fill, border, fg)
            } else if self.kind == Kind::Ghost {
                (Color32::TRANSPARENT, Color32::TRANSPARENT, p.faint)
            } else {
                (p.raised, p.border, p.faint)
            };
            let dim = |c: Color32| c;
            let painter = ui.painter();
            let r = if self.pill { radius::PILL } else { radius::MD };
            painter.rect(rect, r, dim(fill), Stroke::new(1.0, dim(border)), StrokeKind::Inside);
            let content_w = icon_w + galley.size().x;
            let mut x = rect.center().x - content_w / 2.0;
            if let Some(icon) = self.icon {
                let r = Rect::from_center_size(egui::pos2(x + size::ICON_SM / 2.0, rect.center().y), Vec2::splat(size::ICON_SM));
                icon.paint(painter, r, dim(fg));
                x += icon_w;
            }
            let pos = egui::pos2(x, rect.center().y - galley.size().y / 2.0);
            painter.galley(pos, galley, dim(fg));
            focus_ring(ui, rect, if self.pill { radius::PILL } else { radius::MD }, &response);
        }
        if let Some(keys) = self.shortcut {
            response = response.on_hover_text(format!("{} ({keys})", self.text));
        }
        response
    }
}

/// `(fill, outline, text)` for a button at rest, hovered or pressed. Outlined kinds rest on
/// `border_strong` so the button's edge reaches 3:1; the hairline `border` is for frames only.
pub(crate) fn button_colors(kind: Kind, p: &Palette, hot: bool, down: bool) -> (Color32, Color32, Color32) {
    match kind {
        Kind::Primary => {
            let f = if hot || down { p.accent_hover } else { p.accent };
            (f, Color32::TRANSPARENT, p.on_accent)
        }
        Kind::Secondary => {
            let f = if down { p.pressed } else if hot { p.hover } else { p.raised };
            (f, if hot || down { p.muted } else { p.border_strong }, p.text)
        }
        Kind::Ghost => {
            let f = if down { p.pressed } else if hot { p.hover } else { Color32::TRANSPARENT };
            (f, Color32::TRANSPARENT, p.text)
        }
        Kind::Danger => {
            let f = if down { p.pressed } else if hot { p.danger_bg } else { p.raised };
            (f, if hot || down { p.danger } else { p.border_strong }, p.danger)
        }
    }
}

/// A disabled icon button reads as unavailable, never as a faded call to action: the prominent
/// play button drops its accent fill to a neutral disc, the others keep no fill and a faint glyph.
fn disabled_icon_colors(look: IconLook, p: &Palette, fg: Color32) -> (Color32, Color32) {
    match look {
        IconLook::Prominent => (p.hover, p.faint),
        IconLook::Overlay => (Color32::TRANSPARENT, fg.gamma_multiply(0.4)),
        IconLook::Plain => (Color32::TRANSPARENT, p.faint),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IconLook {
    Plain,
    /// Accent-filled circle: the one play/pause per window.
    Prominent,
    /// On top of video: light glyph, translucent hover.
    Overlay,
}

/// Square icon-only button. `label` is mandatory: it is the tooltip and the screen-reader name.
#[must_use = "call .show(ui)"]
pub struct IconButton {
    icon: Icon,
    label: String,
    size: f32,
    selected: bool,
    enabled: bool,
    look: IconLook,
    shortcut: Option<&'static str>,
}

impl IconButton {
    pub fn new(icon: Icon, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
            size: size::CONTROL,
            selected: false,
            enabled: true,
            look: IconLook::Plain,
            shortcut: None,
        }
    }
    pub fn size(mut self, side: f32) -> Self {
        self.size = side;
        self
    }
    /// Toggle state (shuffle on, repeat on, info panel open): accent glyph plus a dot.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn prominent(mut self) -> Self {
        self.look = IconLook::Prominent;
        self.size = self.size.max(size::PLAY);
        self
    }
    pub fn overlay(mut self) -> Self {
        self.look = IconLook::Overlay;
        self
    }
    pub fn shortcut(mut self, keys: &'static str) -> Self {
        self.shortcut = Some(keys);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let p = pal(ui);
        let enabled = self.enabled && ui.is_enabled();
        let sense = if enabled { Sense::click() } else { Sense::hover() };
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(self.size), sense);
        let (label, selected) = (self.label.clone(), self.selected);
        response.widget_info(|| WidgetInfo::selected(WidgetType::Button, enabled, selected, &label));

        if ui.is_rect_visible(rect) {
            let hot = enabled && response.hovered();
            let down = enabled && response.is_pointer_button_down_on();
            let painter = ui.painter();
            let glyph = match self.size {
                s if s >= size::PLAY => size::ICON_LG,
                s if s >= size::CONTROL_LG => size::ICON + 2.0,
                s if s >= size::CONTROL => size::ICON,
                _ => size::ICON_SM,
            };
            // Every icon button is a circle: hover and press read as a disc behind the glyph.
            let corner = CornerRadius::same(radius::PILL);
            let (fill, fg) = match self.look {
                IconLook::Prominent => (if hot || down { p.accent_hover } else { p.accent }, p.on_accent),
                IconLook::Overlay => {
                    let f = if down {
                        Color32::from_white_alpha(46)
                    } else if hot {
                        Color32::from_white_alpha(28)
                    } else {
                        Color32::TRANSPARENT
                    };
                    (f, if self.selected { p.accent_text } else { p.on_overlay })
                }
                IconLook::Plain => {
                    let f = if down {
                        p.pressed
                    } else if hot {
                        p.hover
                    } else if self.selected {
                        p.accent.gamma_multiply(if p.dark { 0.16 } else { 0.12 })
                    } else {
                        Color32::TRANSPARENT
                    };
                    (f, if self.selected { p.accent_text } else { p.text })
                }
            };
            let (fill, fg) = if enabled { (fill, fg) } else { disabled_icon_colors(self.look, &p, fg) };
            painter.rect_filled(rect, corner, fill);
            let glyph_rect = Rect::from_center_size(rect.center(), Vec2::splat(glyph));
            // Optical centring: a play triangle's mass sits left of its box.
            let glyph_rect = if matches!(self.icon, Icon::Play) {
                glyph_rect.translate(Vec2::new(glyph * 0.06, 0.0))
            } else {
                glyph_rect
            };
            self.icon.paint(painter, glyph_rect, fg);
            if self.selected && self.look != IconLook::Prominent {
                painter.circle_filled(egui::pos2(rect.center().x, rect.bottom() - 4.0), 1.6, fg);
            }
            focus_ring(ui, rect, corner, &response);
        }
        let tip = match self.shortcut {
            Some(keys) => format!("{} ({keys})", self.label),
            None => self.label,
        };
        response.on_hover_text(tip)
    }
}

// ---------------------------------------------------------------------------------------------
// Seekbar and volume

/// What the user did to a [`Seekbar`] this frame.
pub struct Seek {
    pub response: Response,
    /// Commit a seek: click, end of a drag, or a key while focused.
    pub seek_to: Option<f64>,
    /// While dragging: where the thumb is. Video may preview-seek here; audio usually waits.
    pub scrubbing: Option<f64>,
}

/// Time rail for audio and video. It does not own the position: pass the player's position in,
/// act on `seek_to` coming out.
#[must_use = "call .show(ui)"]
pub struct Seekbar {
    position: f64,
    duration: Option<f64>,
    buffered: Option<f64>,
    overlay: bool,
    step: f64,
    big_step: f64,
    id_salt: Id,
}

impl Seekbar {
    /// `duration` of `None` or zero (live stream, still probing) shows an inert rail.
    pub fn new(position: f64, duration: Option<f64>) -> Self {
        Self {
            position,
            duration,
            buffered: None,
            overlay: false,
            step: 5.0,
            big_step: 30.0,
            id_salt: Id::new("v2-seekbar"),
        }
    }
    pub fn buffered(mut self, until: f64) -> Self {
        self.buffered = Some(until);
        self
    }
    /// Light-on-dark colours for drawing over video.
    pub fn overlay(mut self) -> Self {
        self.overlay = true;
        self
    }
    /// Arrow-key step and Shift+Arrow step, in seconds. Defaults 5 and 30.
    pub fn steps(mut self, step: f64, big_step: f64) -> Self {
        self.step = step;
        self.big_step = big_step;
        self
    }
    pub fn id_salt(mut self, salt: impl std::hash::Hash + std::fmt::Debug) -> Self {
        self.id_salt = Id::new(salt);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Seek {
        let p = pal(ui);
        let duration = self.duration.filter(|d| d.is_finite() && *d > 0.0);
        let width = ui.available_width().max(48.0);
        let sense = if duration.is_some() { Sense::click_and_drag() } else { Sense::hover() };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 20.0), sense);
        let id = response.id;
        let mut seek = Seek { response, seek_to: None, scrubbing: None };
        let Some(dur) = duration else {
            paint_rail(ui, rect, 0.0, None, size::RAIL, rail_colors(&p, self.overlay), None);
            seek.response.widget_info(|| WidgetInfo::slider(false, 0.0, "Seek"));
            return seek;
        };
        let pos = self.position.clamp(0.0, dur);
        let rail = rect.shrink2(Vec2::new(size::THUMB / 2.0, 0.0));
        let time_at = |x: f32| (((x - rail.left()) / rail.width()).clamp(0.0, 1.0) as f64) * dur;
        let r = &seek.response;

        let mut shown = pos;
        if r.dragged() {
            if let Some(ptr) = r.interact_pointer_pos() {
                shown = time_at(ptr.x);
                seek.scrubbing = Some(shown);
            }
        }
        if r.drag_stopped() || (r.clicked() && !r.dragged()) {
            if let Some(ptr) = r.interact_pointer_pos() {
                seek.seek_to = Some(time_at(ptr.x));
                shown = seek.seek_to.unwrap();
            }
        }
        if r.has_focus() {
            lock_arrows(ui, r.id);
            let t = keyboard_step(ui, pos, dur, self.step, self.big_step);
            if t.is_some() {
                seek.seek_to = t;
            }
        }

        let hot = r.hovered() || r.dragged() || r.has_focus();
        let rail_h = if hot { size::RAIL_HOT } else { size::RAIL };
        let frac = (shown / dur) as f32;
        let buffered = self.buffered.map(|b| (b / dur).clamp(0.0, 1.0) as f32);
        let thumb = hot.then_some(frac);
        paint_rail(ui, rail, frac, buffered, rail_h, rail_colors(&p, self.overlay), thumb);

        if let Some(hover) = r.hover_pos().filter(|_| !r.dragged()) {
            time_bubble(ui, id, egui::pos2(hover.x, rect.top()), &format_duration(time_at(hover.x)), &p);
        } else if r.dragged() {
            let x = rail.left() + rail.width() * frac;
            time_bubble(ui, id, egui::pos2(x, rect.top()), &format_duration(shown), &p);
        }
        focus_ring(ui, rect, radius::SM, &seek.response);
        seek.response.widget_info(|| WidgetInfo::slider(true, pos, "Seek"));
        if seek.seek_to.is_some() {
            seek.response.mark_changed();
        }
        seek
    }
}

fn keyboard_step(ui: &Ui, pos: f64, max: f64, step: f64, big: f64) -> Option<f64> {
    ui.input_mut(|i| {
        let mut t = None;
        for (m, delta) in [(Modifiers::SHIFT, big), (Modifiers::NONE, step)] {
            if i.consume_key(m, Key::ArrowRight) {
                t = Some(pos + delta);
            }
            if i.consume_key(m, Key::ArrowLeft) {
                t = Some(pos - delta);
            }
        }
        if i.consume_key(Modifiers::NONE, Key::Home) {
            t = Some(0.0);
        }
        if i.consume_key(Modifiers::NONE, Key::End) {
            t = Some(max);
        }
        t.map(|t| t.clamp(0.0, max))
    })
}

struct RailColors {
    track: Color32,
    buffered: Color32,
    fill: Color32,
    thumb: Color32,
}

fn rail_colors(p: &Palette, overlay: bool) -> RailColors {
    if overlay {
        RailColors {
            track: Color32::from_white_alpha(56),
            buffered: Color32::from_white_alpha(96),
            fill: p.accent,
            thumb: Color32::WHITE,
        }
    } else {
        RailColors {
            track: if p.dark { p.border } else { p.pressed },
            buffered: p.border_strong.gamma_multiply(0.6),
            fill: p.accent,
            thumb: if p.dark { p.text } else { p.raised },
        }
    }
}

fn paint_rail(ui: &Ui, rect: Rect, frac: f32, buffered: Option<f32>, h: f32, c: RailColors, thumb: Option<f32>) {
    let painter = ui.painter();
    let y = rect.center().y;
    let track = Rect::from_min_max(egui::pos2(rect.left(), y - h / 2.0), egui::pos2(rect.right(), y + h / 2.0));
    painter.rect_filled(track, radius::PILL, c.track);
    if let Some(b) = buffered {
        let mut r = track;
        r.set_right(track.left() + track.width() * b);
        painter.rect_filled(r, radius::PILL, c.buffered);
    }
    let mut done = track;
    done.set_right(track.left() + track.width() * frac.clamp(0.0, 1.0));
    painter.rect_filled(done, radius::PILL, c.fill);
    if let Some(t) = thumb {
        let center = egui::pos2(track.left() + track.width() * t.clamp(0.0, 1.0), y);
        painter.circle_filled(center + Vec2::new(0.0, 1.0), size::THUMB / 2.0, Color32::from_black_alpha(60));
        painter.circle_filled(center, size::THUMB / 2.0, c.thumb);
        painter.circle_stroke(center, size::THUMB / 2.0 - 0.5, Stroke::new(1.0, Color32::from_black_alpha(40)));
    }
}

fn time_bubble(ui: &Ui, id: Id, anchor: egui::Pos2, text: &str, p: &Palette) {
    let layer = egui::LayerId::new(egui::Order::Tooltip, id.with("bubble"));
    let painter = ui.ctx().layer_painter(layer);
    let galley = painter.layout_no_wrap(text.to_owned(), FontId::new(text::CAPTION, crate::fonts::semibold()), p.text);
    let pad = Vec2::new(space::SM, space::XS);
    let size = galley.size() + pad * 2.0;
    let screen = ui.ctx().content_rect();
    let mut r = Rect::from_min_size(egui::pos2(anchor.x - size.x / 2.0, anchor.y - size.y - space::XS), size);
    r = r.translate(Vec2::new((screen.left() + 2.0 - r.left()).max(0.0) + (screen.right() - 2.0 - r.right()).min(0.0), 0.0));
    painter.rect(r, radius::SM, p.raised, Stroke::new(1.0, p.border), StrokeKind::Inside);
    painter.galley(r.min + pad, galley, p.text);
}

/// Horizontal volume rail, 0.0..=1.0. Returns the new value when the user changed it.
pub fn volume_slider(ui: &mut Ui, volume: f32, muted: bool, width: f32) -> (Response, Option<f32>) {
    let p = pal(ui);
    let (rail, response, new) = rail_input(ui, volume, 0.05, width);
    let rect = response.rect;
    let shown = new.unwrap_or(volume);
    let mut c = rail_colors(&p, false);
    if muted {
        c.fill = p.faint;
    }
    paint_rail(ui, rail, shown, None, size::RAIL, c, Some(shown));
    focus_ring(ui, rect, radius::SM, &response);
    let pct = (shown * 100.0).round() as f64;
    response.widget_info(|| WidgetInfo::slider(true, pct, "Volume"));
    let label = if muted { "Volume (muted)".to_owned() } else { format!("Volume {pct}%") };
    let mut response = response.on_hover_text(label);
    if new.is_some() {
        response.mark_changed();
    }
    (response, new)
}

/// A setting slider in the same rail style as volume: accent fill to the thumb, a rail that reads
/// on any surface, arrow keys step by `step` while focused. `label` is the screen-reader name;
/// show the value beside it.
pub fn slider(ui: &mut Ui, value: &mut f32, range: std::ops::RangeInclusive<f32>, step: f32, width: f32, label: &str) -> Response {
    let p = pal(ui);
    let (lo, hi) = (*range.start(), *range.end());
    let span = (hi - lo).max(f32::EPSILON);
    let (rail, response, new) = rail_input(ui, ((*value - lo) / span).clamp(0.0, 1.0), step / span, width);
    let rect = response.rect;
    if let Some(f) = new {
        let v = lo + f * span;
        *value = if step > 0.0 { (lo + ((v - lo) / step).round() * step).clamp(lo, hi) } else { v };
    }
    let shown = ((*value - lo) / span).clamp(0.0, 1.0);
    paint_rail(ui, rail, shown, None, size::RAIL, rail_colors(&p, false), Some(shown));
    focus_ring(ui, rect, radius::SM, &response);
    let v = *value as f64;
    response.widget_info(|| WidgetInfo::slider(true, v, label));
    let mut response = response;
    if new.is_some() {
        response.mark_changed();
    }
    response
}

/// While a rail has keyboard focus its arrow keys adjust it. Without this lock egui would also
/// treat them as focus navigation and move focus to the neighbouring control.
fn lock_arrows(ui: &Ui, id: Id) {
    ui.memory_mut(|m| {
        m.set_focus_lock_filter(
            id,
            egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() },
        )
    });
}

/// Pointer and keyboard input shared by the rail sliders. Returns the rail, the response and the
/// new fraction if it changed this frame.
fn rail_input(ui: &mut Ui, fraction: f32, step: f32, width: f32) -> (Rect, Response, Option<f32>) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 20.0), Sense::click_and_drag());
    let rail = rect.shrink2(Vec2::new(size::THUMB / 2.0, 0.0));
    let mut new = None;
    if response.dragged() || response.clicked() {
        if let Some(ptr) = response.interact_pointer_pos() {
            new = Some(((ptr.x - rail.left()) / rail.width()).clamp(0.0, 1.0));
        }
    }
    if response.has_focus() {
        lock_arrows(ui, response.id);
        new = ui
            .input_mut(|i| {
                let mut v = None;
                for k in [Key::ArrowRight, Key::ArrowUp] {
                    if i.consume_key(Modifiers::NONE, k) {
                        v = Some((fraction + step).min(1.0));
                    }
                }
                for k in [Key::ArrowLeft, Key::ArrowDown] {
                    if i.consume_key(Modifiers::NONE, k) {
                        v = Some((fraction - step).max(0.0));
                    }
                }
                v
            })
            .or(new);
    }
    (rail, response, new)
}

// ---------------------------------------------------------------------------------------------
// Rows, chips, keycaps

/// One line in a queue, playlist or file list. Full width; click selects, double-click plays.
#[must_use = "call .show(ui)"]
pub struct ListRow<'a> {
    title: &'a str,
    subtitle: Option<&'a str>,
    trailing: Option<&'a str>,
    number: Option<usize>,
    selected: bool,
    active: bool,
}

impl<'a> ListRow<'a> {
    pub fn new(title: &'a str) -> Self {
        Self { title, subtitle: None, trailing: None, number: None, selected: false, active: false }
    }
    pub fn subtitle(mut self, s: &'a str) -> Self {
        self.subtitle = Some(s);
        self
    }
    /// Right-aligned, muted, tabular: a duration or a size.
    pub fn trailing(mut self, s: &'a str) -> Self {
        self.trailing = Some(s);
        self
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    /// A leading track number. The playing row shows an equaliser instead, a hovered row a play glyph.
    pub fn number(mut self, n: usize) -> Self {
        self.number = Some(n);
        self
    }
    /// The item currently playing or showing.
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let p = pal(ui);
        let h = if self.subtitle.is_some() { size::ROW_TALL } else { size::ROW };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click());
        let title = self.title.to_owned();
        let selected = self.selected;
        response.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, selected, &title));
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let painter = ui.painter();
        let fill = if self.selected {
            p.accent.gamma_multiply(if p.dark { 0.22 } else { 0.14 })
        } else if response.hovered() {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        painter.rect_filled(rect, radius::MD, fill);

        let lead = space::MD;
        let mut left = rect.left() + lead;
        if let Some(n) = self.number {
            let slot = Rect::from_min_size(egui::pos2(left, rect.top()), Vec2::new(28.0, rect.height()));
            let glyph = Rect::from_center_size(slot.center(), Vec2::splat(size::ICON_SM));
            if self.active {
                Icon::Equalizer.paint(painter, glyph, p.accent_text);
            } else if response.hovered() {
                Icon::Play.paint(painter, glyph.shrink(1.0), p.text);
            } else {
                painter.text(slot.center(), Align2::CENTER_CENTER, n.to_string(), FontId::proportional(text::CAPTION), p.muted);
            }
            left += 28.0 + space::SM;
        } else if self.active {
            let r = Rect::from_center_size(egui::pos2(left + 7.0, rect.center().y), Vec2::splat(size::ICON_SM));
            Icon::VolumeHigh.paint(painter, r, p.accent_text);
            left += size::ICON_SM + space::SM;
        }
        let mut right = rect.right() - space::MD;
        if let Some(t) = self.trailing {
            let g = painter.layout_no_wrap(t.to_owned(), FontId::proportional(text::CAPTION), p.muted);
            right -= g.size().x;
            painter.galley(egui::pos2(right, rect.center().y - g.size().y / 2.0), g, p.muted);
            right -= space::MD;
        }
        let max_w = (right - left).max(10.0);
        let title_color = if self.active { p.accent_text } else { p.text };
        let tg = elide(ui, self.title, FontId::proportional(text::BODY), title_color, max_w);
        match self.subtitle {
            Some(sub) => {
                let sg = elide(ui, sub, FontId::proportional(text::CAPTION), p.muted, max_w);
                let total = tg.size().y + 2.0 + sg.size().y;
                let top = rect.center().y - total / 2.0;
                painter.galley(egui::pos2(left, top), tg.clone(), title_color);
                painter.galley(egui::pos2(left, top + tg.size().y + 2.0), sg, p.muted);
            }
            None => {
                painter.galley(egui::pos2(left, rect.center().y - tg.size().y / 2.0), tg, title_color);
            }
        }
        focus_ring(ui, rect.shrink(1.0), radius::MD, &response);
        response
    }
}

/// Single-line text cut with an ellipsis to fit `max_w`.
fn elide(ui: &Ui, s: &str, font: FontId, color: Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(s.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w);
    ui.fonts_mut(|f| f.layout_job(job))
}

/// Small rounded tag: a format, a bitrate, a resolution. Purely informational.
pub fn chip(ui: &mut Ui, label: &str) -> Response {
    let p = pal(ui);
    Frame::new()
        .fill(if p.dark { p.raised } else { p.hover })
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(radius::PILL)
        .inner_margin(Margin::symmetric(space::SM as i8, 2))
        .show(ui, |ui| ui.label(RichText::new(label).small().color(p.muted)))
        .response
}

/// Keycap for shortcut hints: `kbd(ui, "Ctrl+O")`.
pub fn kbd(ui: &mut Ui, keys: &str) -> Response {
    let p = pal(ui);
    Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border_strong.gamma_multiply(0.7)))
        .corner_radius(radius::SM)
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| ui.label(RichText::new(keys).font(FontId::monospace(text::CAPTION)).color(p.text)))
        .response
}

/// The search field: an inset pill with a leading magnifier, and a clear button once there is
/// text. The outline keeps the 3:1 edge of every other control; focus turns it to the focus colour.
pub fn search_field(ui: &mut Ui, text: &mut String, id: Id, hint: &str) -> Response {
    let p = pal(ui);
    let (rect, shell) = ui.allocate_exact_size(Vec2::new(ui.available_width(), size::CONTROL), Sense::click());
    let focused = ui.memory(|m| m.has_focus(id));
    let stroke = if focused {
        Stroke::new(1.5, p.focus)
    } else if ui.rect_contains_pointer(rect) {
        Stroke::new(1.0, p.muted)
    } else {
        Stroke::new(1.0, p.border_strong)
    };
    let corner = CornerRadius::same(radius::PILL);
    ui.painter().rect(rect, corner, p.bg, stroke, StrokeKind::Inside);
    let glyph = Rect::from_center_size(egui::pos2(rect.left() + 18.0, rect.center().y), Vec2::splat(size::ICON_SM));
    Icon::Search.paint(ui.painter(), glyph, if focused { p.text } else { p.muted });
    let clear = if text.is_empty() { 0.0 } else { 28.0 };
    let inner = Rect::from_min_max(
        egui::pos2(rect.left() + 34.0, rect.top()),
        egui::pos2(rect.right() - space::MD - clear, rect.bottom()),
    );
    let mut response = ui
        .scope_builder(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            ui.add(
                egui::TextEdit::singleline(text)
                    .id(id)
                    .hint_text(hint)
                    .frame(Frame::new())
                    .desired_width(inner.width()),
            )
        })
        .inner;
    if !text.is_empty() {
        let at = Rect::from_center_size(egui::pos2(rect.right() - 20.0, rect.center().y), Vec2::splat(24.0));
        let cleared = ui
            .scope_builder(egui::UiBuilder::new().max_rect(at), |ui| IconButton::new(Icon::Close, "Clear search").size(24.0).show(ui))
            .inner;
        if cleared.clicked() {
            text.clear();
            response.mark_changed();
        }
    }
    if shell.clicked() {
        ui.memory_mut(|m| m.request_focus(id));
    }
    response
}

/// Two-column shortcut sheet for the `?` / F1 overlay.
pub fn shortcut_table(ui: &mut Ui, rows: &[(&str, &str)]) {
    egui::Grid::new("v2-shortcuts").num_columns(2).spacing([space::LG, space::SM]).show(ui, |ui| {
        for (keys, what) in rows {
            ui.horizontal(|ui| {
                for (i, k) in keys.split(" / ").enumerate() {
                    if i > 0 {
                        ui.label(caption("or"));
                    }
                    kbd(ui, k);
                }
            });
            ui.label(*what);
            ui.end_row();
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Surfaces

pub mod frame {
    use super::*;

    /// Side panel, list column, bottom bar.
    pub fn panel(p: &Palette) -> Frame {
        Frame::new().fill(p.surface).inner_margin(Margin::same(space::MD as i8))
    }
    /// Raised group inside a panel.
    pub fn card(p: &Palette) -> Frame {
        Frame::new()
            .fill(p.raised)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(radius::LG)
            .inner_margin(Margin::same(space::LG as i8))
    }
    /// Controls on top of video or a full-screen photo. Same in both themes.
    pub fn overlay(p: &Palette) -> Frame {
        Frame::new().fill(p.overlay).corner_radius(radius::LG).inner_margin(Margin::symmetric(space::MD as i8, space::SM as i8))
    }
}

// ---------------------------------------------------------------------------------------------
// States

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Warning,
    Danger,
}

/// Inline message across the top of a view. Errors say what failed and what still works.
#[must_use = "call .show(ui, |ui| { actions })"]
pub struct Banner<'a> {
    tone: Tone,
    title: &'a str,
    detail: Option<&'a str>,
    dismissible: bool,
}

impl<'a> Banner<'a> {
    pub fn new(tone: Tone, title: &'a str) -> Self {
        Self { tone, title, detail: None, dismissible: false }
    }
    pub fn error(title: &'a str) -> Self {
        Self::new(Tone::Danger, title)
    }
    pub fn warning(title: &'a str) -> Self {
        Self::new(Tone::Warning, title)
    }
    pub fn info(title: &'a str) -> Self {
        Self::new(Tone::Info, title)
    }
    pub fn detail(mut self, d: &'a str) -> Self {
        self.detail = Some(d);
        self
    }
    pub fn dismissible(mut self) -> Self {
        self.dismissible = true;
        self
    }

    /// Returns `true` when the user dismissed it this frame.
    pub fn show(self, ui: &mut Ui, actions: impl FnOnce(&mut Ui)) -> bool {
        let p = pal(ui);
        let (fg, bg, icon) = match self.tone {
            Tone::Info => (p.accent_text, p.raised, Icon::Info),
            Tone::Success => (p.success, p.success_bg, Icon::Check),
            Tone::Warning => (p.warning, p.warning_bg, Icon::Warning),
            Tone::Danger => (p.danger, p.danger_bg, Icon::Error),
        };
        let mut dismissed = false;
        Frame::new()
            .fill(bg)
            .stroke(Stroke::new(1.0, fg.gamma_multiply(0.35)))
            .corner_radius(radius::LG)
            .inner_margin(Margin::symmetric(space::MD as i8, space::SM as i8 + 2))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // The actions on the right are measured the first frame they show (one discarded
                // frame, no animation) and their width is kept clear, so the detail wraps beside
                // them instead of running underneath.
                let id = ui.id().with("v2-banner-actions");
                let reserved: f32 = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(0.0);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(size::ICON), Sense::hover());
                    icon.paint(ui.painter(), r, fg);
                    let text_w = (ui.available_width() - reserved - space::MD).max(120.0);
                    ui.vertical(|ui| {
                        ui.set_max_width(text_w);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(RichText::new(self.title).font(FontId::new(text::BODY, crate::fonts::semibold())).color(p.text));
                        if let Some(d) = self.detail {
                            ui.label(RichText::new(d).color(p.muted));
                        }
                    });
                    let used = ui
                        .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let start = ui.cursor().right();
                            if self.dismissible && IconButton::new(Icon::Close, "Dismiss").size(size::CONTROL_SM).show(ui).clicked() {
                                dismissed = true;
                            }
                            actions(ui);
                            start - ui.min_rect().left()
                        })
                        .inner;
                    if (used - reserved).abs() > 0.5 {
                        ui.ctx().data_mut(|d| d.insert_temp(id, used));
                        ui.ctx().request_discard("v2 banner actions measured");
                    }
                });
            });
        dismissed
    }
}

/// Centred placeholder for a view with nothing in it yet, or nothing it could open.
#[must_use = "call .show(ui, |ui| { actions })"]
pub struct EmptyState<'a> {
    icon: Icon,
    title: &'a str,
    body: Option<&'a str>,
    tone: Option<Tone>,
}

impl<'a> EmptyState<'a> {
    pub fn new(icon: Icon, title: &'a str) -> Self {
        Self { icon, title, body: None, tone: None }
    }
    pub fn body(mut self, body: &'a str) -> Self {
        self.body = Some(body);
        self
    }
    /// Colour the icon disc for an error or warning state.
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = Some(tone);
        self
    }

    pub fn show(self, ui: &mut Ui, actions: impl FnOnce(&mut Ui)) {
        let p = pal(ui);
        let (disc, glyph) = match self.tone {
            Some(Tone::Danger) => (p.danger_bg, p.danger),
            Some(Tone::Warning) => (p.warning_bg, p.warning),
            Some(Tone::Success) => (p.success_bg, p.success),
            _ => (if p.dark { p.raised } else { p.hover }, p.muted),
        };
        let estimate = 72.0 + 40.0 + if self.body.is_some() { 44.0 } else { 0.0 } + 48.0;
        ui.add_space(((ui.available_height() - estimate) / 2.0 - space::XL).max(space::XL));
        ui.vertical_centered(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 32.0, disc);
            self.icon.paint(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(28.0)), glyph);
            ui.add_space(space::LG);
            ui.label(title(self.title).color(p.text));
            if let Some(body) = self.body {
                ui.add_space(space::XS);
                ui.scope(|ui| {
                    ui.set_max_width(380.0);
                    ui.label(RichText::new(body).color(p.muted));
                });
            }
            ui.add_space(space::LG);
            ui.horizontal(|ui| {
                // Centre the action row by measuring it on the first frame it is shown.
                let id = ui.id().with("v2-empty-actions");
                let w: f32 = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(0.0);
                ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                let start = ui.cursor().left();
                actions(ui);
                let used = ui.min_rect().right() - start;
                if (used - w).abs() > 0.5 {
                    ui.ctx().data_mut(|d| d.insert_temp(id, used));
                    ui.ctx().request_discard("v2 empty-state actions measured");
                }
            });
        });
    }
}

/// Inline loading line. The spinner animates only while shown, and not at all with reduced motion.
pub fn loading(ui: &mut Ui, label: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        if !crate::reduced_motion(ui.ctx()) {
            ui.add(egui::Spinner::new().size(size::ICON_SM).color(p.accent_text));
        }
        ui.label(RichText::new(label).color(p.muted));
    });
}

/// Full-window drop target while files are dragged over the window. Draws nothing and costs
/// nothing otherwise. Returns `true` while files hover. The app still reads `dropped_files`.
pub fn drop_overlay(ctx: &egui::Context, label: &str) -> bool {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return false;
    }
    let p = crate::palette(ctx);
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("v2-drop-overlay")));
    let r = ctx.content_rect();
    painter.rect_filled(r, 0, p.bg.gamma_multiply(0.9));
    painter.rect_stroke(r.shrink(space::LG), radius::LG, Stroke::new(2.0, p.accent_text), StrokeKind::Inside);
    let c = r.center();
    painter.circle_filled(c - Vec2::new(0.0, 28.0), 32.0, p.accent.gamma_multiply(0.18));
    Icon::Open.paint(&painter, Rect::from_center_size(c - Vec2::new(0.0, 28.0), Vec2::splat(28.0)), p.accent_text);
    painter.text(c + Vec2::new(0.0, 28.0), Align2::CENTER_CENTER, label, FontId::new(text::TITLE, crate::fonts::semibold()), p.text);
    true
}

/// Auto-hide for on-media controls (video OSD, full-screen photo chrome). Visible while the
/// pointer moved or a key was pressed in the last [`crate::tokens::motion::OSD_IDLE_SECS`], or
/// while `keep_visible` (paused, a control focused, a menu open). Schedules exactly one repaint
/// for the moment it should hide, never a loop. Hide the cursor yourself when this is `false`.
pub fn auto_hide(ctx: &egui::Context, id: Id, keep_visible: bool) -> bool {
    let (now, active) = ctx.input(|i| {
        let keyed = i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, .. }));
        (i.time, i.pointer.is_moving() || i.pointer.any_down() || keyed)
    });
    let last: f64 = ctx.data(|d| d.get_temp(id)).unwrap_or(now);
    let last = if active || keep_visible { now } else { last };
    ctx.data_mut(|d| d.insert_temp(id, last));
    if keep_visible {
        return true;
    }
    let left = crate::tokens::motion::OSD_IDLE_SECS as f64 - (now - last);
    if left > 0.0 {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(left + 0.02));
        true
    } else {
        false
    }
}

/// Draw text centred at a point. Small helper for overlays and readouts.
pub fn centered_text(ui: &Ui, at: egui::Pos2, s: &str, font: FontId, color: Color32) {
    ui.painter().text(at, Align2::CENTER_CENTER, s, font, color);
}

#[cfg(test)]
mod tests {
    use super::{button_colors, disabled_icon_colors, format_duration, IconLook, Kind};
    use crate::palette::{contrast, Palette};
    use crate::App;

    #[test]
    fn a_disabled_play_button_does_not_look_like_a_call_to_action() {
        for app in [App::Sonora, App::Luma, App::Vesper, App::WinImprover] {
            for p in [Palette::dark(app), Palette::light(app)] {
                let (fill, glyph) = disabled_icon_colors(IconLook::Prominent, &p, p.on_accent);
                assert!(fill != p.accent && fill != p.accent_hover, "{app:?} keeps the accent fill");
                assert_eq!(glyph, p.faint, "{app:?}");
                // Still a visible disc, so the transport does not appear to lose a button.
                assert!(fill != p.surface && fill != p.bg, "{app:?} disc vanishes");
            }
        }
    }

    #[test]
    fn outlined_buttons_have_a_3_to_1_edge_in_every_state() {
        let mut failures = Vec::new();
        for app in [App::Sonora, App::Luma, App::Vesper, App::WinImprover] {
            for p in [Palette::dark(app), Palette::light(app)] {
                for kind in [Kind::Secondary, Kind::Danger] {
                    for (state, hot, down) in [("rest", false, false), ("hover", true, false), ("pressed", true, true)] {
                        let (_, edge, _) = button_colors(kind, &p, hot, down);
                        for (ground, g) in [("bg", p.bg), ("surface", p.surface)] {
                            let r = contrast(edge, g);
                            if r < 3.0 {
                                failures.push(format!("{app:?} dark={} {kind:?} {state} on {ground}: {r:.2}", p.dark));
                            }
                        }
                    }
                }
            }
        }
        assert!(failures.is_empty(), "button edges too faint:\n{}", failures.join("\n"));
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(7.9), "0:07");
        assert_eq!(format_duration(187.0), "3:07");
        assert_eq!(format_duration(3727.0), "1:02:07");
        assert_eq!(format_duration(-3.0), "0:00");
        assert_eq!(format_duration(f64::NAN), "0:00");
        assert_eq!(format_duration(f64::INFINITY), "0:00");
    }
}
