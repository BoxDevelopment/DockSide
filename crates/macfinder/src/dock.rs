//! The Dock: pinned + running apps, magnification, launch bounce, running
//! indicators, tooltips and context menus.

use std::collections::HashMap;
use std::path::Path;

use windows::core::Result;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::Shell::ABE_BOTTOM;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use crate::anim::approach;
use crate::config::{Config, DockApp, DockCfg, IconStyle};
use crate::geom::{Color, Rect};
use crate::gfx::icons::{IconStore, RECYCLE_BIN};
use crate::gfx::{Align, Gfx, Weight};
use crate::menu::{Action, Entry, Placement};
use crate::surface::Surface;
use crate::theme::Theme;
use crate::win::apps::{self, AppNames, AppWindow};
use crate::win::{appbar, Monitor, WM_APP_APPBAR};

const BOUNCE_PERIOD: f32 = 0.62;
const MAX_BOUNCES: f32 = 12.0;
const LABEL_H: f32 = 24.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    App,
    Separator,
    Trash,
}

#[derive(Clone, Copy, Debug)]
struct Bounce {
    t: f32,
    until: Option<f32>,
}

struct Item {
    kind: Kind,
    name: String,
    launch: String,
    args: Option<String>,
    icon_key: String,
    custom_icon: Option<String>,
    /// Lower-cased exe file name used to attach running windows.
    match_name: Option<String>,
    pinned: Option<DockApp>,
    windows: Vec<AppWindow>,
    bounce: Option<Bounce>,
}

impl Item {
    fn pinned(app: &DockApp) -> Item {
        let lower = app.path.to_lowercase();
        let match_name = app
            .match_exe
            .as_ref()
            .map(|m| m.to_lowercase())
            .or_else(|| lower.ends_with(".exe").then(|| apps::exe_name(&app.path)))
            .or_else(|| {
                lower.ends_with(".lnk").then(|| apps::resolve_lnk(&app.path)).flatten().map(|t| apps::exe_name(&t))
            });
        let name = app.name.clone().unwrap_or_else(|| {
            Path::new(&app.path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        });
        Item {
            kind: Kind::App,
            name,
            launch: app.path.clone(),
            args: app.args.clone(),
            icon_key: app.path.clone(),
            custom_icon: app.icon.clone(),
            match_name,
            pinned: Some(app.clone()),
            windows: Vec::new(),
            bounce: None,
        }
    }

    fn running(w: &AppWindow, name: String) -> Item {
        Item {
            kind: Kind::App,
            name,
            launch: w.exe.clone(),
            args: None,
            icon_key: w.exe.clone(),
            custom_icon: None,
            match_name: Some(w.exe_name()),
            pinned: None,
            windows: vec![w.clone()],
            bounce: None,
        }
    }

    fn special(kind: Kind) -> Item {
        Item {
            kind,
            name: if kind == Kind::Trash { "Trash".into() } else { String::new() },
            launch: if kind == Kind::Trash { "shell:RecycleBinFolder".into() } else { String::new() },
            args: None,
            icon_key: RECYCLE_BIN.into(),
            custom_icon: None,
            match_name: None,
            pinned: None,
            windows: Vec::new(),
            bounce: None,
        }
    }

    fn bounce_height(&self, tile: f32) -> f32 {
        match self.bounce {
            Some(b) => {
                let phase = (b.t / BOUNCE_PERIOD).fract();
                tile * 0.5 * 4.0 * phase * (1.0 - phase)
            }
            None => 0.0,
        }
    }
}

/// Dock dimensions derived from the tile size, in points.
#[derive(Clone, Copy, Debug)]
struct Metrics {
    tile: f32,
    max: f32,
    gap: f32,
    pad: f32,
    shelf_h: f32,
    sep_w: f32,
    radius: f32,
    margin: f32,
}

impl Metrics {
    fn new(cfg: &DockCfg) -> Self {
        let tile = cfg.tile_size.clamp(16.0, 128.0);
        let max = if cfg.magnification { cfg.magnified_size.clamp(tile, 256.0) } else { tile };
        let shelf_h = tile * 1.22;
        Self {
            tile,
            max,
            gap: tile * 0.08,
            pad: tile * 0.1,
            shelf_h,
            sep_w: tile * 0.22,
            radius: shelf_h * 0.3,
            margin: cfg.bottom_margin.max(0.0),
        }
    }
}

/// Result of laying out items for the current hover state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// (center x, size) per item, window-local points.
    pub items: Vec<(f32, f32)>,
    pub left: f32,
    pub right: f32,
}

