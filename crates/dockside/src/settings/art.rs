//! Illustrations for System Settings: the thumbnails a picker shows its
//! choices as, and the live previews heading the Dock, Menu Bar and Dynamic
//! Island pages. Everything is drawn as vectors from the config, so a
//! preview changes the moment its setting does.

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

use super::{Anim, Key, Page, RowCtx, CLOCKS};
use crate::anim::{feel, lerp};
use crate::config::{Backdrop, IconStyle, IslandFinish, IslandStyle, WidgetKind};
use crate::geom::{Color, Rect};
use crate::gfx::{pt, Align, Painter, Weight};
use crate::menubar;
use crate::theme::Theme;
use crate::win::system::Battery;

/// Corner radius of a picker thumbnail, and of a page's preview.
pub(super) const THUMB_RADIUS: f32 = 8.0;
pub(super) const PREVIEW_RADIUS: f32 = 10.0;

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::rgba(r, g, b, 1.0)
}

fn flag(on: bool) -> f32 {
    if on {
        1.0
    } else {
        0.0
    }
}

/// One geometry of straight-edged closed figures.
fn shape(p: &Painter, figures: &[Vec<(f32, f32)>]) -> Option<ID2D1PathGeometry> {
    p.gfx.path(|s| unsafe {
        for f in figures {
            let Some(first) = f.first() else { continue };
            s.BeginFigure(pt(first.0, first.1), D2D1_FIGURE_BEGIN_FILLED);
            for q in &f[1..] {
                s.AddLine(pt(q.0, q.1));
            }
            s.EndFigure(D2D1_FIGURE_END_CLOSED);
        }
    })
}

fn fill_shape(p: &Painter, figures: &[Vec<(f32, f32)>], c: Color) {
    if let Some(g) = shape(p, figures) {
        p.fill_geometry(&g, c);
    }
}

/// A soft glow of `c`, fading to nothing `radius` out.
fn glow(p: &Painter, cx: f32, cy: f32, radius: f32, c: Color) {
    let stops = [
        D2D1_GRADIENT_STOP { position: 0.0, color: c.d2d() },
        D2D1_GRADIENT_STOP { position: 0.45, color: c.alpha(c.a * 0.55).d2d() },
        D2D1_GRADIENT_STOP { position: 1.0, color: c.alpha(0.0).d2d() },
    ];
    unsafe {
        let Ok(coll) = p.rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else { return };
        let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES { center: pt(cx, cy), gradientOriginOffset: pt(0.0, 0.0), radiusX: radius, radiusY: radius };
        if let Ok(brush) = p.rt.CreateRadialGradientBrush(&props, None, &coll) {
            p.rt.FillEllipse(&D2D1_ELLIPSE { point: pt(cx, cy), radiusX: radius, radiusY: radius }, &brush);
        }
    }
}

/// A stand-in desktop wallpaper filling `r`: a gradient with a few soft
/// patches of colour, so see-through materials have something to show.
fn wallpaper(p: &Painter, r: Rect, radius: f32, dark: bool) {
    let (top, bottom) = if dark { (rgb(24, 30, 78), rgb(66, 28, 88)) } else { (rgb(66, 138, 250), rgb(142, 104, 238)) };
    p.fill_round_gradient(r, radius, top, bottom);
    let a = if dark { 0.5 } else { 0.95 };
    // Each fades out before it reaches the rounded corners.
    p.clipped(r, || {
        glow(p, r.x + r.w * 0.2, r.y + r.h * 0.68, r.h * 0.42, rgb(255, 170, 70).alpha(a));
        glow(p, r.x + r.w * 0.76, r.y + r.h * 0.62, r.h * 0.46, rgb(255, 96, 160).alpha(a));
        glow(p, r.x + r.w * 0.52, r.y + r.h * 0.26, r.h * 0.36, rgb(90, 225, 240).alpha(a * 0.9));
    });
}

/// A neutral backing for thumbnails that aren't a desktop.
fn surface(p: &Painter, r: Rect, dark: bool) {
    p.fill_round(r, THUMB_RADIUS, if dark { Color::gray(255, 0.07) } else { Color::gray(0, 0.045) });
}

pub(super) fn backdrop_name(b: Backdrop) -> &'static str {
    match b {
        Backdrop::Acrylic => "acrylic",
        Backdrop::Blur => "blur",
        Backdrop::Glass => "glass",
        Backdrop::Clear => "clear",
        Backdrop::None => "none",
    }
}

/// A panel of the material called `kind` (a backdrop's config name).
fn panel(p: &Painter, r: Rect, radius: f32, kind: &str, dark: bool) {
    let (fill, rim) = match kind {
        "acrylic" => (if dark { Color::rgba(34, 34, 38, 0.76) } else { Color::rgba(246, 246, 250, 0.74) }, 0.0),
        "blur" => (if dark { Color::rgba(34, 34, 38, 0.4) } else { Color::rgba(255, 255, 255, 0.34) }, 0.0),
        "glass" => (if dark { Color::gray(255, 0.1) } else { Color::gray(255, 0.17) }, 0.75),
        "clear" => (Color::TRANSPARENT, 0.0),
        _ => (if dark { rgb(46, 46, 48) } else { rgb(248, 248, 250) }, 0.0),
    };
    if fill.a > 0.0 {
        p.fill_round(r, radius, fill);
    }
    if rim > 0.0 {
        p.stroke_round(r.inset(0.5, 0.5), radius, Color::gray(255, rim), 1.0);
    }
}

/// What's drawn on a panel of that material: white where the wallpaper shows through.
fn ink(kind: &str, dark: bool) -> Color {
    match kind {
        "clear" | "glass" | "blur" => Color::gray(255, 0.96),
        _ if dark => Color::gray(255, 0.9),
        _ => Color::gray(0, 0.78),
    }
}

/// A menu bar `h` tall across the top of `r` (whose corners are `radius`).
fn top_bar(p: &Painter, r: Rect, radius: f32, h: f32, kind: &str, dark: bool) -> Rect {
    let bar = Rect::new(r.x, r.y, r.w, h);
    p.clipped(bar, || panel(p, Rect::new(r.x, r.y, r.w, h + 2.0 * radius + 4.0), radius, kind, dark));
    bar
}

/// Menu titles and status items as dashes, for a small menu bar.
fn bar_marks(p: &Painter, bar: Rect, ink: Color) {
    let cy = bar.cy();
    p.fill_circle(bar.x + 7.0, cy, 2.0, ink);
    let mut x = bar.x + 13.0;
    for (w, a) in [(11.0, 1.0), (7.0, 0.7), (7.0, 0.7)] {
        p.fill_round(Rect::new(x, cy - 1.25, w, 2.5), 1.25, ink.alpha(ink.a * a));
        x += w + 3.5;
    }
    let mut x = bar.right() - 6.0;
    for w in [13.0, 4.0, 4.0] {
        x -= w;
        p.fill_round(Rect::new(x, cy - 1.25, w, 2.5), 1.25, ink.alpha(ink.a * 0.8));
        x -= 3.5;
    }
}

const APPS: [(Color, Color); 7] = [
    (rgb(96, 176, 255), rgb(24, 112, 246)),
    (rgb(116, 228, 126), rgb(40, 180, 72)),
    (rgb(255, 190, 84), rgb(250, 132, 30)),
    (rgb(255, 124, 154), rgb(240, 60, 102)),
    (rgb(184, 136, 255), rgb(122, 82, 232)),
    (rgb(84, 84, 92), rgb(32, 32, 38)),
    (rgb(104, 228, 232), rgb(40, 170, 202)),
];

