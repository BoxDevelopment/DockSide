//! The Dock: pinned + running apps, magnification, launch bounce, running
//! indicators, tooltips, context menus, drag-to-reorder and auto-hide.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::Shell::ABE_BOTTOM;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use crate::anim::approach;
use crate::config::{Config, ConfigValue, DockApp, DockCfg, IconStyle};
use crate::finder::FINDER_PATH;
use crate::geom::{Color, Rect};
use crate::gfx::icons::{load_shell_icon, IconStore, RECYCLE_BIN};
use crate::gfx::{finder_icon, start_icon, Align, Gfx, Weight};
use crate::icon_picker::{IconTarget, PICKER_PATH};
use crate::onboarding::ONBOARDING_PATH;
use crate::settings::SETTINGS_PATH;
use crate::menu::{Action, Entry, Placement};
use crate::surface::Surface;
use crate::theme::Theme;
use crate::win::apps::{self, AppNames, AppWindow};
use crate::win::{appbar, Monitor, WM_APP_APPBAR};

const BOUNCE_PERIOD: f32 = 0.62;
const MAX_BOUNCES: f32 = 12.0;
const LABEL_H: f32 = 24.0;
/// Pointer travel (points) before a press becomes a drag.
const DRAG_THRESHOLD: f32 = 5.0;
/// How long icons ease into new slots after a reorder.
const EASE_TIME: f32 = 0.35;
/// Delay before an auto-hiding Dock slides away.
const HIDE_DELAY: f32 = 0.3;
/// How long a file dragged onto a running app's icon waits before that app
/// comes forward, so the drag can carry on into its window (macOS spring-loading).
const SPRING_DELAY: f32 = 0.7;
/// A pinned item that opens the Start menu.
pub const START_PATH: &str = "dockside:start";
/// Icon-store keys for the Trash's shell icon, empty and full.
const TRASH_EMPTY: &str = "dockside:trash-empty";
const TRASH_FULL: &str = "dockside:trash-full";

/// What dropping the dragged files at the pointer would do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileDrop {
    None,
    Open,
    Trash,
}

/// A drag from another app over the Dock.
struct FileHover {
    item: Option<usize>,
    since: Instant,
    sprung: bool,
}

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

#[derive(Clone, Copy, Debug)]
struct Drag {
    from: usize,
    /// Insertion index among the pinned items (excluding the dragged one).
    to: usize,
    /// Pointer offset from the icon center when the drag started.
    grab_dx: f32,
    x: f32,
    y: f32,
    /// Dragged far enough above the Dock to remove it on release.
    out: bool,
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

/// A custom icon from `[app_icons]` for an exe file name.
fn exe_icon(icons: &BTreeMap<String, String>, exe: &str) -> Option<String> {
    icons.iter().find(|(k, _)| k.eq_ignore_ascii_case(exe)).map(|(_, v)| v.clone())
}

impl Item {
    fn pinned(app: &DockApp, icons: &BTreeMap<String, String>) -> Item {
        let lower = app.path.to_lowercase();
        let match_name = app
            .match_exe
            .as_ref()
            .map(|m| m.to_lowercase())
            .or_else(|| own_app_name(&app.path).map(|_| app.path.clone()))
            .or_else(|| lower.ends_with(".exe").then(|| apps::exe_name(&app.path)))
            .or_else(|| {
                lower.ends_with(".lnk").then(|| apps::resolve_lnk(&app.path)).flatten().map(|t| apps::exe_name(&t))
            });
        let name = app.name.clone().or_else(|| (app.path == START_PATH).then(|| "Start".into())).unwrap_or_else(|| {
            Path::new(&app.path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        });
        Item {
            kind: Kind::App,
            name,
            launch: app.path.clone(),
            args: app.args.clone(),
            icon_key: icon_source(&app.path),
            custom_icon: app.icon.clone().or_else(|| match_name.as_deref().and_then(|m| exe_icon(icons, m))),
            match_name,
            pinned: Some(app.clone()),
            windows: Vec::new(),
            bounce: None,
        }
    }

    fn running(w: &AppWindow, name: String, icons: &BTreeMap<String, String>) -> Item {
        Item {
            kind: Kind::App,
            name,
            launch: w.exe.clone(),
            args: None,
            icon_key: icon_source(&w.exe),
            custom_icon: exe_icon(icons, &w.exe_name()),
            match_name: Some(w.exe_name()),
            pinned: None,
            windows: vec![w.clone()],
            bounce: None,
        }
    }

    /// What its bitmap is cached under: a custom icon replaces the app's own.
    fn bitmap_key(&self) -> &str {
        self.custom_icon.as_deref().unwrap_or(&self.icon_key)
    }

    fn separator() -> Item {
        Item {
            kind: Kind::Separator,
            name: String::new(),
            launch: "|separator|".into(),
            args: None,
            icon_key: String::new(),
            custom_icon: None,
            match_name: None,
            pinned: None,
            windows: Vec::new(),
            bounce: None,
        }
    }

    /// The Trash, showing its full or empty icon (`trash_icon` / `trash_icon_full` when set).
    fn trash(cfg: &DockCfg, full: bool) -> Item {
        let custom = if full { &cfg.trash_icon_full } else { &cfg.trash_icon };
        Item {
            kind: Kind::Trash,
            name: "Trash".into(),
            launch: "shell:RecycleBinFolder".into(),
            args: None,
            icon_key: if full { TRASH_FULL } else { TRASH_EMPTY }.into(),
            custom_icon: (!custom.is_empty()).then(|| custom.clone()),
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
            gap: tile * cfg.item_spacing.clamp(0.0, 100.0) / 100.0,
            pad: tile * 0.1,
            shelf_h,
            sep_w: tile * 0.22,
            radius: shelf_h * 0.3,
            margin: cfg.bottom_margin.max(0.0),
        }
    }
}

/// Result of laying out a row of items for the current hover state.
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

