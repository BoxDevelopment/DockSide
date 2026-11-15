//! Edit Photo…: a floating panel for a Photo widget. It shows the whole
//! picture with a frame in the widget's shape: drag the frame to choose what
//! shows, drag a corner (or scroll) to zoom. "Fit to Picture" sizes the
//! widget to the frame instead, which can then take any shape, with a slider
//! for how big the widget is.

use std::sync::atomic::AtomicIsize;

use windows::core::Result;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

use crate::anim::Motion;
use crate::chrome::SHADOW;
use crate::config::{Config, WidgetItem, WidgetSize, FIT_MIN};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconPixels;
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::surface::{Fading, Surface};
use crate::theme::Theme;
use crate::widgets::live::{self, SharedFeed};
use crate::widgets::{dims, WidgetCmd, RADIUS};
use crate::win::Monitor;

/// The panel's content window, so the window procedure can close it when it loses focus.
pub static PHOTO_EDIT_HWND: AtomicIsize = AtomicIsize::new(0);

const W: f32 = 640.0;
const HEADER: f32 = 46.0;
const PREVIEW_W: f32 = W - 40.0;
const PREVIEW_H: f32 = 360.0;
const ROW: f32 = 28.0;
const FOOTER: f32 = 56.0;
const H: f32 = HEADER + PREVIEW_H + 14.0 + ROW + 12.0 + ROW + 8.0 + FOOTER;
const PANEL_RADIUS: f32 = 14.0;
/// How close to a corner of the frame a press grabs it, in points.
const GRAB: f32 = 14.0;
/// Smallest frame, as a share of the picture's shorter side.
const MIN_CROP: f32 = 0.05;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Target {
    Size(WidgetSize),
    Fit,
    Reset,
    Cancel,
    Done,
}

#[derive(Clone, Copy, Debug)]
enum Drag {
    /// Moving the frame: where the press was (points) and the crop then.
    Move { from: (f32, f32), crop: [f32; 4] },
    /// Resizing from a corner, the opposite one staying put (normalized).
    Corner { anchor: (f32, f32) },
    Slider,
}

pub enum PhotoEditEvent {
    None,
    Close,
    Run(Action),
}

pub struct PhotoEditor {
    pub surface: Surface,
    id: u32,
    /// A slideshow: no Fit to Picture (each picture has its own shape).
    folder: bool,
    size: WidgetSize,
    fit: bool,
    /// Fit to Picture: the widget's width in points (its height follows the frame).
    fit_w: f32,
    /// Biggest a widget can be, in points (its display).
    max: (f32, f32),
    /// The part of the picture shown: [x, y, w, h], 0-1.
    crop: [f32; 4],
    /// What the widget had, applied once the picture's size is known.
    start: Option<(Option<[f32; 4]>, Option<(f32, f32)>)>,
    feed: SharedFeed,
    frame: Option<live::Frame>,
    image: Option<ID2D1Bitmap>,
    /// The picture's size in pixels (as loaded: its shape is what matters).
    px: Option<(u32, u32)>,
    error: Option<String>,
    drag: Option<Drag>,
    hover: Option<Target>,
    hits: Vec<(Rect, Target)>,
    motion: Motion<Target>,
    pub tracking: bool,
    pub dirty: bool,
}

impl Fading for PhotoEditor {
    fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }
}

fn body() -> Rect {
    Rect::new(SHADOW, SHADOW, W, H)
}

fn preview() -> Rect {
    let b = body();
    Rect::new(b.x + 20.0, b.y + HEADER, PREVIEW_W, PREVIEW_H)
}

fn seg_row() -> Rect {
    let p = preview();
    Rect::new(p.x, p.bottom() + 14.0, p.w, ROW)
}

fn slider_row() -> Rect {
    let s = seg_row();
    Rect::new(s.x, s.bottom() + 12.0, s.w, ROW)
}

fn track() -> Rect {
    let r = slider_row();
    Rect::new(r.x + 56.0, r.cy() - 2.0, r.w - 56.0 - 120.0, 4.0)
}

