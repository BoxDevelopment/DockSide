//! Shared macOS window chrome for our app windows (Finder, System Settings):
//! soft shadow, a rounded body split into sidebar + content, traffic lights,
//! and an optional blurred (vibrant) or floating Liquid Glass sidebar backdrop.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{lerp, Spring};
use crate::config::Backdrop;
use crate::geom::{Color, Rect};
use crate::gfx::Painter;
use crate::theme::Theme;
use crate::win::composition::CompositionBackdrop;
use crate::win::{accent, create_window, glass, system};

/// Margin around the body reserved for the drop shadow, in points.
pub const SHADOW: f32 = 30.0;
pub const RADIUS: f32 = 18.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Light {
    Close,
    Minimize,
    Zoom,
}

/// How an app window is leaving the screen.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Exit {
    Close,
    Minimize,
}

/// Opening starts here (partly faded in) so the window shows up immediately.
const OPEN_FROM: f32 = 0.35;
const OPEN: (f32, f32) = (0.18, 1.0);

/// Open, close and minimize for our own app windows, like macOS: the window
/// fades while scaling about its centre, or shrinks away towards the Dock.
pub struct WindowAnim {
    appear: Spring,
    exit: Option<Exit>,
    /// Was minimized at the last render: zoom back in when restored.
    was_iconic: bool,
}

impl WindowAnim {
    /// Starts zooming in, for a window about to be shown. It's already visible
    /// on its first frame and settles in under a fifth of a second.
    pub fn opening() -> Self {
        let mut appear = Spring::new(OPEN_FROM, OPEN.0, OPEN.1);
        appear.set(1.0);
        Self { appear, exit: None, was_iconic: false }
    }

    /// Advances; returns true while moving. Minimizes `hwnd` once it has shrunk away.
    pub fn tick(&mut self, dt: f32, hwnd: HWND) -> bool {
        let moving = self.appear.step(dt);
        if self.exit == Some(Exit::Minimize) && self.appear.value <= 0.02 {
            self.exit = None;
            // Set here, not left to a render while minimized: the messages
            // ShowWindow sends arrive mid-frame, when the App can't take them.
            self.was_iconic = true;
            unsafe {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            }
            return false;
        }
        moving
    }

    pub fn moving(&self) -> bool {
        !self.appear.settled()
    }

    pub fn close(&mut self) {
        self.exit = Some(Exit::Close);
        self.appear = self.appear.with(0.14, 1.0);
        self.appear.set(0.0);
    }

    pub fn minimize(&mut self) {
        self.exit = Some(Exit::Minimize);
        self.appear = self.appear.with(0.2, 1.0);
        self.appear.set(0.0);
    }

    /// Faded all the way out after [`Self::close`]: the window can go.
    pub fn closed(&self) -> bool {
        self.exit == Some(Exit::Close) && self.appear.value <= 0.02
    }

    /// Back from minimized but not yet redrawn: it needs a frame to zoom in again.
    pub fn restored(&self, hwnd: HWND) -> bool {
        self.was_iconic && !unsafe { IsIconic(hwnd).as_bool() }
    }

    /// Call before drawing: false while `hwnd` is minimized (nothing to draw),
    /// and after a restore the window zooms back up.
    pub fn begin_render(&mut self, hwnd: HWND) -> bool {
        if unsafe { IsIconic(hwnd).as_bool() } {
            self.was_iconic = true;
            return false;
        }
        if std::mem::take(&mut self.was_iconic) && self.exit.is_none() {
            self.appear = self.appear.with(OPEN.0, OPEN.1);
            self.appear.snap(OPEN_FROM);
            self.appear.set(1.0);
        }
        true
    }