    let mut centers = Vec::with_capacity(n);
    let mut x = 0.0;
    for &s in &sizes {
        centers.push(x + s / 2.0);
        x += s + gap;
    }

    // Anchor: map the mouse position in the base row to the same relative spot in the magnified row.
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
    let shift = (span0(k).0 + u * span0(k).1) - (span(k).0 + u * span(k).1);

    let items: Vec<(f32, f32)> = centers.iter().zip(&sizes).map(|(c, s)| (c + shift, *s)).collect();
    let left = items[0].0 - items[0].1 / 2.0 - pad;
    let right = items[n - 1].0 + items[n - 1].1 / 2.0 + pad;
    Layout { items, left, right }
}

/// A logo picked from Dashboard Icons (saved as `dash-*.png`): drawn on a tile
/// like shell icons, where other custom icons are already tile-shaped.
fn is_library_logo(path: &str) -> bool {
    Path::new(path).file_name().is_some_and(|f| f.to_string_lossy().starts_with("dash-"))
}

/// Finder and Start have icons we draw ourselves; our other windows show
/// DockSide's own icon.
fn icon_source(path: &str) -> String {
    match path {
        FINDER_PATH => finder_icon::KEY.to_string(),
        START_PATH => start_icon::KEY.to_string(),
        SETTINGS_PATH | PICKER_PATH | ONBOARDING_PATH => {
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
        }
        _ => path.to_string(),
    }
}

/// Our drawn icons are already tiles.
fn is_drawn_icon(key: &str) -> bool {
    key == finder_icon::KEY || key == start_icon::KEY
}

/// Names for DockSide's own windows, which show in the Dock like apps.
fn own_app_name(path: &str) -> Option<&'static str> {
    match path {
        FINDER_PATH => Some("Finder"),
        SETTINGS_PATH => Some("System Settings"),
        PICKER_PATH => Some("Icon Picker"),
        ONBOARDING_PATH => Some("Setup Assistant"),
        _ => None,
    }
}

/// The icon picker and Setup Assistant only exist while in use: nothing to keep in the Dock.
fn pinnable(path: &str) -> bool {
    path != PICKER_PATH && path != ONBOARDING_PATH
}

/// Pinned files that no longer exist are skipped (URIs and shell ids always pass).
fn pin_exists(app: &DockApp) -> bool {
    let p = Path::new(&app.path);
    !(p.is_absolute() && !app.path.contains("::") && !p.exists())
}

/// Display order of item indices: the dragged item moves to its drop slot,
/// or disappears while it's dragged out of the Dock.
fn display_order(n: usize, drag: Option<&Drag>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if let Some(d) = drag {
        order.remove(d.from);
        if !d.out {
            order.insert(d.to.min(order.len()), d.from);
        }
    }
    order
}

/// Layout in display order, mapped back to item indices.
struct Slots {
    lay: Layout,
    /// (center x, size) per item index; `None` for an item dragged out.
    by_item: Vec<Option<(f32, f32)>>,
}

impl Slots {
    fn get(&self, i: usize) -> Option<(f32, f32)> {
        self.by_item.get(i).copied().flatten()
    }
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
    press_at: (f32, f32),
    drag: Option<Drag>,
    file_hover: Option<FileHover>,
    /// Eased icon centers (screen points) by launch target, used while slots are moving.
    eased: HashMap<String, f32>,
    ease_left: f32,
    /// 0 = fully shown, 1 = fully hidden (auto-hide).
    hide_t: f32,
    idle: f32,
    /// The desktop has focus (for `show_on_desktop`).
    on_desktop: bool,
    pub menu_item: Option<usize>,
    pub tracking: bool,
    reserved: bool,
    /// `[app_icons]`: custom icons by exe file name.
    app_icons: BTreeMap<String, String>,
    /// The Recycle Bin has something in it.
    trash_full: bool,
    pub dirty: bool,
}