/// A made-up app icon: a squircle, or (native) whatever outline the app drew.
fn app_icon(p: &Painter, r: Rect, i: usize, squircle: bool) {
    let (a, b) = APPS[i % APPS.len()];
    if squircle {
        p.fill_round_gradient(r, r.w * 0.23, a, b);
    } else {
        match i % 3 {
            0 => p.fill_circle(r.cx(), r.cy(), r.w * 0.47, b),
            1 => p.fill_round_gradient(r.inset(r.w * 0.05, r.w * 0.05), r.w * 0.05, a, b),
            _ => p.fill_round_gradient(r.inset(r.w * 0.17, r.w * 0.03), r.w * 0.09, a, b),
        }
    }
    p.fill_circle(r.cx(), r.cy(), r.w * 0.15, Color::gray(255, 0.92));
}

/// The pointer, with its tip at (x, y).
fn cursor(p: &Painter, x: f32, y: f32, k: f32, alpha: f32) {
    let pts = [(0.0, 0.0), (0.0, 11.0), (3.0, 8.4), (5.2, 13.0), (7.0, 12.2), (4.9, 7.7), (8.6, 7.7)];
    if let Some(g) = shape(p, &[pts.iter().map(|(a, b)| (x + a * k, y + b * k)).collect()]) {
        p.fill_geometry(&g, Color::WHITE.alpha(alpha));
        p.stroke_geometry(&g, Color::gray(0, 0.8 * alpha), 0.8);
    }
}

// ---------------------------------------------------------------------------
// Picker thumbnails
// ---------------------------------------------------------------------------

/// The picture for choice `value` of the setting `key`, filling `r`.
pub(super) fn thumb(p: &Painter, key: Key, value: &str, r: Rect, theme: &Theme) {
    let dark = theme.dark;
    match key {
        Key::Appearance => appearance(p, r, value, theme.accent),
        Key::Font => {
            surface(p, r, dark);
            let f = p.gfx.format(value, 26.0, Weight::Regular, Align::Center);
            p.text("Aa", &f, r, theme.text);
        }
        Key::ShortcutStyle => shortcuts(p, r, value == "mac", theme),
        Key::MbLogo => {
            wallpaper(p, r, THUMB_RADIUS, false);
            let bar = top_bar(p, r, THUMB_RADIUS, 22.0, "acrylic", dark);
            let c = ink("acrylic", dark);
            let logo = Rect::new(bar.x + 6.0, bar.y, 20.0, bar.h);
            if value == "apple" {
                menubar::draw_apple(p, logo, c);
            } else {
                menubar::draw_windows_logo(p, logo, c);
            }
            p.fill_round(Rect::new(bar.x + 32.0, bar.cy() - 2.0, 22.0, 4.0), 2.0, c);
            p.fill_round(Rect::new(bar.x + 59.0, bar.cy() - 1.5, 12.0, 3.0), 1.5, c.alpha(c.a * 0.6));
            p.fill_round(Rect::new(bar.x + 75.0, bar.cy() - 1.5, 12.0, 3.0), 1.5, c.alpha(c.a * 0.6));
        }
        Key::MbBackdrop | Key::DockBackdrop | Key::SpotlightBackdrop | Key::WidgetsBackdrop => backdrop(p, r, key, value, dark),
        Key::FinderSidebar => finder_sidebar(p, r, value, theme),
        Key::FinderView => finder_view(p, r, value, theme),
        Key::DockIconStyle => {
            wallpaper(p, r, THUMB_RADIUS, false);
            let plate = Rect::new(r.cx() - 41.0, r.cy() - 8.0, 82.0, 26.0);
            panel(p, plate, 8.0, "acrylic", dark);
            for i in 0..4 {
                app_icon(p, Rect::new(plate.x + 5.0 + i as f32 * 18.5, plate.y + 5.0, 16.0, 16.0), i, value == "squircle");
            }
        }
        Key::IslandStyle => island_style(p, r, value == "notch", dark),
        Key::IslandFinish => island_finish(p, r, value),
        Key::MinimizeEffect | Key::OpenEffect | Key::CloseEffect => effect(p, r, value, dark),
        Key::DockDisplays | Key::MbDisplays => displays(p, r, key == Key::DockDisplays, value, dark),
        Key::LaunchpadDock | Key::LaunchpadSpotlight | Key::LaunchpadHotkeyOpens => launcher(p, r, value == "tahoe", dark),
        _ => surface(p, r, dark),
    }
}

/// A small app window: sidebar, traffic lights, a selected row and a button in the accent color.
fn mini_window(p: &Painter, w: Rect, dark: bool, accent: Color) {
    let (body, side, line) =
        if dark { (rgb(38, 38, 40), rgb(54, 54, 58), Color::gray(255, 0.2)) } else { (Color::WHITE, rgb(234, 234, 239), Color::gray(0, 0.15)) };
    p.fill_round(w.inset(-1.0, -1.0).offset(0.0, 1.5), 5.0, Color::gray(0, 0.2));
    p.fill_round(w, 4.0, body);
    let sw = (w.w * 0.32).round();
    p.clipped(Rect::new(w.x, w.y, sw, w.h), || p.fill_round(w, 4.0, side));
    for (i, c) in [rgb(255, 95, 87), rgb(254, 188, 46), rgb(40, 200, 64)].into_iter().enumerate() {
        p.fill_circle(w.x + 5.0 + i as f32 * 4.5, w.y + 5.0, 1.5, c);
    }
    p.fill_round(Rect::new(w.x + 3.0, w.y + 10.5, sw - 6.0, 5.5), 1.75, accent);
    for i in 1..4 {
        p.fill_round(Rect::new(w.x + 5.0, w.y + 12.0 + i as f32 * 7.0, sw - 12.0, 2.0), 1.0, line);
    }
    let (cx, cw) = (w.x + sw + 5.0, w.w - sw - 10.0);
    p.fill_round(Rect::new(cx, w.y + 6.5, cw * 0.5, 3.0), 1.5, line.alpha(line.a * 1.8));
    for i in 0..3 {
        p.fill_round(Rect::new(cx, w.y + 15.0 + i as f32 * 5.5, cw * (1.0 - 0.16 * i as f32), 2.0), 1.0, line);
    }
    p.fill_round(Rect::new(w.right() - 19.0, w.bottom() - 8.5, 14.0, 5.0), 2.5, accent);
}

fn appearance(p: &Painter, r: Rect, value: &str, accent: Color) {
    let scene = |dark: bool| {
        wallpaper(p, r, THUMB_RADIUS, dark);
        mini_window(p, Rect::new(r.x + r.w * 0.15, r.y + r.h * 0.2, r.w * 0.7, r.h * 0.62), dark, accent);
    };
    match value {
        "light" => scene(false),
        "dark" => scene(true),
        // Auto: light on the left, dark on the right.
        _ => {
            p.clipped(Rect::new(r.x, r.y, r.w / 2.0, r.h), || scene(false));
            p.clipped(Rect::new(r.cx(), r.y, r.w / 2.0, r.h), || scene(true));
        }
    }
}

