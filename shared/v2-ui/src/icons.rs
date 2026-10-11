//! Vector icons drawn on a 24-unit grid with the painter. No icon font, no image decoding, no
//! assets: they are exact at every DPI and identical in every app.
//!
//! Stroke weight scales with the icon (1.6 units at 24) and never drops below 1.25 px, so a
//! 16 px icon at 100 % scaling still reads.

use egui::epaint::PathShape;
use egui::{Color32, Painter, Pos2, Rect, Stroke};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Play,
    Pause,
    Stop,
    Prev,
    Next,
    Rewind,
    FastForward,
    VolumeHigh,
    VolumeLow,
    VolumeMute,
    Shuffle,
    Repeat,
    RepeatOne,
    Queue,
    Open,
    Folder,
    Music,
    Image,
    Film,
    Fullscreen,
    ExitFullscreen,
    ZoomIn,
    ZoomOut,
    Fit,
    ActualSize,
    RotateLeft,
    RotateRight,
    Slideshow,
    Info,
    Close,
    Subtitles,
    Speed,
    Search,
    Check,
    Warning,
    Error,
    Plus,
    Minus,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronDown,
    More,
    Equalizer,
    Theme,
    Keyboard,
}

impl Icon {
    pub const ALL: [Icon; 46] = [
        Icon::Play,
        Icon::Pause,
        Icon::Stop,
        Icon::Prev,
        Icon::Next,
        Icon::Rewind,
        Icon::FastForward,
        Icon::VolumeHigh,
        Icon::VolumeLow,
        Icon::VolumeMute,
        Icon::Shuffle,
        Icon::Repeat,
        Icon::RepeatOne,
        Icon::Queue,
        Icon::Open,
        Icon::Folder,
        Icon::Music,
        Icon::Image,
        Icon::Film,
        Icon::Fullscreen,
        Icon::ExitFullscreen,
        Icon::ZoomIn,
        Icon::ZoomOut,
        Icon::Fit,
        Icon::ActualSize,
        Icon::RotateLeft,
        Icon::RotateRight,
        Icon::Slideshow,
        Icon::Info,
        Icon::Close,
        Icon::Subtitles,
        Icon::Speed,
        Icon::Search,
        Icon::Check,
        Icon::Warning,
        Icon::Error,
        Icon::Plus,
        Icon::Minus,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::ChevronUp,
        Icon::ChevronDown,
        Icon::More,
        Icon::Equalizer,
        Icon::Theme,
        Icon::Keyboard,
    ];

    /// Paint into `rect` (square-ish; the glyph is centred and fitted).
    pub fn paint(self, painter: &Painter, rect: Rect, color: Color32) {
        let side = rect.width().min(rect.height());
        let rect = Rect::from_center_size(rect.center(), egui::vec2(side, side));
        let mut pen = Pen { painter, rect, color, w: (side / 24.0 * 1.6).max(1.25) };
        draw(&mut pen, self);
    }
}

struct Pen<'a> {
    painter: &'a Painter,
    rect: Rect,
    color: Color32,
    w: f32,
}

type P = (f32, f32);

