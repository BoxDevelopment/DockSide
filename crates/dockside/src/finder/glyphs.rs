//! Monochrome line glyphs in the style of SF Symbols, for the Tahoe sidebar,
//! toolbar and path bar. Each is drawn centred on (cx, cy) in a ~16pt box.

use crate::geom::{Color, Rect};
use crate::gfx::Painter;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Glyph {
    Home,
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Movies,
    Drive,
    Folder,
}

impl Glyph {
    /// The glyph for a sidebar place (`index` 0 is the home folder).
    pub fn for_place(index: usize, name: &str, is_drive: bool) -> Glyph {
        if is_drive {
            return Glyph::Drive;
        }
        if index == 0 {
            return Glyph::Home;
        }
        match name {
            "Desktop" => Glyph::Desktop,
            "Documents" => Glyph::Documents,
            "Downloads" => Glyph::Downloads,
            "Pictures" => Glyph::Pictures,
            "Music" => Glyph::Music,
            "Movies" => Glyph::Movies,
            _ => Glyph::Folder,
        }
    }
}

const W: f32 = 1.3;

pub fn draw(p: &Painter, g: Glyph, cx: f32, cy: f32, c: Color) {
    match g {
        Glyph::Home => {
            p.line(cx - 7.0, cy - 0.5, cx, cy - 7.0, c, W);
            p.line(cx, cy - 7.0, cx + 7.0, cy - 0.5, c, W);
            p.stroke_round(Rect::new(cx - 5.0, cy - 2.5, 10.0, 9.0), 1.5, c, W);
            p.stroke_round(Rect::new(cx - 1.6, cy + 1.5, 3.2, 5.0), 0.8, c, W);
        }
        Glyph::Desktop => {
            p.stroke_round(Rect::new(cx - 7.5, cy - 6.0, 15.0, 10.0), 1.8, c, W);
            p.line(cx, cy + 4.0, cx, cy + 6.5, c, W);
            p.line(cx - 3.5, cy + 6.8, cx + 3.5, cy + 6.8, c, W);
        }
        Glyph::Documents => {
            let r = Rect::new(cx - 5.5, cy - 7.0, 11.0, 14.0);
            p.stroke_round(r, 1.8, c, W);
            for dy in [-2.5, 0.5, 3.5] {
                p.line(cx - 2.8, cy + dy, cx + 2.8, cy + dy, c, 1.1);
            }
        }
        Glyph::Downloads => {
            p.stroke_circle(cx, cy, 7.0, c, W);
            p.line(cx, cy - 3.8, cx, cy + 3.5, c, W);
            p.line(cx - 3.0, cy + 0.6, cx, cy + 3.6, c, W);
            p.line(cx + 3.0, cy + 0.6, cx, cy + 3.6, c, W);
        }
        Glyph::Pictures => {
            p.stroke_round(Rect::new(cx - 7.5, cy - 5.5, 15.0, 11.0), 2.0, c, W);
            p.fill_circle(cx + 2.8, cy - 1.8, 1.4, c);
            p.line(cx - 6.5, cy + 4.0, cx - 2.0, cy - 0.5, c, W);
            p.line(cx - 2.0, cy - 0.5, cx + 2.5, cy + 4.0, c, W);
            p.line(cx + 1.0, cy + 2.5, cx + 3.5, cy + 0.5, c, W);
            p.line(cx + 3.5, cy + 0.5, cx + 6.5, cy + 3.5, c, W);
        }
        Glyph::Music => {
            p.line(cx - 2.0, cy + 4.0, cx - 2.0, cy - 6.0, c, W);
            p.line(cx + 5.0, cy + 2.5, cx + 5.0, cy - 7.5, c, W);
            p.line(cx - 2.0, cy - 6.0, cx + 5.0, cy - 7.5, c, W + 0.6);
            p.fill_circle(cx - 4.2, cy + 4.5, 2.4, c);
            p.fill_circle(cx + 2.8, cy + 3.0, 2.4, c);
        }
        Glyph::Movies => {
            let r = Rect::new(cx - 7.0, cy - 6.0, 14.0, 12.0);
            p.stroke_round(r, 1.8, c, W);
            p.line(cx - 3.5, r.y, cx - 3.5, r.bottom(), c, 1.0);
            p.line(cx + 3.5, r.y, cx + 3.5, r.bottom(), c, 1.0);
            for dy in [-3.0, 0.0, 3.0] {
                p.line(r.x, cy + dy, cx - 3.5, cy + dy, c, 1.0);
                p.line(cx + 3.5, cy + dy, r.right(), cy + dy, c, 1.0);
            }
        }
        Glyph::Drive => {
            let r = Rect::new(cx - 7.5, cy - 4.5, 15.0, 9.0);
            p.stroke_round(r, 2.2, c, W);
            p.line(r.x + 1.0, cy + 0.5, r.right() - 1.0, cy + 0.5, c, 1.0);
            p.fill_circle(r.right() - 3.2, cy + 2.4, 0.9, c);
        }
        Glyph::Folder => folder(p, cx, cy, 1.0, c),
    }
}