impl PhotoEditor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: &Monitor, item: &WidgetItem, max: (f32, f32), hwnd: isize, msg: u32) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + (mon.bottom - mon.top - h) / 2;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let surface = Surface::new(gfx, rect, s, cfg.menubar.backdrop.for_panels(), theme.menu_tint)?;
        PHOTO_EDIT_HWND.store(surface.content.0 as isize, std::sync::atomic::Ordering::Relaxed);
        let path = std::path::PathBuf::from(item.path.clone().unwrap_or_default());
        let folder = path.is_dir();
        let feed = SharedFeed::default();
        match live::first_picture(&path) {
            Some(file) => live::play_fitted(file, ((PREVIEW_W * s) as u32, (PREVIEW_H * s) as u32), feed.clone(), hwnd, msg),
            None => feed.stop(),
        }
        let fit = item.fit().filter(|_| !folder);
        Ok(Self {
            surface,
            id: item.id,
            folder,
            size: item.size,
            fit: fit.is_some(),
            fit_w: fit.map_or(dims(item.size).0, |f| f.0),
            max,
            crop: [0.0, 0.0, 1.0, 1.0],
            start: Some((item.crop(), fit)),
            feed,
            frame: None,
            image: None,
            px: None,
            error: live::first_picture(&path).is_none().then(|| "No pictures to show".to_string()),
            drag: None,
            hover: None,
            hits: Vec::new(),
            motion: Motion::default(),
            tracking: false,
            dirty: true,
        })
    }

    pub fn tick(&mut self, dt: f32) -> bool {
        let moving = self.motion.tick(dt);
        self.dirty |= moving;
        moving
    }

    pub fn animating(&self) -> bool {
        self.motion.moving()
    }

    pub fn focus(&self) {
        unsafe {
            let _ = SetForegroundWindow(self.surface.content);
        }
    }

    /// The picture loaded (or a GIF's next frame).
    pub fn poll(&mut self) {
        if let Some(f) = self.feed.take() {
            if self.px.is_none() {
                self.px = Some((f.w, f.h));
                self.apply_start();
            }
            self.frame = Some(f);
            self.dirty = true;
        } else if let Some(e) = self.feed.error() {
            self.error = Some(e);
            self.dirty = true;
        }
    }

    fn apply_start(&mut self) {
        let Some((crop, fit)) = self.start.take() else { return };
        if fit.is_some() {
            self.crop = crop.unwrap_or([0.0, 0.0, 1.0, 1.0]);
        } else {
            self.crop = self.cover(crop);
        }
    }

    /// The frame in the widget's shape filling as much of the picture as it
    /// can, around `focus`'s middle (what the widget shows without a crop).
    fn cover(&self, focus: Option<[f32; 4]>) -> [f32; 4] {
        let Some((pw, ph)) = self.px else { return [0.0, 0.0, 1.0, 1.0] };
        let (w, h) = dims(self.size);
        let target = (((w * 100.0) as u32).max(1), ((h * 100.0) as u32).max(1));
        let (x, y, cw, ch) = live::source_rect(pw, ph, target, focus);
        [x as f32 / pw as f32, y as f32 / ph as f32, cw as f32 / pw as f32, ch as f32 / ph as f32]
    }

    /// The frame's width over its height in 0-1 units, when its shape is the widget's.
    fn locked_aspect(&self) -> Option<f32> {
        if self.fit {
            return None;
        }
        let (pw, ph) = self.px?;
        let (w, h) = dims(self.size);
        Some(w / h * ph as f32 / pw as f32)
    }

    /// Fit to Picture: the widget's size in points, from its width and the frame's shape.
    fn fit_dims(&self) -> (f32, f32) {
        let (pw, ph) = self.px.unwrap_or((1, 1));
        let shape = (self.crop[2] * pw as f32) / (self.crop[3] * ph as f32).max(0.001);
        let mut w = self.fit_w.clamp(FIT_MIN, self.max.0);
        let mut h = w / shape;
        if h > self.max.1 {
            h = self.max.1;
            w = h * shape;
        }
        if h < FIT_MIN {
            h = FIT_MIN;
            w = (h * shape).min(self.max.0);
        }
        (w.round(), h.round())
    }

    /// The picture inside the preview, fitted.
    fn image_rect(&self) -> Option<Rect> {
        let (pw, ph) = self.px?;
        let p = preview();
        let k = (p.w / pw as f32).min(p.h / ph as f32);
        let (w, h) = (pw as f32 * k, ph as f32 * k);
        Some(Rect::new(p.cx() - w / 2.0, p.cy() - h / 2.0, w, h))
    }

    fn crop_rect(&self) -> Option<Rect> {
        let r = self.image_rect()?;
        let [x, y, w, h] = self.crop;
        Some(Rect::new(r.x + x * r.w, r.y + y * r.h, w * r.w, h * r.h))
    }

    /// A point in 0-1 picture units, held inside the picture.
    fn to_norm(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let r = self.image_rect()?;
        Some((((x - r.x) / r.w).clamp(0.0, 1.0), ((y - r.y) / r.h).clamp(0.0, 1.0)))
    }

    fn min_side(&self) -> (f32, f32) {
        let (pw, ph) = self.px.unwrap_or((1, 1));
        let short = pw.min(ph) as f32 * MIN_CROP;
        (short / pw as f32, short / ph as f32)
    }

    /// Scales the frame by `k` about its middle (keeping its shape), inside the picture.
    fn zoom(&mut self, k: f32) {
        let [x, y, w, h] = self.crop;
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let (mw, mh) = self.min_side();
        // Never bigger than the picture, never smaller than the least frame.
        let k = k.min(1.0 / w).min(1.0 / h).max(mw / w).max(mh / h);
        let (nw, nh) = (w * k, h * k);
        self.crop = [(cx - nw / 2.0).clamp(0.0, 1.0 - nw), (cy - nh / 2.0).clamp(0.0, 1.0 - nh), nw, nh];
        self.dirty = true;
    }

    fn set_size(&mut self, size: WidgetSize) {
        let was = self.crop;
        let area = was[2] * was[3];
        self.size = size;
        self.fit = false;
        // The same middle and about the same zoom, in the new shape.
        let Some(a) = self.locked_aspect() else { return };
        let mut w = (area * a).sqrt();
        let mut h = w / a;
        let k = (1.0 / w).min(1.0 / h).min(1.0);
        (w, h) = (w * k, h * k);
        let (cx, cy) = (was[0] + was[2] / 2.0, was[1] + was[3] / 2.0);
        self.crop = [(cx - w / 2.0).clamp(0.0, 1.0 - w), (cy - h / 2.0).clamp(0.0, 1.0 - h), w, h];
        self.dirty = true;
    }

    fn set_fit(&mut self) {
        if self.folder || self.fit {
            return;
        }
        self.fit = true;
        // The widget keeps about its width; its height now follows the frame.
        self.fit_w = dims(self.size).0;
        self.dirty = true;
    }

    fn reset(&mut self) {
        self.crop = if self.fit { [0.0, 0.0, 1.0, 1.0] } else { self.cover(None) };
        self.dirty = true;
    }

    fn done(&self) -> PhotoEditEvent {
        if self.px.is_none() {
            return PhotoEditEvent::Close;
        }
        PhotoEditEvent::Run(Action::Widget(WidgetCmd::SetPhoto {
            id: self.id,
            size: self.size,
            crop: Some(self.crop),
            fit: self.fit.then(|| self.fit_dims()),
        }))
    }

    pub fn on_key(&mut self, vk: u16) -> PhotoEditEvent {
        const ENTER: u16 = 0x0D;
        const ESC: u16 = 0x1B;
        match vk {
            ESC => PhotoEditEvent::Close,
            ENTER => self.done(),
            _ => PhotoEditEvent::None,
        }
    }

    pub fn on_wheel(&mut self, delta: i16, x: f32, y: f32) {
        if self.px.is_some() && preview().contains(x, y) {
            self.zoom(if delta > 0 { 0.92 } else { 1.0 / 0.92 });
        }
    }

    /// A press: returns true when it starts a drag (the caller captures the mouse).
    pub fn on_down(&mut self, x: f32, y: f32) -> bool {
        if self.fit && slider_row().contains(x, y) && x >= track().x - 10.0 {
            self.drag = Some(Drag::Slider);
            self.slide(x);
            return true;
        }
        let Some(r) = self.crop_rect() else { return false };
        let corners = [(r.x, r.y), (r.right(), r.y), (r.x, r.bottom()), (r.right(), r.bottom())];
        if let Some(i) = corners.iter().position(|&(cx, cy)| (x - cx).abs() <= GRAB && (y - cy).abs() <= GRAB) {
            let [cx, cy, cw, ch] = self.crop;
            let opposite = [(cx + cw, cy + ch), (cx, cy + ch), (cx + cw, cy), (cx, cy)];
            self.drag = Some(Drag::Corner { anchor: opposite[i] });
            return true;
        }
        if r.contains(x, y) {
            self.drag = Some(Drag::Move { from: (x, y), crop: self.crop });
            return true;
        }
        false
    }

    fn slide(&mut self, x: f32) {
        let t = track();
        let k = ((x - t.x) / t.w).clamp(0.0, 1.0);
        let max_w = self.max.0.min(self.max.1 * self.fit_dims().0 / self.fit_dims().1.max(1.0));
        self.fit_w = FIT_MIN + k * (max_w - FIT_MIN).max(0.0);
        self.dirty = true;
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        match self.drag {
            Some(Drag::Slider) => self.slide(x),
            Some(Drag::Move { from, crop }) => {
                let Some(r) = self.image_rect() else { return };
                let (dx, dy) = ((x - from.0) / r.w, (y - from.1) / r.h);
                self.crop = [(crop[0] + dx).clamp(0.0, 1.0 - crop[2]), (crop[1] + dy).clamp(0.0, 1.0 - crop[3]), crop[2], crop[3]];
                self.dirty = true;
            }
            Some(Drag::Corner { anchor }) => {
                let Some((nx, ny)) = self.to_norm(x, y) else { return };
                let (mw, mh) = self.min_side();
                let (right, down) = (nx >= anchor.0, ny >= anchor.1);
                // Room from the anchor to the picture's edge, towards the pointer.
                let room_w = if right { 1.0 - anchor.0 } else { anchor.0 };
                let room_h = if down { 1.0 - anchor.1 } else { anchor.1 };
                let (mut w, mut h) = ((nx - anchor.0).abs(), (ny - anchor.1).abs());
                if let Some(a) = self.locked_aspect() {
                    // The shape holds: whichever way the pointer went further leads.
                    w = w.max(h * a);
                    h = w / a;
                    let k = (room_w / w).min(room_h / h).min(1.0);
                    (w, h) = (w * k, h * k);
                    let k = (mw / w).max(mh / h).max(1.0);
                    (w, h) = (w * k, h * k);
                } else {
                    w = w.clamp(mw, room_w.max(mw));
                    h = h.clamp(mh, room_h.max(mh));
                }
                let x0 = if right { anchor.0 } else { anchor.0 - w };
                let y0 = if down { anchor.1 } else { anchor.1 - h };
                self.crop = [x0.clamp(0.0, 1.0 - w), y0.clamp(0.0, 1.0 - h), w.min(1.0), h.min(1.0)];
                self.dirty = true;
            }
            None => {
                let hover = self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|&(_, t)| t);
                if hover != self.hover {
                    self.hover = hover;
                    self.dirty = true;
                }
            }
        }
    }

    /// The button let go: ends a drag, or presses what's under it.
    pub fn on_up(&mut self, x: f32, y: f32) -> PhotoEditEvent {
        if self.drag.take().is_some() {
            return PhotoEditEvent::None;
        }
        let Some(t) = self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|&(_, t)| t) else { return PhotoEditEvent::None };
        match t {
            Target::Size(s) => self.set_size(s),
            Target::Fit => self.set_fit(),
            Target::Reset => self.reset(),
            Target::Cancel => return PhotoEditEvent::Close,
            Target::Done => return self.done(),
        }
        PhotoEditEvent::None
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if self.drag.is_none() && self.hover.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        let body = body();
        self.surface.set_backdrop_shape(body, PANEL_RADIUS);
        if let Some(f) = self.frame.take() {
            let target = self.surface.canvas.target().clone();
            self.image = IconPixels { width: f.w, height: f.h, data: f.data }.to_bitmap(&target);
        }
        let title_font = gfx.format(&theme.font, 13.0, Weight::SemiBold, Align::Leading);
        let regular = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Center);
        let small_left = gfx.format(&theme.font, 12.0, Weight::Regular, Align::Leading);
        let small_right = gfx.format(&theme.font, 12.0, Weight::Regular, Align::Trailing);
        let has_backdrop = self.surface.backdrop.is_some();
        let (image_rect, crop_rect) = (self.image_rect(), self.crop_rect());
        let widget = if self.fit { self.fit_dims() } else { dims(self.size) };
        let (fit, folder, size, hover) = (self.fit, self.folder, self.size, self.hover);
        let slider_k = {
            let max_w = self.max.0.min(self.max.1 * widget.0 / widget.1.max(1.0));
            ((widget.0 - FIT_MIN) / (max_w - FIT_MIN).max(1.0)).clamp(0.0, 1.0)
        };
        let error = self.error.clone();
        let image = self.image.clone();
        self.motion.sweep();
        let motion = &self.motion;
        let mut hits = Vec::new();

        let r = self.surface.render_part(gfx, None, |p: &Painter| {
            p.soft_shadow(body, PANEL_RADIUS, 12, 2.0, 0.03, 6.0, theme.shadow);
            p.clear_rect(body.inset(3.0, 3.0));
            p.fill_round(body, PANEL_RADIUS, if has_backdrop { theme.menu_tint.alpha(0.3) } else { theme.menu_tint.alpha(1.0) });
            p.stroke_round(body.inset(0.25, 0.25), PANEL_RADIUS, theme.menu_border, 0.5);
            p.text("Edit Photo", &title_font, Rect::new(body.x + 20.0, body.y + 6.0, W - 40.0, HEADER - 10.0), theme.text);

            // The picture, dimmed outside the frame.
            let pv = preview();
            p.fill_round(pv, 10.0, Color::gray(0, if theme.dark { 0.35 } else { 0.12 }));
            match (image_rect, crop_rect, image.as_ref()) {
                (Some(ir), Some(cr), Some(bmp)) => {
                    p.bitmap(bmp, ir, 1.0);
                    let shade = Color::gray(0, 0.55);
                    p.fill_rect(Rect::new(ir.x, ir.y, ir.w, cr.y - ir.y), shade);
                    p.fill_rect(Rect::new(ir.x, cr.bottom(), ir.w, ir.bottom() - cr.bottom()), shade);
                    p.fill_rect(Rect::new(ir.x, cr.y, cr.x - ir.x, cr.h), shade);
                    p.fill_rect(Rect::new(cr.right(), cr.y, ir.right() - cr.right(), cr.h), shade);
                    // Thirds, and the widget's rounded corners at this scale.
                    for i in 1..3 {
                        let (fx, fy) = (cr.x + cr.w * i as f32 / 3.0, cr.y + cr.h * i as f32 / 3.0);
                        p.line(fx, cr.y, fx, cr.bottom(), Color::gray(255, 0.3), 0.5);
                        p.line(cr.x, fy, cr.right(), fy, Color::gray(255, 0.3), 0.5);
                    }
                    let radius = RADIUS * cr.w / widget.0.max(1.0);
                    p.stroke_round(cr, radius, Color::WHITE, 1.5);
                    let l = 16.0f32.min(cr.w / 3.0).min(cr.h / 3.0);
                    for (x, y, sx, sy) in [(cr.x, cr.y, 1.0, 1.0), (cr.right(), cr.y, -1.0, 1.0), (cr.x, cr.bottom(), 1.0, -1.0), (cr.right(), cr.bottom(), -1.0, -1.0)] {
                        p.line(x, y, x + sx * l, y, Color::WHITE, 4.0);
                        p.line(x, y, x, y + sy * l, Color::WHITE, 4.0);
                    }
                }
                _ => {
                    let msg = error.as_deref().unwrap_or("Loading\u{2026}");
                    p.text(msg, &regular, pv, theme.text_secondary);
                }
            }

            // Sizes: a segmented control.
            let seg = seg_row();
            let mut opts: Vec<(&str, Target)> =
                vec![("Small", Target::Size(WidgetSize::Small)), ("Medium", Target::Size(WidgetSize::Medium)), ("Large", Target::Size(WidgetSize::Large))];
            if !folder {
                opts.push(("Fit to Picture", Target::Fit));
            }
            let sw = 116.0;
            let total = sw * opts.len() as f32;
            let sx = seg.cx() - total / 2.0;
            p.fill_round(Rect::new(sx, seg.y, total, seg.h), 7.0, if theme.dark { Color::gray(255, 0.08) } else { Color::gray(0, 0.06) });
            for (i, (label, t)) in opts.into_iter().enumerate() {
                let r = Rect::new(sx + i as f32 * sw + 2.0, seg.y + 2.0, sw - 4.0, seg.h - 4.0);
                let on = match t {
                    Target::Fit => fit,
                    Target::Size(s) => !fit && s == size,
                    _ => false,
                };
                let hot = motion.fade(t, hover == Some(t));
                if on {
                    p.fill_round(r, 6.0, if theme.dark { Color::gray(255, 0.22) } else { Color::WHITE });
                } else if hot > 0.0 {
                    p.fill_round(r, 6.0, if theme.dark { Color::gray(255, 0.06 * hot) } else { Color::gray(0, 0.04 * hot) });
                }
                p.text(label, &regular, r, theme.text);
                hits.push((r, t));
            }

            // Fit to Picture: how big; otherwise, how it works.
            let row = slider_row();
            if fit {
                p.text("Size", &small_left, Rect::new(row.x, row.y, 56.0, row.h), theme.text);
                let t = track();
                p.fill_round(t, 2.0, if theme.dark { Color::gray(255, 0.18) } else { Color::gray(0, 0.12) });
                p.fill_round(Rect::new(t.x, t.y, t.w * slider_k, t.h), 2.0, theme.accent);
                let kx = t.x + t.w * slider_k;
                p.fill_circle(kx, t.cy() + 0.5, 9.5, Color::gray(0, 0.18));
                p.fill_circle(kx, t.cy(), 9.0, Color::WHITE);
                let label = format!("{} \u{00D7} {} pt", widget.0 as i32, widget.1 as i32);
                p.text(&label, &small_right, Rect::new(row.right() - 110.0, row.y, 110.0, row.h), theme.text_secondary);
            } else {
                let hint = if folder {
                    "Drag the frame to choose what shows; drag a corner or scroll to zoom. Each picture is centred on the same spot."
                } else {
                    "Drag the frame to choose what shows; drag a corner or scroll to zoom."
                };
                p.text(hint, &small_left, row, theme.text_secondary);
            }

            // Buttons.
            let fy = body.bottom() - FOOTER;
            p.line(body.x, fy, body.right(), fy, theme.menu_separator, 1.0);
            let mut bx = body.right() - 16.0;
            for (label, t, w) in [("Done", Target::Done, 84.0), ("Cancel", Target::Cancel, 76.0), ("Reset", Target::Reset, 76.0)] {
                let r = Rect::new(bx - w, fy + (FOOTER - 28.0) / 2.0, w, 28.0);
                bx -= w + 8.0;
                let primary = t == Target::Done;
                let hot = motion.fade(t, hover == Some(t));
                let fill = if primary {
                    theme.accent
                } else if theme.dark {
                    Color::gray(255, 0.1 + 0.06 * hot)
                } else {
                    Color::gray(0, 0.06 + 0.04 * hot)
                };
                p.fill_round(r, 7.0, fill);
                p.text(label, &regular, r, if primary { Color::WHITE } else { theme.text });
                hits.push((r, t));
            }
        });
        self.hits = hits;
        if let Err(e) = r {
            log!("photo editor render failed: {e}");
        }
    }
}