/// A menu showing its shortcut hints the Mac or the Windows way.
fn shortcuts(p: &Painter, r: Rect, mac: bool, theme: &Theme) {
    surface(p, r, theme.dark);
    let card = Rect::new(r.x + 9.0, r.y + 7.0, r.w - 18.0, r.h - 14.0);
    p.fill_round(card.inset(-1.0, -1.0).offset(0.0, 1.5), 7.0, Color::gray(0, 0.1));
    p.fill_round(card, 6.0, if theme.dark { rgb(56, 56, 60) } else { Color::WHITE });
    let left = p.gfx.format(&theme.font, 9.0, Weight::Regular, Align::Leading);
    let right = p.gfx.format(&theme.font, 9.0, Weight::Regular, Align::Trailing);
    let rows = if mac { [("Cut", "\u{2318}X"), ("Copy", "\u{2318}C"), ("Paste", "\u{2318}V")] } else { [("Cut", "Ctrl+X"), ("Copy", "Ctrl+C"), ("Paste", "Ctrl+V")] };
    let row_h = (card.h - 6.0) / 3.0;
    for (i, (name, keys)) in rows.into_iter().enumerate() {
        let row = Rect::new(card.x + 3.0, card.y + 3.0 + i as f32 * row_h, card.w - 6.0, row_h);
        let (text, hint) = if i == 1 {
            p.fill_round(row, 3.5, theme.accent);
            (Color::WHITE, Color::gray(255, 0.85))
        } else {
            (theme.text, theme.text_secondary)
        };
        let inner = Rect::new(row.x + 5.0, row.y, row.w - 10.0, row.h);
        p.text(name, &left, inner, text);
        p.text(keys, &right, inner, hint);
    }
}

/// The surface `key` puts its backdrop behind (menu bar, Dock, Spotlight,
/// widgets), made of the material `kind`.
fn backdrop(p: &Painter, r: Rect, key: Key, kind: &str, dark: bool) {
    wallpaper(p, r, THUMB_RADIUS, false);
    let c = ink(kind, dark);
    match key {
        Key::MbBackdrop => {
            let bar = top_bar(p, r, THUMB_RADIUS, 14.0, kind, dark);
            bar_marks(p, bar, c);
        }
        Key::DockBackdrop => {
            let plate = Rect::new(r.cx() - 36.0, r.bottom() - 26.0, 72.0, 19.0);
            panel(p, plate, 6.5, kind, dark);
            for i in 0..5 {
                app_icon(p, Rect::new(plate.x + 4.5 + i as f32 * 13.0, plate.y + 4.0, 11.0, 11.0), i, true);
            }
        }
        Key::SpotlightBackdrop => {
            let bar = Rect::new(r.cx() - 38.0, r.cy() - 10.0, 76.0, 20.0);
            panel(p, bar, 10.0, kind, dark);
            p.stroke_circle(bar.x + 11.0, bar.cy() - 0.8, 3.2, c, 1.4);
            p.line(bar.x + 13.4, bar.cy() + 1.6, bar.x + 15.6, bar.cy() + 3.8, c, 1.4);
            p.fill_round(Rect::new(bar.x + 21.0, bar.cy() - 1.5, 34.0, 3.0), 1.5, c.alpha(c.a * 0.55));
        }
        _ => {
            let a = Rect::new(r.cx() - 34.0, r.cy() - 16.0, 32.0, 32.0);
            let b = a.offset(36.0, 0.0);
            panel(p, a, 8.5, kind, dark);
            panel(p, b, 8.5, kind, dark);
            // A clock and a little chart.
            p.stroke_circle(a.cx(), a.cy(), 9.0, c, 1.4);
            p.line(a.cx(), a.cy(), a.cx(), a.cy() - 5.5, c, 1.4);
            p.line(a.cx(), a.cy(), a.cx() + 4.0, a.cy() + 2.0, c, 1.4);
            for (i, h) in [7.0, 13.0, 9.0, 16.0].into_iter().enumerate() {
                p.fill_round(Rect::new(b.x + 6.5 + i as f32 * 5.2, b.bottom() - 7.0 - h, 3.2, h), 1.6, c.alpha(c.a * 0.85));
            }
        }
    }
}

/// A Finder window whose sidebar is made of `kind`.
fn finder_sidebar(p: &Painter, r: Rect, kind: &str, theme: &Theme) {
    let dark = theme.dark;
    wallpaper(p, r, THUMB_RADIUS, false);
    let w = Rect::new(r.x + 9.0, r.y + 7.0, r.w - 18.0, r.h - 14.0);
    let sw = (w.w * 0.36).round();
    // Something bright behind the sidebar, for the material to show (or hide).
    p.clipped(r, || glow(p, w.x + sw * 0.5, w.cy(), r.h * 0.5, rgb(255, 214, 90)));
    p.fill_round(w.inset(-1.0, -1.0).offset(0.0, 1.5), 6.0, Color::gray(0, 0.18));
    p.clipped(Rect::new(w.x + sw, w.y, w.w - sw, w.h), || p.fill_round(w, 5.0, if dark { rgb(36, 36, 38) } else { Color::WHITE }));
    p.clipped(Rect::new(w.x, w.y, sw, w.h), || panel(p, w, 5.0, kind, dark));
    let c = ink(kind, dark);
    for (i, col) in [rgb(255, 95, 87), rgb(254, 188, 46), rgb(40, 200, 64)].into_iter().enumerate() {
        p.fill_circle(w.x + 5.0 + i as f32 * 4.5, w.y + 5.0, 1.5, col);
    }
    for i in 0..4 {
        let y = w.y + 12.0 + i as f32 * 7.5;
        p.fill_circle(w.x + 6.0, y + 1.5, 1.6, theme.accent);
        p.fill_round(Rect::new(w.x + 10.0, y, sw - 15.0, 3.0), 1.5, c.alpha(c.a * 0.6));
    }
    for i in 0..6 {
        let (col, row) = ((i % 3) as f32, (i / 3) as f32);
        p.fill_round(Rect::new(w.x + sw + 6.0 + col * 15.0, w.y + 8.0 + row * 17.0, 10.0, 8.0), 2.0, rgb(84, 168, 255));
    }
}

/// How a Finder window lists a folder.
fn finder_view(p: &Painter, r: Rect, view: &str, theme: &Theme) {
    let dark = theme.dark;
    p.fill_round(r, THUMB_RADIUS, if dark { rgb(40, 40, 42) } else { Color::WHITE });
    let line = if dark { Color::gray(255, 0.22) } else { Color::gray(0, 0.16) };
    let folder = rgb(84, 168, 255);
    match view {
        "icons" => {
            let cw = (r.w - 12.0) / 4.0;
            for i in 0..8 {
                let (cx, y) = (r.x + 6.0 + ((i % 4) as f32 + 0.5) * cw, r.y + 8.0 + (i / 4) as f32 * 25.0);
                p.fill_round(Rect::new(cx - 7.0, y, 14.0, 11.0), 2.5, folder);
                p.fill_round(Rect::new(cx - 6.0, y + 14.5, 12.0, 2.5), 1.25, line);
            }
        }
        "list" => {
            for (i, w) in [0.5, 0.38, 0.56, 0.32, 0.46].into_iter().enumerate() {
                let y = r.y + 7.0 + i as f32 * 10.0;
                if i % 2 == 1 {
                    p.fill_rect(Rect::new(r.x + 4.0, y - 1.5, r.w - 8.0, 10.0), line.alpha(line.a * 0.3));
                }
                p.fill_round(Rect::new(r.x + 8.0, y + 0.5, 6.0, 6.0), 1.5, folder);
                p.fill_round(Rect::new(r.x + 18.0, y + 2.0, r.w * w, 3.0), 1.5, line);
                p.fill_round(Rect::new(r.right() - 20.0, y + 2.0, 12.0, 3.0), 1.5, line.alpha(line.a * 0.6));
            }
        }
        "columns" => {
            let cw = r.w / 3.0;
            for col in 0..3 {
                let x = r.x + col as f32 * cw;
                if col > 0 {
                    p.line(x, r.y + 5.0, x, r.bottom() - 5.0, line.alpha(line.a * 0.7), 1.0);
                }
                for row in 0..(5 - col) {
                    let y = r.y + 7.0 + row as f32 * 10.0;
                    let picked = col < 2 && row == 1 + col;
                    if picked {
                        p.fill_round(Rect::new(x + 3.0, y - 1.5, cw - 6.0, 9.0), 2.5, theme.accent);
                    }
                    p.fill_round(Rect::new(x + 6.0, y + 0.5, 5.0, 5.0), 1.2, if picked { Color::WHITE } else { folder });
                    p.fill_round(Rect::new(x + 14.0, y + 1.5, cw - 22.0, 3.0), 1.5, if picked { Color::gray(255, 0.9) } else { line });
                }
            }
        }
        _ => {
            let big = Rect::new(r.cx() - 22.0, r.y + 6.0, 44.0, 30.0);
            p.fill_round_gradient(big, 3.5, rgb(96, 176, 255), rgb(142, 104, 238));
            let n = 6;
            let x0 = r.cx() - (n as f32 * 11.0 - 2.0) / 2.0;
            for i in 0..n {
                let t = Rect::new(x0 + i as f32 * 11.0, r.bottom() - 17.0, 9.0, 9.0);
                p.fill_round(t, 2.0, if i == 2 { rgb(96, 176, 255) } else { line });
                if i == 2 {
                    p.stroke_round(t.inset(-1.5, -1.5), 3.0, theme.accent, 1.2);
                }
            }
        }
    }
}

