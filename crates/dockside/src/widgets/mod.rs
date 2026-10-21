//! Desktop widgets, like macOS Sonoma's: frosted cards that live on the
//! wallpaper, below every app window.
//!
//! * **Layer**: each widget is a surface (blurred backdrop + layered content)
//!   kept just above the desktop window in the z-order. Windows can't lift it:
//!   z-order changes are refused (see [`TAG`]) unless we're placing it.
//! * **Arranging**: drag a widget anywhere; it snaps to the screen edges and
//!   to its neighbours. Right-click to resize or remove it, or open Settings
//!   to add more. Positions are saved to `[[widgets.items]]` in the config.
//! * **Data**: weather, calendar, Bluetooth batteries and Now Playing come
//!   from worker threads ([`sources`], and the island's calendar / media
//!   sources), started only for the widgets on screen. Clocks and stats tick
//!   with the app's one-second timer.

mod glyphs;
pub mod sources;
mod store;
mod views;

use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Timelike;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Backdrop, Config, WidgetItem, WidgetKind, WidgetSize, WidgetsCfg};
use crate::finder::text::{KeyResult, TextField};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconPixels;
use crate::gfx::Gfx;
use crate::island::calendar::{self, CalEvent};
use crate::island::media::{Cmd, Media, NowPlaying};
use crate::island::sources::{Inbox, Signal, Stop};
use crate::menu::{Action, Entry, Placement};
use crate::surface::Surface;
use crate::theme::Theme;
use crate::win::system::{self, Battery};
use crate::win::{apps, Monitor};
use sources::{Sampler, SharedDevices, SharedWeather, SysStats, WeatherState, Zone};
use store::Store;

pub const RADIUS: f32 = 22.0;
/// Transparent room around a widget for its shadow, in points.
const SHADOW: f32 = 16.0;
const SMALL: f32 = 170.0;
/// Space between widgets: two small ones and a gap make a medium one.
const GAP: f32 = 24.0;
/// Distance from the screen edges.
const EDGE: f32 = 24.0;
/// How close an edge must come to snap.
const SNAP: f32 = 12.0;
const DRAG_THRESHOLD: f32 = 4.0;
/// How far ahead calendar widgets look: the rest of this month's grid and beyond.
const CALENDAR_HORIZON: i64 = 42 * 86_400;

/// `GWLP_USERDATA` of widget windows, so the window procedure can keep them
/// on the desktop layer.
pub const TAG: isize = 0x5749_4447;
static PLACING: AtomicBool = AtomicBool::new(false);

/// True while we're the ones changing a widget's z-order.
pub fn placing() -> bool {
    PLACING.load(Ordering::Relaxed)
}

pub const KINDS: [WidgetKind; 10] = [
    WidgetKind::Clock,
    WidgetKind::WorldClock,
    WidgetKind::Weather,
    WidgetKind::Calendar,
    WidgetKind::UpNext,
    WidgetKind::Reminders,
    WidgetKind::Notes,
    WidgetKind::Batteries,
    WidgetKind::NowPlaying,
    WidgetKind::System,
];

pub fn title(kind: WidgetKind) -> &'static str {
    match kind {
        WidgetKind::Clock => "Clock",
        WidgetKind::WorldClock => "World Clock",
        WidgetKind::Weather => "Weather",
        WidgetKind::Calendar => "Calendar",
        WidgetKind::UpNext => "Up Next",
        WidgetKind::Batteries => "Batteries",
        WidgetKind::Notes => "Notes",
        WidgetKind::Reminders => "Reminders",
        WidgetKind::NowPlaying => "Now Playing",
        WidgetKind::System => "System",
    }
}

/// The sizes a widget comes in; the first is what it's added at.
pub fn sizes(kind: WidgetKind) -> &'static [WidgetSize] {
    use WidgetSize::*;
    match kind {
        WidgetKind::Clock | WidgetKind::Batteries | WidgetKind::NowPlaying | WidgetKind::System => &[Small, Medium],
        WidgetKind::Weather => &[Medium, Small, Large],
        _ => &[Small, Medium, Large],
    }
}

/// Width and height in points.
pub fn dims(size: WidgetSize) -> (f32, f32) {
    let big = 2.0 * SMALL + GAP;
    match size {
        WidgetSize::Small => (SMALL, SMALL),
        WidgetSize::Medium => (big, SMALL),
        WidgetSize::Large => (big, big),
    }
}

