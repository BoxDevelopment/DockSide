//! A surface is what every on-screen element (menu bar, Dock, menus) is made of:
//!
//! * an optional *backdrop* window: blurred, tinted and clipped to a rounded
//!   rect with Windows.UI.Composition (falling back to a DWM accent policy), and
//! * a *content* window (layered, per-pixel alpha) owned by, and therefore
//!   always above, the backdrop, which Direct2D renders into.
//!
//! A surface may also cast a soft drop shadow from a third, click-through
//! window under the backdrop (see [`Surface::set_shadow`]).
//!
//! Both are non-activating tool windows, so clicking them never steals focus
//! from the app the user is working in. Shell surfaces are topmost; desktop
//! widgets are not (they sit just above the wallpaper).

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Backdrop;
use crate::geom::{Color, Rect};
use crate::gfx::{Canvas, Gfx, Painter};
use crate::win::composition::CompositionBackdrop;
use crate::win::{accent, create_window, glass};

pub struct Surface {
    pub content: HWND,
    pub backdrop: Option<HWND>,
    composition: Option<CompositionBackdrop>,
    pub canvas: Canvas,
    /// Physical screen rect of the content window.
    pub rect: RECT,
    pub scale: f32,
    /// Screen bounds + radius of a shaped backdrop.
    shape: Option<[i32; 5]>,
    shown: bool,
    hidden: bool,
    shadow: Option<Shadow>,
    /// Whole-surface fade (0..1), for appearing and disappearing.
    opacity: f32,
    /// The opacity last pushed to the windows (lags `opacity` by imperceptible steps).
    applied: f32,
}

/// Something built on a [`Surface`] that can fade away after it's closed.
pub trait Fading {
    fn surface_mut(&mut self) -> &mut Surface;
}

/// Room around a shadowed shape for the shadow to fall into, in points.
const SHADOW_MARGIN: f32 = 26.0;

/// A drop shadow under a shaped surface, in a layered window that clicks
/// through, so the shadow never blocks the windows around the surface.
struct Shadow {
    hwnd: HWND,
    canvas: Canvas,
    /// Peak opacity of the ambient shadow.
    strength: f32,
    /// Screen bounds + radius of the shape to shadow, and what's on screen now.
    want: Option<[i32; 5]>,
    drawn: Option<[i32; 5]>,
}

/// One soft layer: rounded rects stacked outward from `body`, weighted so
/// opacity falls off smoothly (an eased curve, not a linear ramp) over `blur`.
fn soft_shadow(p: &Painter, body: Rect, radius: f32, blur: f32, dy: f32, alpha: f32) {
    const STEPS: usize = 28;
    let falloff = |d: f32| {
        let t = (d / blur).clamp(0.0, 1.0);
        let s = 1.0 - t * t * (3.0 - 2.0 * t);
        s * s
    };
    // Tucked in slightly, so the shadow reads as under the shape, not a halo around it.
    let spread = blur * 0.2;
    let base = body.inset(spread, spread).offset(0.0, dy);
    for k in (0..STEPS).rev() {
        let (g0, g1) = (blur * k as f32 / STEPS as f32, blur * (k + 1) as f32 / STEPS as f32);
        let a = alpha * (falloff(g0) - falloff(g1));
        p.fill_round(base.inset(-g0, -g0), (radius - spread + g0).max(0.0), Color::gray(0, a));
    }
}

/// "blur" is the same blurred backdrop with a much lighter tint; "glass" is
/// lighter still, leaving the sheen and rim to define the surface.
fn effective_tint(kind: Backdrop, tint: Color) -> Color {
    match kind {
        Backdrop::Blur => tint.alpha(tint.a * 0.35),
        Backdrop::Glass => tint.alpha(tint.a * 0.25),
        _ => tint,
    }
}

impl Surface {
    pub fn new(gfx: &Gfx, rect: RECT, scale: f32, kind: Backdrop, tint: Color) -> Result<Self> {
        Self::with_layer(gfx, rect, scale, kind, tint, true, glass::Source::Screen)
    }

    /// `topmost: false` makes an ordinary-band surface (desktop widgets).
    /// `source` is what glass refracts: `Wallpaper` for surfaces that only sit
    /// over the desktop, which keeps them in screenshots and recordings.
    pub fn with_layer(gfx: &Gfx, rect: RECT, scale: f32, kind: Backdrop, tint: Color, topmost: bool, source: glass::Source) -> Result<Self> {
        let ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | if topmost { WS_EX_TOPMOST } else { WINDOW_EX_STYLE(0) };
        let tint = effective_tint(kind, tint);
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);