fn island_style(p: &Painter, r: Rect, notch: bool, dark: bool) {
    wallpaper(p, r, THUMB_RADIUS, false);
    let bar = top_bar(p, r, THUMB_RADIUS, 15.0, "acrylic", dark);
    bar_marks(p, bar, ink("acrylic", dark));
    let (w, cx) = (46.0, r.cx());
    let island = if notch {
        // Flat against the top edge, rounded below.
        let body = Rect::new(cx - w / 2.0, r.y, w, 17.0);
        p.clipped(body, || p.fill_round(Rect::new(body.x, body.y - 10.0, body.w, body.h + 10.0), 7.0, Color::BLACK));
        body
    } else {
        let body = Rect::new(cx - w / 2.0, r.y + 4.5, w, 13.0);
        p.fill_round(body, 6.5, Color::BLACK);
        body
    };
    let cy = island.bottom() - 6.5;
    p.fill_round(Rect::new(island.x + 5.0, cy - 3.0, 6.0, 6.0), 1.8, rgb(255, 110, 140));
    for (i, h) in [3.0, 6.0, 4.0].into_iter().enumerate() {
        p.fill_round(Rect::new(island.right() - 13.0 + i as f32 * 3.0, cy - h / 2.0, 1.6, h), 0.8, rgb(255, 150, 170));
    }
}

/// An open island made of `finish`.
fn island_finish(p: &Painter, r: Rect, finish: &str) {
    wallpaper(p, r, THUMB_RADIUS, false);
    let body = Rect::new(r.cx() - 37.0, r.y + 9.0, 74.0, r.h - 18.0);
    island_body(p, body, 13.0, finish);
    island_playing(p, body, 0.34);
}

fn island_body(p: &Painter, body: Rect, radius: f32, finish: &str) {
    match finish {
        "black" => p.fill_round(body, radius, Color::BLACK),
        "dynamic" => {
            p.fill_round_gradient(body, radius, Color::BLACK, Color::rgba(16, 16, 22, 0.3));
            p.stroke_round(body.inset(0.5, 0.5), radius, Color::gray(255, 0.18), 1.0);
        }
        "glass" => {
            p.fill_round(body, radius, Color::rgba(14, 14, 18, 0.36));
            p.stroke_round(body.inset(0.5, 0.5), radius, Color::gray(255, 0.5), 1.0);
        }
        _ => p.fill_round(body, radius, Color::rgba(26, 26, 30, 0.8)),
    }
}

/// Now Playing inside an island body, at `k` of full size (a 250 x 84 panel).
fn island_playing(p: &Painter, body: Rect, k: f32) {
    let at = |x: f32, y: f32, w: f32, h: f32| Rect::new(body.x + x * k, body.y + y * k, w * k, h * k);
    p.fill_round_gradient(at(16.0, 14.0, 38.0, 38.0), 9.0 * k, rgb(255, 104, 136), rgb(255, 172, 76));
    p.fill_round(at(64.0, 19.0, 92.0, 7.0), 3.5 * k, Color::gray(255, 0.95));
    p.fill_round(at(64.0, 33.0, 62.0, 6.0), 3.0 * k, Color::gray(255, 0.5));
    for (i, h) in [10.0, 20.0, 28.0, 14.0, 22.0].into_iter().enumerate() {
        p.fill_round(at(200.0 + i as f32 * 7.0, 33.0 - h / 2.0, 3.6, h), 1.8 * k, rgb(255, 140, 160));
    }
    p.fill_round(at(16.0, 64.0, 218.0, 4.0), 2.0 * k, Color::gray(255, 0.22));
    p.fill_round(at(16.0, 64.0, 92.0, 4.0), 2.0 * k, Color::gray(255, 0.92));
}

