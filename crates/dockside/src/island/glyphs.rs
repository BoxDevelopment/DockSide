//! Vector glyphs for the island, drawn SF Symbols-style (round caps and joins).
//! Each takes a center and a nominal size in points.

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

use crate::geom::{Color, Rect};
use crate::gfx::{pt, Painter};

pub fn triangle(p: &Painter, pts: [(f32, f32); 3], c: Color) {
    let geom = p.gfx.path(|s| unsafe {
        s.BeginFigure(pt(pts[0].0, pts[0].1), D2D1_FIGURE_BEGIN_FILLED);
        s.AddLine(pt(pts[1].0, pts[1].1));
        s.AddLine(pt(pts[2].0, pts[2].1));
        s.EndFigure(D2D1_FIGURE_END_CLOSED);
    });
    if let Some(g) = geom {
        p.fill_geometry(&g, c);
        // A round-joined stroke softens the corners.
        p.stroke_geometry(&g, c, 2.2);
    }
}

pub fn play(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let x = cx + s * 0.06;
    triangle(p, [(x - s * 0.36, cy - s * 0.46), (x + s * 0.44, cy), (x - s * 0.36, cy + s * 0.46)], c);
}

pub fn pause(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let (bw, gap) = (s * 0.26, s * 0.2);
    p.fill_round(Rect::new(cx - gap / 2.0 - bw, cy - s / 2.0, bw, s), bw * 0.3, c);
    p.fill_round(Rect::new(cx + gap / 2.0, cy - s / 2.0, bw, s), bw * 0.3, c);
}

pub fn skip(p: &Painter, cx: f32, cy: f32, s: f32, c: Color, back: bool) {
    let (tw, th) = (s * 0.52, s * 0.62);
    let dir = if back { -1.0 } else { 1.0 };
    for i in 0..2 {
        let x0 = cx + dir * (i as f32 * tw - tw);
        let x1 = x0 + dir * tw;
        triangle(p, [(x0, cy - th / 2.0), (x1, cy), (x0, cy + th / 2.0)], c);
    }
}

pub fn xmark(p: &Painter, cx: f32, cy: f32, s: f32, c: Color, w: f32) {
    let d = s / 2.0;
    p.line(cx - d, cy - d, cx + d, cy + d, c, w);
    p.line(cx - d, cy + d, cx + d, cy - d, c, w);
}

pub fn bolt(p: &Painter, cx: f32, cy: f32, h: f32, c: Color) {
    let k = h / 10.0;
    let geom = p.gfx.path(|s| unsafe {
        s.BeginFigure(pt(cx + 1.2 * k, cy - 5.0 * k), D2D1_FIGURE_BEGIN_FILLED);
        s.AddLine(pt(cx - 3.2 * k, cy + 0.9 * k));
        s.AddLine(pt(cx - 0.2 * k, cy + 0.9 * k));
        s.AddLine(pt(cx - 1.2 * k, cy + 5.0 * k));
        s.AddLine(pt(cx + 3.2 * k, cy - 0.9 * k));
        s.AddLine(pt(cx + 0.2 * k, cy - 0.9 * k));
        s.EndFigure(D2D1_FIGURE_END_CLOSED);
    });
    if let Some(g) = geom {
        p.fill_geometry(&g, c);
    }
}

/// An arc from 12 o'clock, clockwise, covering `frac` of the circle.
pub fn arc(p: &Painter, cx: f32, cy: f32, r: f32, frac: f32, c: Color, w: f32) {
    let frac = frac.clamp(0.0, 1.0);
    if frac >= 0.999 {
        return p.stroke_circle(cx, cy, r, c, w);
    }
    if frac <= 0.001 {
        return;
    }
    let a = frac * std::f32::consts::TAU;
    let end = (cx + r * a.sin(), cy - r * a.cos());
    let geom = p.gfx.path(|s| unsafe {
        s.BeginFigure(pt(cx, cy - r), D2D1_FIGURE_BEGIN_HOLLOW);
        s.AddArc(&D2D1_ARC_SEGMENT {
            point: pt(end.0, end.1),
            size: D2D_SIZE_F { width: r, height: r },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: if frac > 0.5 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
        });
        s.EndFigure(D2D1_FIGURE_END_OPEN);
    });
    if let Some(g) = geom {
        p.stroke_geometry(&g, c, w);
    }
}

