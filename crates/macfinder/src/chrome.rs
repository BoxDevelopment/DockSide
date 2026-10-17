//! Shared macOS window chrome for our app windows (Finder, System Settings):
//! soft shadow, a rounded body split into sidebar + content, and traffic lights.

use crate::geom::{Color, Rect};
use crate::gfx::Painter;
use crate::theme::Theme;

/// Margin around the body reserved for the drop shadow, in points.
pub const SHADOW: f32 = 30.0;
pub const RADIUS: f32 = 12.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Light {
    Close,
    Minimize,
    Zoom,
}

pub struct Palette {
    pub sidebar: Color,
    pub content: Color,
    pub divider: Color,
}

impl Palette {
    pub fn new(theme: &Theme) -> Self {
        if theme.dark {
            Palette {
                sidebar: Color::rgba(40, 40, 40, 1.0),
                content: Color::rgba(30, 30, 30, 1.0),
                divider: Color::gray(255, 0.08),
            }
        } else {
            Palette {
                sidebar: Color::rgba(232, 230, 233, 1.0),
                content: Color::rgba(250, 250, 250, 1.0),
                divider: Color::gray(0, 0.08),
            }
        }
    }
}

/// Shadow, body, sidebar split and hairline border.
pub fn window(p: &Painter, body: Rect, sidebar_w: f32, theme: &Theme, pal: &Palette) {
    for i in 0..14 {
        let grow = i as f32 * 1.8;
        let a = 0.035 * (14 - i) as f32 / 14.0;
        p.fill_round(body.inset(-grow, -grow).offset(0.0, 8.0), RADIUS + grow, theme.shadow.alpha(a));
    }
    p.clear_rect(body.inset(4.0, 4.0));
    p.fill_round(body, RADIUS, pal.sidebar);
    let content = Rect::new(body.x + sidebar_w, body.y, body.w - sidebar_w, body.h);
    p.fill_round(content, RADIUS, pal.content);
    p.fill_rect(Rect::new(content.x, body.y, RADIUS, body.h), pal.content);
    p.line(content.x, body.y, content.x, body.bottom(), pal.divider, 1.0);
    p.stroke_round(body.inset(0.25, 0.25), RADIUS, theme.menu_border, 0.5);
}

/// Draws close / minimize / zoom at the body's top-left. Glyphs appear while
/// the pointer is over the group, like macOS. Returns hit rects.
pub fn traffic_lights(p: &Painter, body: Rect, hovered: bool, zoom_enabled: bool) -> Vec<(Rect, Light)> {
    let mut hits = Vec::new();
    let glyph = Color::gray(0, 0.55);
    for (i, light) in [Light::Close, Light::Minimize, Light::Zoom].into_iter().enumerate() {
        let (cx, cy) = (body.x + 20.0 + i as f32 * 20.0, body.y + 20.0);
        let enabled = light != Light::Zoom || zoom_enabled;
        let color = match (light, enabled) {
            (_, false) => Color::gray(128, 0.35),
            (Light::Close, _) => Color::rgba(255, 95, 87, 1.0),
            (Light::Minimize, _) => Color::rgba(254, 188, 46, 1.0),
            (Light::Zoom, _) => Color::rgba(40, 200, 64, 1.0),
        };
        p.fill_circle(cx, cy, 6.0, color);
        p.stroke_circle(cx, cy, 6.0, Color::gray(0, 0.12), 0.5);
        if !enabled {
            continue;
        }
        if hovered {
            match light {
                Light::Close => {
                    p.line(cx - 2.5, cy - 2.5, cx + 2.5, cy + 2.5, glyph, 1.1);
                    p.line(cx - 2.5, cy + 2.5, cx + 2.5, cy - 2.5, glyph, 1.1);
                }
                Light::Minimize => p.line(cx - 3.0, cy, cx + 3.0, cy, glyph, 1.2),
                Light::Zoom => {
                    p.line(cx - 3.0, cy, cx + 3.0, cy, glyph, 1.2);
                    p.line(cx, cy - 3.0, cx, cy + 3.0, glyph, 1.2);
                }
            }
        }
        hits.push((Rect::new(cx - 8.0, cy - 8.0, 16.0, 16.0), light));
    }
    hits
}