/// A window on its way to (or from) the Dock, the way `effect` moves it.
fn effect(p: &Painter, r: Rect, effect: &str, dark: bool) {
    wallpaper(p, r, THUMB_RADIUS, false);
    let dock = Rect::new(r.cx() - 24.0, r.bottom() - 11.0, 48.0, 7.0);
    panel(p, dock, 3.5, "acrylic", dark);
    let white = Color::gray(255, 0.95);
    let win = Rect::new(r.x + 17.0, r.y + 6.0, r.w - 34.0, 30.0);
    let (tx, ty) = (r.cx() + 7.0, dock.y);
    let lights = |x: f32, y: f32| {
        for (i, c) in [rgb(255, 95, 87), rgb(254, 188, 46), rgb(40, 200, 64)].into_iter().enumerate() {
            p.fill_circle(x + 4.5 + i as f32 * 4.2, y + 4.5, 1.4, c);
        }
    };
    match effect {
        "genie" => {
            let g = p.gfx.path(|s| unsafe {
                let bez = |c1: (f32, f32), c2: (f32, f32), end: (f32, f32)| {
                    s.AddBezier(&D2D1_BEZIER_SEGMENT { point1: pt(c1.0, c1.1), point2: pt(c2.0, c2.1), point3: pt(end.0, end.1) })
                };
                s.BeginFigure(pt(win.x, win.y), D2D1_FIGURE_BEGIN_FILLED);
                s.AddLine(pt(win.right(), win.y));
                bez((win.right(), win.y + 24.0), (tx + 3.5, ty - 20.0), (tx + 3.5, ty));
                s.AddLine(pt(tx - 3.5, ty));
                bez((tx - 3.5, ty - 20.0), (win.x, win.y + 24.0), (win.x, win.y));
                s.EndFigure(D2D1_FIGURE_END_CLOSED);
            });
            if let Some(g) = g {
                p.fill_geometry(&g, white);
            }
            lights(win.x, win.y);
        }
        "classic" => {
            // The same funnel, a row at a time.
            let n = 10;
            for j in 0..n {
                let t = j as f32 / (n - 1) as f32;
                let e = t * t * (3.0 - 2.0 * t);
                let (x0, x1) = (lerp(win.x, tx - 3.5, e), lerp(win.right(), tx + 3.5, e));
                let y = lerp(win.y, ty - 2.8, t);
                p.fill_round(Rect::new(x0, y, x1 - x0, 2.8), 1.2, white.alpha(lerp(0.95, 0.6, t)));
            }
        }
        "scale" => {
            for (k, t, a) in [(1.0, 0.0, 0.3), (0.62, 0.5, 0.55), (0.28, 1.0, 0.95)] {
                let (w, h) = (win.w * k, win.h * k);
                let cx = lerp(win.cx(), tx, t);
                let y = lerp(win.y, ty - h - 1.5, t);
                let rect = Rect::new(cx - w / 2.0, y, w, h);
                if k < 0.5 {
                    p.fill_round(rect, 2.0, white);
                } else {
                    p.stroke_round(rect, 3.0 * k + 1.0, Color::gray(255, a + 0.25), 1.3);
                    p.fill_round(rect, 3.0 * k + 1.0, Color::gray(255, a * 0.5));
                }
            }
        }
        "shatter" => {
            // Cracked from one point: wedges out to the edges, each broken in
            // two, the outer pieces flying furthest.
            let win = win.inset(7.0, 5.0);
            let (cx, cy) = (win.x + win.w * 0.42, win.y + win.h * 0.46);
            let mut edge: Vec<(f32, f32)> = Vec::new();
            edge.extend([0.0, 0.3, 0.64].map(|t| (win.x + win.w * t, win.y)));
            edge.extend([0.0, 0.55].map(|t| (win.right(), win.y + win.h * t)));
            edge.extend([0.0, 0.36, 0.7].map(|t| (win.right() - win.w * t, win.bottom())));
            edge.extend([0.0, 0.5].map(|t| (win.x, win.bottom() - win.h * t)));
            let mut shards = Vec::new();
            for i in 0..edge.len() {
                let (a, b) = (edge[i], edge[(i + 1) % edge.len()]);
                let split = 0.4 + (i % 3) as f32 * 0.09;
                let toward = |q: (f32, f32)| (cx + (q.0 - cx) * split, cy + (q.1 - cy) * split);
                let (mx, my) = ((a.0 + b.0) / 2.0 - cx, (a.1 + b.1) / 2.0 - cy);
                let len = (mx * mx + my * my).sqrt().max(1.0);
                let fly = |pts: &[(f32, f32)], d: f32| -> Vec<(f32, f32)> { pts.iter().map(|q| (q.0 + mx / len * d, q.1 + my / len * d)).collect() };
                shards.push(fly(&[(cx, cy), toward(a), toward(b)], 1.6));
                shards.push(fly(&[toward(a), a, b, toward(b)], 4.5 + (i % 2) as f32 * 2.5));
            }
            fill_shape(p, &shards, white);
        }
        "fragments" => {
            // A few big pieces, each off its own way.
            let c = (0.46, 0.5);
            let pieces: [(&[(f32, f32)], (f32, f32)); 5] = [
                (&[(0.0, 0.0), (0.52, 0.0), c, (0.0, 0.45)], (-5.0, -3.0)),
                (&[(0.52, 0.0), (1.0, 0.0), (1.0, 0.4), c], (5.0, -3.5)),
                (&[(1.0, 0.4), (1.0, 1.0), (0.62, 1.0), c], (6.0, 3.0)),
                (&[(0.62, 1.0), (0.16, 1.0), c], (0.5, 5.5)),
                (&[(0.16, 1.0), (0.0, 1.0), (0.0, 0.45), c], (-6.0, 3.5)),
            ];
            let figures: Vec<Vec<(f32, f32)>> = pieces
                .iter()
                .map(|(pts, (dx, dy))| pts.iter().map(|q| (win.x + q.0 * win.w + dx, win.y + q.1 * win.h + dy)).collect())
                .collect();
            fill_shape(p, &figures, white);
        }
        _ => {
            // Windows' own: the window simply scales and fades.
            let w = Rect::new(win.x + 8.0, win.y + 3.0, win.w - 16.0, win.h - 4.0);
            p.stroke_round(w.inset(-4.0, -3.0), 5.0, Color::gray(255, 0.45), 1.0);
            p.fill_round(w, 3.0, white);
            let (s, gap) = (5.0, 1.2);
            let (x0, y0) = (w.cx() - s - gap / 2.0, w.cy() - s - gap / 2.0);
            for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                p.fill_rect(Rect::new(x0 + dx * (s + gap), y0 + dy * (s + gap), s, s), rgb(0, 120, 212));
            }
        }
    }
}

/// Two displays, with the Dock (or menu bar) on the ones `mode` puts it on.
fn displays(p: &Painter, r: Rect, dock: bool, mode: &str, dark: bool) {
    surface(p, r, dark);
    let main = Rect::new(r.x + r.w * 0.1, r.y + 9.0, r.w * 0.46, 32.0);
    let second = Rect::new(main.right() + 6.0, r.y + 16.0, r.w * 0.3, 25.0);
    let stand = if dark { Color::gray(255, 0.3) } else { Color::gray(0, 0.26) };
    for m in [main, second] {
        p.fill_rect(Rect::new(m.cx() - 3.0, m.bottom(), 6.0, 4.0), stand);
        p.fill_round(Rect::new(m.cx() - 9.0, m.bottom() + 4.0, 18.0, 2.0), 1.0, stand);
        p.fill_round(m, 3.5, rgb(28, 28, 30));
        p.fill_round_gradient(m.inset(1.5, 1.5), 2.2, rgb(66, 138, 250), rgb(142, 104, 238));
    }
    let bar = |m: Rect, a: f32| {
        let screen = m.inset(1.5, 1.5);
        if dock {
            p.fill_round(Rect::new(m.cx() - m.w * 0.26, screen.bottom() - 6.5, m.w * 0.52, 4.5), 2.0, Color::gray(255, a));
        } else {
            p.clipped(Rect::new(screen.x, screen.y, screen.w, 4.0), || p.fill_round(screen, 2.2, Color::gray(255, a)));
        }
    };
    match mode {
        "all" => {
            bar(main, 0.95);
            // Menu bars away from the active window are dimmed.
            bar(second, if dock { 0.95 } else { 0.5 });
        }
        "follow" => {
            bar(main, 0.28);
            bar(second, 0.95);
            cursor(p, second.cx() + 2.0, second.cy() - 3.0, 0.72, 1.0);
        }
        _ => bar(main, 0.95),
    }
}

/// Launchpad full screen (Sequoia) or as the Apps panel (Tahoe).
fn launcher(p: &Painter, r: Rect, tahoe: bool, dark: bool) {
    wallpaper(p, r, THUMB_RADIUS, false);
    if tahoe {
        let pane = Rect::new(r.cx() - 35.0, r.y + 6.0, 70.0, r.h - 12.0);
        panel(p, pane, 8.0, "acrylic", dark);
        let c = ink("acrylic", dark);
        p.fill_round(Rect::new(pane.x + 6.0, pane.y + 5.0, pane.w - 12.0, 8.0), 4.0, c.alpha(0.13));
        p.stroke_circle(pane.x + 11.0, pane.y + 8.7, 1.7, c.alpha(0.6), 1.0);
        for i in 0..10 {
            app_icon(p, Rect::new(pane.x + 6.5 + (i % 5) as f32 * 12.0, pane.y + 18.0 + (i / 5) as f32 * 13.5, 9.0, 9.0), i, true);
        }
    } else {
        p.fill_round(r, THUMB_RADIUS, Color::gray(255, 0.2));
        let (s, gap) = (9.5, 6.0);
        let x0 = r.cx() - (5.0 * s + 4.0 * gap) / 2.0;
        for i in 0..15 {
            app_icon(p, Rect::new(x0 + (i % 5) as f32 * (s + gap), r.y + 6.0 + (i / 5) as f32 * (s + 5.0), s, s), i + i / 5, true);
        }
        for i in 0..3 {
            p.fill_circle(r.cx() - 5.0 + i as f32 * 5.0, r.bottom() - 5.5, 1.2, Color::gray(255, if i == 0 { 0.95 } else { 0.45 }));
        }
    }
}

// ---------------------------------------------------------------------------
// Page previews
// ---------------------------------------------------------------------------

/// Whether `page` opens with a live preview.
pub(super) fn has_preview(page: Page) -> bool {
    matches!(page, Page::Dock | Page::MenuBar | Page::Island)
}