/// Small arcs (speaker waves), centered on (cx, cy), opening to the right.
fn wave(p: &Painter, cx: f32, cy: f32, r: f32, c: Color, w: f32) {
    let d = r * 0.7071;
    let geom = p.gfx.path(|s| unsafe {
        s.BeginFigure(pt(cx + d, cy - d), D2D1_FIGURE_BEGIN_HOLLOW);
        s.AddArc(&D2D1_ARC_SEGMENT {
            point: pt(cx + d, cy + d),
            size: D2D_SIZE_F { width: r, height: r },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_SMALL,
        });
        s.EndFigure(D2D1_FIGURE_END_OPEN);
    });
    if let Some(g) = geom {
        p.stroke_geometry(&g, c, w);
    }
}

/// Speaker with 0-3 waves by level, or a slash when muted.
pub fn speaker(p: &Painter, cx: f32, cy: f32, s: f32, level: f32, muted: bool, c: Color) {
    let k = s / 16.0;
    let x0 = cx - 7.0 * k;
    let geom = p.gfx.path(|g| unsafe {
        g.BeginFigure(pt(x0, cy - 2.6 * k), D2D1_FIGURE_BEGIN_FILLED);
        g.AddLine(pt(x0 + 3.0 * k, cy - 2.6 * k));
        g.AddLine(pt(x0 + 7.0 * k, cy - 6.2 * k));
        g.AddLine(pt(x0 + 7.0 * k, cy + 6.2 * k));
        g.AddLine(pt(x0 + 3.0 * k, cy + 2.6 * k));
        g.AddLine(pt(x0, cy + 2.6 * k));
        g.EndFigure(D2D1_FIGURE_END_CLOSED);
    });
    if let Some(g) = geom {
        p.fill_geometry(&g, c);
        p.stroke_geometry(&g, c, 1.2 * k);
    }
    let wx = x0 + 6.0 * k;
    if muted {
        p.line(x0 - 1.0 * k, cy - 7.0 * k, x0 + 13.0 * k, cy + 7.0 * k, c, 1.6 * k);
        return;
    }
    let waves = if level <= 0.0 { 0 } else if level < 0.34 { 1 } else if level < 0.67 { 2 } else { 3 };
    for i in 0..3 {
        let a = if i < waves { 1.0 } else { 0.25 };
        wave(p, wx, cy, (3.8 + 3.2 * i as f32) * k, c.alpha(c.a * a), 1.5 * k);
    }
}

pub fn sun(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    p.fill_circle(cx, cy, 3.6 * k, c);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (sx, sy) = (a.cos(), a.sin());
        p.line(cx + sx * 5.8 * k, cy + sy * 5.8 * k, cx + sx * 7.6 * k, cy + sy * 7.6 * k, c, 1.5 * k);
    }
}

pub fn moon(p: &Painter, cx: f32, cy: f32, s: f32, c: Color, bg: Color) {
    let r = s * 0.42;
    p.fill_circle(cx, cy, r, c);
    p.fill_circle(cx + r * 0.55, cy - r * 0.45, r * 0.85, bg);
}

/// ⇪: an outlined up-arrow over a bar.
pub fn caps(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    let geom = p.gfx.path(|g| unsafe {
        g.BeginFigure(pt(cx, cy - 7.0 * k), D2D1_FIGURE_BEGIN_HOLLOW);
        g.AddLine(pt(cx + 6.5 * k, cy - 0.5 * k));
        g.AddLine(pt(cx + 3.0 * k, cy - 0.5 * k));
        g.AddLine(pt(cx + 3.0 * k, cy + 3.0 * k));
        g.AddLine(pt(cx - 3.0 * k, cy + 3.0 * k));
        g.AddLine(pt(cx - 3.0 * k, cy - 0.5 * k));
        g.AddLine(pt(cx - 6.5 * k, cy - 0.5 * k));
        g.EndFigure(D2D1_FIGURE_END_CLOSED);
    });
    if let Some(g) = geom {
        p.stroke_geometry(&g, c, 1.4 * k);
    }
    p.fill_round(Rect::new(cx - 3.0 * k, cy + 5.0 * k, 6.0 * k, 2.2 * k), 0.6 * k, c);
}

pub fn camera(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    p.fill_round(Rect::new(cx - 7.5 * k, cy - 4.5 * k, 10.5 * k, 9.0 * k), 2.2 * k, c);
    triangle(p, [(cx + 3.6 * k, cy - 1.0 * k), (cx + 7.4 * k, cy - 3.6 * k), (cx + 7.4 * k, cy + 3.6 * k)], c);
}

