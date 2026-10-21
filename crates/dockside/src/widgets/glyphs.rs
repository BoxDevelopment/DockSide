//! Vector glyphs for the widgets: weather symbols (colored, SF Symbols
//! multicolor-style), devices and a music note. Each takes a center and a
//! nominal size in points.

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

use crate::geom::{Color, Rect};
use crate::gfx::{pt, Painter};

const SUN: Color = Color::rgba(255, 204, 0, 1.0);
const MOON: Color = Color::rgba(255, 255, 255, 1.0);
const CLOUD: Color = Color::rgba(255, 255, 255, 1.0);
const DARK_CLOUD: Color = Color::rgba(222, 226, 232, 1.0);
const RAIN: Color = Color::rgba(90, 200, 250, 1.0);
const BOLT: Color = Color::rgba(255, 214, 10, 1.0);

pub fn sun(p: &Painter, cx: f32, cy: f32, s: f32) {
    p.fill_circle(cx, cy, s * 0.24, SUN);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (dx, dy) = (a.cos(), a.sin());
        p.line(cx + dx * s * 0.34, cy + dy * s * 0.34, cx + dx * s * 0.46, cy + dy * s * 0.46, SUN, (s * 0.07).max(1.2));
    }
}

/// A crescent: a disc with an offset disc cut out of it.
pub fn moon(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let r = s * 0.36;
    unsafe {
        let disc = |x: f32, y: f32, r: f32| {
            p.gfx.d2d.CreateEllipseGeometry(&D2D1_ELLIPSE { point: pt(x, y), radiusX: r, radiusY: r }).ok()
        };
        let (Some(a), Some(b)) = (disc(cx, cy, r), disc(cx + r * 0.62, cy - r * 0.42, r * 0.86)) else { return };
        let Ok(path) = p.gfx.d2d.CreatePathGeometry() else { return };
        let Ok(sink) = path.Open() else { return };
        let ok = a.CombineWithGeometry(&b, D2D1_COMBINE_MODE_EXCLUDE, None, 0.25, &sink).is_ok();
        if sink.Close().is_ok() && ok {
            p.fill_geometry(&path, c);
        }
    }
}

/// A cloud whose base sits at `cy + s * 0.22`.
pub fn cloud(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    p.fill_circle(cx - s * 0.2, cy + s * 0.04, s * 0.18, c);
    p.fill_circle(cx + s * 0.04, cy - s * 0.06, s * 0.26, c);
    p.fill_circle(cx + s * 0.26, cy + s * 0.06, s * 0.16, c);
    p.fill_round(Rect::new(cx - s * 0.38, cy + s * 0.02, s * 0.8, s * 0.2), s * 0.1, c);
}

fn drops(p: &Painter, cx: f32, cy: f32, s: f32, snow: bool) {
    for i in 0..3 {
        let x = cx - s * 0.2 + i as f32 * s * 0.2;
        let y = cy + s * 0.32;
        if snow {
            p.fill_circle(x, y + s * 0.04, s * 0.05, CLOUD);
        } else {
            p.line(x + s * 0.04, y, x - s * 0.02, y + s * 0.14, RAIN, (s * 0.07).max(1.2));
        }
    }
}

/// The symbol for a WMO weather code.
pub fn weather(p: &Painter, cx: f32, cy: f32, s: f32, code: u8, is_day: bool) {
    let small = |p: &Painter| {
        // The sun or moon peeking out from behind a cloud.
        let (x, y) = (cx - s * 0.14, cy - s * 0.16);
        if is_day {
            sun(p, x, y, s * 0.7);
        } else {
            moon(p, x, y, s * 0.7, MOON);
        }
    };
    match code {
        0 if is_day => sun(p, cx, cy, s),
        0 => moon(p, cx, cy, s, MOON),
        1 | 2 => {
            small(p);
            cloud(p, cx + s * 0.08, cy + s * 0.08, s * 0.8, CLOUD);
        }
        3 => cloud(p, cx, cy, s, CLOUD),
        45 | 48 => {
            cloud(p, cx, cy - s * 0.1, s * 0.9, DARK_CLOUD);
            for i in 0..2 {
                let y = cy + s * 0.24 + i as f32 * s * 0.12;
                p.line(cx - s * 0.32, y, cx + s * 0.32, y, CLOUD, (s * 0.06).max(1.2));
            }
        }
        51..=67 | 80..=82 => {
            cloud(p, cx, cy - s * 0.12, s * 0.9, DARK_CLOUD);
            drops(p, cx, cy - s * 0.1, s, false);
        }
        71..=77 | 85 | 86 => {
            cloud(p, cx, cy - s * 0.12, s * 0.9, CLOUD);
            drops(p, cx, cy - s * 0.1, s, true);
        }
        95..=99 => {
            cloud(p, cx, cy - s * 0.14, s * 0.9, DARK_CLOUD);
            crate::island::glyphs::bolt(p, cx, cy + s * 0.26, s * 0.42, BOLT);
        }
        _ => cloud(p, cx, cy, s, CLOUD),
    }
}