/// The live preview heading `page`, filling `r`.
pub(super) fn preview(p: &Painter, page: Page, r: Rect, ctx: &RowCtx) {
    wallpaper(p, r, PREVIEW_RADIUS, false);
    p.clipped(r, || match page {
        Page::Dock => dock_preview(p, r, ctx),
        Page::MenuBar => menubar_preview(p, r, ctx),
        _ => island_preview(p, r, ctx),
    });
    p.stroke_round(r.inset(0.25, 0.25), PREVIEW_RADIUS, Color::gray(0, 0.14), 0.5);
}

/// "It's off", in the middle of a preview.
fn off_label(p: &Painter, r: Rect, text: &str, alpha: f32, theme: &Theme) {
    if alpha > 0.01 {
        let f = p.gfx.format(&theme.font, 12.0, Weight::SemiBold, Align::Center);
        let w = p.gfx.measure(text, &f) + 24.0;
        let pill = Rect::new(r.cx() - w / 2.0, r.cy() - 12.0, w, 24.0);
        p.fill_round(pill, 12.0, Color::gray(0, 0.28 * alpha));
        p.text(text, &f, pill, Color::gray(255, 0.95 * alpha));
    }
}

/// A plain menu bar across a preview's top, as the Menu Bar page has it set.
fn context_bar(p: &Painter, r: Rect, ctx: &RowCtx) -> f32 {
    let m = &ctx.cfg.menubar;
    if !m.enabled {
        return 0.0;
    }
    let kind = backdrop_name(m.backdrop);
    let bar = top_bar(p, r, PREVIEW_RADIUS, 13.0, kind, ctx.theme.dark);
    bar_marks(p, bar, ink(kind, ctx.theme.dark));
    bar.h
}

fn dock_preview(p: &Painter, r: Rect, ctx: &RowCtx) {
    let d = &ctx.cfg.dock;
    let dark = ctx.theme.dark;
    context_bar(p, r, ctx);
    let on = ctx.anim(Anim::Art(0), flag(d.enabled), feel::SMOOTH).clamp(0.0, 1.0);
    off_label(p, r, "The Dock is off", 1.0 - on, ctx.theme);
    if on < 0.01 {
        return;
    }
    let (size, spacing, offset) = (ctx.live(Key::DockSize), ctx.live(Key::DockSpacing) + 4.0, ctx.live(Key::DockOffset));
    let big = ctx.live(Key::DockMagSize).max(size);
    // A little over half real size, or less when that wouldn't fit: the
    // largest, widest-spaced Dock with everything in it still shows whole.
    let full_w = 8.0 * size + 2.2 * (big - size) + 7.0 * spacing + 0.28 * size + 9.0;
    let full_h = offset + 1.28 * size + (big - size) + 4.0;
    let k = 0.58_f32.min((r.w - 40.0) / full_w).min((r.h - 34.0) / full_h);
    let (tile, big, gap, margin) = (size * k, big * k, spacing * k, offset * k);
    let mag = ctx.anim(Anim::Art(1), flag(d.magnification), feel::SMOOTH).clamp(0.0, 1.0);
    let hide = ctx.anim(Anim::Art(2), flag(d.autohide), feel::SMOOTH).clamp(0.0, 1.0);
    let trash = ctx.anim(Anim::Art(3), flag(d.show_trash), feel::SMOOTH).clamp(0.0, 1.0);
    let running = ctx.anim(Anim::Art(4), flag(d.show_running), feel::SMOOTH).clamp(0.0, 1.0);
    let squircle = d.icon_style == IconStyle::Squircle;

    // The pointer rests on the middle icon, as when magnification shows.
    const SWELL: [f32; 7] = [0.0, 0.1, 0.5, 1.0, 0.5, 0.1, 0.0];
    let sizes: Vec<f32> = SWELL.iter().map(|w| tile + (big - tile) * mag * w).collect();
    let pad = tile * 0.14 + 2.0;
    let trash_w = (gap + 5.0 + tile) * trash;
    let w = sizes.iter().sum::<f32>() + gap * (sizes.len() - 1) as f32 + trash_w + 2.0 * pad;
    let h = tile + 2.0 * pad;
    // Auto-hide: mostly tucked under the bottom edge.
    let plate = Rect::new(r.cx() - w / 2.0, r.bottom() - 4.0 - margin - h + hide * (margin + h * 0.62), w, h);
    let radius = h * 0.3;
    let kind = backdrop_name(d.backdrop);
    let c = ink(kind, dark);
    p.faded(on * lerp(1.0, 0.6, hide), || {
        if d.shadow {
            for i in 1..=4 {
                let grow = i as f32 * 1.3;
                p.fill_round(plate.inset(-grow, -grow).offset(0.0, 2.5), radius + grow, Color::gray(0, 0.05));
            }
        }
        panel(p, plate, radius, kind, dark);
        if d.border {
            p.stroke_round(plate.inset(0.5, 0.5), radius, Color::gray(255, if dark { 0.26 } else { 0.55 }), 1.0);
        }
        let mut x = plate.x + pad;
        let base = plate.bottom() - pad;
        for (i, s) in sizes.iter().enumerate() {
            let icon = Rect::new(x, base - s, *s, *s);
            app_icon(p, icon, i, squircle);
            if running > 0.01 && matches!(i, 0 | 2 | 3 | 5) {
                p.fill_circle(icon.cx(), plate.bottom() - pad * 0.45, (tile * 0.045).max(0.9), c.alpha(c.a * 0.8 * running));
            }
            if i == 3 && mag > 0.01 {
                cursor(p, icon.cx() + s * 0.1, icon.cy() + s * 0.12, 0.9, mag);
            }
            x += s + gap;
        }
        if trash > 0.01 {
            p.faded(trash, || {
                p.line(x + 0.5, plate.y + pad, x + 0.5, plate.bottom() - pad, c.alpha(c.a * 0.3), 1.0);
                let can = Rect::new(x + 5.0, base - tile, tile, tile);
                let body = Rect::new(can.x + tile * 0.2, can.y + tile * 0.26, tile * 0.6, tile * 0.68);
                p.fill_round(body, tile * 0.12, Color::gray(255, 0.9));
                p.fill_round(Rect::new(can.x + tile * 0.12, can.y + tile * 0.12, tile * 0.76, tile * 0.11), tile * 0.05, Color::gray(255, 0.9));
                for f in [0.38, 0.5, 0.62] {
                    p.line(can.x + tile * f, body.y + tile * 0.14, can.x + tile * f, body.bottom() - tile * 0.12, Color::gray(0, 0.22), (tile * 0.04).max(0.6));
                }
            });
        }
    });
}

/// A window with a sidebar, as wide as a preview needs.
fn window(p: &Painter, w: Rect, theme: &Theme) {
    let (body, side, line) =
        if theme.dark { (rgb(38, 38, 40), rgb(52, 52, 56), Color::gray(255, 0.13)) } else { (Color::WHITE, rgb(238, 238, 242), Color::gray(0, 0.09)) };
    for i in 1..=4 {
        let grow = i as f32 * 1.5;
        p.fill_round(w.inset(-grow, -grow).offset(0.0, 3.0), 10.0 + grow, Color::gray(0, 0.045));
    }
    p.fill_round(w, 10.0, body);
    let sw = (w.w * 0.27).round();
    p.clipped(Rect::new(w.x, w.y, sw, w.h), || p.fill_round(w, 10.0, side));
    for (i, c) in [rgb(255, 95, 87), rgb(254, 188, 46), rgb(40, 200, 64)].into_iter().enumerate() {
        p.fill_circle(w.x + 14.0 + i as f32 * 12.0, w.y + 14.0, 4.0, c);
    }
    p.fill_round(Rect::new(w.x + 8.0, w.y + 30.0, sw - 16.0, 13.0), 4.0, theme.accent);
    for i in 1..6 {
        p.fill_round(Rect::new(w.x + 13.0, w.y + 34.0 + i as f32 * 15.0, sw - 34.0, 5.0), 2.5, line.alpha(line.a * 1.6));
    }
    let (cx, cw) = (w.x + sw + 16.0, w.w - sw - 32.0);
    p.fill_round(Rect::new(cx, w.y + 11.0, 84.0, 7.0), 3.5, line.alpha(line.a * 2.2));
    for i in 0..6 {
        p.fill_round(Rect::new(cx, w.y + 36.0 + i as f32 * 14.0, cw * (1.0 - 0.14 * (i % 3) as f32), 5.0), 2.5, line);
    }
}