impl Drop for PhotoEditor {
    fn drop(&mut self) {
        self.feed.stop();
        let me = self.surface.content.0 as isize;
        let _ = PHOTO_EDIT_HWND.compare_exchange(me, 0, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crop is used as is when it has the widget's shape; otherwise its
    /// middle is the focus of a frame that fills the widget.
    #[test]
    fn source_rect_crop_and_focus() {
        // No crop: the middle, in the widget's (square) shape.
        assert_eq!(live::source_rect(300, 200, (100, 100), None), (50, 0, 200, 200));
        // A square crop for a square widget: as is.
        assert_eq!(live::source_rect(300, 200, (100, 100), Some([0.0, 0.25, 1.0 / 3.0, 0.5])), (0, 50, 100, 100));
        // A crop of another shape: its middle (far left) as the focus, held inside.
        assert_eq!(live::source_rect(300, 200, (100, 100), Some([0.0, 0.0, 0.1, 1.0])), (0, 0, 200, 200));
        // Focus on the right edge, wide widget.
        let (x, y, w, h) = live::source_rect(300, 200, (300, 100), Some([0.9, 0.9, 0.1, 0.1]));
        assert_eq!((w, h), (300, 100));
        assert_eq!((x, y), (0, 100));
    }

    /// Renders the panel (never shown) to `DOCKSIDE_PHOTO_EDIT_OUT` with the
    /// picture at `DOCKSIDE_PHOTO`: `cargo test renders_photo_edit -- --ignored`.
    #[test]
    #[ignore]
    fn renders_photo_edit() {
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        let (Ok(out), Ok(photo)) = (std::env::var("DOCKSIDE_PHOTO_EDIT_OUT"), std::env::var("DOCKSIDE_PHOTO")) else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let _ = crate::win::register_class(Some(proc_));
        let gfx = Gfx::new().unwrap();
        let cfg = crate::config::defaults();
        let theme = Theme::new(&cfg, &gfx);
        let fit = std::env::var_os("DOCKSIDE_PHOTO_FIT").is_some();
        let item = WidgetItem {
            id: 1,
            kind: crate::config::WidgetKind::Photo,
            size: WidgetSize::Medium,
            x: 0.0,
            y: 0.0,
            zone: None,
            display: None,
            sensors: None,
            path: Some(photo),
            options: None,
            crop: Some(vec![0.1, 0.2, 0.5, 0.5]),
            fit: fit.then(|| vec![300.0, 300.0]),
        };
        let mut e = PhotoEditor::new(&gfx, &theme, &cfg, &Monitor::primary(), &item, (1800.0, 1000.0), 0, 0).unwrap();
        e.surface.set_hidden(true);
        for _ in 0..200 {
            e.poll();
            if e.px.is_some() || e.error.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        e.render(&gfx, &theme);
        let c = &e.surface.canvas;
        crate::gfx::write_png(&gfx, c.width as u32, c.height as u32, &c.pixels(), &out);
    }
}

#[cfg(test)]
mod player {
    /// The widget's player crops a still to the widget's shape: `cargo test player_crops -- --ignored`
    /// with `DOCKSIDE_PHOTO` set to a picture.
    #[test]
    #[ignore]
    fn player_crops() {
        let Ok(photo) = std::env::var("DOCKSIDE_PHOTO") else { return };
        let feed = crate::widgets::live::SharedFeed::default();
        // A tall widget over a wide picture, framed on its left.
        crate::widgets::live::play_photo(photo.into(), (200, 400), Some([0.0, 0.0, 0.2, 0.6]), feed.clone(), 0, 0);
        let start = std::time::Instant::now();
        let f = loop {
            if let Some(f) = feed.take() {
                break f;
            }
            assert!(start.elapsed().as_secs() < 5, "no frame");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        feed.stop();
        println!("frame {}x{}", f.w, f.h);
        assert!(((f.w as f32 / f.h as f32) - 0.5).abs() < 0.02);
    }
}
