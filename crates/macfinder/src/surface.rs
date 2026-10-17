//! A surface is what every on-screen element (menu bar, Dock, menus) is made of:
//!
//! * an optional *backdrop* window: blurred, tinted and clipped to a rounded
//!   rect with Windows.UI.Composition (falling back to a DWM accent policy), and
//! * a *content* window (layered, per-pixel alpha) owned by, and therefore
//!   always above, the backdrop, which Direct2D renders into.
//!
//! Both are topmost, non-activating tool windows, so clicking them never steals
//! focus from the app the user is working in.

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Backdrop;
use crate::geom::{Color, Rect};
use crate::gfx::{Canvas, Gfx, Painter};
use crate::win::composition::CompositionBackdrop;
use crate::win::{accent, create_window};

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
}

/// "blur" is the same blurred backdrop with a much lighter tint.
fn effective_tint(kind: Backdrop, tint: Color) -> Color {
    match kind {
        Backdrop::Blur => tint.alpha(tint.a * 0.35),
        _ => tint,
    }
}

impl Surface {
    pub fn new(gfx: &Gfx, rect: RECT, scale: f32, kind: Backdrop, tint: Color) -> Result<Self> {
        let ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE;
        let tint = effective_tint(kind, tint);
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);

        let mut composition = None;
        let backdrop = if kind == Backdrop::None {
            None
        } else {
            let hwnd = create_window(ex | WS_EX_NOREDIRECTIONBITMAP, WS_POPUP, rect, HWND::default())?;
            accent::prepare(hwnd);
            match CompositionBackdrop::new(hwnd, tint) {
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
                    accent::apply(hwnd, Backdrop::Acrylic, tint);
                    Some(hwnd)
                }
            }
        };
        let content = create_window(ex | WS_EX_LAYERED, WS_POPUP, rect, backdrop.unwrap_or_default())?;
        accent::prepare(content);
        let canvas = Canvas::new(gfx, w, h, scale)?;
        Ok(Self { content, backdrop, composition, canvas, rect, scale, shape: None, shown: false, hidden: false })
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
                }
            }
            // Shaped backdrops are positioned relative to the surface; force re-apply.
            (Some(_), Some(_)) => self.shape = None,
            _ => {}
        }
        self.canvas.resize(w, h)
    }

    /// Fits the backdrop to a rounded rect given in surface-local points.
    ///
    /// The backdrop window is sized to the shape's bounds (so the rest of the
    /// surface stays click-through) and Composition rounds it at any radius.
    pub fn set_backdrop_shape(&mut self, r: Rect, radius: f32) {
        let Some(bd) = self.backdrop else { return };
        let s = self.scale;
        let key = [
            self.rect.left + (r.x * s).round() as i32,
            self.rect.top + (r.y * s).round() as i32,
            self.rect.left + (r.right() * s).round() as i32,
            self.rect.top + (r.bottom() * s).round() as i32,
            (radius * s).round() as i32,
        ];
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
    }

    pub fn render(&mut self, gfx: &Gfx, f: impl FnOnce(&Painter)) -> Result<()> {
        self.canvas.draw(gfx, f)?;
        self.canvas.present(self.content, self.rect.left, self.rect.top)?;
        if !self.shown && !self.hidden {
            self.shown = true;
            self.show_windows(true);
        }
        Ok(())
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

    fn show_windows(&self, show: bool) {
        let cmd = if show { SW_SHOWNOACTIVATE } else { SW_HIDE };
        unsafe {
            if let Some(bd) = self.backdrop {
                let _ = ShowWindow(bd, cmd);
            }
            let _ = ShowWindow(self.content, cmd);
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
        }
    }
}