fn menubar_preview(p: &Painter, r: Rect, ctx: &RowCtx) {
    let (m, theme) = (&ctx.cfg.menubar, ctx.theme);
    let on = ctx.anim(Anim::Art(0), flag(m.enabled), feel::SMOOTH).clamp(0.0, 1.0);
    let reserve = ctx.anim(Anim::Art(1), flag(m.reserve_space), feel::SMOOTH);
    // Drawn close to full size: this strip is the top of the screen.
    let bar_h = ctx.live(Key::MbHeight) * 0.85;
    // A window below the bar, or (when it isn't kept clear) running under it.
    let top = r.y + lerp(7.0, bar_h * on + 9.0, reserve);
    window(p, Rect::new(r.x + r.w * 0.19, top, r.w * 0.62, r.bottom() - top + 24.0), theme);
    off_label(p, r, "The menu bar is off", 1.0 - on, theme);
    if on < 0.01 {
        return;
    }
    let kind = backdrop_name(m.backdrop);
    p.faded(on, || {
        top_bar(p, r, PREVIEW_RADIUS, bar_h, kind, theme.dark);
        // The bar's own glyphs are drawn for a 26 pt bar: scale them to this one.
        let g = (bar_h / 26.0).clamp(0.72, 1.25);
        p.transformed(r.x, r.y, g, || bar_items(p, r.w / g, bar_h / g, ink(kind, theme.dark), ctx));
    });
}

/// The menu bar's contents in a `w` x `h` strip at the origin.
fn bar_items(p: &Painter, w: f32, h: f32, c: Color, ctx: &RowCtx) {
    let m = &ctx.cfg.menubar;
    let bold = p.gfx.format(&ctx.theme.font, 12.5, Weight::Bold, Align::Leading);
    let regular = p.gfx.format(&ctx.theme.font, 12.5, Weight::Regular, Align::Leading);
    let logo = Rect::new(10.0, 0.0, 20.0, h);
    match m.logo.to_lowercase().as_str() {
        "apple" => menubar::draw_apple(p, logo, c),
        "windows" => menubar::draw_windows_logo(p, logo, c),
        _ => p.fill_circle(logo.cx(), logo.cy(), 5.5, c),
    }
    let mut x = 42.0;
    for (i, name) in ["Finder", "File", "Edit", "View", "Go"].into_iter().enumerate() {
        let f = if i == 0 { &bold } else { &regular };
        let tw = p.gfx.measure(name, f);
        p.text(name, f, Rect::new(x, 0.0, tw + 4.0, h), c);
        x += tw + 17.0;
    }

    // Status items, from the right edge inwards. Each folds away as it's switched off.
    let clock = match CLOCKS.iter().find(|k| k.1 == m.clock_format).map(|k| k.0) {
        Some("24-hour") => "Fri Oct 17  09:41",
        Some(label) => label,
        None => "9:41 AM",
    };
    let tw = p.gfx.measure(clock, &regular);
    let mut x = w - 14.0 - tw;
    p.text(clock, &regular, Rect::new(x, 0.0, tw + 4.0, h), c);
    let pct_w = p.gfx.measure("87%", &regular) + 2.0;
    let mut item = |slot: u8, show: bool, width: f32, draw: &dyn Fn(Rect)| {
        let t = ctx.anim(Anim::Art(slot), flag(show), feel::SMOOTH).clamp(0.0, 1.0);
        x -= (width + 8.0) * t;
        if t > 0.01 {
            p.faded(t, || draw(Rect::new(x, 0.0, width, h)));
        }
    };
    item(2, m.show_control_center, 20.0, &|rc| menubar::draw_control_center(p, rc, c));
    item(3, m.show_search, 18.0, &|rc| menubar::draw_search(p, rc, c));
    item(4, m.show_wifi, 24.0, &|rc| menubar::draw_wifi(p, rc, c));
    item(5, m.show_battery, 28.0, &|rc| {
        menubar::draw_battery(p, Rect::new(rc.x, rc.y, rc.w + 8.0, rc.h), Battery { percent: 87, charging: false }, c)
    });
    item(6, m.show_battery && m.show_battery_percent, pct_w - 4.0, &|rc| p.text("87%", &regular, Rect::new(rc.x, rc.y, pct_w + 4.0, rc.h), c));
    item(7, m.show_tray, 14.0, &|rc| menubar::draw_tray(p, rc, c, 0.0));
}

fn island_preview(p: &Painter, r: Rect, ctx: &RowCtx) {
    let c = &ctx.cfg.island;
    context_bar(p, r, ctx);
    let on = ctx.anim(Anim::Art(0), flag(c.enabled), feel::SMOOTH).clamp(0.0, 1.0);
    off_label(p, r, "The Dynamic Island is off", 1.0 - on, ctx.theme);
    if on < 0.01 {
        return;
    }
    let notch = ctx.anim(Anim::Art(1), flag(c.style == IslandStyle::Notch), feel::SMOOTH).clamp(0.0, 1.0);
    let offset = ctx.live(Key::IslandOffset) * 0.6;
    let (w, h) = (250.0, 84.0);
    // A pill floats below the top edge; a notch hangs from it (its top corners out of sight).
    let floating = r.y + (7.0 + offset).max(2.0);
    let top = lerp(floating, r.y - 30.0, notch);
    let bottom = lerp(floating + h, r.y + h + offset.max(0.0), notch);
    let body = Rect::new(r.cx() - w / 2.0, top, w, bottom - top);
    let finish = match c.finish {
        IslandFinish::Black => "black",
        IslandFinish::Dynamic => "dynamic",
        IslandFinish::Glass => "glass",
        IslandFinish::Frosted => "frosted",
    };
    p.faded(on, || {
        island_body(p, body, lerp(30.0, 18.0, notch), finish);
        island_playing(p, Rect::new(body.x, body.bottom() - h, w, h), 1.0);
    });
}

// ---------------------------------------------------------------------------
// Widget gallery
// ---------------------------------------------------------------------------

