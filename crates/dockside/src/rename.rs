//! Rename…: a small floating panel for the name an app shows under in the
//! Dock, Launchpad and the menu bar.

use std::sync::atomic::AtomicIsize;

use windows::core::Result;
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

use crate::anim::Motion;
use crate::chrome::SHADOW;
use crate::config::Config;
use crate::finder::text::TextField;
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::icon_picker::IconTarget;
use crate::menu::Action;
use crate::surface::{Fading, Surface};
use crate::theme::Theme;
use crate::win::Monitor;

/// The panel's content window, so the window procedure can close it when it loses focus.
pub static RENAME_HWND: AtomicIsize = AtomicIsize::new(0);

const W: f32 = 380.0;
const TITLE_H: f32 = 44.0;
const FIELD_H: f32 = 34.0;
const FOOTER_H: f32 = 56.0;
const H: f32 = TITLE_H + FIELD_H + 8.0 + FOOTER_H;
const RADIUS: f32 = 14.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Button {
    Default,
    Cancel,
    Rename,
}

pub enum RenameEvent {
    None,
    Close,
    Run(Action),
}

pub struct RenamePanel {
    pub surface: Surface,
    target: IconTarget,
    /// The name it has now.
    current: String,
    field: TextField,
    hover: Option<Button>,
    hits: Vec<(Rect, Button)>,
    motion: Motion<Button>,
    pub tracking: bool,
    pub dirty: bool,
}

impl Fading for RenamePanel {
    fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }
}

