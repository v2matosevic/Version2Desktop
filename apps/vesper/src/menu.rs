//! The Subtitles menu body. A menu taller than the window (many tracks, the expanded text-encoding
//! list) scrolls inside it instead of running off the screen, and every row stays reachable.
use eframe::egui;

/// Tallest the menu may grow: the window minus the control bar and a margin.
pub fn max_height(window_height: f32) -> f32 {
    (window_height - 120.0).max(160.0)
}

/// Lays `content` out in a vertical scroll area no taller than `max_height` for this window.
pub fn scrolling<R>(
    ui: &mut egui::Ui,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    egui::ScrollArea::vertical()
        .max_height(max_height(ui.ctx().content_rect().height()))
        .auto_shrink([false, true])
        .show(ui, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Align, Pos2, RawInput, Rect};
    use v2_ui::chrome::{menu_check, menu_heading, menu_separator};

    const ROWS: usize = 40;

    #[test]
    fn height_follows_the_window_and_never_collapses() {
        assert_eq!(max_height(720.0), 600.0);
        assert_eq!(max_height(440.0), 320.0);
        assert_eq!(
            max_height(100.0),
            160.0,
            "a tiny window still leaves a usable list"
        );
        assert_eq!(
            max_height(f32::NAN),
            160.0,
            "a bad size cannot make it vanish"
        );
    }

    /// What one layout pass of the menu reported.
    struct Pass {
        viewport: Rect,
        content_height: f32,
        first: Rect,
        last: Rect,
    }

    /// Draws a track-and-encoding-sized menu (headings, a long list of checkable rows, a trailing
    /// section) in a popup-like area of a window `size` large; `target` is the row to bring into view.
    fn run(size: egui::Vec2, target: usize, passes: usize) -> Pass {
        let ctx = egui::Context::default();
        v2_ui::install(&ctx, v2_ui::App::Vesper);
        let screen = Rect::from_min_size(Pos2::ZERO, size);
        let mut result = None;
        for _ in 0..passes {
            let _ = ctx.run_ui(
                RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                },
                |ui| {
                    egui::Area::new(egui::Id::new("tracks-menu"))
                        .fixed_pos(Pos2::new(size.x - 300.0, 12.0))
                        .show(ui.ctx(), |ui| {
                            ui.set_min_width(260.0);
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let mut rows = Vec::new();
                            let out = scrolling(ui, |ui| {
                                menu_heading(ui, "Subtitles");
                                for i in 0..ROWS {
                                    let r = menu_check(ui, false, &format!("Row {i}"));
                                    if i == target {
                                        r.scroll_to_me_animation(
                                            Some(Align::Center),
                                            egui::style::ScrollAnimation::none(),
                                        );
                                    }
                                    rows.push(r.rect);
                                }
                                menu_separator(ui);
                                menu_heading(ui, "Adjust subtitles");
                            });
                            result = Some(Pass {
                                viewport: out.inner_rect,
                                content_height: out.content_size.y,
                                first: rows[0],
                                last: rows[ROWS - 1],
                            });
                        });
                },
            );
        }
        result.expect("the menu was drawn")
    }

    fn inside(outer: Rect, row: Rect) -> bool {
        row.top() >= outer.top() - 0.5 && row.bottom() <= outer.bottom() + 0.5
    }

    #[test]
    fn a_menu_taller_than_a_compact_window_scrolls_and_stays_inside_it() {
        for size in [egui::vec2(680.0, 440.0), egui::vec2(1100.0, 720.0)] {
            let top = run(size, 0, 6);
            assert!(
                top.content_height > top.viewport.height(),
                "{size:?}: the test content must be taller than the menu so scrolling is exercised"
            );
            assert!(
                top.viewport.height() <= max_height(size.y) + 0.5,
                "{size:?}: menu {} px exceeds {}",
                top.viewport.height(),
                max_height(size.y)
            );
            assert!(
                top.viewport.bottom() <= size.y && top.viewport.top() >= 0.0,
                "{size:?}: the menu runs off the window: {:?}",
                top.viewport
            );
        }
    }

    #[test]
    fn the_first_and_the_last_row_are_both_reachable_by_scrolling() {
        for size in [egui::vec2(680.0, 440.0), egui::vec2(1100.0, 720.0)] {
            let first = run(size, 0, 6);
            assert!(
                inside(first.viewport, first.first),
                "{size:?}: the first row cannot be reached: {:?} not in {:?}",
                first.first,
                first.viewport
            );
            let last = run(size, ROWS - 1, 6);
            assert!(
                inside(last.viewport, last.last),
                "{size:?}: the last row cannot be reached: {:?} not in {:?}",
                last.last,
                last.viewport
            );
        }
    }
}