/// A small picture of the `kind` widget, centred in `r`.
pub(super) fn widget(p: &Painter, kind: WidgetKind, r: Rect, theme: &Theme) {
    let s = 46.0;
    let w = Rect::new(r.cx() - s / 2.0, r.cy() - s / 2.0, s, s);
    let (bg, fg, dim) = if theme.dark {
        (rgb(58, 58, 62), Color::gray(255, 0.92), Color::gray(255, 0.26))
    } else {
        (Color::WHITE, Color::gray(0, 0.8), Color::gray(0, 0.15))
    };
    let (green, blue, orange, red) = (rgb(52, 199, 89), rgb(0, 122, 255), rgb(255, 149, 0), rgb(255, 59, 48));
    p.fill_round(w.inset(-1.0, -1.0).offset(0.0, 1.5), 11.0, Color::gray(0, 0.12));
    let plate = |top: Color, bottom: Color| p.fill_round_gradient(w, 10.0, top, bottom);
    let bar = |x: f32, y: f32, len: f32, h: f32, c: Color| p.fill_round(Rect::new(w.x + x, w.y + y, len, h), h / 2.0, c);
    let (cx, cy) = (w.cx(), w.cy());
    match kind {
        WidgetKind::Clock => {
            plate(bg, bg);
            p.stroke_circle(cx, cy, 15.0, fg, 1.6);
            p.line(cx, cy, cx, cy - 9.5, fg, 1.8);
            p.line(cx, cy, cx + 6.5, cy + 3.0, fg, 1.8);
            p.line(cx, cy, cx - 5.0, cy + 8.0, orange, 1.0);
            p.fill_circle(cx, cy, 1.6, fg);
        }
        WidgetKind::WorldClock => {
            plate(bg, bg);
            for (dx, hx, hy) in [(-10.0, 3.5, -3.5), (10.0, -4.0, 2.5)] {
                let (x, y) = (cx + dx, cy - 4.0);
                p.stroke_circle(x, y, 7.5, fg, 1.3);
                p.line(x, y, x, y - 4.5, fg, 1.3);
                p.line(x, y, x + hx, y + hy, fg, 1.3);
                p.fill_round(Rect::new(x - 5.0, cy + 9.0, 10.0, 2.5), 1.25, dim);
            }
        }
        WidgetKind::Weather => {
            plate(rgb(84, 168, 255), rgb(40, 112, 232));
            p.fill_circle(cx - 6.0, cy - 6.0, 8.0, rgb(255, 214, 10));
            for (dx, dy, q) in [(-2.0, 8.0, 5.5), (5.0, 4.5, 7.5), (11.5, 8.5, 5.0)] {
                p.fill_circle(cx + dx, cy + dy, q, Color::WHITE);
            }
            p.fill_round(Rect::new(cx - 4.0, cy + 8.0, 17.0, 5.5), 2.75, Color::WHITE);
        }
        WidgetKind::Calendar => {
            plate(bg, bg);
            let day = p.gfx.format(&theme.font, 7.5, Weight::Bold, Align::Center);
            let date = p.gfx.format(&theme.font, 21.0, Weight::Regular, Align::Center);
            p.text("FRI", &day, Rect::new(w.x, w.y + 5.0, w.w, 11.0), red);
            p.text("17", &date, Rect::new(w.x, w.y + 14.0, w.w, 27.0), fg);
        }
        WidgetKind::UpNext => {
            plate(bg, bg);
            for (i, c) in [red, blue].into_iter().enumerate() {
                let y = 9.0 + i as f32 * 15.5;
                bar(8.0, y, 3.0, 12.0, c);
                bar(15.0, y + 1.5, 22.0, 3.0, fg);
                bar(15.0, y + 7.5, 15.0, 2.5, dim);
            }
        }
        WidgetKind::Reminders => {
            plate(bg, bg);
            for (i, len) in [18.0, 22.0, 15.0].into_iter().enumerate() {
                let y = w.y + 12.0 + i as f32 * 11.0;
                if i == 0 {
                    p.fill_circle(w.x + 12.0, y, 3.6, orange);
                } else {
                    p.stroke_circle(w.x + 12.0, y, 3.1, dim.alpha(dim.a * 2.0), 1.2);
                }
                bar(20.0, y - w.y - 1.5, len, 3.0, if i == 0 { dim } else { fg });
            }
        }
        WidgetKind::Notes => {
            plate(rgb(255, 226, 110), rgb(255, 204, 60));
            for (i, len) in [26.0, 30.0, 20.0, 27.0].into_iter().enumerate() {
                bar(8.0, 10.0 + i as f32 * 7.5, len, 2.5, Color::gray(0, 0.42));
            }
        }
        WidgetKind::Notifications => {
            plate(bg, bg);
            for (i, c) in [green, blue].into_iter().enumerate() {
                let y = 7.0 + i as f32 * 17.0;
                p.fill_round(Rect::new(w.x + 6.0, w.y + y, 34.0, 14.0), 4.5, dim.alpha(dim.a * 0.6));
                p.fill_round(Rect::new(w.x + 9.0, w.y + y + 3.5, 7.0, 7.0), 2.0, c);
                bar(19.0, y + 3.5, 16.0, 2.5, fg);
                bar(19.0, y + 8.5, 11.0, 2.0, dim);
            }
        }
        WidgetKind::Batteries => {
            plate(bg, bg);
            for (dx, level, c) in [(-10.5, 0.75, green), (10.5, 0.35, rgb(255, 204, 0))] {
                let x = cx + dx;
                p.stroke_circle(x, cy, 7.5, dim, 2.4);
                // Filled from the bottom, as far as its charge.
                p.clipped(Rect::new(x - 10.0, cy + 9.0 - 18.0 * level, 20.0, 20.0), || p.stroke_circle(x, cy, 7.5, c, 2.4));
                p.fill_round(Rect::new(x - 2.2, cy - 3.2, 4.4, 6.4), 1.2, fg);
            }
        }
        WidgetKind::NowPlaying => {
            plate(rgb(96, 60, 120), rgb(44, 28, 66));
            p.fill_round_gradient(Rect::new(w.x + 8.0, w.y + 8.0, 16.0, 16.0), 4.0, rgb(255, 104, 136), rgb(255, 172, 76));
            bar(8.0, 29.0, 24.0, 3.0, Color::gray(255, 0.95));
            bar(8.0, 35.0, 16.0, 2.5, Color::gray(255, 0.5));
            fill_shape(p, &[vec![(w.right() - 15.0, w.y + 10.5), (w.right() - 7.5, w.y + 16.0), (w.right() - 15.0, w.y + 21.5)]], Color::gray(255, 0.95));
        }
        WidgetKind::MusicWave => {
            plate(rgb(36, 36, 40), rgb(22, 22, 24));
            for (i, h) in [8.0, 16.0, 26.0, 18.0, 30.0, 14.0, 22.0, 10.0].into_iter().enumerate() {
                let c = rgb(255, 80, 140).mix(rgb(150, 110, 255), i as f32 / 7.0);
                p.fill_round(Rect::new(w.x + 6.2 + i as f32 * 4.4, cy - h / 2.0, 2.8, h), 1.4, c);
            }
        }
        WidgetKind::Photo => {
            plate(rgb(120, 190, 255), rgb(196, 228, 255));
            p.fill_circle(w.right() - 13.0, w.y + 13.0, 5.0, rgb(255, 214, 10));
            let b = w.bottom() - 5.0;
            fill_shape(p, &[vec![(w.x + 5.0, b), (w.x + 17.0, w.y + 19.0), (w.x + 26.0, b - 6.0), (w.x + 32.0, w.y + 25.0), (w.right() - 5.0, b)]], rgb(52, 168, 83));
        }
        WidgetKind::Video => {
            plate(rgb(50, 50, 56), rgb(24, 24, 26));
            p.fill_circle(cx, cy, 12.0, Color::gray(255, 0.18));
            fill_shape(p, &[vec![(cx - 3.5, cy - 6.0), (cx + 6.0, cy), (cx - 3.5, cy + 6.0)]], Color::WHITE);
        }
        WidgetKind::System => {
            plate(bg, bg);
            for (i, (level, c)) in [(0.7, green), (0.45, blue), (0.85, orange)].into_iter().enumerate() {
                let y = 11.5 + i as f32 * 10.0;
                bar(8.0, y, 30.0, 4.0, dim);
                bar(8.0, y, 30.0 * level, 4.0, c);
            }
        }
        WidgetKind::User => {
            plate(bg, bg);
            p.fill_circle(cx, cy - 5.5, 7.0, theme.accent);
            p.clipped(Rect::new(cx - 13.0, cy + 4.0, 26.0, 11.0), || p.fill_circle(cx, cy + 17.5, 13.0, theme.accent));
        }
    }
}