/// The filled arrow shown next to an automatically found place.
pub fn location(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 10.0;
    let geom = p.gfx.path(|g| unsafe {
        g.BeginFigure(pt(cx + 4.5 * k, cy - 4.5 * k), D2D1_FIGURE_BEGIN_FILLED);
        g.AddLine(pt(cx + 0.6 * k, cy + 4.8 * k));
        g.AddLine(pt(cx - 0.2 * k, cy + 0.2 * k));
        g.AddLine(pt(cx - 4.8 * k, cy - 0.6 * k));
        g.EndFigure(D2D1_FIGURE_END_CLOSED);
    });
    if let Some(g) = geom {
        p.fill_geometry(&g, c);
    }
}

pub fn laptop(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    p.stroke_round(Rect::new(cx - 5.5 * k, cy - 4.5 * k, 11.0 * k, 7.5 * k), 1.2 * k, c, 1.4 * k);
    p.line(cx - 7.5 * k, cy + 4.6 * k, cx + 7.5 * k, cy + 4.6 * k, c, 1.6 * k);
}

pub fn mouse(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    p.stroke_round(Rect::new(cx - 4.5 * k, cy - 6.5 * k, 9.0 * k, 13.0 * k), 4.5 * k, c, 1.4 * k);
    p.line(cx, cy - 4.5 * k, cx, cy - 2.0 * k, c, 1.4 * k);
}

pub fn gamepad(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    p.stroke_round(Rect::new(cx - 7.5 * k, cy - 4.0 * k, 15.0 * k, 8.5 * k), 4.0 * k, c, 1.4 * k);
    let (dx, dy) = (cx - 3.6 * k, cy + 0.2 * k);
    p.line(dx - 1.6 * k, dy, dx + 1.6 * k, dy, c, 1.2 * k);
    p.line(dx, dy - 1.6 * k, dx, dy + 1.6 * k, c, 1.2 * k);
    p.fill_circle(cx + 3.0 * k, cy + 1.2 * k, 0.9 * k, c);
    p.fill_circle(cx + 4.8 * k, cy - 0.6 * k, 0.9 * k, c);
}

pub fn keyboard(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    p.stroke_round(Rect::new(cx - 7.5 * k, cy - 4.5 * k, 15.0 * k, 9.0 * k), 1.6 * k, c, 1.4 * k);
    for row in 0..2 {
        for col in 0..4 {
            p.fill_circle(cx - 4.2 * k + col as f32 * 2.8 * k, cy - 1.8 * k + row as f32 * 2.4 * k, 0.7 * k, c);
        }
    }
    p.line(cx - 3.0 * k, cy + 2.6 * k, cx + 3.0 * k, cy + 2.6 * k, c, 1.1 * k);
}

/// The Bluetooth rune, for accessories we don't have a picture of.
pub fn bluetooth(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    let pts = [(-3.5, -3.2), (3.5, 3.2), (0.0, 6.5), (0.0, -6.5), (3.5, -3.2), (-3.5, 3.2)];
    let geom = p.gfx.path(|g| unsafe {
        g.BeginFigure(pt(cx + pts[0].0 * k, cy + pts[0].1 * k), D2D1_FIGURE_BEGIN_HOLLOW);
        for (x, y) in &pts[1..] {
            g.AddLine(pt(cx + x * k, cy + y * k));
        }
        g.EndFigure(D2D1_FIGURE_END_OPEN);
    });
    if let Some(g) = geom {
        p.stroke_geometry(&g, c, 1.4 * k);
    }
}

/// A beamed pair of eighth notes.
pub fn music(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    let (x0, x1) = (cx - 4.0 * k, cx + 5.0 * k);
    p.fill_circle(x0 - 2.2 * k, cy + 4.6 * k, 2.6 * k, c);
    p.fill_circle(x1 - 2.2 * k, cy + 3.4 * k, 2.6 * k, c);
    p.line(x0, cy + 4.4 * k, x0, cy - 5.0 * k, c, 1.4 * k);
    p.line(x1, cy + 3.2 * k, x1, cy - 6.2 * k, c, 1.4 * k);
    p.line(x0, cy - 5.0 * k, x1, cy - 6.2 * k, c, 2.6 * k);
}

pub fn plus(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let d = s / 2.0;
    p.line(cx - d, cy, cx + d, cy, c, 1.6);
    p.line(cx, cy - d, cx, cy + d, c, 1.6);
}