pub fn mic(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    p.fill_round(Rect::new(cx - 2.8 * k, cy - 7.5 * k, 5.6 * k, 10.0 * k), 2.8 * k, c);
    let geom = p.gfx.path(|g| unsafe {
        g.BeginFigure(pt(cx - 5.2 * k, cy - 1.0 * k), D2D1_FIGURE_BEGIN_HOLLOW);
        g.AddArc(&D2D1_ARC_SEGMENT {
            point: pt(cx + 5.2 * k, cy - 1.0 * k),
            size: D2D_SIZE_F { width: 5.2 * k, height: 5.2 * k },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_SMALL,
        });
        g.EndFigure(D2D1_FIGURE_END_OPEN);
    });
    if let Some(g) = geom {
        p.stroke_geometry(&g, c, 1.4 * k);
    }
    p.line(cx, cy + 4.2 * k, cx, cy + 7.2 * k, c, 1.4 * k);
}

/// Over-ear headphones (Bluetooth audio devices).
pub fn headphones(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    let geom = p.gfx.path(|g| unsafe {
        g.BeginFigure(pt(cx - 6.5 * k, cy + 2.0 * k), D2D1_FIGURE_BEGIN_HOLLOW);
        g.AddArc(&D2D1_ARC_SEGMENT {
            point: pt(cx + 6.5 * k, cy + 2.0 * k),
            size: D2D_SIZE_F { width: 6.5 * k, height: 6.5 * k },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_LARGE,
        });
        g.EndFigure(D2D1_FIGURE_END_OPEN);
    });
    if let Some(g) = geom {
        p.stroke_geometry(&g, c, 1.6 * k);
    }
    p.fill_round(Rect::new(cx - 8.0 * k, cy + 0.5 * k, 4.2 * k, 7.0 * k), 1.8 * k, c);
    p.fill_round(Rect::new(cx + 3.8 * k, cy + 0.5 * k, 4.2 * k, 7.0 * k), 1.8 * k, c);
}

pub fn check(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    p.fill_circle(cx, cy, 8.0 * k, c);
    let ink = Color::BLACK.alpha(c.a);
    p.line(cx - 3.6 * k, cy + 0.2 * k, cx - 1.0 * k, cy + 2.8 * k, ink, 1.8 * k);
    p.line(cx - 1.0 * k, cy + 2.8 * k, cx + 3.8 * k, cy - 2.6 * k, ink, 1.8 * k);
}

/// Arrow into a tray.
pub fn download(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    p.line(cx, cy - 7.0 * k, cx, cy + 1.5 * k, c, 1.6 * k);
    p.line(cx - 3.8 * k, cy - 2.2 * k, cx, cy + 1.8 * k, c, 1.6 * k);
    p.line(cx + 3.8 * k, cy - 2.2 * k, cx, cy + 1.8 * k, c, 1.6 * k);
    p.line(cx - 7.0 * k, cy + 3.0 * k, cx - 7.0 * k, cy + 6.5 * k, c, 1.6 * k);
    p.line(cx - 7.0 * k, cy + 6.5 * k, cx + 7.0 * k, cy + 6.5 * k, c, 1.6 * k);
    p.line(cx + 7.0 * k, cy + 3.0 * k, cx + 7.0 * k, cy + 6.5 * k, c, 1.6 * k);
}

/// The file shelf: an open tray.
pub fn tray(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    let geom = p.gfx.path(|g| unsafe {
        g.BeginFigure(pt(cx - 7.5 * k, cy + 0.5 * k), D2D1_FIGURE_BEGIN_HOLLOW);
        g.AddLine(pt(cx - 5.0 * k, cy - 6.0 * k));
        g.AddLine(pt(cx + 5.0 * k, cy - 6.0 * k));
        g.AddLine(pt(cx + 7.5 * k, cy + 0.5 * k));
        g.AddLine(pt(cx + 7.5 * k, cy + 6.0 * k));
        g.AddLine(pt(cx - 7.5 * k, cy + 6.0 * k));
        g.EndFigure(D2D1_FIGURE_END_CLOSED);
        g.BeginFigure(pt(cx - 7.5 * k, cy + 0.5 * k), D2D1_FIGURE_BEGIN_HOLLOW);
        g.AddLine(pt(cx - 2.8 * k, cy + 0.5 * k));
        g.AddLine(pt(cx - 2.0 * k, cy + 2.6 * k));
        g.AddLine(pt(cx + 2.0 * k, cy + 2.6 * k));
        g.AddLine(pt(cx + 2.8 * k, cy + 0.5 * k));
        g.AddLine(pt(cx + 7.5 * k, cy + 0.5 * k));
        g.EndFigure(D2D1_FIGURE_END_OPEN);
    });
    if let Some(g) = geom {
        p.stroke_geometry(&g, c, 1.4 * k);
    }
}