    /// Zoom origin (points), scale and opacity for a window whose body is `body`.
    pub fn transform(&self, body: Rect) -> (f32, f32, f32, f32) {
        let a = self.appear.value;
        let alpha = a.clamp(0.0, 1.0);
        match self.exit {
            // Down into its bottom edge, towards the Dock.
            Some(Exit::Minimize) => (body.cx(), body.bottom(), lerp(0.3, 1.0, a), alpha),
            Some(Exit::Close) => (body.cx(), body.cy(), lerp(0.97, 1.0, a), alpha),
            None => (body.cx(), body.cy(), lerp(0.96, 1.0, a), alpha),
        }
    }
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

/// The soft drop shadow under a window body.
pub fn shadow(p: &Painter, body: Rect, theme: &Theme) {
    p.soft_shadow(body, RADIUS, 14, 1.8, 0.035, 8.0, theme.shadow);
}

/// Shadow, body, sidebar split and hairline border.
pub fn window(p: &Painter, body: Rect, sidebar_w: f32, theme: &Theme, pal: &Palette) {
    shadow(p, body, theme);
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

/// Inset of the floating glass sidebar from the body's edges, and its corner radius.
pub const PANE_INSET: f32 = 8.0;
pub const PANE_RADIUS: f32 = 12.0;

/// The floating sidebar pane for a body whose sidebar column is `sidebar_w` wide.
pub fn sidebar_pane(body: Rect, sidebar_w: f32) -> Rect {
    Rect::new(body.x + PANE_INSET, body.y + PANE_INSET, sidebar_w - PANE_INSET - 2.0, body.h - 2.0 * PANE_INSET)
}

/// Shadow and body with a see-through floating sidebar pane (macOS Tahoe):
/// the window background runs edge to edge and the pane is cut out of it.
pub fn window_floating(p: &Painter, body: Rect, sidebar_w: f32, theme: &Theme, pal: &Palette) {
    shadow(p, body, theme);
    p.clear_rect(body.inset(4.0, 4.0));
    p.fill_round(body, RADIUS, pal.content);
    let pane = sidebar_pane(body, sidebar_w);
    p.clear_round(pane, PANE_RADIUS);
    p.fill_round(pane, PANE_RADIUS, SidebarBackdrop::fill());
    p.stroke_round(body.inset(0.25, 0.25), RADIUS, theme.menu_border, 0.5);
}

/// Draws close / minimize / zoom at the body's top-left. Glyphs fade in while
/// the pointer is over the group (`hover`, 0..1), like macOS. Returns hit rects.
pub fn traffic_lights(p: &Painter, body: Rect, hover: f32, zoom_enabled: bool) -> Vec<(Rect, Light)> {
    let mut hits = Vec::new();
    let glyph = Color::gray(0, 0.55 * hover);
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
        if hover > 0.01 {
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
    owner: HWND,
    composition: CompositionBackdrop,
    tint: Color,
    kind: Backdrop,
    /// Last applied screen rect + radius, to skip redundant updates.
    placed: Option<[i32; 5]>,
    opacity: f32,
}

impl SidebarBackdrop {
    /// Sidebar tint over the blur, like macOS's sidebar material.
    pub fn tint(theme: &Theme, kind: Backdrop) -> Color {
        if kind == Backdrop::Glass {
            // Mostly frosted white (or graphite), like Tahoe's sidebar: the desktop only tints it.
            return if theme.dark { Color::rgba(38, 38, 40, 0.74) } else { Color::rgba(250, 250, 252, 0.74) };
        }
        let acrylic = if theme.dark { Color::rgba(40, 40, 42, 0.62) } else { Color::rgba(234, 232, 236, 0.58) };
        // Blur: the same backdrop with a much lighter tint, as for bars.
        if kind == Backdrop::Blur { acrylic.alpha(acrylic.a * 0.35) } else { acrylic }
    }

    /// The near-transparent sidebar fill to paint over the backdrop: just enough
    /// alpha that the layered window still receives clicks there.
    pub fn fill() -> Color {
        Color::gray(128, 0.012)
    }

    /// A see-through backdrop for `owner`'s sidebar pane. `None` for a solid
    /// sidebar, when Composition is unavailable, or when Transparency effects
    /// are off and nothing would refract.
    pub fn new(theme: &Theme, owner: HWND, kind: Backdrop) -> Option<Self> {
        let glass = kind == Backdrop::Glass;
        if matches!(kind, Backdrop::None | Backdrop::Clear) || (!system::transparency_enabled() && !(glass && glass::enabled())) {
            return None;
        }
        let ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TRANSPARENT;
        let hwnd = create_window(ex, WS_POPUP, RECT::default(), HWND::default()).ok()?;
        accent::prepare(hwnd);
        let tint = Self::tint(theme, kind);
        match CompositionBackdrop::new(hwnd, tint, glass.then_some(glass::Style::Subtle), glass::Source::Screen) {
            Ok(composition) => {
                if composition.hidden_from_capture() {
                    // The pane shows through the owner, so the owner mustn't be captured either.
                    glass::exclude_from_capture(owner, true);
                }
                Some(Self { hwnd, owner, composition, tint, kind, placed: None, opacity: 1.0 })
            }
            Err(e) => {
                log!("sidebar backdrop unavailable: {e}");
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                None
            }
        }
    }

    /// Fades the backdrop along with its window.
    pub fn set_opacity(&mut self, opacity: f32) {
        if (opacity - self.opacity).abs() > 0.002 {
            self.opacity = opacity;
            self.composition.set_opacity(opacity);
        }
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        let tint = Self::tint(theme, self.kind);
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
        self.composition.refresh();
    }
}

impl Drop for SidebarBackdrop {
    fn drop(&mut self) {
        if self.composition.hidden_from_capture() {
            glass::exclude_from_capture(self.owner, false);
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