/// macOS-style magnification. Items within `radius` of the mouse grow along a
/// cosine falloff; the row is then shifted so the point under the cursor stays
/// under the cursor, which makes the Dock grow outward on both sides.
pub fn compute_layout(
    widths: &[f32],
    scalable: &[bool],
    max: f32,
    gap: f32,
    pad: f32,
    window_w: f32,
    mouse_x: f32,
    t: f32,
) -> Layout {
    let n = widths.len();
    if n == 0 {
        return Layout { items: vec![], left: window_w / 2.0 - pad, right: window_w / 2.0 + pad };
    }
    let total0: f32 = widths.iter().sum::<f32>() + gap * (n - 1) as f32 + 2.0 * pad;
    let left0 = (window_w - total0) / 2.0 + pad;

    let mut centers0 = Vec::with_capacity(n);
    let mut x = left0;
    for &w in widths {
        centers0.push(x + w / 2.0);
        x += w + gap;
    }

    let tile = widths.iter().zip(scalable).find(|(_, s)| **s).map(|(w, _)| *w).unwrap_or(max);
    let radius = (tile + gap) * 2.6;
    let sizes: Vec<f32> = (0..n)
        .map(|i| {
            if !scalable[i] || t <= 0.0 || max <= widths[i] {
                return widths[i];
            }
            let d = (mouse_x - centers0[i]).abs();
            if d >= radius {
                return widths[i];
            }
            let f = ((std::f32::consts::PI * d / radius).cos() + 1.0) / 2.0;
            widths[i] + (max - widths[i]) * f * t
        })
        .collect();

    // Magnified positions starting at 0.
    let mut centers = Vec::with_capacity(n);
    let mut x = 0.0;
    for &s in &sizes {
        centers.push(x + s / 2.0);
        x += s + gap;
    }

    // Anchor: map mouse position in the base row to the same relative spot in the magnified row.
    let span0 = |i: usize| (centers0[i] - widths[i] / 2.0 - gap / 2.0, widths[i] + gap);
    let span = |i: usize| (centers[i] - sizes[i] / 2.0 - gap / 2.0, sizes[i] + gap);
    let (k, u) = if mouse_x < span0(0).0 {
        (0, 0.0)
    } else if mouse_x >= span0(n - 1).0 + span0(n - 1).1 {
        (n - 1, 1.0)
    } else {
        let k = (0..n).find(|&i| mouse_x < span0(i).0 + span0(i).1).unwrap_or(n - 1);
        (k, (mouse_x - span0(k).0) / span0(k).1)
    };
    let mag_point = span(k).0 + u * span(k).1;
    let base_point = span0(k).0 + u * span0(k).1;
    // Blend the anchor with the hover amount so t = 0 yields the unmagnified row exactly.
    let shift = base_point - mag_point;

    let items: Vec<(f32, f32)> = centers.iter().zip(&sizes).map(|(c, s)| (c + shift, *s)).collect();
    let left = items[0].0 - items[0].1 / 2.0 - pad;
    let right = items[n - 1].0 + items[n - 1].1 / 2.0 + pad;
    Layout { items, left, right }
}

pub struct Dock {
    pub surface: Surface,
    cfg: DockCfg,
    monitor: Monitor,
    metrics: Metrics,
    items: Vec<Item>,
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    hover_t: f32,
    hovering: bool,
    mouse: (f32, f32),
    pressed: Option<usize>,
    pub menu_item: Option<usize>,
    pub tracking: bool,
    reserved: bool,
    pub dirty: bool,
}

impl Dock {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: Monitor) -> Result<Self> {
        let d = &cfg.dock;
        let mut items: Vec<Item> = d
            .apps
            .iter()
            .filter(|a| {
                // Skip pinned files that don't exist (URIs and shell ids always pass).
                let p = Path::new(&a.path);
                !(p.is_absolute() && !a.path.contains("::") && !p.exists())
            })
            .map(Item::pinned)
            .collect();
        if d.show_trash {
            items.push(Item::special(Kind::Separator));
            items.push(Item::special(Kind::Trash));
        }