fn item_rect(i: &WidgetItem) -> Rect {
    let (w, h) = dims(i.size);
    Rect::new(i.x, i.y, w, h)
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

/// Snaps a widget being dragged to `area`'s edges and to its neighbours
/// (aligned, or a gap apart), keeping it on `screen`.
fn snap(x: f32, y: f32, w: f32, h: f32, others: &[Rect], area: Rect, screen: Rect) -> (f32, f32) {
    let mut xs = vec![area.x, area.right() - w];
    let mut ys = vec![area.y, area.bottom() - h];
    for o in others {
        xs.extend([o.x, o.right() - w, o.right() + GAP, o.x - w - GAP]);
        ys.extend([o.y, o.bottom() - h, o.bottom() + GAP, o.y - h - GAP]);
    }
    let pick = |v: f32, targets: &[f32]| {
        targets.iter().copied().filter(|t| (t - v).abs() <= SNAP).min_by(|a, b| (a - v).abs().total_cmp(&(b - v).abs())).unwrap_or(v)
    };
    let x = pick(x, &xs).clamp(screen.x, (screen.right() - w).max(screen.x));
    let y = pick(y, &ys).clamp(screen.y, (screen.bottom() - h).max(screen.y));
    (x.round(), y.round())
}

/// The first spot, column by column from the top left, where a widget of
/// `size` fits without touching the others.
fn free_spot(items: &[WidgetItem], size: WidgetSize, area: Rect) -> (f32, f32) {
    let (w, h) = dims(size);
    let taken: Vec<Rect> = items.iter().map(item_rect).collect();
    let step = (SMALL + GAP) / 2.0;
    let mut x = area.x;
    while x + w <= area.right() {
        let mut y = area.y;
        while y + h <= area.bottom() {
            let spot = Rect::new(x - GAP + 1.0, y - GAP + 1.0, w + 2.0 * GAP - 2.0, h + 2.0 * GAP - 2.0);
            if !taken.iter().any(|t| overlaps(*t, spot)) {
                return (x, y);
            }
            y += step;
        }
        x += step;
    }
    (area.x, area.y)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Note,
    Reminder(usize),
    NewReminder,
    Prev,
    Play,
    Next,
    Art,
}

/// Text being typed into a Notes widget, or a new reminder.
pub struct Edit {
    pub field: TextField,
    /// Notes: how far the text is scrolled up, in points.
    pub scroll: f32,
}

#[derive(Clone, Debug)]
pub enum WidgetCmd {
    Add(WidgetKind),
    Remove(u32),
    Resize(u32, WidgetSize),
    /// Fetch the weather again now.
    Refresh,
    /// Put back the default widgets, where they start out (notes are cleared).
    Reset,
}

pub enum WidgetEvent {
    None,
    Action(Action),
    /// Write these items (after a drag).
    Save(Vec<WidgetItem>),
    /// Bring this app's media session forward.
    OpenMedia(String),
    /// Give a widget the keyboard (it's being typed into).
    Focus(HWND),
}

/// Everything the views draw from.
pub struct Data {
    pub weather: WeatherState,
    pub events: Vec<CalEvent>,
    pub devices: Vec<(String, u8)>,
    pub battery: Option<Battery>,
    pub sys: SysStats,
    pub now: Option<NowPlaying>,
    pub zones: Vec<Zone>,
    pub first_weekday: chrono::Weekday,
    pub h24: bool,
}

/// This PC's battery, then Bluetooth devices.
pub fn devices(data: &Data) -> Vec<views::Device> {
    let mut v = Vec::new();
    if let Some(b) = data.battery {
        v.push(views::Device { name: "This PC".into(), percent: b.percent, charging: b.charging, laptop: true });
    }
    for (name, pct) in &data.devices {
        v.push(views::Device { name: name.clone(), percent: *pct, charging: false, laptop: false });
    }
    v
}

struct Widget {
    item: WidgetItem,
    surface: Surface,
    dirty: bool,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    tracking: bool,
    edit: Option<Edit>,
    art: Option<(u64, ID2D1Bitmap)>,
}

impl Widget {
    fn body(&self) -> Rect {
        let (w, h) = dims(self.item.size);
        Rect::new(SHADOW, SHADOW, w, h)
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }
}

struct Drag {
    id: u32,
    /// Pointer offset inside the widget, in points.
    grab: (f32, f32),
    start: (f32, f32),
    moved: bool,
}

/// What the background sources were started for; they restart when it changes.
#[derive(Clone, PartialEq, Debug, Default)]
struct SourceKey {
    weather: Option<(String, bool, bool)>,
    calendar: Option<String>,
    devices: bool,
    media: bool,
}

struct Sources {
    key: SourceKey,
    stop: Stop,
    calendar: Option<Inbox>,
    weather: Option<SharedWeather>,
    devices: Option<SharedDevices>,
    media: Option<Media>,
}

impl Drop for Sources {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

/// The top-level window that holds the desktop icons (Progman, or a WorkerW
/// on older builds or with a live wallpaper).
fn desktop_window() -> Option<HWND> {
    use windows::core::w;
    unsafe {
        let progman = FindWindowW(w!("Progman"), None).ok()?;
        if FindWindowExW(progman, None, w!("SHELLDLL_DefView"), None).is_ok() {
            return Some(progman);
        }
        let mut worker = HWND::default();
        while let Ok(w) = FindWindowExW(None, worker, w!("WorkerW"), None) {
            if FindWindowExW(w, None, w!("SHELLDLL_DefView"), None).is_ok() {
                return Some(w);
            }
            worker = w;
        }
        Some(progman)
    }
}

/// Puts a widget just above the desktop: under every app window.
fn place_on_desktop(hwnd: HWND) {
    let pid = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };
    let after = desktop_window().and_then(|d| {
        let mut above = unsafe { GetWindow(d, GW_HWNDPREV) }.ok();
        // Our other widgets sit there too; go above them.
        while let Some(a) = above.filter(|a| apps::window_pid(*a) == pid) {
            above = unsafe { GetWindow(a, GW_HWNDPREV) }.ok();
        }
        above
    });
    PLACING.store(true, Ordering::Relaxed);
    unsafe {
        let _ = SetWindowPos(hwnd, after.unwrap_or(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
    PLACING.store(false, Ordering::Relaxed);
}

fn cursor_pt(monitor: &Monitor) -> (f32, f32) {
    let mut p = POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p);
    }
    ((p.x - monitor.rect.left) as f32 / monitor.scale, (p.y - monitor.rect.top) as f32 / monitor.scale)
}

/// Shadow, card and contents of one widget, in surface points.
fn paint(p: &crate::gfx::Painter, body: Rect, ctx: &views::Ctx, solid: bool, hits: &mut Vec<(Rect, Hit)>) {
    let theme = ctx.theme;
    for i in 0..8 {
        let grow = i as f32 * 1.6;
        let a = 0.022 * (8 - i) as f32 / 8.0;
        p.fill_round(body.inset(-grow, -grow).offset(0.0, 3.0), RADIUS + grow, theme.shadow.alpha(a));
    }
    p.clear_round(body, RADIUS);
    let fill = match (solid, theme.dark) {
        (true, true) => Color::rgba(36, 36, 38, 1.0),
        (true, false) => Color::rgba(250, 250, 250, 1.0),
        // Not drawn over the blur, but it keeps the whole card clickable.
        _ => Color::HIT,
    };
    p.fill_round(body, RADIUS, fill);
    views::draw(p, body, ctx, hits);
    let rim = if theme.dark { Color::gray(255, 0.1) } else { Color::gray(255, 0.35) };
    p.stroke_round(body.inset(0.5, 0.5), RADIUS - 0.5, rim, 1.0);
}

pub struct Widgets {
    widgets: Vec<Widget>,
    cfg: WidgetsCfg,
    calendar_ics: String,
    monitor: Monitor,
    /// Screen area widgets may use, in points (below the menu bar).
    screen: Rect,
    pub data: Data,
    sources: Option<Sources>,
    sampler: Sampler,
    store: Store,
    drag: Option<Drag>,
    ticks: u64,
    minute: u32,
    hwnd: isize,
    msg_data: u32,
    msg_media: u32,
}

const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_UP: u16 = 0x26;
const VK_DOWN: u16 = 0x28;

impl Widgets {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: Monitor, hwnd: isize, msg_data: u32, msg_media: u32) -> Widgets {
        let top = if cfg.menubar.enabled { cfg.menubar.height } else { 0.0 };
        let screen = Rect::new(0.0, top, monitor.width_pt(), (monitor.rect.bottom - monitor.rect.top) as f32 / monitor.scale - top);
        let mut w = Widgets {
            widgets: Vec::new(),
            cfg: cfg.widgets.clone(),
            calendar_ics: cfg.island.calendar_ics.clone(),
            monitor,
            screen,
            data: Data {
                weather: WeatherState::Loading,
                events: Vec::new(),
                devices: Vec::new(),
                battery: system::battery(),
                sys: SysStats::default(),
                now: None,
                zones: cfg.widgets.world_clocks.iter().filter_map(|z| sources::parse_zone(z)).collect(),
                first_weekday: sources::first_weekday(),
                h24: cfg.menubar.clock_format.contains("%H"),
            },
            sources: None,
            sampler: Sampler::default(),
            store: Store::load(),
            drag: None,
            ticks: 0,
            minute: chrono::Local::now().minute(),
            hwnd,
            msg_data,
            msg_media,
        };
        for item in cfg.widgets.items.clone() {
            w.add_widget(gfx, theme, item);
        }
        w.restart_sources();
        if w.has(WidgetKind::System) {
            w.sample();
        }
        w
    }

    fn add_widget(&mut self, gfx: &Gfx, theme: &Theme, mut item: WidgetItem) {
        let (w, h) = dims(item.size);
        // Keep it on screen (the resolution may have changed since it was placed).
        item.x = item.x.clamp(self.screen.x, (self.screen.right() - w).max(self.screen.x));
        item.y = item.y.clamp(self.screen.y, (self.screen.bottom() - h).max(self.screen.y));
        let s = self.monitor.scale;
        let (left, top) = (self.monitor.rect.left, self.monitor.rect.top);
        let rect = RECT {
            left: left + ((item.x - SHADOW) * s).round() as i32,
            top: top + ((item.y - SHADOW) * s).round() as i32,
            right: left + ((item.x + w + SHADOW) * s).round() as i32,
            bottom: top + ((item.y + h + SHADOW) * s).round() as i32,
        };
        let tint = if theme.dark { Color::rgba(30, 30, 32, 0.55) } else { Color::rgba(246, 246, 248, 0.55) };
        let mut surface = match Surface::with_layer(gfx, rect, s, self.cfg.backdrop, tint, false, crate::win::glass::Source::Wallpaper) {
            Ok(s) => s,
            Err(e) => return log!("{} widget failed: {e}", title(item.kind)),
        };
        surface.set_backdrop_shape(Rect::new(SHADOW, SHADOW, w, h), RADIUS);
        unsafe {
            SetWindowLongPtrW(surface.content, GWLP_USERDATA, TAG);
            if let Some(bd) = surface.backdrop {
                SetWindowLongPtrW(bd, GWLP_USERDATA, TAG);
            }
        }
        place_on_desktop(surface.root());
        self.widgets.push(Widget { item, surface, dirty: true, hits: Vec::new(), hover: None, tracking: false, edit: None, art: None });
    }

    fn has(&self, kind: WidgetKind) -> bool {
        self.widgets.iter().any(|w| w.item.kind == kind)
    }

    fn mark(&mut self, kinds: &[WidgetKind]) {
        for w in self.widgets.iter_mut().filter(|w| kinds.contains(&w.item.kind)) {
            w.dirty = true;
        }
    }

    pub fn items(&self) -> Vec<WidgetItem> {
        self.widgets.iter().map(|w| w.item.clone()).collect()
    }

    /// Applies a changed `[widgets]` config: widgets that are still there stay
    /// (moved if needed); new and resized ones are created.
    pub fn sync(&mut self, gfx: &Gfx, theme: &Theme, cfg: &Config) {
        self.cfg = cfg.widgets.clone();
        self.calendar_ics = cfg.island.calendar_ics.clone();
        self.data.zones = cfg.widgets.world_clocks.iter().filter_map(|z| sources::parse_zone(z)).collect();
        self.data.h24 = cfg.menubar.clock_format.contains("%H");
        let mut old = std::mem::take(&mut self.widgets);
        for item in &cfg.widgets.items {
            let same = |w: &Widget| w.item.id == item.id && w.item.kind == item.kind && w.item.size == item.size;
            match old.iter().position(same) {
                Some(i) => {
                    let mut w = old.remove(i);
                    if (w.item.x, w.item.y) != (item.x, item.y) {
                        let s = self.monitor.scale;
                        w.surface.move_to(
                            self.monitor.rect.left + ((item.x - SHADOW) * s).round() as i32,
                            self.monitor.rect.top + ((item.y - SHADOW) * s).round() as i32,
                        );
                    }
                    w.item = item.clone();
                    w.dirty = true;
                    self.widgets.push(w);
                }
                None => self.add_widget(gfx, theme, item.clone()),
            }
        }
        // Whatever's left in `old` was removed; dropping it closes the windows.
        self.store.save_if_dirty();
        drop(old);
        self.restart_sources();
    }

    fn source_key(&self) -> SourceKey {
        let fahrenheit = sources::fahrenheit(&self.cfg.temperature_unit);
        SourceKey {
            weather: self.has(WidgetKind::Weather).then(|| (self.cfg.location.clone(), fahrenheit, self.data.h24)),
            calendar: (self.has(WidgetKind::Calendar) || self.has(WidgetKind::UpNext)).then(|| self.calendar_ics.clone()),
            devices: self.has(WidgetKind::Batteries),
            media: self.has(WidgetKind::NowPlaying),
        }
    }

    /// (Re)starts the background sources the current widgets need.
    fn restart_sources(&mut self) {
        let key = self.source_key();
        if self.sources.as_ref().is_some_and(|s| s.key == key) {
            return;
        }
        // Dropping the old set stops its threads.
        self.sources = None;
        if key == SourceKey::default() {
            return;
        }
        let stop = Stop::default();
        let calendar = key.calendar.as_ref().map(|ics| {
            let inbox = Inbox::new(self.hwnd, self.msg_data);
            calendar::watch(inbox.clone(), stop.clone(), calendar::links(ics), CALENDAR_HORIZON);
            inbox
        });
        let weather = key.weather.as_ref().map(|(location, fahrenheit, h24)| {
            let shared = SharedWeather::default();
            sources::watch_weather(shared.clone(), stop.clone(), location.clone(), *fahrenheit, *h24, self.hwnd, self.msg_data);
            shared
        });
        let devices = key.devices.then(|| {
            let shared = SharedDevices::default();
            sources::watch_devices(shared.clone(), stop.clone(), self.hwnd, self.msg_data);
            shared
        });
        let media = key.media.then(|| Media::start(self.hwnd, self.msg_media));
        if key.weather.is_some() {
            self.data.weather = WeatherState::Loading;
        }
        self.sources = Some(Sources { key, stop, calendar, weather, devices, media });
    }

    /// Re-asserts the desktop layer (after Show Desktop, Explorer changes...).
    pub fn place(&self) {
        for w in &self.widgets {
            place_on_desktop(w.surface.root());
        }
    }

    // --- data --------------------------------------------------------------

    /// A background source has news.
    pub fn on_data(&mut self) {
        let Some(src) = &self.sources else { return };
        let mut changed = Vec::new();
        if let Some(inbox) = &src.calendar {
            for s in inbox.drain() {
                if let Signal::Calendar(events) = s {
                    self.data.events = events;
                    changed.extend([WidgetKind::Calendar, WidgetKind::UpNext]);
                }
            }
        }
        if let Some(w) = src.weather.as_ref().and_then(|w| w.lock().ok().map(|w| w.clone())) {
            if w != self.data.weather {
                self.data.weather = w;
                changed.push(WidgetKind::Weather);
            }
        }
        if let Some(d) = src.devices.as_ref().and_then(|d| d.lock().ok().map(|d| d.clone())) {
            if d != self.data.devices {
                self.data.devices = d;
                changed.push(WidgetKind::Batteries);
            }
        }
        self.mark(&changed);
    }

    pub fn on_media(&mut self) {
        if let Some(m) = self.sources.as_ref().and_then(|s| s.media.as_ref()) {
            self.data.now = m.snapshot();
            self.mark(&[WidgetKind::NowPlaying]);
        }
    }

    fn sample(&mut self) {
        self.sampler.sample();
        self.data.sys = self.sampler.stats.clone();
    }

    /// Every second: clocks, playback progress, stats, and the date rolling over.
    pub fn on_tick(&mut self) {
        self.ticks += 1;
        self.mark(&[WidgetKind::Clock, WidgetKind::WorldClock]);
        if self.data.now.as_ref().is_some_and(|n| n.playing) {
            self.mark(&[WidgetKind::NowPlaying]);
        }
        if self.ticks % 2 == 0 && self.has(WidgetKind::System) {
            self.sample();
            self.mark(&[WidgetKind::System]);
        }
        if self.ticks % 10 == 0 {
            let b = system::battery();
            if b != self.data.battery {
                self.data.battery = b;
                self.mark(&[WidgetKind::Batteries]);
            }
        }
        let minute = chrono::Local::now().minute();
        if minute != self.minute {
            self.minute = minute;
            let now = calendar::unix_now();
            self.data.events.retain(|e| e.end > now);
            self.mark(&[WidgetKind::Calendar, WidgetKind::UpNext]);
        }
        if self.store.sweep() {
            self.mark(&[WidgetKind::Reminders]);
        }
        self.store.save_if_dirty();
    }

    // --- drawing -----------------------------------------------------------

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        let solid = self.cfg.backdrop == Backdrop::None;
        let Widgets { widgets, data, store, .. } = self;
        for w in widgets.iter_mut().filter(|w| w.dirty) {
            w.dirty = false;
            if w.item.kind == WidgetKind::NowPlaying {
                let art = data.now.as_ref().and_then(|n| n.art.clone());
                if art.as_ref().map(|a| a.id) != w.art.as_ref().map(|(id, _)| *id) {
                    let rt = w.surface.canvas.target().clone();
                    w.art = art.and_then(|a| {
                        let px = IconPixels { width: a.size, height: a.size, data: a.data.clone() };
                        px.to_bitmap(&rt).map(|b| (a.id, b))
                    });
                }
            }
            let body = w.body();
            let mut hits = Vec::new();
            let ctx = views::Ctx {
                theme,
                data,
                store,
                item: &w.item,
                edit: w.edit.as_ref(),
                art: w.art.as_ref().map(|(_, b)| b),
                hover: w.hover,
            };
            let result = w.surface.render(gfx, |p| paint(p, body, &ctx, solid, &mut hits));
            if let Err(e) = result {
                log!("widget render failed: {e}");
            }
            w.hits = hits;
        }
    }

    /// Repaint everything (font or accent changed).
    pub fn invalidate(&mut self) {
        for w in &mut self.widgets {
            w.dirty = true;
        }
    }

    pub fn needs_render(&self) -> bool {
        self.widgets.iter().any(|w| w.dirty)
    }

    // --- input -------------------------------------------------------------

    fn index(&self, hwnd: HWND) -> Option<usize> {
        self.widgets.iter().position(|w| w.surface.content == hwnd)
    }

    pub fn owns(&self, hwnd: HWND) -> bool {
        self.index(hwnd).is_some()
    }

    pub fn scale(&self) -> f32 {
        self.monitor.scale
    }

    /// Pointer moved over a widget (or anywhere, while one is held).
    pub fn on_move(&mut self, hwnd: HWND, x: f32, y: f32) -> bool {
        let Some(i) = self.index(hwnd) else { return false };
        if let Some(d) = &mut self.drag {
            let (cx, cy) = cursor_pt(&self.monitor);
            if !d.moved && ((cx - d.start.0).powi(2) + (cy - d.start.1).powi(2)).sqrt() < DRAG_THRESHOLD {
                return false;
            }
            d.moved = true;
            let id = d.id;
            let (gx, gy) = d.grab;
            let Some(j) = self.widgets.iter().position(|w| w.item.id == id) else { return false };
            let (w, h) = dims(self.widgets[j].item.size);
            let others: Vec<Rect> = self.widgets.iter().filter(|o| o.item.id != id).map(|o| item_rect(&o.item)).collect();
            let area = Rect::new(self.screen.x + EDGE, self.screen.y + EDGE, self.screen.w - 2.0 * EDGE, self.screen.h - 2.0 * EDGE);
            let (nx, ny) = snap(cx - gx, cy - gy, w, h, &others, area, self.screen);
            let s = self.monitor.scale;
            let wd = &mut self.widgets[j];
            if (nx, ny) != (wd.item.x, wd.item.y) {
                wd.item.x = nx;
                wd.item.y = ny;
                wd.surface.move_to(
                    self.monitor.rect.left + ((nx - SHADOW) * s).round() as i32,
                    self.monitor.rect.top + ((ny - SHADOW) * s).round() as i32,
                );
            }
            return false;
        }
        let w = &mut self.widgets[i];
        let hover = w.hit_at(x, y);
        if hover != w.hover {
            w.hover = hover;
            w.dirty = true;
        }
        if !w.tracking {
            w.tracking = true;
            return true;
        }
        false
    }

    pub fn on_leave(&mut self, hwnd: HWND) {
        if let Some(i) = self.index(hwnd) {
            let w = &mut self.widgets[i];
            w.tracking = false;
            if w.hover.take().is_some() {
                w.dirty = true;
            }
        }
    }

    pub fn on_down(&mut self, hwnd: HWND, x: f32, y: f32) {
        let Some(i) = self.index(hwnd) else { return };
        let w = &self.widgets[i];
        // Clicks in text being typed place the caret instead of dragging.
        if w.edit.is_some() && matches!(w.hit_at(x, y), Some(Hit::Note | Hit::NewReminder)) {
            return;
        }
        let body = w.body();
        self.drag = Some(Drag { id: w.item.id, grab: (x - body.x, y - body.y), start: cursor_pt(&self.monitor), moved: false });
    }

    /// The mouse capture went away mid-drag: keep where it got to.
    pub fn on_capture_lost(&mut self) -> WidgetEvent {
        match self.drag.take() {
            Some(d) if d.moved => WidgetEvent::Save(self.items()),
            _ => WidgetEvent::None,
        }
    }

    pub fn on_up(&mut self, hwnd: HWND, x: f32, y: f32, gfx: &Gfx, theme: &Theme) -> WidgetEvent {
        if let Some(d) = self.drag.take() {
            if d.moved {
                return WidgetEvent::Save(self.items());
            }
        }
        let Some(i) = self.index(hwnd) else { return WidgetEvent::None };
        let hit = self.widgets[i].hit_at(x, y);
        let kind = self.widgets[i].item.kind;
        if hit != Some(Hit::NewReminder) && kind == WidgetKind::Reminders && self.widgets[i].edit.is_some() {
            self.end_edit(i);
        }
        let media = |cmd| {
            if let Some(m) = self.sources.as_ref().and_then(|s| s.media.as_ref()) {
                m.send(cmd);
            }
            WidgetEvent::None
        };
        match hit {
            Some(Hit::Prev) => media(Cmd::Prev),
            Some(Hit::Play) => media(Cmd::TogglePlay),
            Some(Hit::Next) => media(Cmd::Next),
            Some(Hit::Art) => match &self.data.now {
                Some(n) => WidgetEvent::OpenMedia(n.app_id.clone()),
                None => WidgetEvent::None,
            },
            Some(Hit::Reminder(r)) => {
                self.store.toggle_reminder(r);
                self.store.save_if_dirty();
                self.mark(&[WidgetKind::Reminders]);
                WidgetEvent::None
            }
            Some(Hit::NewReminder) => {
                let w = &mut self.widgets[i];
                if w.edit.is_none() {
                    w.edit = Some(Edit { field: TextField::default(), scroll: 0.0 });
                    w.dirty = true;
                }
                WidgetEvent::Focus(w.surface.content)
            }
            Some(Hit::Note) => {
                let w = &mut self.widgets[i];
                let id = w.item.id;
                let area = views::note_area(w.body());
                let edit = w.edit.get_or_insert_with(|| Edit { field: TextField::new(self.store.note(id)), scroll: 0.0 });
                if let Some(layout) = views::note_layout(gfx, theme, &edit.field.text, area.w) {
                    let at = views::index_at(&layout, &edit.field.text, x - area.x, y - area.y + edit.scroll);
                    edit.field.set_caret(at, false);
                }
                w.dirty = true;
                WidgetEvent::Focus(w.surface.content)
            }
            None => match kind {
                WidgetKind::Clock | WidgetKind::WorldClock => WidgetEvent::Action(Action::Open("ms-clock:".into())),
                WidgetKind::Calendar | WidgetKind::UpNext => WidgetEvent::Action(Action::Open("outlookcal:".into())),
                WidgetKind::Batteries => WidgetEvent::Action(Action::Open("ms-settings:bluetooth".into())),
                WidgetKind::System => WidgetEvent::Action(Action::Open("taskmgr.exe".into())),
                _ => WidgetEvent::None,
            },
        }
    }

    pub fn context_menu(&mut self, hwnd: HWND, x: f32, y: f32) -> Option<(Vec<Entry>, Placement)> {
        let i = self.index(hwnd)?;
        let w = &self.widgets[i];
        let (id, kind) = (w.item.id, w.item.kind);
        let mut entries: Vec<Entry> = sizes(kind)
            .iter()
            .map(|&s| {
                let label = match s {
                    WidgetSize::Small => "Small",
                    WidgetSize::Medium => "Medium",
                    WidgetSize::Large => "Large",
                };
                Entry::item(label, Action::Widget(WidgetCmd::Resize(id, s))).checked(s == w.item.size)
            })
            .collect();
        entries.push(Entry::Separator);
        if kind == WidgetKind::Weather {
            entries.push(Entry::item("Refresh Weather", Action::Widget(WidgetCmd::Refresh)));
            entries.push(Entry::Separator);
        }
        entries.push(Entry::item("Edit Widgets…", Action::OpenSettings(Some(crate::settings::Page::Widgets))));
        entries.push(Entry::item("Reset Widgets…", Action::Widget(WidgetCmd::Reset)));
        entries.push(Entry::item(format!("Remove “{}”", title(kind)), Action::Widget(WidgetCmd::Remove(id))));
        let r = w.surface.rect;
        let s = w.surface.scale;
        Some((entries, Placement::Below { x: r.left as f32 / s + x, y: r.top as f32 / s + y }))
    }

    /// Finishes typing into widget `i`: the note or new reminder is kept.
    fn end_edit(&mut self, i: usize) {
        let w = &mut self.widgets[i];
        let Some(edit) = w.edit.take() else { return };
        match w.item.kind {
            WidgetKind::Notes => self.store.set_note(w.item.id, edit.field.string()),
            WidgetKind::Reminders => self.store.add_reminder(&edit.field.string()),
            _ => {}
        }
        w.dirty = true;
        self.store.save_if_dirty();
        self.mark(&[WidgetKind::Reminders]);
    }

    /// The widget lost the keyboard.
    pub fn on_deactivate(&mut self, hwnd: HWND) {
        if let Some(i) = self.index(hwnd) {
            self.end_edit(i);
        }
    }

    /// Keeps a note's caret in view.
    fn scroll_to_caret(w: &mut Widget, gfx: &Gfx, theme: &Theme) {
        let area = views::note_area(w.body());
        let Some(edit) = &mut w.edit else { return };
        let Some(layout) = views::note_layout(gfx, theme, &edit.field.text, area.w) else { return };
        let (_, y, h) = views::caret_pos(&layout, &edit.field.text, edit.field.caret);
        if y < edit.scroll {
            edit.scroll = y;
        } else if y + h > edit.scroll + area.h {
            edit.scroll = y + h - area.h;
        }
    }

    /// Inserts text as typed (notes keep line breaks).
    fn insert(edit: &mut Edit, s: &str, multiline: bool) {
        if !multiline {
            return edit.field.insert(s);
        }
        let (a, b) = edit.field.range();
        let chars: Vec<char> = s.chars().filter(|&c| c != '\r').collect();
        let n = chars.len();
        edit.field.text.splice(a..b, chars);
        edit.field.set_caret(a + n, false);
    }

    pub fn on_key(&mut self, hwnd: HWND, vk: u16, ctrl: bool, shift: bool, gfx: &Gfx, theme: &Theme) -> bool {
        let Some(i) = self.index(hwnd) else { return false };
        if self.widgets[i].edit.is_none() {
            return false;
        }
        let notes = self.widgets[i].item.kind == WidgetKind::Notes;
        match vk {
            VK_ESCAPE => {
                self.end_edit(i);
                return true;
            }
            VK_RETURN if !notes => {
                let w = &mut self.widgets[i];
                if let Some(e) = &mut w.edit {
                    self.store.add_reminder(&e.field.string());
                    e.field.clear();
                }
                self.store.save_if_dirty();
                self.mark(&[WidgetKind::Reminders]);
                return true;
            }
            _ => {}
        }
        let area = views::note_area(self.widgets[i].body());
        let w = &mut self.widgets[i];
        let Some(edit) = &mut w.edit else { return false };
        let text = &edit.field.text;
        let line_start = |i: usize| text[..i].iter().rposition(|&c| c == '\n').map_or(0, |p| p + 1);
        let line_end = |i: usize| text[i..].iter().position(|&c| c == '\n').map_or(text.len(), |p| i + p);
        match vk {
            VK_RETURN => Self::insert(edit, "\n", true),
            VK_TAB => {}
            VK_HOME if notes && !ctrl => {
                let to = line_start(edit.field.caret);
                edit.field.set_caret(to, shift);
            }
            VK_END if notes && !ctrl => {
                let to = line_end(edit.field.caret);
                edit.field.set_caret(to, shift);
            }
            VK_UP | VK_DOWN if notes => {
                if let Some(layout) = views::note_layout(gfx, theme, &edit.field.text, area.w) {
                    let (x, y, h) = views::caret_pos(&layout, &edit.field.text, edit.field.caret);
                    let ty = if vk == VK_UP { y - h / 2.0 } else { y + h * 1.5 };
                    let to = if ty < 0.0 { 0 } else { views::index_at(&layout, &edit.field.text, x, ty) };
                    edit.field.set_caret(to, shift);
                }
            }
            _ if notes && ctrl && vk == b'V' as u16 => {
                if let Some(s) = crate::finder::fs::paste_text() {
                    Self::insert(edit, &s, true);
                }
            }
            _ => {
                if edit.field.on_key(vk, ctrl, shift) == KeyResult::Ignored {
                    return true;
                }
            }
        }
        if notes {
            let id = w.item.id;
            let s = edit.field.string();
            Self::scroll_to_caret(w, gfx, theme);
            self.store.set_note(id, s);
        }
        self.widgets[i].dirty = true;
        true
    }

    pub fn on_char(&mut self, hwnd: HWND, c: char, gfx: &Gfx, theme: &Theme) {
        let Some(i) = self.index(hwnd) else { return };
        let w = &mut self.widgets[i];
        let notes = w.item.kind == WidgetKind::Notes;
        let Some(edit) = &mut w.edit else { return };
        if c.is_control() {
            return;
        }
        Self::insert(edit, &c.to_string(), notes);
        if notes {
            let (id, s) = (w.item.id, edit.field.string());
            Self::scroll_to_caret(w, gfx, theme);
            self.store.set_note(id, s);
        }
        self.widgets[i].dirty = true;
    }

    // --- commands ----------------------------------------------------------

    /// The widget list after `cmd`, to be saved; None if nothing's to save.
    pub fn command(&mut self, cmd: WidgetCmd) -> Option<Vec<WidgetItem>> {
        let mut items = self.items();
        let area = Rect::new(self.screen.x + EDGE, self.screen.y + EDGE, self.screen.w - 2.0 * EDGE, self.screen.h - 2.0 * EDGE);
        match cmd {
            WidgetCmd::Add(kind) => {
                let size = sizes(kind)[0];
                let (x, y) = free_spot(&items, size, area);
                let id = items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
                items.push(WidgetItem { id, kind, size, x, y, zone: None });
            }
            WidgetCmd::Remove(id) => {
                items.retain(|i| i.id != id);
                self.store.notes.remove(&id);
            }
            WidgetCmd::Resize(id, size) => {
                let (w, h) = dims(size);
                for i in items.iter_mut().filter(|i| i.id == id) {
                    i.size = size;
                    i.x = i.x.min((self.screen.right() - w).max(self.screen.x));
                    i.y = i.y.min((self.screen.bottom() - h).max(self.screen.y));
                }
            }
            WidgetCmd::Reset => {
                items = crate::config::defaults().widgets.items;
                self.store.clear_notes();
                self.store.save_if_dirty();
            }
            WidgetCmd::Refresh => {
                // A new key restarts the weather thread.
                if let Some(s) = &mut self.sources {
                    s.key.weather = None;
                }
                self.restart_sources();
                self.mark(&[WidgetKind::Weather]);
                return None;
            }
        }
        Some(items)
    }
}

