//! Docky's speech box: a small field that floats just under (or over) his
//! widget when you click him, for saying something to him. It rises and fades
//! in, and sinks and fades away; both are done by moving the window and
//! changing its opacity, so only typing repaints it.

use std::time::Instant;

use windows::core::Result;
use windows::Win32::Foundation::RECT;

use crate::anim::Spring;
use crate::config::Backdrop;
use crate::finder::text::TextField;
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Gfx, Weight};
use crate::surface::Surface;
use crate::theme::Theme;

/// The field: at least this wide (points), this tall, with room around it for its shadow.
const W_MIN: f32 = 250.0;
const FIELD_H: f32 = 38.0;
const MARGIN: f32 = 14.0;
/// Gap between the widget and the field.
const GAP: f32 = 8.0;
/// How far it rises as it appears (points).
const RISE: f32 = 10.0;
/// An empty box nobody's typing into or pointing at goes away after this long.
pub const IDLE_SECS: f32 = 8.0;

const BLUE: Color = Color::rgba(10, 132, 255, 1.0);
const PIXEL_BG: Color = Color::rgba(16, 16, 24, 0.97);
const PIXEL_INK: Color = Color::rgba(226, 229, 240, 1.0);

pub struct SpeechBox {
    pub surface: Surface,
    /// The Docky widget it belongs to.
    pub id: u32,
    pub field: TextField,
    appear: Spring,
    pub closing: bool,
    pub last_input: Instant,
    pub hover: bool,
    pub tracking: bool,
    pub dirty: bool,
    /// Drawn as a terminal line, for a pixel-art Docky.
    pixel: bool,
    /// Where the window sits once it's in (physical pixels).
    rest: (i32, i32),
    /// Below the widget (rising up into place) or above it (settling down).
    below: bool,
    send_at: Option<Rect>,
}

impl SpeechBox {
    /// Opens under widget body `anchor` (screen pixels), or over it when there's
    /// no room below on a screen ending at `screen_bottom`.
    pub fn open(gfx: &Gfx, anchor: RECT, screen_bottom: i32, scale: f32, id: u32, pixel: bool) -> Result<Self> {
        let s = scale;
        let anchor_w = (anchor.right - anchor.left) as f32 / s;
        let (w, h) = (anchor_w.max(W_MIN) + 2.0 * MARGIN, FIELD_H + 2.0 * MARGIN);
        let (wp, hp) = ((w * s).round() as i32, (h * s).round() as i32);
        let left = (anchor.left + anchor.right) / 2 - wp / 2;
        let mut top = anchor.bottom + ((GAP - MARGIN) * s).round() as i32;
        let below = top + hp <= screen_bottom;
        if !below {
            top = anchor.top - hp - ((GAP - MARGIN) * s).round() as i32;
        }
        let rect = RECT { left, top, right: left + wp, bottom: top + hp };
        let mut surface = Surface::with_layer(gfx, rect, s, Backdrop::None, Color::rgba(0, 0, 0, 0.0), false, crate::win::glass::Source::Wallpaper)?;
        surface.set_opacity(0.0);
        let mut appear = Spring::new(0.0, 0.26, 0.78);
        appear.set(1.0);
        let mut b = Self {
            surface,
            id,
            field: TextField::default(),
            appear,
            closing: false,
            last_input: Instant::now(),
            hover: false,
            tracking: false,
            dirty: true,
            pixel,
            rest: (left, top),
            below,
            send_at: None,
        };
        b.follow();
        Ok(b)
    }

    /// Moves and fades the window to where its appear spring has got to.
    fn follow(&mut self) {
        let v = self.appear.value;
        self.surface.set_opacity(v.clamp(0.0, 1.0));
        let off = ((1.0 - v.clamp(0.0, 1.2)) * RISE * self.surface.scale).round() as i32;
        let dy = if self.below { off } else { -off };
        self.surface.move_to(self.rest.0, self.rest.1 + dy);
    }

    /// Advances its rise or fall; true while moving.
    pub fn tick(&mut self, dt: f32) -> bool {
        let moving = self.appear.step(dt);
        if moving {
            self.follow();
        }
        moving
    }

    pub fn moving(&self) -> bool {
        !self.appear.settled()
    }

    /// Sinks and fades away (see [`Self::gone`]).
    pub fn close(&mut self) {
        if !self.closing {
            self.closing = true;
            self.appear = self.appear.with(0.18, 1.0);
            self.appear.set(0.0);
            self.surface.set_click_through();
        }
    }

    pub fn gone(&self) -> bool {
        self.closing && self.appear.value <= 0.02
    }

    /// Nobody's typing in it or pointing at it, and it's empty: time to go.
    pub fn idle(&self) -> bool {
        !self.closing && !self.hover && self.field.is_empty() && self.last_input.elapsed().as_secs_f32() >= IDLE_SECS
    }