        let metrics = Metrics::new(d);
        let placeholder = RECT { left: 0, top: 0, right: 1, bottom: 1 };
        let surface = Surface::new(gfx, placeholder, monitor.scale, d.backdrop, theme.dock_tint)?;
        let reserved = d.reserve_space;
        if reserved {
            let thickness = ((metrics.margin + metrics.shelf_h) * monitor.scale).round() as i32;
            appbar::register(surface.content, WM_APP_APPBAR, ABE_BOTTOM, monitor.rect, thickness);
        }
        let mut dock = Self {
            surface,
            cfg: d.clone(),
            monitor,
            metrics,
            items,
            bitmaps: HashMap::new(),
            hover_t: 0.0,
            hovering: false,
            mouse: (0.0, 0.0),
            pressed: None,
            menu_item: None,
            tracking: false,
            reserved,
            dirty: true,
        };
        dock.fit_window()?;
        Ok(dock)
    }

    fn base_widths(&self) -> (Vec<f32>, Vec<bool>) {
        let m = &self.metrics;
        self.items
            .iter()
            .map(|i| if i.kind == Kind::Separator { (m.sep_w, false) } else { (m.tile, true) })
            .unzip()
    }

    /// Sizes the window to fit the fully magnified Dock plus tooltip room.
    fn fit_window(&mut self) -> Result<()> {
        let m = self.metrics;
        let (widths, _) = self.base_widths();
        let total0 = widths.iter().sum::<f32>() + m.gap * widths.len().saturating_sub(1) as f32 + 2.0 * m.pad;
        let grow = (m.max - m.tile) * 2.6 * 2.0;
        let width_pt = (total0 + grow + 280.0).min(self.monitor.width_pt());
        let height_pt = m.margin + m.shelf_h + m.max.max(m.tile * 1.55) + LABEL_H + 16.0;
        let s = self.monitor.scale;
        let (w, h) = ((width_pt * s).round() as i32, (height_pt * s).round() as i32);
        let mon = self.monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        self.surface.set_rect(RECT { left, top: mon.bottom - h, right: left + w, bottom: mon.bottom })
    }

    fn shelf_bottom(&self) -> f32 {
        self.surface.height_pt() - self.metrics.margin
    }

    fn icon_bottom(&self) -> f32 {
        let m = &self.metrics;
        self.shelf_bottom() - (m.shelf_h - m.tile) / 2.0 - m.tile * 0.02
    }

    fn layout(&self) -> Layout {
        let m = &self.metrics;
        let (widths, scalable) = self.base_widths();
        compute_layout(&widths, &scalable, m.max, m.gap, m.pad, self.surface.width_pt(), self.mouse.0, self.hover_t)
    }

    fn shelf_rect(&self, lay: &Layout) -> Rect {
        let m = &self.metrics;
        Rect::new(lay.left, self.shelf_bottom() - m.shelf_h, lay.right - lay.left, m.shelf_h)
    }

    /// The area that captures the mouse: the shelf plus the magnified icons above it.
    fn hit_rect(&self, lay: &Layout) -> Rect {
        let shelf = self.shelf_rect(lay);
        let tallest = lay.items.iter().map(|(_, s)| *s).fold(self.metrics.tile, f32::max);
        let top = shelf.y.min(self.icon_bottom() - tallest);
        Rect::new(shelf.x, top, shelf.w, shelf.bottom() - top)
    }

    fn item_at(&self, x: f32, y: f32) -> Option<usize> {
        let lay = self.layout();
        if !self.hit_rect(&lay).contains(x, y) {
            return None;
        }
        let gap = self.metrics.gap;
        lay.items
            .iter()
            .position(|(cx, s)| x >= cx - s / 2.0 - gap / 2.0 && x < cx + s / 2.0 + gap / 2.0)
            .filter(|&i| self.items[i].kind != Kind::Separator)
    }

    /// Attaches the current window list to items, adding/removing running apps.
    pub fn update_running(&mut self, windows: &[AppWindow], names: &mut AppNames, cfg: &Config) {
        let before: Vec<(String, bool)> = self.items.iter().map(|i| (i.launch.clone(), !i.windows.is_empty())).collect();
        for item in &mut self.items {
            item.windows.clear();
        }
        for w in windows {
            let exe_name = w.exe_name();
            if let Some(item) = self
                .items
                .iter_mut()
                .find(|i| i.kind == Kind::App && i.pinned.is_some() && i.match_name.as_deref() == Some(exe_name.as_str()))
            {
                item.windows.push(w.clone());
                continue;
            }
            if !self.cfg.show_running {
                continue;
            }
            if let Some(item) = self
                .items
                .iter_mut()
                .find(|i| i.kind == Kind::App && i.pinned.is_none() && i.launch.eq_ignore_ascii_case(&w.exe))
            {
                item.windows.push(w.clone());
            } else {
                let at = self.items.iter().position(|i| i.kind != Kind::App).unwrap_or(self.items.len());
                self.items.insert(at, Item::running(w, names.name_for(&w.exe, &cfg.app_names)));
            }
        }
        self.items.retain(|i| i.kind != Kind::App || i.pinned.is_some() || !i.windows.is_empty() || i.bounce.is_some());
        for item in &mut self.items {
            if let Some(b) = &mut item.bounce {
                if !item.windows.is_empty() && b.until.is_none() {
                    b.until = Some(((b.t / BOUNCE_PERIOD).floor() + 1.0) * BOUNCE_PERIOD);
                }
            }
        }

        let after: Vec<(String, bool)> = self.items.iter().map(|i| (i.launch.clone(), !i.windows.is_empty())).collect();
        if before != after {
            if before.len() != after.len() {
                self.pressed = None;
                self.menu_item = None;
                if let Err(e) = self.fit_window() {
                    log!("dock resize failed: {e}");
                }
            }
            self.dirty = true;
        }
    }

    /// Starts the launch bounce on the item that launches `path`.
    pub fn bounce(&mut self, path: &str) -> bool {
        match self.items.iter_mut().find(|i| i.kind == Kind::App && i.launch.eq_ignore_ascii_case(path)) {
            Some(item) if item.windows.is_empty() => {
                item.bounce = Some(Bounce { t: 0.0, until: None });
                true
            }
            _ => false,
        }
    }

    /// Returns true while animating.
    pub fn tick(&mut self, dt: f32) -> bool {
        let mut animating = false;
        let target = if self.hovering && self.cfg.magnification { 1.0 } else { 0.0 };
        if self.hover_t != target {
            self.hover_t = approach(self.hover_t, target, dt, 15.0);
            animating = true;
            self.dirty = true;
        }
        for item in &mut self.items {
            if let Some(b) = &mut item.bounce {
                b.t += dt;
                let limit = b.until.unwrap_or(MAX_BOUNCES * BOUNCE_PERIOD);
                if b.t >= limit {
                    item.bounce = None;
                } else {
                    animating = true;
                }
                self.dirty = true;
            }
        }
        if self.items.iter().any(|i| i.kind == Kind::App && i.pinned.is_none() && i.windows.is_empty() && i.bounce.is_none()) {
            self.items.retain(|i| i.kind != Kind::App || i.pinned.is_some() || !i.windows.is_empty() || i.bounce.is_some());
            let _ = self.fit_window();
        }
        animating
    }

    /// Returns true when an animation should start.
    pub fn on_move(&mut self, x: f32, y: f32) -> bool {
        self.mouse = (x, y);
        let inside = self.hit_rect(&self.layout()).contains(x, y);
        let changed = inside != self.hovering;
        self.hovering = inside;
        self.dirty = true;
        changed
    }

    pub fn on_leave(&mut self) -> bool {
        self.tracking = false;
        self.pressed = None;
        let changed = self.hovering;
        self.hovering = false;
        self.dirty = true;
        changed
    }

    pub fn on_down(&mut self, x: f32, y: f32) {
        self.pressed = self.item_at(x, y);
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> Option<Action> {
        let pressed = self.pressed.take();
        self.dirty = true;
        let idx = self.item_at(x, y).filter(|i| Some(*i) == pressed)?;
        let item = &self.items[idx];
        match item.kind {
            Kind::Trash => Some(Action::Open(item.launch.clone())),
            Kind::Separator => None,
            Kind::App if item.windows.is_empty() => {
                Some(Action::Launch { path: item.launch.clone(), args: item.args.clone() })
            }
            Kind::App => {
                let fg = unsafe { GetForegroundWindow() };
                let owns_fg = item.windows.iter().any(|w| w.hwnd == fg);
                let hwnd = if owns_fg && item.windows.len() > 1 {
                    // Already frontmost: cycle to its backmost window.
                    item.windows.last().unwrap().hwnd
                } else {
                    item.windows[0].hwnd
                };
                Some(Action::Activate(hwnd))
            }
        }
    }

    pub fn context_menu(&self, x: f32, y: f32) -> Option<(usize, Vec<Entry>, Placement)> {
        let idx = self.item_at(x, y)?;
        let item = &self.items[idx];
        let entries = match item.kind {
            Kind::Separator => return None,
            Kind::Trash => vec![
                Entry::item("Open", Action::Open(item.launch.clone())),
                Entry::Separator,
                Entry::item("Empty Trash", Action::EmptyTrash),
            ],
            Kind::App => {
                let mut v = Vec::new();
                for w in &item.windows {
                    v.push(Entry::item(w.title.clone(), Action::Activate(w.hwnd)));
                }
                if !v.is_empty() {
                    v.push(Entry::Separator);
                }
                v.push(
                    Entry::item(
                        "Keep in Dock",
                        Action::TogglePin { path: item.launch.clone(), name: item.name.clone(), pin: item.pinned.is_none() },
                    )
                    .checked(item.pinned.is_some()),
                );
                if Path::new(&item.launch).exists() {
                    v.push(Entry::item("Show in Explorer", Action::Reveal(item.launch.clone())));
                }
                v.push(Entry::Separator);
                if item.windows.is_empty() {
                    v.push(Entry::item("Open", Action::Launch { path: item.launch.clone(), args: item.args.clone() }));
                } else {
                    v.push(Entry::item("Quit", Action::CloseWindows(item.windows.iter().map(|w| w.hwnd).collect())));
                }
                v
            }
        };
        let lay = self.layout();
        let (cx, size) = lay.items[idx];
        let s = self.surface.scale;
        let placement = Placement::Above {
            cx: self.surface.rect.left as f32 / s + cx,
            bottom: self.surface.rect.top as f32 / s + self.icon_bottom() - size - 10.0,
        };
        Some((idx, entries, placement))
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme, icons: &mut IconStore) {
        self.dirty = false;

        for item in &self.items {
            if item.kind == Kind::Separator || self.bitmaps.contains_key(&item.icon_key) {
                continue;
            }
            let bmp = icons
                .get(&gfx.wic, &item.icon_key, item.custom_icon.as_deref())
                .and_then(|px| px.to_bitmap(self.surface.canvas.target()));
            self.bitmaps.insert(item.icon_key.clone(), bmp);
        }

        let lay = self.layout();
        let shelf = self.shelf_rect(&lay);
        let hit = self.hit_rect(&lay);
        let m = self.metrics;
        self.surface.set_backdrop_shape(shelf, m.radius);

        let label = if self.hovering && self.pressed.is_none() && self.menu_item.is_none() {
            self.item_at(self.mouse.0, self.mouse.1)
        } else {
            None
        };
        let font = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Center);
        let label_w = label.map(|i| gfx.measure(&self.items[i].name, &font) + 22.0).unwrap_or(0.0);
        let icon_bottom = self.icon_bottom();
        let has_backdrop = self.surface.backdrop.is_some();
        let squircle = self.cfg.icon_style == IconStyle::Squircle;
        let win_w = self.surface.width_pt();
        let (items, bitmaps, pressed, menu_item) = (&self.items, &self.bitmaps, self.pressed, self.menu_item);

        let r = self.surface.render(gfx, |p| {
            p.fill_rect(hit, Color::HIT);
            p.fill_round(shelf, m.radius, if has_backdrop { theme.dock_tint.alpha(0.0) } else { theme.dock_tint.alpha(1.0) });
            p.stroke_round(shelf.inset(0.5, 0.5), m.radius - 0.5, theme.dock_border, 1.0);

            for (i, (item, &(cx, size))) in items.iter().zip(&lay.items).enumerate() {
                if item.kind == Kind::Separator {
                    let inset = m.shelf_h * 0.2;
                    p.line(cx, shelf.y + inset, cx, shelf.bottom() - inset, theme.dock_separator, 1.0);
                    continue;
                }
                let lift = item.bounce_height(m.tile);
                let rect = Rect::new(cx - size / 2.0, icon_bottom - size - lift, size, size);
                let Some(Some(bmp)) = bitmaps.get(&item.icon_key) else {
                    p.fill_round(rect.inset(size * 0.08, size * 0.08), size * 0.2, theme.text_disabled);
                    continue;
                };
                let draw_rect = if squircle && item.custom_icon.is_none() {
                    let tile = rect.inset(size * 0.06, size * 0.06);
                    p.fill_round(tile.offset(0.0, size * 0.01), tile.w * 0.225, Color::gray(0, 0.12));
                    p.fill_round(tile, tile.w * 0.225, Color::WHITE);
                    tile.inset(tile.w * 0.16, tile.w * 0.16)
                } else {
                    rect
                };
                p.bitmap(bmp, draw_rect, 1.0);
                if pressed == Some(i) || menu_item == Some(i) {
                    p.tint_bitmap(bmp, draw_rect, Color::gray(0, 0.35));
                }
                if !item.windows.is_empty() {
                    let dot = (m.tile / 48.0 * 2.0).max(1.5);
                    p.fill_circle(cx, shelf.bottom() - (m.shelf_h - m.tile) / 4.0 - 0.5, dot, theme.indicator);
                }
            }

            if let Some(i) = label {
                let (cx, size) = lay.items[i];
                let top = icon_bottom - size - items[i].bounce_height(m.tile);
                let x = (cx - label_w / 2.0).clamp(2.0, win_w - label_w - 2.0);
                let r = Rect::new(x, top - 10.0 - LABEL_H, label_w, LABEL_H);
                p.fill_round(r, 6.0, theme.label_bg);
                p.stroke_round(r.inset(0.25, 0.25), 6.0, theme.menu_border, 0.5);
                p.text(&items[i].name, &font, r, theme.label_text);
            }
        });
        if let Err(e) = r {
            log!("dock render failed: {e}");
        }
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        if self.reserved {
            appbar::remove(self.surface.content);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(n: usize) -> (Vec<f32>, Vec<bool>) {
        (vec![48.0; n], vec![true; n])
    }

    #[test]
    fn unmagnified_is_centered() {
        let (w, s) = row(5);
        let lay = compute_layout(&w, &s, 80.0, 4.0, 5.0, 1000.0, 500.0, 0.0);
        assert!(((lay.left + lay.right) / 2.0 - 500.0).abs() < 0.01);
        assert!(lay.items.iter().all(|(_, size)| *size == 48.0));
    }

    #[test]
    fn hovered_icon_magnifies_and_stays_under_cursor() {
        let (w, s) = row(7);
        let base = compute_layout(&w, &s, 80.0, 4.0, 5.0, 1000.0, 0.0, 0.0);
        let mx = base.items[3].0;
        let lay = compute_layout(&w, &s, 80.0, 4.0, 5.0, 1000.0, mx, 1.0);
        assert!((lay.items[3].1 - 80.0).abs() < 0.01);
        assert!((lay.items[3].0 - mx).abs() < 0.01);
        assert!(lay.right - lay.left > base.right - base.left);
        assert!(lay.items[2].1 < 80.0 && lay.items[2].1 > 48.0);
    }

    #[test]
    fn separators_do_not_scale() {
        let w = vec![48.0, 10.0, 48.0];
        let s = vec![true, false, true];
        let base = compute_layout(&w, &s, 80.0, 4.0, 5.0, 1000.0, 0.0, 0.0);
        let lay = compute_layout(&w, &s, 80.0, 4.0, 5.0, 1000.0, base.items[1].0, 1.0);
        assert_eq!(lay.items[1].1, 10.0);
    }
}