impl Widgets {
    /// Ends any open edit and saves, so a replacement loads the latest state.
    pub fn save_state(&mut self) {
        let open: Vec<usize> = (0..self.widgets.len()).filter(|&i| self.widgets[i].edit.is_some()).collect();
        for i in open {
            self.end_edit(i);
        }
        self.store.save_if_dirty();
    }
}

impl Drop for Widgets {
    fn drop(&mut self) {
        self.save_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u32, size: WidgetSize, x: f32, y: f32) -> WidgetItem {
        WidgetItem { id, kind: WidgetKind::Clock, size, x, y, zone: None }
    }

    /// Paints every widget, light and dark, with sample data into
    /// `$DOCKSIDE_GALLERY-light.bmp` / `-dark.bmp`, to look at by eye.
    #[test]
    #[ignore]
    fn gallery() {
        use crate::gfx::Canvas;
        use std::time::Instant;
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let out = std::env::var("DOCKSIDE_GALLERY").unwrap_or_else(|_| "widgets-gallery".into());
        let gfx = Gfx::new().unwrap();
        let now = calendar::unix_now();
        let ev = |title: &str, from: i64, mins: i64| CalEvent { title: title.into(), location: String::new(), start: now + from, end: now + from + mins * 60 };
        let hours = ["Now", "5PM", "6PM", "7PM", "8PM", "9PM"];
        let w = sources::Weather {
            place: "Lausanne".into(),
            located: true,
            temp: 10.2,
            code: 61,
            is_day: true,
            high: 13.4,
            low: 9.1,
            hours: hours.iter().enumerate().map(|(i, l)| sources::Hour { label: l.to_string(), code: [61, 2, 80, 3, 0, 95][i], is_day: i < 3, temp: 10.0 - i as f32 * 0.6 }).collect(),
            days: ["Today", "Thu", "Fri", "Sat", "Sun"].iter().enumerate().map(|(i, l)| sources::Day { label: l.to_string(), code: [80, 0, 95, 71, 2][i], low: 6.0 + i as f32, high: 12.0 + (i % 3) as f32 * 2.0 }).collect(),
        };
        let data = Data {
            weather: WeatherState::Ready(w),
            events: vec![ev("Design review", -600, 45), ev("1:1 with Sam", 7200, 30), ev("Dentist", 86_400, 60)],
            devices: vec![("AirPods Pro".into(), 80), ("Xbox Wireless Controller".into(), 45), ("MX Master 3".into(), 15)],
            battery: Some(Battery { percent: 72, charging: true }),
            sys: SysStats {
                cpu: 0.34,
                cpu_history: (0..60).map(|i| 0.3 + 0.2 * (i as f32 / 5.0).sin()).collect(),
                mem: 0.62,
                mem_used: 10 << 30,
                mem_total: 16 << 30,
                disk: 0.71,
                disk_free: 140 << 30,
                down: 1.2e6,
                up: 45e3,
            },
            now: Some(NowPlaying {
                app_id: "Spotify.exe".into(),
                title: "Midnight City".into(),
                artist: "M83".into(),
                playing: true,
                position: 95.0,
                duration: 243.0,
                at: Instant::now(),
                can_prev: true,
                can_next: true,
                can_seek: true,
                art: None,
            }),
            zones: crate::config::defaults().widgets.world_clocks.iter().filter_map(|z| sources::parse_zone(z)).collect(),
            first_weekday: chrono::Weekday::Mon,
            h24: false,
        };
        let mut store = Store::default();
        store.set_note(1, "Groceries\nMilk, eggs, coffee beans\nPick up the dry cleaning before 6pm on Thursday.".into());
        for r in ["Call the dentist", "Renew passport", "Send invoice to Alex", "Water the plants", "Book train tickets"] {
            store.add_reminder(r);
        }
        for dark in [false, true] {
            let mut cfg = crate::config::defaults();
            cfg.general.appearance = if dark { crate::config::Appearance::Dark } else { crate::config::Appearance::Light };
            let theme = Theme::new(&cfg, &gfx);
            let pad = 8.0;
            let (iw, mut placed) = (1100usize, Vec::new());
            let mut y = pad;
            for kind in KINDS {
                let mut x = pad;
                let mut row_h: f32 = 0.0;
                for &size in sizes(kind) {
                    let (w, h) = dims(size);
                    placed.push((kind, size, x, y));
                    x += w + 2.0 * SHADOW + pad;
                    row_h = row_h.max(h + 2.0 * SHADOW);
                }
                y += row_h + pad;
            }
            let ih = y as usize;
            let mut img = vec![0u8; iw * ih * 3];
            for py in 0..ih {
                let t = py as f32 / ih as f32;
                for px in 0..iw {
                    let o = (py * iw + px) * 3;
                    let (r, g, b) = (60.0 + 80.0 * t, 70.0 + 40.0 * t, 120.0 + 40.0 * t);
                    img[o..o + 3].copy_from_slice(&[b as u8, g as u8, r as u8]);
                }
            }
            for (kind, size, x, y) in placed {
                let item = WidgetItem { id: 1, kind, size, x: 0.0, y: 0.0, zone: None };
                let (w, h) = dims(size);
                let (cw, ch) = ((w + 2.0 * SHADOW) as i32, (h + 2.0 * SHADOW) as i32);
                let mut canvas = Canvas::new(&gfx, cw, ch, 1.0).unwrap();
                let ctx = views::Ctx { theme: &theme, data: &data, store: &store, item: &item, edit: None, art: None, hover: None };
                let mut hits = Vec::new();
                // Solid cards: there's no blurred backdrop window here.
                canvas.draw(&gfx, |p| paint(p, Rect::new(SHADOW, SHADOW, w, h), &ctx, true, &mut hits)).unwrap();
                let px = canvas.pixels();
                for cy in 0..ch as usize {
                    for cx in 0..cw as usize {
                        let s = (cy * cw as usize + cx) * 4;
                        let (dx, dy) = (x as usize + cx, y as usize + cy);
                        if dx >= iw || dy >= ih {
                            continue;
                        }
                        let o = (dy * iw + dx) * 3;
                        let a = px[s + 3] as f32 / 255.0;
                        for k in 0..3 {
                            img[o + k] = (px[s + k] as f32 + img[o + k] as f32 * (1.0 - a)).min(255.0) as u8;
                        }
                    }
                }
            }
            // 24-bit bottom-up BMP.
            let stride = (iw * 3 + 3) & !3;
            let size = 54 + stride * ih;
            let mut f = Vec::with_capacity(size);
            f.extend_from_slice(b"BM");
            f.extend_from_slice(&(size as u32).to_le_bytes());
            f.extend_from_slice(&[0; 4]);
            f.extend_from_slice(&54u32.to_le_bytes());
            f.extend_from_slice(&40u32.to_le_bytes());
            f.extend_from_slice(&(iw as i32).to_le_bytes());
            f.extend_from_slice(&(ih as i32).to_le_bytes());
            f.extend_from_slice(&1u16.to_le_bytes());
            f.extend_from_slice(&24u16.to_le_bytes());
            f.extend_from_slice(&[0; 24]);
            for row in (0..ih).rev() {
                f.extend_from_slice(&img[row * iw * 3..(row + 1) * iw * 3]);
                f.extend(std::iter::repeat(0).take(stride - iw * 3));
            }
            std::fs::write(format!("{out}-{}.bmp", if dark { "dark" } else { "light" }), f).unwrap();
        }
    }