    /// Whether (`x`, `y`) is on the send arrow.
    pub fn on_send(&self, x: f32, y: f32) -> bool {
        self.send_at.is_some_and(|r| r.contains(x, y))
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        let w = self.surface.width_pt() - 2.0 * MARGIN;
        let field = Rect::new(MARGIN, MARGIN, w, FIELD_H);
        let pixel = self.pixel;
        let text = self.field.string();
        let font = if pixel {
            let mono = if gfx.font_exists("Cascadia Mono") { "Cascadia Mono" } else { "Consolas" };
            gfx.format(mono, 12.5, Weight::Regular, Align::Leading)
        } else {
            gfx.format(&theme.font, 13.5, Weight::Regular, Align::Leading)
        };
        let caret_x = gfx.measure(&self.field.text[..self.field.caret].iter().collect::<String>(), &font);
        let (sa, sb) = self.field.range();
        let sel = (sa != sb).then(|| {
            let m = |n: usize| gfx.measure(&self.field.text[..n].iter().collect::<String>(), &font);
            (m(sa), m(sb))
        });
        let can_send = !text.trim().is_empty();
        let send = Rect::new(field.right() - 33.0, field.cy() - 14.0, 28.0, 28.0);
        self.send_at = can_send.then_some(send);
        let r = self.surface.render(gfx, |p| {
            let (bg, ink, dim) = if pixel {
                (PIXEL_BG, PIXEL_INK, PIXEL_INK.alpha(0.45))
            } else if theme.dark {
                (Color::rgba(44, 44, 46, 0.98), Color::WHITE, Color::gray(255, 0.45))
            } else {
                (Color::gray(255, 0.98), Color::rgba(24, 30, 64, 1.0), Color::rgba(24, 30, 64, 0.42))
            };
            let radius = if pixel { 9.0 } else { FIELD_H / 2.0 };
            p.soft_shadow(field, radius, 6, 1.5, 0.09, 3.0, Color::rgba(10, 20, 70, 1.0));
            p.fill_round(field, radius, bg);
            if pixel {
                p.stroke_round(field.inset(0.5, 0.5), radius, PIXEL_INK.alpha(0.55), 1.0);
            } else {
                p.stroke_round(field.inset(0.5, 0.5), radius, ink.alpha(0.1), 1.0);
            }
            let inner = Rect::new(field.x + 16.0, field.y, field.w - 16.0 - 40.0, field.h);
            // A long message slides left to keep the caret in view.
            let shift = (caret_x - (inner.w - 4.0)).max(0.0);
            p.clipped(inner, || {
                if text.is_empty() {
                    let hint = if pixel { "> say something to docky\u{2026}" } else { "say something to docky\u{2026}" };
                    p.text(hint, &font, inner, dim);
                } else {
                    if let Some((x0, x1)) = sel {
                        p.fill_rect(Rect::new(inner.x + x0 - shift, inner.cy() - 9.0, x1 - x0, 18.0), BLUE.alpha(0.3));
                    }
                    p.text(&text, &font, Rect::new(inner.x - shift, inner.y, inner.w + shift + 800.0, inner.h), ink);
                }
                let cx = inner.x + caret_x - shift + 0.5;
                p.line(cx, inner.cy() - 9.0, cx, inner.cy() + 9.0, if pixel { PIXEL_INK } else { BLUE }, 1.5);
            });
            // Send: an arrow in a circle, blue once there's something to say.
            let (sx, sy) = (send.cx(), send.cy());
            let fill = match (can_send, pixel) {
                (true, true) => PIXEL_INK,
                (true, false) => BLUE,
                (false, _) => ink.alpha(0.14),
            };
            p.fill_circle(sx, sy, 13.0, fill);
            let arrow = if pixel && can_send { PIXEL_BG } else { Color::WHITE };
            p.line(sx, sy + 5.5, sx, sy - 5.5, arrow, 1.9);
            p.line(sx - 5.0, sy - 0.5, sx, sy - 5.5, arrow, 1.9);
            p.line(sx + 5.0, sy - 0.5, sx, sy - 5.5, arrow, 1.9);
        });
        if let Err(e) = r {
            log!("docky speech box render failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the box (never shown), typed into, normal and pixel, to
    /// `DOCKSIDE_SPEECH_OUT`: `cargo test renders_speech_box -- --ignored`.
    #[test]
    #[ignore]
    fn renders_speech_box() {
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        let Ok(out) = std::env::var("DOCKSIDE_SPEECH_OUT") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let _ = crate::win::register_class(Some(proc_));
        let gfx = Gfx::new().unwrap();
        let theme = Theme::new(&crate::config::defaults(), &gfx);
        let anchor = RECT { left: -4000, top: -4000, right: -3830, bottom: -3830 };
        for (pixel, name) in [(false, "speech"), (true, "speech-pixel")] {
            let mut b = SpeechBox::open(&gfx, anchor, 0, 1.0, 1, pixel).unwrap();
            b.surface.set_hidden(true);
            b.field = TextField::new("what should i name my goldfish?");
            b.render(&gfx, &theme);
            let c = &b.surface.canvas;
            crate::gfx::write_png(&gfx, c.width as u32, c.height as u32, &c.pixels(), &format!("{out}/{name}.png"));
        }
    }
}