impl RenamePanel {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: &Monitor, target: IconTarget, name: &str) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + (mon.bottom - mon.top - h) / 2;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let surface = Surface::new(gfx, rect, s, cfg.menubar.backdrop.for_panels(), theme.menu_tint)?;
        RENAME_HWND.store(surface.content.0 as isize, std::sync::atomic::Ordering::Relaxed);
        let mut field = TextField::new(name);
        field.select_all();
        Ok(Self {
            surface,
            target,
            current: name.to_string(),
            field,
            hover: None,
            hits: Vec::new(),
            motion: Motion::default(),
            tracking: false,
            dirty: true,
        })
    }

    pub fn tick(&mut self, dt: f32) -> bool {
        let moving = self.motion.tick(dt);
        if moving {
            self.dirty = true;
        }
        moving
    }

    pub fn animating(&self) -> bool {
        self.motion.moving()
    }

    /// Gives the panel keyboard focus (allowed: we're handling a click).
    pub fn focus(&self) {
        unsafe {
            let _ = SetForegroundWindow(self.surface.content);
        }
    }

    fn apply(&self) -> RenameEvent {
        let name = self.field.string().trim().to_string();
        if name.is_empty() {
            return RenameEvent::None;
        }
        if name == self.current {
            return RenameEvent::Close;
        }
        RenameEvent::Run(Action::SetAppName { target: self.target.clone(), name: Some(name) })
    }

    fn press(&mut self, b: Button) -> RenameEvent {
        match b {
            Button::Rename => self.apply(),
            Button::Cancel => RenameEvent::Close,
            Button::Default => RenameEvent::Run(Action::SetAppName { target: self.target.clone(), name: None }),
        }
    }

    pub fn on_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.field.insert(&c.to_string());
        self.dirty = true;
    }

    pub fn on_key(&mut self, vk: u16, ctrl: bool, shift: bool) -> RenameEvent {
        const ENTER: u16 = 0x0D;
        const ESC: u16 = 0x1B;
        self.dirty = true;
        match vk {
            ESC => RenameEvent::Close,
            ENTER => self.apply(),
            _ => {
                self.field.on_key(vk, ctrl, shift);
                RenameEvent::None
            }
        }
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        let hover = self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|&(_, b)| b);
        if hover != self.hover {
            self.hover = hover;
            self.dirty = true;
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if self.hover.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn on_click(&mut self, x: f32, y: f32) -> RenameEvent {
        match self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|&(_, b)| b) {
            Some(b) => self.press(b),
            None => RenameEvent::None,
        }
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        let body = Rect::new(SHADOW, SHADOW, W, H);
        self.surface.set_backdrop_shape(body, RADIUS);
        let title_font = gfx.format(&theme.font, 13.0, Weight::SemiBold, Align::Leading);
        let field_font = gfx.format(&theme.font, 15.0, Weight::Regular, Align::Leading);
        let regular = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Center);
        let has_backdrop = self.surface.backdrop.is_some();

        let text = self.field.string();
        let prefix_w = |n: usize| gfx.measure(&self.field.text[..n].iter().collect::<String>(), &field_font);
        let (a, b) = self.field.range();
        let (sel_x0, sel_x1, caret_x) = (prefix_w(a), prefix_w(b), prefix_w(self.field.caret));
        let can_rename = !text.trim().is_empty();
        let title = format!("Rename \u{201C}{}\u{201D}", self.current);
        let hover = self.hover;
        self.motion.sweep();
        let motion = &self.motion;
        let mut hits = Vec::new();

        let r = self.surface.render_part(gfx, None, |p: &Painter| {
            p.soft_shadow(body, RADIUS, 12, 2.0, 0.03, 6.0, theme.shadow);
            p.clear_rect(body.inset(3.0, 3.0));
            p.fill_round(body, RADIUS, if has_backdrop { theme.menu_tint.alpha(0.3) } else { theme.menu_tint.alpha(1.0) });
            p.stroke_round(body.inset(0.25, 0.25), RADIUS, theme.menu_border, 0.5);

            p.text(&title, &title_font, Rect::new(body.x + 18.0, body.y + 4.0, W - 36.0, TITLE_H - 4.0), theme.text);

            // The name field.
            let field = Rect::new(body.x + 16.0, body.y + TITLE_H, W - 32.0, FIELD_H);
            p.fill_round(field, 7.0, if theme.dark { Color::gray(255, 0.08) } else { Color::WHITE.alpha(0.85) });
            p.stroke_round(field.inset(0.5, 0.5), 7.0, theme.accent.alpha(0.8), 1.5);
            let inner = Rect::new(field.x + 10.0, field.y, field.w - 20.0, field.h);
            p.push_clip(inner);
            if text.is_empty() {
                p.text("Name", &field_font, inner, theme.text_disabled);
            } else if a != b {
                p.fill_rect(Rect::new(inner.x + sel_x0, inner.cy() - 10.0, sel_x1 - sel_x0, 20.0), theme.accent.alpha(0.35));
            }
            p.text(&text, &field_font, inner, theme.text);
            if a == b {
                let cx = inner.x + caret_x + 0.5;
                p.line(cx, inner.cy() - 9.0, cx, inner.cy() + 9.0, theme.accent, 1.5);
            }
            p.pop_clip();

            // Buttons.
            let fy = body.bottom() - FOOTER_H;
            let mut bx = body.right() - 16.0;
            for (label, button, w) in [("Rename", Button::Rename, 84.0), ("Cancel", Button::Cancel, 76.0), ("Use Default", Button::Default, 100.0)] {
                let r = Rect::new(bx - w, fy + (FOOTER_H - 28.0) / 2.0, w, 28.0);
                bx -= w + 8.0;
                let primary = button == Button::Rename;
                let enabled = !primary || can_rename;
                let t = motion.fade(button, hover == Some(button));
                let fill = if primary && enabled {
                    theme.accent
                } else if theme.dark {
                    Color::gray(255, 0.1 + 0.06 * t)
                } else {
                    Color::gray(0, 0.06 + 0.04 * t)
                };
                p.fill_round(r, 7.0, fill);
                let ink = if primary && enabled { Color::WHITE } else if enabled { theme.text } else { theme.text_disabled };
                p.text(label, &regular, r, ink);
                if enabled {
                    hits.push((r, button));
                }
            }
        });
        self.hits = hits;
        if let Err(e) = r {
            log!("rename panel render failed: {e}");
        }
    }
}

impl Drop for RenamePanel {
    fn drop(&mut self) {
        let me = self.surface.content.0 as isize;
        let _ = RENAME_HWND.compare_exchange(me, 0, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the panel (never shown) to `DOCKSIDE_RENAME_OUT`:
    /// `cargo test renders_rename -- --ignored`.
    #[test]
    #[ignore]
    fn renders_rename() {
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        let Ok(out) = std::env::var("DOCKSIDE_RENAME_OUT") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let _ = crate::win::register_class(Some(proc_));
        let gfx = Gfx::new().unwrap();
        let cfg = crate::config::defaults();
        let theme = Theme::new(&cfg, &gfx);
        let target = IconTarget { pinned: None, exe: Some("githubdesktop.exe".into()), trash: None };
        let mut p = RenamePanel::new(&gfx, &theme, &cfg, &Monitor::primary(), target, "Electron").unwrap();
        p.surface.set_hidden(true);
        p.render(&gfx, &theme);
        let c = &p.surface.canvas;
        crate::gfx::write_png(&gfx, c.width as u32, c.height as u32, &c.pixels(), &out);
    }
}