/// A folder outline, `k` times the 16pt size.
pub fn folder(p: &Painter, cx: f32, cy: f32, k: f32, c: Color) {
    let (w, h) = (15.0 * k, 11.0 * k);
    let r = Rect::new(cx - w / 2.0, cy - h / 2.0 + 1.0 * k, w, h);
    p.stroke_round(r, 2.0 * k, c, W * k.max(0.8));
    p.line(r.x + 0.5, r.y + 2.6 * k, r.right() - 0.5, r.y + 2.6 * k, c, 1.0 * k.max(0.8));
    p.line(r.x + 1.0 * k, r.y, r.x + 2.2 * k, r.y - 1.8 * k, c, W * k.max(0.8));
    p.line(r.x + 2.2 * k, r.y - 1.8 * k, r.x + 5.6 * k, r.y - 1.8 * k, c, W * k.max(0.8));
    p.line(r.x + 5.6 * k, r.y - 1.8 * k, r.x + 6.8 * k, r.y, c, W * k.max(0.8));
}

pub fn magnifier(p: &Painter, cx: f32, cy: f32, c: Color) {
    p.stroke_circle(cx - 1.2, cy - 1.2, 5.2, c, 1.6);
    p.line(cx + 2.6, cy + 2.6, cx + 6.2, cy + 6.2, c, 1.9);
}

pub fn chevron(p: &Painter, cx: f32, cy: f32, back: bool, size: f32, c: Color, width: f32) {
    let d = if back { 1.0 } else { -1.0 };
    p.line(cx + size * 0.45 * d, cy - size, cx - size * 0.55 * d, cy, c, width);
    p.line(cx - size * 0.55 * d, cy, cx + size * 0.45 * d, cy + size, c, width);
}

/// A Tahoe glass toolbar control: a soft shadow, a near-opaque capsule and a hairline rim.
pub fn capsule(p: &Painter, r: Rect, dark: bool, hovered: bool) {
    let rr = r.h / 2.0;
    p.fill_round(r.inset(-1.0, -1.0).offset(0.0, 1.5), rr + 1.0, Color::gray(0, if dark { 0.25 } else { 0.05 }));
    p.fill_round(r.inset(-3.0, -3.0).offset(0.0, 2.5), rr + 3.0, Color::gray(0, if dark { 0.10 } else { 0.025 }));
    let body = if dark { Color::rgba(58, 58, 60, 1.0) } else { Color::WHITE };
    p.fill_round(r, rr, body);
    if hovered {
        p.fill_round(r, rr, Color::gray(if dark { 255 } else { 0 }, 0.05));
    }
    p.stroke_round(r.inset(0.25, 0.25), rr, Color::gray(if dark { 255 } else { 0 }, if dark { 0.12 } else { 0.07 }), 0.5);
}
