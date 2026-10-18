//! Shared macOS window chrome for our app windows (Finder, System Settings):
//! soft shadow, a rounded body split into sidebar + content, traffic lights,
//! and an optional blurred (vibrant) sidebar backdrop.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::geom::{Color, Rect};
use crate::gfx::Painter;
use crate::theme::Theme;
use crate::win::composition::CompositionBackdrop;
use crate::win::{accent, create_window, system};

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
    if pal.sidebar.a < 1.0 {
        // A see-through sidebar must not show the shadow under the body.
        p.clear_round(body, RADIUS);
    } else {
        p.clear_rect(body.inset(4.0, 4.0));
    }
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

/// Sidebar vibrancy: a blurred Composition backdrop window kept directly below
/// an app window, showing through its translucent sidebar.
pub struct SidebarBackdrop {
    hwnd: HWND,
    composition: CompositionBackdrop,
    tint: Color,
    /// Last applied screen rect + radius, to skip redundant updates.
    placed: Option<[i32; 5]>,
}

impl SidebarBackdrop {
    /// Sidebar tint over the blur, like macOS's sidebar material.
    pub fn tint(theme: &Theme) -> Color {
        if theme.dark {
            Color::rgba(40, 40, 42, 0.62)
        } else {
            Color::rgba(234, 232, 236, 0.58)
        }
    }

    /// The near-transparent sidebar fill to paint over the backdrop: just enough
    /// alpha that the layered window still receives clicks there.
    pub fn fill() -> Color {
        Color::gray(128, 0.012)
    }

    /// `None` when Transparency effects are off or Composition is unavailable.
    pub fn new(theme: &Theme) -> Option<Self> {
        if !system::transparency_enabled() {
            return None;
        }
        let ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TRANSPARENT;
        let hwnd = create_window(ex, WS_POPUP, RECT::default(), HWND::default()).ok()?;
        accent::prepare(hwnd);
        let tint = Self::tint(theme);
        match CompositionBackdrop::new(hwnd, tint) {
            Ok(composition) => Some(Self { hwnd, composition, tint, placed: None }),
            Err(e) => {
                log!("sidebar backdrop unavailable: {e}");
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                None
            }
        }
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        let tint = Self::tint(theme);
        if tint != self.tint {
            self.tint = tint;
            self.composition.set_tint(tint);
        }
    }

    /// Covers `r` (owner-local points) with a rounded backdrop just below `owner`
    /// in z-order, or hides it while the owner is hidden or minimized.
    pub fn sync(&mut self, owner: HWND, r: Rect, radius: f32, scale: f32) {
        unsafe {
            if !IsWindowVisible(owner).as_bool() || IsIconic(owner).as_bool() {
                if self.placed.take().is_some() {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
                return;
            }
            let mut rc = RECT::default();
            let _ = GetWindowRect(owner, &mut rc);
            let x = rc.left + (r.x * scale).round() as i32;
            let y = rc.top + (r.y * scale).round() as i32;
            let w = (r.w * scale).round() as i32;
            let h = (r.h * scale).round() as i32;
            let key = [x, y, w, h, (radius * scale).round() as i32];
            if self.placed != Some(key) {
                self.composition.set_shape(0.0, 0.0, w as f32, h as f32, key[4] as f32);
            }
            self.placed = Some(key);
            // Re-applied every time: the owner may have moved up in z-order.
            let _ = SetWindowPos(self.hwnd, owner, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    }
}

impl Drop for SidebarBackdrop {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