        let mut composition = None;
        let backdrop = if matches!(kind, Backdrop::None | Backdrop::Clear) {
            None
        } else {
            let hwnd = create_window(ex | WS_EX_NOREDIRECTIONBITMAP, WS_POPUP, rect, HWND::default())?;
            accent::prepare(hwnd);
            match CompositionBackdrop::new(hwnd, tint, (kind == Backdrop::Glass).then_some(glass::Style::Bold), source) {
                Ok(c) => {
                    c.set_shape(0.0, 0.0, w as f32, h as f32, 0.0);
                    composition = Some(c);
                    Some(hwnd)
                }
                Err(e) => {
                    log!("composition backdrop failed ({e}); using DWM accent");
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    let hwnd = create_window(ex, WS_POPUP, rect, HWND::default())?;
                    accent::prepare(hwnd);
                    accent::apply(hwnd, if kind == Backdrop::Glass { Backdrop::Blur } else { Backdrop::Acrylic }, tint);
                    Some(hwnd)
                }
            }
        };
        let content = create_window(ex | WS_EX_LAYERED, WS_POPUP, rect, backdrop.unwrap_or_default())?;
        accent::prepare(content);
        if composition.as_ref().is_some_and(|c| c.hidden_from_capture()) {
            // Glass refracting the screen mustn't see its own content.
            glass::exclude_from_capture(content, true);
        }
        let canvas = Canvas::new(gfx, w, h, scale)?;
        Ok(Self { content, backdrop, composition, canvas, rect, scale, shape: None, shown: false, hidden: false, shadow: None, opacity: 1.0, applied: 1.0 })
    }

    /// Casts a soft drop shadow under the backdrop shape. `strength` is the
    /// ambient shadow's peak opacity.
    pub fn set_shadow(&mut self, gfx: &Gfx, strength: f32) -> Result<()> {
        let owned = self.backdrop.unwrap_or(self.content);
        let topmost = unsafe { GetWindowLongW(owned, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0;
        let ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT | WINDOW_EX_STYLE(topmost);
        let hwnd = create_window(ex, WS_POPUP, RECT { left: 0, top: 0, right: 1, bottom: 1 }, HWND::default())?;
        accent::prepare(hwnd);
        if self.composition.as_ref().is_some_and(|c| c.hidden_from_capture()) {
            glass::exclude_from_capture(hwnd, true);
        }
        unsafe {
            // Owned windows stay above their owner, so the surface always covers its shadow.
            SetWindowLongPtrW(owned, GWLP_HWNDPARENT, hwnd.0 as isize);
            let _ = SetWindowPos(hwnd, owned, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            if self.shown && !self.hidden {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
        }
        let canvas = Canvas::new(gfx, 1, 1, self.scale)?;
        self.shadow = Some(Shadow { hwnd, canvas, strength, want: None, drawn: None });
        Ok(())
    }

    /// Redraws the shadow if its shape changed since it was last drawn.
    fn update_shadow(&mut self, gfx: &Gfx) {
        let (s, bottom, opacity) = (self.scale, self.rect.bottom, self.opacity);
        let Some(sh) = &mut self.shadow else { return };
        let Some(key) = sh.want.filter(|k| sh.drawn != Some(*k)) else { return };
        sh.drawn = Some(key);
        let m = (SHADOW_MARGIN * s).ceil() as i32;
        let (left, top) = (key[0] - m, key[1] - m);
        let w = key[2] - key[0] + 2 * m;
        // Clipped to the surface, like the backdrop, so a Dock sliding away
        // doesn't cast onto a monitor below.
        let visible = (key[3] + m).min(bottom) - top;
        let h = visible.max(1);
        let body = Rect::new(m as f32 / s, m as f32 / s, (key[2] - key[0]) as f32 / s, (key[3] - key[1]) as f32 / s);
        let (radius, strength) = (key[4] as f32 / s, sh.strength);
        let r = sh.canvas.resize(w, h).and_then(|_| {
            sh.canvas.draw(gfx, |p| {
                if visible > 0 {
                    soft_shadow(p, body, radius, SHADOW_MARGIN * 0.75, 6.0, strength);
                    // A tight contact shadow that defines the edge.
                    soft_shadow(p, body, radius, 3.0, 1.0, strength * 0.45);
                }
            })
        });
        if let Err(e) = r.and_then(|_| sh.canvas.present_alpha(sh.hwnd, left, top, opacity)) {
            log!("shadow render failed: {e}");
        }
    }

    pub fn width_pt(&self) -> f32 {
        (self.rect.right - self.rect.left) as f32 / self.scale
    }

    pub fn height_pt(&self) -> f32 {
        (self.rect.bottom - self.rect.top) as f32 / self.scale
    }

    pub fn set_rect(&mut self, rect: RECT) -> Result<()> {
        if rect == self.rect {
            return Ok(());
        }
        self.rect = rect;
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        match (self.backdrop, self.shape) {
            (Some(bd), None) => {
                unsafe { SetWindowPos(bd, None, rect.left, rect.top, w, h, SWP_NOACTIVATE | SWP_NOZORDER)? };
                if let Some(c) = &self.composition {
                    c.set_shape(0.0, 0.0, w as f32, h as f32, 0.0);
                    c.refresh();
                }
            }
            // Shaped backdrops are positioned relative to the surface; force re-apply.
            (Some(_), Some(_)) => self.shape = None,
            _ => {}
        }
        self.canvas.resize(w, h)
    }

    /// Moves the surface (and a shaped backdrop with it) without redrawing.
    pub fn move_to(&mut self, left: i32, top: i32) {
        let (dx, dy) = (left - self.rect.left, top - self.rect.top);
        if dx == 0 && dy == 0 {
            return;
        }
        if let Some(sh) = &mut self.shadow {
            for k in [&mut sh.want, &mut sh.drawn].into_iter().flatten() {
                k[0] += dx;
                k[1] += dy;
                k[2] += dx;
                k[3] += dy;
            }
            let m = (SHADOW_MARGIN * self.scale).ceil() as i32;
            if let Some(k) = sh.drawn {
                unsafe {
                    let _ = SetWindowPos(sh.hwnd, None, k[0] - m, k[1] - m, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
        }
        self.rect = RECT { left, top, right: self.rect.right + dx, bottom: self.rect.bottom + dy };
        let flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
        unsafe {
            let _ = SetWindowPos(self.content, None, left, top, 0, 0, flags);
            if let Some(bd) = self.backdrop {
                let (x, y) = match &mut self.shape {
                    Some(k) => {
                        k[0] += dx;
                        k[1] += dy;
                        k[2] += dx;
                        k[3] += dy;
                        (k[0], k[1])
                    }
                    None => (left, top),
                };
                let _ = SetWindowPos(bd, None, x, y, 0, 0, flags);
            }
        }
        if let Some(c) = &self.composition {
            c.refresh();
        }
    }

    /// The window that carries the surface's z-order (the rest are owned by it).
    pub fn root(&self) -> HWND {
        self.shadow.as_ref().map(|s| s.hwnd).or(self.backdrop).unwrap_or(self.content)
    }

    /// Fits the backdrop to a rounded rect given in surface-local points.
    ///
    /// The backdrop window is sized to the shape's bounds (so the rest of the
    /// surface stays click-through) and Composition rounds it at any radius.
    pub fn set_backdrop_shape(&mut self, r: Rect, radius: f32) {
        let s = self.scale;
        let key = [
            self.rect.left + (r.x * s).round() as i32,
            self.rect.top + (r.y * s).round() as i32,
            self.rect.left + (r.right() * s).round() as i32,
            self.rect.top + (r.bottom() * s).round() as i32,
            (radius * s).round() as i32,
        ];
        if let Some(sh) = &mut self.shadow {
            sh.want = Some(key);
        }
        let Some(bd) = self.backdrop else { return };
        if self.shape == Some(key) {
            return;
        }
        let first = self.shape.is_none();
        self.shape = Some(key);
        let (w, h) = (key[2] - key[0], key[3] - key[1]);
        match &self.composition {
            Some(c) => c.set_shape(0.0, 0.0, w as f32, h as f32, key[4] as f32),
            // Accent fallback can only get Windows 11's fixed 8px rounding.
            None if first => accent::round_corners(bd),
            None => {}
        }
        // The window is clipped to the surface, so a shape sliding past the
        // bottom edge (Dock auto-hide) is cut off rather than re-rounded.
        let visible_h = (self.rect.bottom.min(key[3]) - key[1]).max(0);
        unsafe {
            let _ = SetWindowPos(bd, None, key[0], key[1], w, visible_h, SWP_NOACTIVATE | SWP_NOZORDER);
        }
        if let Some(c) = &self.composition {
            c.refresh();
        }
    }

    pub fn render(&mut self, gfx: &Gfx, f: impl FnOnce(&Painter)) -> Result<()> {
        self.render_part(gfx, None, f)
    }

    /// Like [`Self::render`], repainting only `part` (points) when given: for
    /// when just one area is animating. See [`Canvas::draw_part`].
    pub fn render_part(&mut self, gfx: &Gfx, part: Option<Rect>, f: impl FnOnce(&Painter)) -> Result<()> {
        let part = part.filter(|_| self.shown).and_then(|r| self.canvas.part_px(r));
        self.canvas.draw_part(gfx, part, f)?;
        self.canvas.present_part(self.content, self.rect.left, self.rect.top, self.opacity, part)?;
        self.update_shadow(gfx);
        if !self.shown && !self.hidden {
            self.shown = true;
            self.show_windows(true);
        }
        Ok(())
    }

    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// Fades the content, backdrop and shadow together, without redrawing.
    pub fn set_opacity(&mut self, opacity: f32) {
        let opacity = opacity.clamp(0.0, 1.0);
        // Always keep the new value: fades read it back for their next step, and
        // dropping small steps would stall them just short of zero (still there,
        // invisible, swallowing clicks).
        self.opacity = opacity;
        // Skip imperceptible steps, but always land exactly on the ends.
        let end = opacity == 0.0 || opacity == 1.0;
        if opacity == self.applied || (!end && (opacity - self.applied).abs() < 0.002) {
            return;
        }
        self.applied = opacity;
        if let Some(c) = &self.composition {
            c.set_opacity(opacity);
        }
        if !self.shown {
            // Applied by the first render.
            return;
        }
        crate::gfx::set_window_alpha(self.content, opacity);
        if let Some(sh) = self.shadow.as_ref().filter(|sh| sh.drawn.is_some()) {
            crate::gfx::set_window_alpha(sh.hwnd, opacity);
        }
        if !self.hidden {
            self.show_accent_backdrop();
        }
    }

    /// A DWM accent backdrop can't fade, so it shows only while the surface is mostly opaque.
    fn show_accent_backdrop(&self) {
        let Some(bd) = self.backdrop.filter(|_| self.composition.is_none()) else { return };
        let show = self.opacity > 0.4;
        unsafe {
            if IsWindowVisible(bd).as_bool() != show {
                let _ = ShowWindow(bd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
    }

    /// Lets clicks fall through to whatever is underneath (a closing surface
    /// that's still fading out mustn't swallow them).
    pub fn set_click_through(&self) {
        unsafe {
            let ex = GetWindowLongW(self.content, GWL_EXSTYLE);
            SetWindowLongW(self.content, GWL_EXSTYLE, ex | WS_EX_TRANSPARENT.0 as i32);
        }
    }

    /// Hide for fullscreen apps; `false` brings the surface back.
    pub fn set_hidden(&mut self, hidden: bool) {
        if self.hidden == hidden {
            return;
        }
        self.hidden = hidden;
        if self.shown {
            self.show_windows(!hidden);
        }
    }

    /// Puts the surface back at the top of the topmost band. Windows can lift a
    /// newly focused, maximized window above topmost windows, so this is re-asserted.
    pub fn raise(&self) {
        // The other windows are owned by the root, so they come along on top.
        let hwnd = self.root();
        unsafe {
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    fn show_windows(&self, show: bool) {
        let cmd = if show { SW_SHOWNOACTIVATE } else { SW_HIDE };
        unsafe {
            if let Some(sh) = &self.shadow {
                let _ = ShowWindow(sh.hwnd, cmd);
            }
            if let Some(bd) = self.backdrop {
                let faded_out = self.composition.is_none() && self.opacity <= 0.4;
                let _ = ShowWindow(bd, if faded_out { SW_HIDE } else { cmd });
            }
            let _ = ShowWindow(self.content, cmd);
        }
        // Glass stops capturing behind hidden panes; showing again starts it back up.
        if let Some(c) = self.composition.as_ref().filter(|_| show) {
            c.refresh();
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.composition = None;
        unsafe {
            let _ = DestroyWindow(self.content);
            if let Some(bd) = self.backdrop {
                let _ = DestroyWindow(bd);
            }
            if let Some(sh) = &self.shadow {
                let _ = DestroyWindow(sh.hwnd);
            }
        }
    }
}