pub fn calendar(p: &Painter, cx: f32, cy: f32, s: f32, c: Color, accent: Color) {
    let k = s / 16.0;
    let body = Rect::new(cx - 7.0 * k, cy - 6.0 * k, 14.0 * k, 13.0 * k);
    p.stroke_round(body, 2.6 * k, c, 1.4 * k);
    p.fill_round(Rect::new(body.x, body.y, body.w, 4.0 * k), 2.0 * k, accent);
    for (dx, dy) in [(-3.5, 1.5), (0.0, 1.5), (3.5, 1.5), (-3.5, 4.5), (0.0, 4.5)] {
        p.fill_circle(cx + dx * k, cy + dy * k, 0.9 * k, c);
    }
}

/// A timer face: ring with the remaining fraction lit.
pub fn timer(p: &Painter, cx: f32, cy: f32, s: f32, frac: f32, c: Color) {
    let r = s * 0.42;
    p.stroke_circle(cx, cy, r, c.alpha(c.a * 0.3), s * 0.12);
    arc(p, cx, cy, r, frac, c, s * 0.12);
}

pub fn stopwatch(p: &Painter, cx: f32, cy: f32, s: f32, frac: f32, c: Color) {
    let k = s / 16.0;
    p.stroke_circle(cx, cy + 1.0 * k, 6.4 * k, c, 1.5 * k);
    p.fill_round(Rect::new(cx - 1.6 * k, cy - 8.4 * k, 3.2 * k, 1.8 * k), 0.6 * k, c);
    let a = frac * std::f32::consts::TAU;
    p.line(cx, cy + 1.0 * k, cx + a.sin() * 4.2 * k, cy + 1.0 * k - a.cos() * 4.2 * k, c, 1.4 * k);
}

pub fn bell(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let k = s / 16.0;
    let geom = p.gfx.path(|g| unsafe {
        g.BeginFigure(pt(cx - 6.5 * k, cy + 4.0 * k), D2D1_FIGURE_BEGIN_FILLED);
        g.AddLine(pt(cx - 5.0 * k, cy + 1.5 * k));
        g.AddLine(pt(cx - 5.0 * k, cy - 2.0 * k));
        g.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(cx - 5.0 * k, cy - 8.0 * k),
            point2: pt(cx + 5.0 * k, cy - 8.0 * k),
            point3: pt(cx + 5.0 * k, cy - 2.0 * k),
        });
        g.AddLine(pt(cx + 5.0 * k, cy + 1.5 * k));
        g.AddLine(pt(cx + 6.5 * k, cy + 4.0 * k));
        g.EndFigure(D2D1_FIGURE_END_CLOSED);
    });
    if let Some(g) = geom {
        p.fill_geometry(&g, c);
    }
    p.fill_circle(cx, cy + 6.2 * k, 1.8 * k, c);
}

/// A starburst of rounded rays (Claude Code), turned by `angle` radians.
pub fn spark(p: &Painter, cx: f32, cy: f32, s: f32, angle: f32, c: Color) {
    const RAYS: usize = 10;
    let r = s * 0.5;
    for i in 0..RAYS {
        let a = angle + i as f32 * std::f32::consts::TAU / RAYS as f32;
        // Uneven rays give it a hand-drawn feel.
        let len = r * [1.0, 0.78, 0.92, 0.72, 0.96][i % 5];
        let (sx, sy) = (a.sin(), -a.cos());
        p.line(cx + sx * r * 0.22, cy + sy * r * 0.22, cx + sx * len, cy + sy * len, c, (s * 0.13).max(1.2));
    }
}

/// A battery outline filled to `pct`.
pub fn battery(p: &Painter, right: f32, cy: f32, pct: u8, c: Color, a: f32) {
    let bw = 25.0;
    let body = Rect::new(right - bw, cy - 6.0, bw - 2.5, 12.0);
    p.stroke_round(body.inset(0.5, 0.5), 3.6, c.alpha(0.45 * a), 1.0);
    p.fill_round(Rect::new(body.right() + 1.0, cy - 2.0, 1.6, 4.0), 0.8, c.alpha(0.45 * a));
    let inner = body.inset(2.0, 2.0);
    p.fill_round(Rect::new(inner.x, inner.y, (inner.w * pct.min(100) as f32 / 100.0).max(2.0), inner.h), 1.6, c.alpha(a));
}
