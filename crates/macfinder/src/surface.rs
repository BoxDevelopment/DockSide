//! A surface is what every on-screen element (menu bar, Dock, menus) is made of:
//!
//! * an optional *backdrop* window with a DWM acrylic/blur effect, clipped to a
//!   rounded region, and
//! * a *content* window (layered, per-pixel alpha) owned by â€” and therefore
//!   always above â€” the backdrop, which Direct2D renders into.
//!
//! Both are topmost, non-activating tool windows, so clicking them never steals
//! focus from the app the user is working in.

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Backdrop;
use crate::geom::{Color, Rect};
use crate::gfx::{Canvas, Gfx, Painter};
use crate::win::{accent, create_window};

pub struct Surface {
    pub content: HWND,
    pub backdrop: Option<HWND>,
    pub canvas: Canvas,
    /// Physical screen rect of both windows.
    pub rect: RECT,
    pub scale: f32,
    region: Option<[i32; 5]>,
    shown: bool,
    hidden: bool,
}

impl Surface {
    pub fn new(gfx: &Gfx, rect: RECT, scale: f32, backdrop: Backdrop, tint: Color) -> Result<Self> {
        let ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE;
        let bd = if backdrop != Backdrop::None {
            let h = create_window(ex, WS_POPUP, rect, HWND::default())?;
            accent::prepare(h);
            accent::apply(h, backdrop, tint);
            Some(h)
        } else {
            None
        };
        let content = create_window(ex | WS_EX_LAYERED, WS_POPUP, rect, bd.unwrap_or_default())?;
        accent::prepare(content);
        let canvas = Canvas::new(gfx, rect.right - rect.left, rect.bottom - rect.top, scale)?;
        Ok(Self { content, backdrop: bd, canvas, rect, scale, region: None, shown: false, hidden: false })
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
        match (self.backdrop, self.region) {
            (Some(bd), None) => unsafe {
                SetWindowPos(bd, None, rect.left, rect.top, w, h, SWP_NOACTIVATE | SWP_NOZORDER)?;
            },
            // Shaped backdrops are positioned relative to the surface; re-apply.
            (Some(_), Some(_)) => self.region = None,
            _ => {}
        }
        self.canvas.resize(w, h)
    }

    /// Shrinks the backdrop to a rounded rect given in surface-local points.
    ///
    /// DWM paints accent effects over a window's *full* bounds and ignores
    /// window regions, so the backdrop window itself must match the shape.
    /// Its corners use Windows 11's native (antialiased) rounding.
    pub fn set_backdrop_shape(&mut self, r: Rect, radius: f32) {
        let Some(bd) = self.backdrop else { return };
        let s = self.scale;
        let key = [
            self.rect.left + (r.x * s).round() as i32,
            self.rect.top + (r.y * s).round() as i32,
            self.rect.left + (r.right() * s).round() as i32,
            self.rect.top + (r.bottom() * s).round() as i32,
            (radius * 2.0 * s).round() as i32,
        ];
        if self.region == Some(key) {
            return;
        }
        if self.region.is_none() {
            accent::round_corners(bd);
        }
        self.region = Some(key);
        // No window region: Windows 11 disables native corner rounding for regioned windows.
        unsafe {
            let _ = SetWindowPos(bd, None, key[0], key[1], key[2] - key[0], key[3] - key[1], SWP_NOACTIVATE | SWP_NOZORDER);
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
        unsafe {
            let _ = DestroyWindow(self.content);
            if let Some(bd) = self.backdrop {
                let _ = DestroyWindow(bd);
            }
        }
    }
}