    #[test]
    fn sizes_tile() {
        let (mw, mh) = dims(WidgetSize::Medium);
        assert_eq!(mw, 2.0 * SMALL + GAP);
        assert_eq!(mh, SMALL);
        assert_eq!(dims(WidgetSize::Large), (mw, mw));
    }

    #[test]
    fn snaps_to_edges_and_neighbours() {
        let screen = Rect::new(0.0, 24.0, 1280.0, 776.0);
        let area = Rect::new(24.0, 48.0, 1232.0, 728.0);
        // Near the top-left margin.
        assert_eq!(snap(30.0, 52.0, 170.0, 170.0, &[], area, screen), (24.0, 48.0));
        // A gap to the right of a neighbour, aligned with its top.
        let other = Rect::new(24.0, 48.0, 170.0, 170.0);
        assert_eq!(snap(222.0, 55.0, 170.0, 170.0, &[other], area, screen), (218.0, 48.0));
        // Far from anything: left alone, but kept on screen.
        assert_eq!(snap(600.0, 400.0, 170.0, 170.0, &[other], area, screen), (600.0, 400.0));
        assert_eq!(snap(-80.0, 2000.0, 170.0, 170.0, &[], area, screen), (0.0, 630.0));
    }

    #[test]
    fn free_spot_skips_taken() {
        let area = Rect::new(24.0, 48.0, 1232.0, 728.0);
        assert_eq!(free_spot(&[], WidgetSize::Small, area), (24.0, 48.0));
        let items = [item(1, WidgetSize::Small, 24.0, 48.0)];
        let (x, y) = free_spot(&items, WidgetSize::Small, area);
        assert!(!overlaps(Rect::new(x, y, SMALL, SMALL), item_rect(&items[0])));
        assert_eq!(x, 24.0);
        assert_eq!(y, 48.0 + SMALL + GAP);
    }
}