impl Pen<'_> {
    fn p(&self, (x, y): P) -> Pos2 {
        let s = self.rect.width() / 24.0;
        self.rect.min + egui::vec2(x * s, y * s)
    }
    fn u(&self, v: f32) -> f32 {
        v * self.rect.width() / 24.0
    }
    fn stroke(&self) -> Stroke {
        Stroke::new(self.w, self.color)
    }
    fn caps(&self, a: Pos2, b: Pos2) {
        self.painter.circle_filled(a, self.w * 0.5, self.color);
        self.painter.circle_filled(b, self.w * 0.5, self.color);
    }
    /// Open polyline with round ends.
    fn line(&self, pts: &[P]) {
        let pts: Vec<Pos2> = pts.iter().map(|&q| self.p(q)).collect();
        self.caps(pts[0], pts[pts.len() - 1]);
        self.painter.add(PathShape::line(pts, self.stroke()));
    }
    fn closed(&self, pts: &[P]) {
        let pts: Vec<Pos2> = pts.iter().map(|&q| self.p(q)).collect();
        self.painter.add(PathShape::closed_line(pts, self.stroke()));
    }
    /// Filled convex polygon.
    fn fill(&self, pts: &[P]) {
        let pts: Vec<Pos2> = pts.iter().map(|&q| self.p(q)).collect();
        self.painter.add(PathShape::convex_polygon(pts, self.color, Stroke::NONE));
    }
    fn circle(&self, c: P, r: f32) {
        self.painter.circle_stroke(self.p(c), self.u(r), self.stroke());
    }
    fn disc(&self, c: P, r: f32) {
        self.painter.circle_filled(self.p(c), self.u(r), self.color);
    }
    fn rrect(&self, a: P, b: P, r: f32) {
        let rect = Rect::from_two_pos(self.p(a), self.p(b));
        self.painter.rect_stroke(rect, self.u(r), self.stroke(), egui::StrokeKind::Middle);
    }
    fn rrect_fill(&self, a: P, b: P, r: f32) {
        let rect = Rect::from_two_pos(self.p(a), self.p(b));
        self.painter.rect_filled(rect, self.u(r), self.color);
    }
    /// Arc in degrees, screen orientation (0° = right, 90° = down).
    fn arc(&self, c: P, r: f32, from: f32, to: f32) {
        let n = (((to - from).abs() / 8.0).ceil() as usize).max(4);
        let pts: Vec<P> = (0..=n)
            .map(|i| {
                let t = (from + (to - from) * i as f32 / n as f32).to_radians();
                (c.0 + r * t.cos(), c.1 + r * t.sin())
            })
            .collect();
        self.line(&pts);
    }
    fn speaker(&self) {
        self.fill(&[(3.5, 9.5), (7.5, 9.5), (12.0, 5.5), (12.0, 18.5), (7.5, 14.5), (3.5, 14.5)]);
    }
    fn mirror(pts: &[P]) -> Vec<P> {
        pts.iter().map(|&(x, y)| (24.0 - x, y)).collect()
    }
}