impl Dock {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: Monitor) -> Result<Self> {
        let d = &cfg.dock;
        let mut items: Vec<Item> = d.apps.iter().filter(|a| pin_exists(a)).map(|a| Item::pinned(a, &cfg.app_icons)).collect();
        let trash_full = d.show_trash && crate::win::system::trash_full();
        if d.show_trash {
            items.push(Item::separator());
            items.push(Item::trash(d, trash_full));
        }

        let metrics = Metrics::new(d);
        let placeholder = RECT { left: 0, top: 0, right: 1, bottom: 1 };
        let reserved = d.reserve_space && !d.autohide;
        // With its space reserved, only the desktop is ever behind the Dock.
        let source = if reserved { crate::win::glass::Source::Wallpaper } else { crate::win::glass::Source::Screen };
        let mut surface = Surface::with_layer(gfx, placeholder, monitor.scale, d.backdrop, theme.dock_tint, true, source)?;
        // A see-through ("clear") shelf would show its own shadow through it.
        if d.shadow && d.backdrop != crate::config::Backdrop::Clear {
            if let Err(e) = surface.set_shadow(gfx, if theme.dark { 0.42 } else { 0.24 }) {
                log!("dock shadow failed: {e}");
            }
        }
        let on_desktop = apps::is_desktop(unsafe { GetForegroundWindow() });
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
            press_at: (0.0, 0.0),
            drag: None,
            file_hover: None,
            eased: HashMap::new(),
            ease_left: 0.0,
            hide_t: if d.autohide && !(d.show_on_desktop && on_desktop) { 1.0 } else { 0.0 },
            idle: 0.0,
            on_desktop,
            menu_item: None,
            tracking: false,
            reserved,
            app_icons: cfg.app_icons.clone(),
            trash_full,
            dirty: true,
        };
        dock.fit_window()?;
        crate::dock_drop::register(dock.surface.content);
        Ok(dock)
    }

    /// Applies a new pinned-app list in place: running windows and bounces are
    /// kept, and icons slide from where they are now to their new slots.
    pub fn set_apps(&mut self, apps: &[DockApp]) {
        self.snapshot_positions();
        self.cfg.apps = apps.to_vec();
        self.drag = None;
        self.pressed = None;
        self.menu_item = None;

        let mut old: Vec<Item> = std::mem::take(&mut self.items).into_iter().filter(|i| i.kind == Kind::App).collect();
        let mut items: Vec<Item> = Vec::new();
        for app in apps.iter().filter(|a| pin_exists(a)) {
            let mut item = Item::pinned(app, &self.app_icons);
            if let Some(pos) = old.iter().position(|o| o.launch.eq_ignore_ascii_case(&app.path)) {
                let prev = old.remove(pos);
                item.windows = prev.windows;
                item.bounce = prev.bounce;
            }
            items.push(item);
        }
        // Apps that are still running stay in the Dock unpinned.
        for prev in old.into_iter().filter(|o| !o.windows.is_empty()) {
            let mut item = Item::running(&prev.windows[0], prev.name.clone(), &self.app_icons);
            item.windows = prev.windows;
            items.push(item);
        }
        if self.cfg.show_trash {
            items.push(Item::separator());
            items.push(Item::trash(&self.cfg, self.trash_full));
        }
        self.items = items;
        if let Err(e) = self.fit_window() {
            log!("dock resize failed: {e}");
        }
        self.ease_left = EASE_TIME;
        self.dirty = true;
    }

    /// Records current icon centers as the starting point for easing.
    fn snapshot_positions(&mut self) {
        if self.ease_left > 0.0 {
            return; // already easing from live positions
        }
        let slots = self.slots();
        let left_pt = self.left_pt();
        // Store in screen points so a window resize doesn't make icons jump.
        self.eased.clear();
        for (i, item) in self.items.iter().enumerate() {
            if let Some((cx, _)) = slots.get(i) {
                self.eased.insert(item.launch.clone(), left_pt + cx);
            }
        }
    }

    fn left_pt(&self) -> f32 {
        self.surface.rect.left as f32 / self.surface.scale
    }

    fn pinned_count(&self) -> usize {
        self.items.iter().take_while(|i| i.pinned.is_some()).count()
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    fn width_of(&self, i: usize) -> f32 {
        if self.items[i].kind == Kind::Separator {
            self.metrics.sep_w
        } else {
            self.metrics.tile
        }
    }

    /// Sizes the window to fit the fully magnified Dock plus tooltip room.
    fn fit_window(&mut self) -> Result<()> {
        let m = self.metrics;
        let total0 = (0..self.items.len()).map(|i| self.width_of(i)).sum::<f32>()
            + m.gap * self.items.len().saturating_sub(1) as f32
            + 2.0 * m.pad;
        let grow = (m.max - m.tile) * 2.6 * 2.0;
        let width_pt = (total0 + grow + 280.0).min(self.monitor.width_pt());
        let height_pt = m.margin + m.shelf_h + m.max.max(m.tile * 1.55) + LABEL_H + 16.0;
        let s = self.monitor.scale;
        let (w, h) = ((width_pt * s).round() as i32, (height_pt * s).round() as i32);
        let mon = self.monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        self.surface.set_rect(RECT { left, top: mon.bottom - h, right: left + w, bottom: mon.bottom })
    }

    /// Vertical slide for auto-hide, in points.
    fn hide_offset(&self) -> f32 {
        self.hide_t * (self.metrics.shelf_h + self.metrics.margin + 6.0)
    }

    fn shelf_bottom(&self) -> f32 {
        self.surface.height_pt() - self.metrics.margin + self.hide_offset()
    }

    fn icon_bottom(&self) -> f32 {
        let m = &self.metrics;
        self.shelf_bottom() - (m.shelf_h - m.tile) / 2.0 - m.tile * 0.02
    }

    fn slots(&self) -> Slots {
        let m = &self.metrics;
        let order = display_order(self.items.len(), self.drag.as_ref());
        let widths: Vec<f32> = order.iter().map(|&i| self.width_of(i)).collect();
        let scalable: Vec<bool> = order.iter().map(|&i| self.items[i].kind != Kind::Separator).collect();
        let lay = compute_layout(&widths, &scalable, m.max, m.gap, m.pad, self.surface.width_pt(), self.mouse.0, self.hover_t);
        let mut by_item = vec![None; self.items.len()];
        for (slot, &i) in order.iter().enumerate() {
            by_item[i] = Some(lay.items[slot]);
        }
        Slots { lay, by_item }
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

    /// The bottom-edge strip that reveals a hidden Dock. It also covers the
    /// margin under the shelf, so moving between shelf and edge isn't a "leave".
    fn reveal_rect(&self, lay: &Layout) -> Rect {
        let h = self.surface.height_pt();
        let strip = self.metrics.margin + 2.0;
        Rect::new(lay.left, h - strip, lay.right - lay.left, strip)
    }

    fn item_at(&self, x: f32, y: f32) -> Option<usize> {
        let slots = self.slots();
        if !self.hit_rect(&slots.lay).contains(x, y) {
            return None;
        }
        let gap = self.metrics.gap;
        (0..self.items.len())
            .find(|&i| {
                slots.get(i).is_some_and(|(cx, s)| x >= cx - s / 2.0 - gap / 2.0 && x < cx + s / 2.0 + gap / 2.0)
            })
            .filter(|&i| self.items[i].kind != Kind::Separator)
    }

    /// Attaches the current window list to items, adding/removing running apps.
    /// Returns true when an animation should start (icons sliding to new slots).
    pub fn update_running(&mut self, windows: &[AppWindow], names: &mut AppNames, cfg: &Config) -> bool {
        if self.drag.is_some() {
            return false; // indices must stay stable mid-drag
        }
        self.snapshot_positions();
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
                let name = own_app_name(&w.exe).map(String::from).unwrap_or_else(|| names.name_for(&w.exe, &cfg.app_names));
                self.items.insert(at, Item::running(w, name, &cfg.app_icons));
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
                self.ease_left = EASE_TIME;
                if let Err(e) = self.fit_window() {
                    log!("dock resize failed: {e}");
                }
            }
            self.dirty = true;
        }
        self.ease_left > 0.0
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

    /// Where the icon of the app owning `hwnd` is on screen, in physical pixels
    /// (under the screen's bottom edge while auto-hidden), for windows minimizing into it.
    /// A window the Dock hasn't listed yet (one just opening) gets its app's icon.
    pub fn icon_rect(&self, hwnd: HWND) -> Option<RECT> {
        let i = self.items.iter().position(|it| it.windows.iter().any(|w| w.hwnd == hwnd)).or_else(|| {
            let exe = apps::exe_name(&apps::process_path(apps::effective_pid(hwnd))?);
            self.items.iter().position(|it| it.kind == Kind::App && it.match_name.as_deref() == Some(exe.as_str()))
        })?;
        let (cx, size) = self.slots().get(i)?;
        let bottom = self.icon_bottom();
        let (s, r) = (self.surface.scale, self.surface.rect);
        let px = |v: f32| (v * s).round() as i32;
        Some(RECT {
            left: r.left + px(cx - size / 2.0),
            top: r.top + px(bottom - size),
            right: r.left + px(cx + size / 2.0),
            bottom: r.top + px(bottom),
        })
    }

    /// Switches the Trash between its empty and full icons; returns true when it changed.
    pub fn set_trash_full(&mut self, full: bool) -> bool {
        if full == self.trash_full {
            return false;
        }
        self.trash_full = full;
        let trash = Item::trash(&self.cfg, full);
        for item in self.items.iter_mut().filter(|i| i.kind == Kind::Trash) {
            item.icon_key = trash.icon_key.clone();
            item.custom_icon = trash.custom_icon.clone();
        }
        self.dirty = true;
        true
    }

    /// Records whether the desktop has focus; returns true when an animation should start.
    pub fn set_on_desktop(&mut self, on: bool) -> bool {
        let changed = self.on_desktop != on;
        self.on_desktop = on;
        changed && self.cfg.autohide && self.cfg.show_on_desktop
    }

    /// Advances animations; returns true while anything is still moving.
    pub fn tick(&mut self, dt: f32) -> bool {
        let mut animating = false;

        let target = if self.hovering && self.cfg.magnification && self.drag.is_none() { 1.0 } else { 0.0 };
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

        if self.cfg.autohide {
            let wants_shown = self.hovering
                || self.drag.is_some()
                || self.file_hover.is_some()
                || self.menu_item.is_some()
                || self.pressed.is_some()
                || (self.cfg.show_on_desktop && self.on_desktop);
            let target = if wants_shown {
                self.idle = 0.0;
                0.0
            } else {
                self.idle += dt;
                if self.idle >= HIDE_DELAY {
                    1.0
                } else {
                    animating = true;
                    self.hide_t
                }
            };
            if self.hide_t != target {
                self.hide_t = approach(self.hide_t, target, dt, 11.0);
                animating = true;
                self.dirty = true;
            }
        }

        if self.ease_left > 0.0 {
            self.ease_left = (self.ease_left - dt).max(0.0);
            let slots = self.slots();
            let left_pt = self.left_pt();
            for (i, item) in self.items.iter().enumerate() {
                if let Some((cx, _)) = slots.get(i) {
                    let cx = left_pt + cx;
                    let e = self.eased.entry(item.launch.clone()).or_insert(cx);
                    *e = if self.ease_left == 0.0 { cx } else { approach(*e, cx, dt, 18.0) };
                }
            }
            animating = true;
            self.dirty = true;
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

        if self.drag.is_none() {
            if let Some(i) = self.pressed {
                let (dx, dy) = (x - self.press_at.0, y - self.press_at.1);
                if self.items[i].pinned.is_some() && (dx * dx + dy * dy).sqrt() > DRAG_THRESHOLD {
                    let cx = self.slots().get(i).map(|s| s.0).unwrap_or(x);
                    self.drag = Some(Drag { from: i, to: i, grab_dx: x - cx, x, y, out: false });
                    self.eased.clear();
                }
            }
        }

        if let Some(mut d) = self.drag {
            d.x = x;
            d.y = y;
            let shelf_top = self.shelf_bottom() - self.metrics.shelf_h;
            d.out = y < shelf_top - self.metrics.tile * 1.1;
            // Drop slot: how many other pinned icons sit left of the dragged icon's center.
            let others = Drag { out: true, ..d };
            let slots_without = {
                let saved = self.drag.replace(others);
                let s = self.slots();
                self.drag = saved;
                s
            };
            let center = x - d.grab_dx;
            let pinned = self.pinned_count();
            d.to = (0..pinned)
                .filter(|&i| i != d.from)
                .filter(|&i| slots_without.get(i).is_some_and(|(cx, _)| cx < center))
                .count()
                .min(pinned.saturating_sub(1));
            self.drag = Some(d);
            self.ease_left = EASE_TIME;
            self.hovering = true;
            self.dirty = true;
            return true;
        }

        let slots = self.slots();
        let inside = self.hit_rect(&slots.lay).contains(x, y)
            || (self.cfg.autohide && self.reveal_rect(&slots.lay).contains(x, y));
        let changed = inside != self.hovering;
        self.hovering = inside;
        self.dirty = true;
        changed || self.cfg.autohide
    }

    pub fn on_leave(&mut self) -> bool {
        if self.drag.is_some() {
            return false;
        }
        self.tracking = false;
        self.pressed = None;
        let changed = self.hovering;
        self.hovering = false;
        self.dirty = true;
        changed || self.cfg.autohide
    }

    /// What dropping `paths` on item `i` would do.
    fn file_drop_on(&self, i: usize, paths: &[PathBuf]) -> FileDrop {
        let item = &self.items[i];
        match item.kind {
            _ if paths.is_empty() => FileDrop::None,
            Kind::Trash => FileDrop::Trash,
            Kind::Separator => FileDrop::None,
            Kind::App if item.launch == FINDER_PATH => {
                if paths.len() == 1 && paths[0].is_dir() { FileDrop::Open } else { FileDrop::None }
            }
            Kind::App if [START_PATH, SETTINGS_PATH, PICKER_PATH, ONBOARDING_PATH].contains(&item.launch.as_str()) => FileDrop::None,
            Kind::App => FileDrop::Open,
        }
    }

    /// A drag from another app moved to (x, y): the Dock shows and magnifies as
    /// for the mouse, and resting on a running app's icon brings that app forward.
    /// Returns what a drop here would do, plus an action to run now.
    pub fn on_file_drag(&mut self, x: f32, y: f32, paths: &[PathBuf]) -> (FileDrop, Option<Action>) {
        self.on_move(x, y);
        let item = self.item_at(x, y);
        let hover = self.file_hover.get_or_insert(FileHover { item, since: Instant::now(), sprung: false });
        if hover.item != item {
            *hover = FileHover { item, since: Instant::now(), sprung: false };
        }
        let mut spring = None;
        if let Some(i) = item.filter(|&i| !self.items[i].windows.is_empty()) {
            if !hover.sprung && hover.since.elapsed().as_secs_f32() >= SPRING_DELAY {
                hover.sprung = true;
                spring = Some(Action::BringToFront(self.items[i].windows.iter().map(|w| w.hwnd).collect()));
            }
        }
        (item.map_or(FileDrop::None, |i| self.file_drop_on(i, paths)), spring)
    }

    pub fn on_file_leave(&mut self) -> bool {
        self.file_hover = None;
        self.on_leave()
    }

    pub fn on_file_drop(&mut self, x: f32, y: f32, paths: &[PathBuf]) -> (FileDrop, Option<Action>) {
        let item = self.item_at(x, y);
        self.on_file_leave();
        let Some(i) = item else { return (FileDrop::None, None) };
        let drop = self.file_drop_on(i, paths);
        let item = &self.items[i];
        let action = match drop {
            FileDrop::None => None,
            FileDrop::Trash => Some(Action::TrashFiles(paths.to_vec())),
            FileDrop::Open if item.launch == FINDER_PATH => Some(Action::OpenFinder(Some(paths[0].clone()))),
            FileDrop::Open => {
                let files = paths.iter().map(|p| format!("\"{}\"", p.display())).collect::<Vec<_>>().join(" ");
                let args = match item.args.as_deref().map(str::trim) {
                    Some(a) if !a.is_empty() => format!("{a} {files}"),
                    _ => files,
                };
                Some(Action::Launch { path: item.launch.clone(), args: Some(args) })
            }
        };
        (drop, action)
    }

    pub fn on_down(&mut self, x: f32, y: f32) {
        self.pressed = self.item_at(x, y);
        self.press_at = (x, y);
        self.dirty = true;
    }

    /// Mouse capture was lost mid-drag (e.g. a system dialog): drop the drag.
    pub fn cancel_drag(&mut self) {
        if self.drag.take().is_some() {
            self.pressed = None;
            self.ease_left = EASE_TIME;
            self.dirty = true;
        }
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> Option<Action> {
        let pressed = self.pressed.take();
        self.dirty = true;

        if let Some(d) = self.drag.take() {
            self.ease_left = EASE_TIME;
            // The dropped icon glides from under the pointer into its slot.
            let from_x = self.left_pt() + d.x - d.grab_dx;
            self.eased.insert(self.items[d.from].launch.clone(), from_x);
            let item = self.items.remove(d.from);
            if !d.out {
                self.items.insert(d.to, item);
            } else {
                let _ = self.fit_window();
            }
            let apps: Vec<DockApp> = self.items.iter().filter_map(|i| i.pinned.clone()).collect();
            return Some(Action::SetDockApps(apps));
        }

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
                let hwnds: Vec<HWND> = item.windows.iter().map(|w| w.hwnd).collect();
                if item.launch == PICKER_PATH {
                    // A panel, not a window: it can't minimize.
                    Some(Action::BringToFront(hwnds))
                } else if hwnds.contains(&fg) && !apps::is_minimized(fg) {
                    // Already in front: minimize it, like the Windows taskbar.
                    let open = hwnds.into_iter().filter(|&h| !apps::is_minimized(h)).collect();
                    Some(Action::MinimizeWindows(open))
                } else {
                    // Behind other windows or minimized: restore and bring all its windows forward.
                    Some(Action::BringToFront(hwnds))
                }
            }
        }
    }

    /// Right-click on the separator or an empty part of the shelf (like macOS).
    fn options_menu(&self, x: f32, y: f32) -> Option<(usize, Vec<Entry>, Placement)> {
        if !self.hit_rect(&self.slots().lay).contains(x, y) {
            return None;
        }
        let (hide, mag) = (self.cfg.autohide, self.cfg.magnification);
        let entries = vec![
            Entry::item(
                if hide { "Turn Hiding Off" } else { "Turn Hiding On" },
                Action::SetConfig { table: "dock", key: "autohide", value: ConfigValue::Bool(!hide) },
            ),
            Entry::item(
                if mag { "Turn Magnification Off" } else { "Turn Magnification On" },
                Action::SetConfig { table: "dock", key: "magnification", value: ConfigValue::Bool(!mag) },
            ),
            Entry::Separator,
            Entry::item("Dock Settings…", Action::OpenSettings(Some(crate::settings::Page::Dock))),
        ];
        let s = self.surface.scale;
        let shelf_top = self.shelf_bottom() - self.metrics.shelf_h;
        let placement = Placement::Above {
            cx: self.surface.rect.left as f32 / s + x,
            bottom: self.surface.rect.top as f32 / s + shelf_top.min(y) - 10.0,
        };
        Some((usize::MAX, entries, placement))
    }

    pub fn context_menu(&self, x: f32, y: f32) -> Option<(usize, Vec<Entry>, Placement)> {
        let Some(idx) = self.item_at(x, y) else { return self.options_menu(x, y) };
        let item = &self.items[idx];
        let entries = match item.kind {
            Kind::Separator => return None,
            Kind::Trash => {
                // Sets the icon for the state the Trash is in now (empty or full).
                let target = IconTarget { pinned: None, exe: None, trash: Some(self.trash_full) };
                vec![
                    Entry::item("Open", Action::Open(item.launch.clone())),
                    Entry::item("Change Icon\u{2026}", Action::ChangeIcon { target, name: item.name.clone() }),
                    Entry::Separator,
                    Entry::item("Empty Trash", Action::EmptyTrash),
                ]
            }
            Kind::App => {
                let mut v = Vec::new();
                for w in &item.windows {
                    v.push(Entry::item(w.title.clone(), Action::Activate(w.hwnd)));
                }
                if !v.is_empty() {
                    v.push(Entry::Separator);
                }
                if item.launch == FINDER_PATH {
                    v.push(Entry::item("New Finder Window", Action::OpenFinder(None)));
                    v.push(Entry::Separator);
                }
                if pinnable(&item.launch) {
                    v.push(
                        Entry::item(
                            "Keep in Dock",
                            Action::TogglePin { path: item.launch.clone(), name: item.name.clone(), pin: item.pinned.is_none() },
                        )
                        .checked(item.pinned.is_some()),
                    );
                }
                if Path::new(&item.launch).exists() {
                    v.push(Entry::item("Show in Explorer", Action::Reveal(item.launch.clone())));
                }
                let target = IconTarget {
                    pinned: item.pinned.as_ref().map(|p| p.path.clone()),
                    exe: item.match_name.clone().filter(|m| m.ends_with(".exe")),
                    trash: None,
                };
                if target.pinned.is_some() || target.exe.is_some() {
                    v.push(Entry::item("Change Icon\u{2026}", Action::ChangeIcon { target, name: item.name.clone() }));
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
        let (cx, size) = self.slots().get(idx)?;
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
            if item.kind == Kind::Separator || self.bitmaps.contains_key(item.bitmap_key()) {
                continue;
            }
            let key = item.icon_key.as_str();
            let px = match (key, item.custom_icon.as_deref()) {
                (finder_icon::KEY, None) => icons.builtin(key, || finder_icon::render(gfx)),
                (start_icon::KEY, None) => icons.builtin(key, || start_icon::render(gfx)),
                // The shell's Recycle Bin icon follows its state: cache one per state.
                (TRASH_EMPTY | TRASH_FULL, None) => icons.builtin(key, || load_shell_icon(RECYCLE_BIN)),
                (TRASH_EMPTY | TRASH_FULL, custom) => icons.get(&gfx.wic, RECYCLE_BIN, custom),
                (_, custom) => icons.get(&gfx.wic, key, custom),
            };
            let bmp = px.and_then(|px| px.to_bitmap(self.surface.canvas.target()));
            self.bitmaps.insert(item.bitmap_key().to_string(), bmp);
        }

        let slots = self.slots();
        let shelf = self.shelf_rect(&slots.lay);
        let hit = self.hit_rect(&slots.lay);
        let reveal = self.reveal_rect(&slots.lay);
        let m = self.metrics;
        self.surface.set_backdrop_shape(shelf, m.radius);

        // Icon centers, eased while slots are moving.
        let left_pt = self.left_pt();
        let centers: Vec<Option<(f32, f32)>> = (0..self.items.len())
            .map(|i| {
                let (cx, size) = slots.get(i)?;
                let eased = self.eased.get(&self.items[i].launch).filter(|_| self.ease_left > 0.0);
                Some((eased.map(|e| e - left_pt).unwrap_or(cx), size))
            })
            .collect();

        let drag = self.drag;
        let label: Option<(usize, String)> = if let Some(d) = drag {
            d.out.then(|| (d.from, "Remove".to_string()))
        } else if self.hovering && self.pressed.is_none() && self.menu_item.is_none() && self.hide_t < 0.01 {
            self.item_at(self.mouse.0, self.mouse.1).map(|i| (i, self.items[i].name.clone()))
        } else {
            None
        };
        let font = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Center);
        let label_w = label.as_ref().map(|(_, t)| gfx.measure(t, &font) + 22.0).unwrap_or(0.0);
        let icon_bottom = self.icon_bottom();
        let has_backdrop = self.surface.backdrop.is_some();
        let squircle = self.cfg.icon_style == IconStyle::Squircle;
        let autohide = self.cfg.autohide;
        let border = self.cfg.border;
        let fully_hidden = self.hide_t >= 0.999;
        let win_w = self.surface.width_pt();
        let (items, bitmaps, pressed, menu_item) = (&self.items, &self.bitmaps, self.pressed, self.menu_item);

        let draw_icon = |p: &crate::gfx::Painter, item: &Item, rect: Rect, opacity: f32, darken: bool| {
            let Some(Some(bmp)) = bitmaps.get(item.bitmap_key()) else {
                p.fill_round(rect.inset(rect.w * 0.08, rect.w * 0.08), rect.w * 0.2, theme.text_disabled);
                return;
            };
            // Our drawn icons and most custom icons are already tiles; library logos aren't.
            let tiled = !is_drawn_icon(&item.icon_key) && item.custom_icon.as_deref().is_none_or(is_library_logo);
            let draw_rect = if squircle && tiled {
                let tile = rect.inset(rect.w * 0.06, rect.w * 0.06);
                p.fill_round(tile.offset(0.0, rect.w * 0.01), tile.w * 0.225, Color::gray(0, 0.12 * opacity));
                // macOS-style tile: white in light mode, near-black (with a faint edge) in dark mode.
                let tile_color = if theme.dark { Color::rgba(28, 28, 30, opacity) } else { Color::WHITE.alpha(opacity) };
                p.fill_round(tile, tile.w * 0.225, tile_color);
                if theme.dark {
                    p.stroke_round(tile.inset(0.25, 0.25), tile.w * 0.225, Color::gray(255, 0.14 * opacity), 0.5);
                }
                tile.inset(tile.w * 0.16, tile.w * 0.16)
            } else {
                rect
            };
            p.bitmap(bmp, draw_rect, opacity);
            if darken {
                p.tint_bitmap(bmp, draw_rect, Color::gray(0, 0.35));
            }
        };

        let r = self.surface.render(gfx, |p| {
            if !fully_hidden {
                p.fill_rect(hit, Color::HIT);
                p.fill_round(shelf, m.radius, if has_backdrop { theme.dock_tint.alpha(0.0) } else { theme.dock_tint.alpha(1.0) });
                if border {
                    p.stroke_round(shelf.inset(0.5, 0.5), m.radius - 0.5, theme.dock_border, 1.0);
                }
            }
            if autohide {
                p.fill_rect(reveal, Color::HIT);
            }

            for (i, item) in items.iter().enumerate() {
                let Some((cx, size)) = centers[i] else { continue };
                if drag.is_some_and(|d| d.from == i) {
                    continue; // drawn on top, under the pointer
                }
                if item.kind == Kind::Separator {
                    let inset = m.shelf_h * 0.2;
                    p.line(cx, shelf.y + inset, cx, shelf.bottom() - inset, theme.dock_separator, 1.0);
                    continue;
                }
                let lift = item.bounce_height(m.tile);
                let rect = Rect::new(cx - size / 2.0, icon_bottom - size - lift, size, size);
                draw_icon(p, item, rect, 1.0, pressed == Some(i) || menu_item == Some(i));
                if !item.windows.is_empty() {
                    let dot = (m.tile / 48.0 * 2.0).max(1.5);
                    p.fill_circle(cx, shelf.bottom() - (m.shelf_h - m.tile) / 4.0 - 0.5, dot, theme.indicator);
                }
            }

            let mut label_anchor = None;
            if let Some(d) = drag {
                let item = &items[d.from];
                let size = m.tile;
                let cx = d.x - d.grab_dx;
                let top = if d.out { d.y - size / 2.0 } else { icon_bottom - size };
                let rect = Rect::new(cx - size / 2.0, top, size, size);
                draw_icon(p, item, rect, if d.out { 0.6 } else { 1.0 }, false);
                label_anchor = Some((cx, top));
            } else if let Some((i, _)) = &label {
                if let Some((cx, size)) = centers[*i] {
                    label_anchor = Some((cx, icon_bottom - size - items[*i].bounce_height(m.tile)));
                }
            }

            if let (Some((_, text)), Some((cx, top))) = (&label, label_anchor) {
                let x = (cx - label_w / 2.0).clamp(2.0, win_w - label_w - 2.0);
                let r = Rect::new(x, (top - 10.0 - LABEL_H).max(0.0), label_w, LABEL_H);
                p.fill_round(r, 6.0, theme.label_bg);
                p.stroke_round(r.inset(0.25, 0.25), 6.0, theme.menu_border, 0.5);
                p.text(text, &font, r, theme.label_text);
            }
        });
        if let Err(e) = r {
            log!("dock render failed: {e}");
        }
    }
}

impl Dock {
    /// Gives back reserved screen space early, before a replacement reserves its own.
    pub fn release_space(&mut self) {
        if std::mem::take(&mut self.reserved) {
            appbar::remove(self.surface.content);
        }
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        crate::dock_drop::revoke(self.surface.content);
        self.release_space();
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

    #[test]
    fn drag_moves_item_in_display_order() {
        let d = Drag { from: 0, to: 2, grab_dx: 0.0, x: 0.0, y: 0.0, out: false };
        assert_eq!(display_order(4, Some(&d)), vec![1, 2, 0, 3]);
        let out = Drag { out: true, ..d };
        assert_eq!(display_order(4, Some(&out)), vec![1, 2, 3]);
        assert_eq!(display_order(3, None), vec![0, 1, 2]);
    }
}