fn draw(g: &mut Pen<'_>, icon: Icon) {
    match icon {
        Icon::Play => g.fill(&[(7.5, 4.8), (19.5, 12.0), (7.5, 19.2)]),
        Icon::Pause => {
            g.rrect_fill((6.0, 5.0), (10.0, 19.0), 1.2);
            g.rrect_fill((14.0, 5.0), (18.0, 19.0), 1.2);
        }
        Icon::Stop => g.rrect_fill((6.0, 6.0), (18.0, 18.0), 2.0),
        Icon::Prev => {
            g.rrect_fill((5.0, 5.5), (7.4, 18.5), 1.0);
            g.fill(&[(19.0, 5.5), (8.8, 12.0), (19.0, 18.5)]);
        }
        Icon::Next => {
            g.rrect_fill((16.6, 5.5), (19.0, 18.5), 1.0);
            g.fill(&[(5.0, 5.5), (15.2, 12.0), (5.0, 18.5)]);
        }
        Icon::Rewind => {
            g.fill(&[(12.0, 6.0), (3.0, 12.0), (12.0, 18.0)]);
            g.fill(&[(21.0, 6.0), (12.0, 12.0), (21.0, 18.0)]);
        }
        Icon::FastForward => {
            g.fill(&[(3.0, 6.0), (12.0, 12.0), (3.0, 18.0)]);
            g.fill(&[(12.0, 6.0), (21.0, 12.0), (12.0, 18.0)]);
        }
        Icon::VolumeHigh => {
            g.speaker();
            g.arc((12.0, 12.0), 4.0, -50.0, 50.0);
            g.arc((12.0, 12.0), 7.8, -50.0, 50.0);
        }
        Icon::VolumeLow => {
            g.speaker();
            g.arc((12.0, 12.0), 4.0, -50.0, 50.0);
        }
        Icon::VolumeMute => {
            g.speaker();
            g.line(&[(15.5, 9.5), (20.5, 14.5)]);
            g.line(&[(20.5, 9.5), (15.5, 14.5)]);
        }
        Icon::Shuffle => {
            g.line(&[(3.5, 7.0), (7.5, 7.0), (15.0, 17.0), (20.0, 17.0)]);
            g.line(&[(3.5, 17.0), (7.5, 17.0), (15.0, 7.0), (20.0, 7.0)]);
            g.line(&[(17.5, 4.5), (20.0, 7.0), (17.5, 9.5)]);
            g.line(&[(17.5, 14.5), (20.0, 17.0), (17.5, 19.5)]);
        }
        Icon::Repeat | Icon::RepeatOne => {
            g.line(&[(4.5, 12.5), (4.5, 9.5), (7.0, 7.0), (19.5, 7.0)]);
            g.line(&[(17.0, 4.5), (19.5, 7.0), (17.0, 9.5)]);
            g.line(&[(19.5, 11.5), (19.5, 14.5), (17.0, 17.0), (4.5, 17.0)]);
            g.line(&[(7.0, 14.5), (4.5, 17.0), (7.0, 19.5)]);
            if icon == Icon::RepeatOne {
                g.line(&[(10.8, 11.0), (12.2, 10.0), (12.2, 14.2)]);
            }
        }
        Icon::Queue => {
            g.line(&[(4.0, 6.5), (20.0, 6.5)]);
            g.line(&[(4.0, 11.5), (20.0, 11.5)]);
            g.line(&[(4.0, 16.5), (12.0, 16.5)]);
            g.fill(&[(15.5, 14.0), (20.5, 17.0), (15.5, 20.0)]);
        }
        Icon::Open => {
            g.closed(&[(6.0, 3.5), (13.5, 3.5), (18.5, 8.5), (18.5, 20.5), (6.0, 20.5)]);
            g.line(&[(13.5, 3.5), (13.5, 8.5), (18.5, 8.5)]);
        }
        Icon::Folder => {
            g.closed(&[(3.5, 5.5), (9.5, 5.5), (11.5, 8.0), (20.5, 8.0), (20.5, 18.5), (3.5, 18.5)]);
            g.line(&[(3.5, 11.0), (20.5, 11.0)]);
        }
        Icon::Music => {
            g.disc((8.0, 17.0), 3.0);
            g.disc((17.0, 15.0), 3.0);
            g.line(&[(10.6, 17.0), (10.6, 6.0), (19.6, 4.0), (19.6, 15.0)]);
            g.line(&[(10.6, 9.5), (19.6, 7.5)]);
        }
        Icon::Image => {
            g.rrect((3.5, 4.5), (20.5, 19.5), 2.5);
            g.line(&[(3.8, 16.5), (9.0, 11.0), (13.0, 15.0), (15.5, 12.5), (20.2, 17.2)]);
            g.disc((15.5, 8.8), 1.7);
        }
        Icon::Film => {
            g.rrect((3.5, 4.5), (20.5, 19.5), 2.5);
            g.fill(&[(10.0, 8.8), (15.8, 12.0), (10.0, 15.2)]);
        }
        Icon::Fullscreen => {
            g.line(&[(4.0, 9.0), (4.0, 4.0), (9.0, 4.0)]);
            g.line(&[(15.0, 4.0), (20.0, 4.0), (20.0, 9.0)]);
            g.line(&[(20.0, 15.0), (20.0, 20.0), (15.0, 20.0)]);
            g.line(&[(9.0, 20.0), (4.0, 20.0), (4.0, 15.0)]);
        }
        Icon::ExitFullscreen => {
            g.line(&[(4.0, 9.0), (9.0, 9.0), (9.0, 4.0)]);
            g.line(&[(15.0, 4.0), (15.0, 9.0), (20.0, 9.0)]);
            g.line(&[(20.0, 15.0), (15.0, 15.0), (15.0, 20.0)]);
            g.line(&[(9.0, 20.0), (9.0, 15.0), (4.0, 15.0)]);
        }
        Icon::ZoomIn | Icon::ZoomOut | Icon::Search => {
            g.circle((10.5, 10.5), 6.5);
            g.line(&[(15.3, 15.3), (20.0, 20.0)]);
            if icon != Icon::Search {
                g.line(&[(7.8, 10.5), (13.2, 10.5)]);
            }
            if icon == Icon::ZoomIn {
                g.line(&[(10.5, 7.8), (10.5, 13.2)]);
            }
        }
        Icon::Fit => {
            g.rrect((3.5, 4.5), (20.5, 19.5), 2.5);
            g.rrect_fill((8.0, 8.5), (16.0, 15.5), 1.0);
        }
        Icon::ActualSize => {
            g.line(&[(5.5, 9.0), (7.5, 7.5), (7.5, 16.5)]);
            g.disc((12.0, 10.0), 1.1);
            g.disc((12.0, 14.5), 1.1);
            g.line(&[(15.0, 9.0), (17.0, 7.5), (17.0, 16.5)]);
        }
        Icon::RotateLeft | Icon::RotateRight => {
            let arc: Vec<P> = (0..=24)
                .map(|i| {
                    let t = (180.0 + 250.0 * i as f32 / 24.0f32).to_radians();
                    (12.5 + 7.0 * t.cos(), 12.5 + 7.0 * t.sin())
                })
                .collect();
            let head = [(3.0, 10.0), (5.5, 13.0), (8.3, 10.3)];
            if icon == Icon::RotateLeft {
                g.line(&arc);
                g.line(&head);
            } else {
                g.line(&Pen::mirror(&arc));
                g.line(&Pen::mirror(&head));
            }
        }
        Icon::Slideshow => {
            g.rrect((3.5, 4.0), (20.5, 16.5), 2.0);
            g.fill(&[(10.0, 7.2), (14.8, 10.25), (10.0, 13.3)]);
            g.line(&[(8.0, 20.0), (16.0, 20.0)]);
        }
        Icon::Info => {
            g.circle((12.0, 12.0), 8.5);
            g.disc((12.0, 8.0), 1.15);
            g.line(&[(12.0, 11.2), (12.0, 16.5)]);
        }
        Icon::Close => {
            g.line(&[(6.0, 6.0), (18.0, 18.0)]);
            g.line(&[(18.0, 6.0), (6.0, 18.0)]);
        }
        Icon::Subtitles => {
            g.rrect((3.5, 5.0), (20.5, 19.0), 2.5);
            g.line(&[(7.0, 12.5), (10.5, 12.5)]);
            g.line(&[(13.0, 12.5), (17.0, 12.5)]);
            g.line(&[(7.0, 15.5), (13.5, 15.5)]);
            g.line(&[(16.0, 15.5), (17.0, 15.5)]);
        }
        Icon::Speed => {
            g.arc((12.0, 14.5), 8.0, 180.0, 360.0);
            g.line(&[(12.0, 14.5), (16.2, 9.6)]);
            g.disc((12.0, 14.5), 1.5);
        }
        Icon::Check => g.line(&[(5.0, 12.5), (10.0, 17.5), (19.0, 7.0)]),
        Icon::Warning => {
            g.closed(&[(12.0, 3.8), (21.0, 19.5), (3.0, 19.5)]);
            g.line(&[(12.0, 9.5), (12.0, 13.8)]);
            g.disc((12.0, 16.6), 1.1);
        }
        Icon::Error => {
            g.circle((12.0, 12.0), 8.5);
            g.line(&[(12.0, 7.5), (12.0, 12.8)]);
            g.disc((12.0, 16.0), 1.1);
        }
        Icon::Plus => {
            g.line(&[(12.0, 5.0), (12.0, 19.0)]);
            g.line(&[(5.0, 12.0), (19.0, 12.0)]);
        }
        Icon::Minus => g.line(&[(5.0, 12.0), (19.0, 12.0)]),
        Icon::ChevronLeft => g.line(&[(15.0, 5.0), (8.0, 12.0), (15.0, 19.0)]),
        Icon::ChevronRight => g.line(&[(9.0, 5.0), (16.0, 12.0), (9.0, 19.0)]),
        Icon::ChevronUp => g.line(&[(5.0, 15.0), (12.0, 8.0), (19.0, 15.0)]),
        Icon::ChevronDown => g.line(&[(5.0, 9.0), (12.0, 16.0), (19.0, 9.0)]),
        Icon::More => {
            g.disc((5.5, 12.0), 1.7);
            g.disc((12.0, 12.0), 1.7);
            g.disc((18.5, 12.0), 1.7);
        }
        // Static bars: the playing row is marked without animating anything.
        Icon::Equalizer => {
            g.rrect_fill((4.5, 10.0), (7.5, 19.0), 1.0);
            g.rrect_fill((10.5, 5.0), (13.5, 19.0), 1.0);
            g.rrect_fill((16.5, 12.5), (19.5, 19.0), 1.0);
        }
        Icon::Theme => {
            g.circle((12.0, 12.0), 8.0);
            let c = g.p((12.0, 12.0));
            let r = g.u(8.0);
            let pts: Vec<Pos2> = (0..=16)
                .map(|i| {
                    let t = (-90.0 + 180.0 * i as f32 / 16.0f32).to_radians();
                    c + egui::vec2(r * t.cos(), r * t.sin())
                })
                .collect();
            g.painter.add(PathShape::convex_polygon(pts, g.color, Stroke::NONE));
        }
        Icon::Keyboard => {
            g.rrect((3.0, 6.5), (21.0, 17.5), 2.0);
            for x in [6.5, 10.0, 13.5, 17.0] {
                g.disc((x, 10.0), 0.9);
            }
            g.line(&[(8.0, 14.0), (16.0, 14.0)]);
        }
    }
}
