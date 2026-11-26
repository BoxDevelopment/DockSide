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

pub(crate) mod glyphs;
pub mod live;
pub mod sensors;
pub mod sources;
mod speech;
mod store;
pub mod user;
mod views;

use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Timelike;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{approach, Motion, Spring};
use crate::config::{Backdrop, Config, WidgetItem, WidgetKind, WidgetSize, WidgetsCfg};
use crate::finder::text::{KeyResult, TextField};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconPixels;
use crate::gfx::Gfx;
use crate::island::calendar::{self, CalEvent};
use crate::island::media::{Cmd, Media, NowPlaying};
use crate::island::notifications::Note;
use crate::island::sources::{Inbox, Signal, Stop};
use live::{SharedBands, SharedFeed};
use user::{SharedUser, UserInfo};
use crate::menu::{Action, Entry, Placement};
use crate::surface::Surface;
use crate::theme::Theme;
use crate::win::system::{self, Battery};
use crate::win::{apps, Monitor};
use sensors::{Sampler, SharedGpus, SysStats};
use sources::{SharedDevices, SharedWeather, WeatherState, Zone};
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

pub const KINDS: [WidgetKind; 16] = [
    WidgetKind::Docky,
    WidgetKind::Clock,
    WidgetKind::WorldClock,
    WidgetKind::Weather,
    WidgetKind::Calendar,
    WidgetKind::UpNext,
    WidgetKind::Reminders,
    WidgetKind::Notes,
    WidgetKind::Notifications,
    WidgetKind::Batteries,
    WidgetKind::NowPlaying,
    WidgetKind::MusicWave,
    WidgetKind::Photo,
    WidgetKind::Video,
    WidgetKind::System,
    WidgetKind::User,
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
        WidgetKind::MusicWave => "Music Wave",
        WidgetKind::Photo => "Photo",
        WidgetKind::Video => "Video",
        WidgetKind::Notifications => "Notifications",
        WidgetKind::User => "User",
        WidgetKind::Docky => "Docky",
    }
}

/// The sizes a widget comes in; the first is what it's added at.
pub fn sizes(kind: WidgetKind) -> &'static [WidgetSize] {
    use WidgetSize::*;
    match kind {
        WidgetKind::Clock | WidgetKind::NowPlaying | WidgetKind::MusicWave | WidgetKind::User | WidgetKind::Docky => &[Small, Medium],
        WidgetKind::Weather | WidgetKind::Notifications => &[Medium, Small, Large],
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

/// A widget's width and height in points: its size's, or a Photo sized to its picture.
pub fn item_dims(i: &WidgetItem) -> (f32, f32) {
    i.fit().unwrap_or_else(|| dims(i.size))
}

fn item_rect(i: &WidgetItem) -> Rect {
    let (w, h) = item_dims(i);
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

/// Which edges (or centre lines) to line up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alignment {
    Left,
    Centre,
    Right,
    Top,
    Middle,
    Bottom,
}

impl Alignment {
    const ALL: [Alignment; 6] = [Self::Left, Self::Centre, Self::Right, Self::Top, Self::Middle, Self::Bottom];

    fn label(self) -> &'static str {
        match self {
            Self::Left => "Left Edges",
            Self::Centre => "Centres",
            Self::Right => "Right Edges",
            Self::Top => "Top Edges",
            Self::Middle => "Middles",
            Self::Bottom => "Bottom Edges",
        }
    }
}

/// How far apart two rects are (0 when they touch or overlap).
fn distance(a: Rect, b: Rect) -> f32 {
    let dx = (a.x - b.right()).max(b.x - a.right()).max(0.0);
    let dy = (a.y - b.bottom()).max(b.y - a.bottom()).max(0.0);
    dx.hypot(dy)
}

/// Where `r` goes to line its `how` edge up with the nearest of `others` it
/// can, without overlapping any and staying on `screen`. It's left where it
/// is when it's already lined up with that one; None when nothing fits.
fn aligned(r: Rect, others: &[Rect], how: Alignment, screen: Rect) -> Option<(f32, f32)> {
    let mut near: Vec<Rect> = others.to_vec();
    near.sort_by(|a, b| distance(r, *a).total_cmp(&distance(r, *b)));
    near.into_iter().find_map(|o| {
        let (x, y) = match how {
            Alignment::Left => (o.x, r.y),
            Alignment::Centre => (o.cx() - r.w / 2.0, r.y),
            Alignment::Right => (o.right() - r.w, r.y),
            Alignment::Top => (r.x, o.y),
            Alignment::Middle => (r.x, o.cy() - r.h / 2.0),
            Alignment::Bottom => (r.x, o.bottom() - r.h),
        };
        let x = x.round().clamp(screen.x, (screen.right() - r.w).max(screen.x));
        let y = y.round().clamp(screen.y, (screen.bottom() - r.h).max(screen.y));
        let to = Rect::new(x, y, r.w, r.h);
        (!others.iter().any(|t| overlaps(*t, to))).then_some((x, y))
    })
}

/// The spot in column `col` and row `row` (each 0-2: start, centre, end) of
/// `area` for a `w` × `h` widget.
fn spot(col: u8, row: u8, w: f32, h: f32, area: Rect) -> (f32, f32) {
    let at = |start: f32, span: f32, size: f32, i: u8| (start + (span - size).max(0.0) * i as f32 / 2.0).round();
    (at(area.x, area.w, w, col), at(area.y, area.h, h, row))
}

/// The first spot, column by column from the top left, where a widget of
/// `size` fits without touching the others.
fn free_spot(items: &[WidgetItem], (w, h): (f32, f32), area: Rect) -> (f32, f32) {
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

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Hit {
    Note,
    Reminder(usize),
    NewReminder,
    Prev,
    Play,
    Next,
    Art,
    /// A notification in the list (by its place).
    Notification(usize),
    Lock,
    SignOut,
    Sleep,
    /// "Choose…" on an empty Photo or Video widget.
    Choose,
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
    /// Line a widget up with its nearest neighbour.
    Align(u32, Alignment),
    /// Move a widget to a column and row (each 0-2) of the screen.
    MoveTo(u32, u8, u8),
    /// Move a widget to a free spot on another display (by its id).
    MoveToDisplay(u32, String),
    /// System: show or hide a sensor (by config key).
    ToggleSensor(u32, String),
    /// System: move a sensor to the front (it gets the graph).
    LeadSensor(u32, String),
    /// System: back to what it shows by default.
    DefaultSensors(u32),
    /// Photo / Video: ask for a file (or, `true`, a folder) to show.
    Choose(u32, bool),
    /// User: show or hide one of its extras.
    ToggleOption(u32, &'static str),
    /// Clock: a digital (true) or analog face.
    SetDigital(u32, bool),
    /// Docky: his look (see `docky::STYLES`).
    SetDockyStyle(u32, &'static str),
    /// Add a Docky widget with this look.
    AddDocky(&'static str),
    /// Photo: its size, the part of the picture shown, and (Fit to Picture)
    /// its own width and height in points.
    SetPhoto { id: u32, size: WidgetSize, crop: Option<[f32; 4]>, fit: Option<(f32, f32)> },
    /// Photo: open Edit Photo… for it.
    EditPhoto(u32),
    /// Notifications: clear them from Windows' notification center.
    ClearNotifications,
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
    /// Music Wave: spectrum levels, 0..1 low to high (empty or zero when silent).
    pub bands: Vec<f32>,
    /// Notifications, newest first.
    pub notes: Vec<Note>,
    /// User: who's signed in (None until looked up).
    pub user: Option<UserInfo>,
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
    /// Only Music Wave's bars changed: they alone are redrawn, unless `dirty`
    /// asks for the whole card too.
    bars_dirty: bool,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    tracking: bool,
    edit: Option<Edit>,
    art: Option<(u64, ID2D1Bitmap)>,
    /// Fades in when added; hover and checkmark springs.
    appear: Spring,
    motion: Motion<views::Anim>,
    /// Where the card is on screen, in points: it glides to a new spot
    /// (aligned, or moved) rather than jumping.
    at: (Spring, Spring),
    /// Photo / Video: the worker playing it.
    live: Option<Live>,
    /// The newest frame from `live`, not drawn yet.
    frame: Option<live::Frame>,
    /// Notification icons, by art id.
    pictures: Vec<(u64, ID2D1Bitmap)>,
    /// Hidden behind an app window: nothing moves.
    covered: bool,
    /// Docky: what he's up to.
    pet: crate::docky::Pet,
    /// Docky: the pointer is close by.
    near: bool,
    /// Docky (Auto): picks his look and what to say.
    auto: crate::docky::Auto,
    /// Docky moved (and nothing else did): only the part of the card around
    /// him is redrawn, `drawn_at` being where he was last drawn.
    docky_dirty: bool,
    drawn_at: Option<Rect>,
    /// When he was last drawn moving (for pacing his frames).
    docky_frame: std::time::Instant,
    /// Docky: you asked him something here and he's answering; his reply,
    /// shown until then; and kept awake until then after you talk to him.
    asked: bool,
    reply: Option<(String, std::time::Instant, std::time::Instant)>,
    awake_until: std::time::Instant,
}

impl Widget {
    /// Docky's look right now: his style, or (Auto) the one he's chosen.
    fn docky_look(&self) -> &str {
        match self.item.docky_style() {
            "auto" | "pixel-auto" => self.auto.look(),
            s => s,
        }
    }

    fn auto(&self) -> bool {
        self.item.kind == WidgetKind::Docky && matches!(self.item.docky_style(), "auto" | "pixel-auto")
    }
}

/// A Photo or Video widget's player; it stops when dropped.
struct Live {
    feed: SharedFeed,
    /// What it was started for: the path, size in pixels and (Photo) crop.
    key: (String, (u32, u32), Option<[f32; 4]>),
}

impl Drop for Live {
    fn drop(&mut self) {
        self.feed.stop();
    }
}

/// The glide to a new spot.
const GLIDE: (f32, f32) = (0.32, 0.9);

impl Widget {
    fn body(&self) -> Rect {
        let (w, h) = item_dims(&self.item);
        Rect::new(SHADOW, SHADOW, w, h)
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }
}

/// A cheap coin toss from where the pointer is.
fn x_seed(p: POINT) -> u32 {
    (p.x.unsigned_abs() ^ p.y.unsigned_abs().rotate_left(7)).wrapping_mul(2654435761) >> 31
}

/// What Docky keeps an eye on between polls (see `Widgets::poll_docky`).
struct Watch {
    cursor: POINT,
    /// When the last key press or mouse move was (GetTickCount ms).
    input: u32,
    /// Seconds without input, as of the last poll.
    idle: u32,
    fg: isize,
    /// Which way the pointer last moved fast, and when it turned back on itself: a shake.
    dir: i32,
    turns: Vec<std::time::Instant>,
    last_nod: std::time::Instant,
    /// Typing: since when (without a long pause), and the last key.
    typing_since: std::time::Instant,
    last_typed: std::time::Instant,
    /// The song last announced, and whether music was playing.
    track: String,
    playing: bool,
    /// The machine's CPU use, how many samples in a row it's been flat out,
    /// and when Docky may get hot and bothered again.
    cpu: crate::docky::Cpu,
    hot_streak: u8,
    hot_after: std::time::Instant,
    /// The calendar event he last waved about (its start).
    waved_event: i64,
    /// No dizzy spells or hellos again before these.
    dizzy_after: std::time::Instant,
    hello_after: std::time::Instant,
}

impl Watch {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Watch {
            cursor: cursor_px(),
            input: crate::docky::last_input(),
            idle: 0,
            fg: 0,
            dir: 0,
            turns: Vec::new(),
            last_nod: now,
            typing_since: now,
            last_typed: now.checked_sub(std::time::Duration::from_secs(3600)).unwrap_or(now),
            track: String::new(),
            playing: false,
            cpu: crate::docky::Cpu::default(),
            hot_streak: 0,
            hot_after: now,
            waved_event: 0,
            dizzy_after: now,
            hello_after: now,
        }
    }
}

struct Drag {
    id: u32,
    /// Pointer offset inside the widget, in points.
    grab: (f32, f32),
    /// Where the press was, in physical pixels.
    start: POINT,
    moved: bool,
}

/// What the background sources were started for; they restart when it changes.
#[derive(Clone, PartialEq, Debug, Default)]
struct SourceKey {
    weather: Option<(String, bool, bool)>,
    calendar: Option<String>,
    devices: bool,
    media: bool,
    /// A System widget shows a GPU.
    gpu: bool,
    notes: bool,
    music: bool,
    user: bool,
}

struct Sources {
    key: SourceKey,
    stop: Stop,
    calendar: Option<Inbox>,
    weather: Option<SharedWeather>,
    devices: Option<SharedDevices>,
    media: Option<Media>,
    gpus: Option<SharedGpus>,
    notes: Option<Inbox>,
    bands: Option<SharedBands>,
    /// The device `bands` listens to.
    audio: String,
    user: Option<SharedUser>,
}

impl Drop for Sources {
    fn drop(&mut self) {
        self.stop.stop();
        if let Some(b) = &self.bands {
            b.stop();
        }
    }
}

/// Passed from rebuilt widgets to their replacement: the running sources and
/// what they've reported (sources only report changes, so it isn't sent again).
pub struct Handover {
    sources: Option<Sources>,
    weather: WeatherState,
    events: Vec<CalEvent>,
    devices: Vec<(String, u8)>,
    now: Option<NowPlaying>,
    notes: Vec<Note>,
    user: Option<UserInfo>,
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

/// The pointer, in physical pixels.
fn cursor_px() -> POINT {
    let mut p = POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p);
    }
    p
}

/// A display widgets can go on.
#[derive(Clone, Debug)]
pub struct Screen {
    /// Its id (`win::displays`), stored with the widgets placed on it.
    pub id: String,
    /// The main display: widgets without a display go here.
    pub main: bool,
    pub monitor: Monitor,
    /// Points kept clear at the top (its menu bar).
    pub top: f32,
}

impl Screen {
    /// Where widgets may sit, in points from the display's top-left.
    fn bounds(&self) -> Rect {
        Rect::new(0.0, self.top, self.monitor.width_pt(), self.monitor.height_pt() - self.top)
    }

    /// The bounds less a margin, for placing and snapping.
    fn area(&self) -> Rect {
        let b = self.bounds();
        Rect::new(b.x + EDGE, b.y + EDGE, b.w - 2.0 * EDGE, b.h - 2.0 * EDGE)
    }

    /// The pointer in this display's points.
    fn local(&self, p: POINT) -> (f32, f32) {
        let m = &self.monitor;
        ((p.x - m.rect.left) as f32 / m.scale, (p.y - m.rect.top) as f32 / m.scale)
    }

    /// What a widget on this display stores as its display (nothing for the main one).
    fn tag(&self) -> Option<String> {
        (!self.main).then(|| self.id.clone())
    }
}

/// Puts a Photo / Video frame into `slot`: into the bitmap already there
/// when it's the same size (video frames), else a new one.
fn upload_frame(rt: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget, slot: &mut Option<(u64, ID2D1Bitmap)>, f: live::Frame) {
    if let Some((_, b)) = slot {
        let size = unsafe { b.GetPixelSize() };
        if size.width == f.w && size.height == f.h {
            let ok = unsafe { b.CopyFromMemory(None, f.data.as_ptr() as *const _, f.w * 4) }.is_ok();
            if ok {
                return;
            }
        }
    }
    *slot = IconPixels { width: f.w, height: f.h, data: f.data }.to_bitmap(rt).map(|b| (1, b));
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
    docky: crate::config::DockyCfg,
    watch: Watch,
    calendar_ics: String,
    /// Every display, the main one first.
    screens: Vec<Screen>,
    pub data: Data,
    sources: Option<Sources>,
    sampler: Sampler,
    /// The first GPU's id, as of the last System menu (what `gpu` stands for).
    first_gpu: Option<String>,
    store: Store,
    drag: Option<Drag>,
    ticks: u64,
    minute: u32,
    hwnd: isize,
    msg_data: u32,
    /// Removed (or replaced, when resized) widgets fading out.
    leaving: Vec<Surface>,
    /// Docky's speech box, floating by his widget while you talk to him.
    speech: Option<speech::SpeechBox>,
}

/// Docky's reply bubble: how long it takes to pop in and to fade away (seconds).
const REPLY_IN: f32 = 0.24;
const REPLY_OUT: f32 = 0.32;

const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_UP: u16 = 0x26;
const VK_DOWN: u16 = 0x28;

impl Widgets {
    pub fn new(
        gfx: &Gfx,
        theme: &Theme,
        cfg: &Config,
        mut screens: Vec<Screen>,
        hwnd: isize,
        msg_data: u32,
        handover: Option<Handover>,
    ) -> Widgets {
        if screens.is_empty() {
            let monitor = Monitor::primary();
            let top = if cfg.menubar.enabled { cfg.menubar.height } else { 0.0 };
            screens.push(Screen { id: String::new(), main: true, monitor, top });
        }
        let mut w = Widgets {
            widgets: Vec::new(),
            cfg: cfg.widgets.clone(),
            docky: cfg.docky.clone(),
            watch: Watch::new(),
            calendar_ics: cfg.island.calendar_ics.clone(),
            screens,
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
                bands: Vec::new(),
                notes: Vec::new(),
                user: None,
            },
            sources: None,
            sampler: Sampler::default(),
            first_gpu: None,
            store: Store::load(),
            drag: None,
            ticks: 0,
            minute: chrono::Local::now().minute(),
            hwnd,
            msg_data,
            leaving: Vec::new(),
            speech: None,
        };
        for item in cfg.widgets.items.clone() {
            w.add_widget(gfx, theme, item);
        }
        // Keep the old sources if they're the ones needed; otherwise they stop as they're dropped.
        if let Some(h) = handover.filter(|h| h.sources.as_ref().is_some_and(|s| s.key == w.source_key())) {
            w.sources = h.sources;
            w.data.weather = h.weather;
            w.data.events = h.events;
            w.data.devices = h.devices;
            w.data.now = h.now;
            w.data.notes = h.notes;
            w.data.user = h.user;
        }
        w.restart_sources();
        if w.has(WidgetKind::System) {
            w.sample();
        }
        w
    }

    /// The display `item` is on: the one it names, else (none named, or that
    /// display isn't connected now) the main one. It keeps its display's name,
    /// so it goes back when that display returns.
    fn screen_of(&self, item: &WidgetItem) -> &Screen {
        let named = item.display.as_deref().and_then(|id| self.screens.iter().find(|s| s.id.eq_ignore_ascii_case(id)));
        named.or_else(|| self.screens.iter().find(|s| s.main)).unwrap_or(&self.screens[0])
    }

    fn same_screen(&self, a: &WidgetItem, b: &WidgetItem) -> bool {
        std::ptr::eq(self.screen_of(a), self.screen_of(b))
    }

    fn add_widget(&mut self, gfx: &Gfx, theme: &Theme, mut item: WidgetItem) {
        let (w, h) = item_dims(&item);
        let screen = self.screen_of(&item).clone();
        let bounds = screen.bounds();
        // Keep it on screen (the resolution may have changed since it was placed).
        item.x = item.x.clamp(bounds.x, (bounds.right() - w).max(bounds.x));
        item.y = item.y.clamp(bounds.y, (bounds.bottom() - h).max(bounds.y));
        let s = screen.monitor.scale;
        let (left, top) = (screen.monitor.rect.left, screen.monitor.rect.top);
        let rect = RECT {
            left: left + ((item.x - SHADOW) * s).round() as i32,
            top: top + ((item.y - SHADOW) * s).round() as i32,
            right: left + ((item.x + w + SHADOW) * s).round() as i32,
            bottom: top + ((item.y + h + SHADOW) * s).round() as i32,
        };
        let tint = if theme.dark { Color::rgba(30, 30, 32, 0.55) } else { Color::rgba(248, 248, 250, 0.7) };
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
        surface.set_opacity(0.0);
        let mut appear = Spring::new(0.0, 0.24, 1.0);
        appear.set(1.0);
        let at = (Spring::new(item.x, GLIDE.0, GLIDE.1), Spring::new(item.y, GLIDE.0, GLIDE.1));
        self.widgets.push(Widget {
            item,
            surface,
            dirty: true,
            bars_dirty: false,
            hits: Vec::new(),
            hover: None,
            tracking: false,
            edit: None,
            art: None,
            appear,
            motion: Motion::default(),
            at,
            live: None,
            frame: None,
            pictures: Vec::new(),
            covered: false,
            pet: crate::docky::Pet::new(),
            near: false,
            auto: crate::docky::Auto::new(),
            docky_dirty: false,
            drawn_at: None,
            docky_frame: std::time::Instant::now(),
            asked: false,
            reply: None,
            awake_until: std::time::Instant::now(),
        });
        self.refresh_live(self.widgets.len() - 1);
    }

    /// Starts (or restarts, for a new file or size) a Photo / Video widget's player.
    fn refresh_live(&mut self, i: usize) {
        let w = &mut self.widgets[i];
        let want = match (w.item.kind, &w.item.path) {
            (WidgetKind::Photo | WidgetKind::Video, Some(p)) if !p.is_empty() => {
                let (pw, ph) = item_dims(&w.item);
                let s = w.surface.scale;
                let crop = (w.item.kind == WidgetKind::Photo).then(|| w.item.crop()).flatten();
                Some((p.clone(), ((pw * s).round() as u32, (ph * s).round() as u32), crop))
            }
            _ => None,
        };
        if w.live.as_ref().map(|l| &l.key) == want.as_ref() {
            return;
        }
        w.live = None;
        w.frame = None;
        w.art = None;
        w.dirty = true;
        let Some(key) = want else { return };
        let feed = SharedFeed::default();
        let path = std::path::PathBuf::from(&key.0);
        match w.item.kind {
            WidgetKind::Video => live::play_video(path, key.1, feed.clone(), self.hwnd, self.msg_data),
            _ => live::play_photo(path, key.1, key.2, feed.clone(), self.hwnd, self.msg_data),
        }
        w.live = Some(Live { feed, key });
    }

    /// Moves widget `i`'s windows to where its glide has got to.
    fn follow(&mut self, i: usize) {
        let m = self.screen_of(&self.widgets[i].item).monitor;
        let s = m.scale;
        let (left, top) = (m.rect.left, m.rect.top);
        let w = &mut self.widgets[i];
        w.surface.move_to(left + ((w.at.0.value - SHADOW) * s).round() as i32, top + ((w.at.1.value - SHADOW) * s).round() as i32);
    }

    fn has(&self, kind: WidgetKind) -> bool {
        self.widgets.iter().any(|w| w.item.kind == kind)
    }

    fn mark(&mut self, kinds: &[WidgetKind]) {
        for w in self.widgets.iter_mut().filter(|w| kinds.contains(&w.item.kind)) {
            w.dirty = true;
        }
    }

    /// Music Wave's levels moved: only its bars need drawing again.
    fn mark_bands(&mut self) {
        for w in self.widgets.iter_mut().filter(|w| w.item.kind == WidgetKind::MusicWave) {
            w.bars_dirty = true;
        }
    }

    pub fn items(&self) -> Vec<WidgetItem> {
        self.widgets.iter().map(|w| w.item.clone()).collect()
    }

    /// A widget, the display it's on, and the biggest it can be there (points).
    pub fn widget_info(&self, id: u32) -> Option<(WidgetItem, Monitor, (f32, f32))> {
        let w = self.widgets.iter().find(|w| w.item.id == id)?;
        let screen = self.screen_of(&w.item);
        let b = screen.area();
        Some((w.item.clone(), screen.monitor, (b.w, b.h)))
    }

    /// Applies a changed `[widgets]` config: widgets that are still there stay
    /// (moved if needed); new and resized ones are created.
    pub fn sync(&mut self, gfx: &Gfx, theme: &Theme, cfg: &Config) {
        self.cfg = cfg.widgets.clone();
        self.docky = cfg.docky.clone();
        self.calendar_ics = cfg.island.calendar_ics.clone();
        self.data.zones = cfg.widgets.world_clocks.iter().filter_map(|z| sources::parse_zone(z)).collect();
        self.data.h24 = cfg.menubar.clock_format.contains("%H");
        let mut old = std::mem::take(&mut self.widgets);
        for item in &cfg.widgets.items {
            // Moved to a display of another scale, it's drawn afresh at that scale.
            let scale = self.screen_of(item).monitor.scale;
            let same = |w: &Widget| {
                w.item.id == item.id && w.item.kind == item.kind && item_dims(&w.item) == item_dims(item) && w.surface.scale == scale
            };
            match old.iter().position(same) {
                Some(i) => {
                    let mut w = old.remove(i);
                    // Moved (not by a drag, which moves it as it goes): glide there.
                    w.at.0.set(item.x);
                    w.at.1.set(item.y);
                    w.item = item.clone();
                    w.dirty = true;
                    self.widgets.push(w);
                    // With motion reduced, it's already there.
                    self.follow(self.widgets.len() - 1);
                    // A new picture or video picked for it.
                    self.refresh_live(self.widgets.len() - 1);
                }
                None => self.add_widget(gfx, theme, item.clone()),
            }
        }
        // Whatever's left in `old` was removed: it fades away, then its windows close.
        self.store.save_if_dirty();
        for w in old {
            w.surface.set_click_through();
            self.leaving.push(w.surface);
        }
        self.restart_sources();
        // Drives just picked are read now, not at the next tick.
        if self.has(WidgetKind::System) {
            self.sample();
        }
    }

    fn source_key(&self) -> SourceKey {
        let fahrenheit = sources::fahrenheit(&self.cfg.temperature_unit);
        SourceKey {
            weather: self.has(WidgetKind::Weather).then(|| (self.cfg.location.clone(), fahrenheit, self.data.h24)),
            calendar: (self.has(WidgetKind::Calendar) || self.has(WidgetKind::UpNext) || self.has(WidgetKind::Docky)).then(|| self.calendar_ics.clone()),
            devices: self.has(WidgetKind::Batteries),
            // Music Wave names the track playing too.
            media: self.has(WidgetKind::NowPlaying) || self.has(WidgetKind::MusicWave) || self.has(WidgetKind::Docky),
            gpu: sensors::wants_gpu(self.system_items()),
            notes: self.has(WidgetKind::Notifications),
            music: self.has(WidgetKind::MusicWave),
            user: self.has(WidgetKind::User),
        }
    }

    /// (Re)starts the background sources the current widgets need.
    fn restart_sources(&mut self) {
        let key = self.source_key();
        if let Some(s) = self.sources.as_mut().filter(|s| s.key == key) {
            // Another device picked for Music Wave: only its listener starts over.
            if s.bands.is_some() && s.audio != self.cfg.audio_device {
                if let Some(old) = s.bands.take() {
                    old.stop();
                }
                let shared = SharedBands::default();
                live::listen(shared.clone(), self.cfg.audio_device.clone(), self.hwnd, self.msg_data);
                s.bands = Some(shared);
                s.audio = self.cfg.audio_device.clone();
            }
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
        let media = key.media.then(|| Media::shared(self.hwnd));
        let gpus = key.gpu.then(|| {
            let shared = SharedGpus::default();
            sensors::watch_gpus(shared.clone(), stop.clone());
            shared
        });
        let notes = key.notes.then(|| {
            let inbox = Inbox::new(self.hwnd, self.msg_data);
            crate::island::notifications::watch(inbox.clone(), stop.clone());
            inbox
        });
        if !key.notes {
            self.data.notes.clear();
        }
        let bands = key.music.then(|| {
            let shared = SharedBands::default();
            live::listen(shared.clone(), self.cfg.audio_device.clone(), self.hwnd, self.msg_data);
            shared
        });
        let audio = self.cfg.audio_device.clone();
        let user = key.user.then(|| {
            let shared = SharedUser::default();
            if self.data.user.is_none() {
                user::load(shared.clone(), self.hwnd, self.msg_data);
            }
            shared
        });
        if key.weather.is_some() {
            self.data.weather = WeatherState::Loading;
        }
        self.sources = Some(Sources { key, stop, calendar, weather, devices, media, gpus, notes, bands, audio, user });
    }

    /// Gives up the running sources (and their data) for the widgets replacing these.
    pub fn hand_over(&mut self) -> Handover {
        Handover {
            sources: self.sources.take(),
            weather: std::mem::replace(&mut self.data.weather, WeatherState::Loading),
            events: std::mem::take(&mut self.data.events),
            devices: std::mem::take(&mut self.data.devices),
            now: self.data.now.take(),
            notes: std::mem::take(&mut self.data.notes),
            user: self.data.user.take(),
        }
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
        // Photo / Video frames (their players aren't among the shared sources).
        for w in &mut self.widgets {
            if let Some(l) = &w.live {
                if let Some(f) = l.feed.take() {
                    w.frame = Some(f);
                    w.dirty = true;
                } else if w.frame.is_none() && w.art.is_none() && l.feed.error().is_some() {
                    w.dirty = true;
                }
            }
        }
        let Some(src) = &self.sources else { return };
        let mut changed = Vec::new();
        if let Some(inbox) = &src.notes {
            for s in inbox.drain() {
                match s {
                    Signal::NoteHistory(list) => self.data.notes = list,
                    Signal::Notification(n) => {
                        self.data.notes.retain(|o| o.id != n.id);
                        self.data.notes.insert(0, n);
                        self.data.notes.truncate(30);
                    }
                    Signal::NotesGone(ids) => self.data.notes.retain(|n| !ids.contains(&n.id)),
                    _ => continue,
                }
                changed.push(WidgetKind::Notifications);
            }
        }
        if let Some(u) = src.user.as_ref().and_then(|u| u.lock().ok().and_then(|mut u| u.take())) {
            self.data.user = Some(u);
            changed.push(WidgetKind::User);
        }
        let bands = src.bands.as_ref().and_then(|b| b.take());
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
        if let Some(b) = bands {
            self.data.bands = b;
            self.mark_bands();
        }
    }

    pub fn on_media(&mut self) {
        if let Some(m) = self.sources.as_ref().and_then(|s| s.media.as_ref()) {
            self.data.now = m.snapshot();
            self.mark(&[WidgetKind::NowPlaying, WidgetKind::MusicWave]);
            // Docky dances to whatever's playing, starting the moment it does.
            let playing = self.data.now.as_ref().is_some_and(|n| n.playing);
            let title = self.data.now.as_ref().filter(|n| n.playing).map(|n| n.title.clone()).unwrap_or_default();
            let new_song = !title.is_empty() && title != self.watch.track;
            if !title.is_empty() {
                self.watch.track = title.clone();
            }
            // When music starts, he usually has a little dance (then does as he likes).
            let started = playing && !self.watch.playing;
            self.watch.playing = playing;
            for w in self.widgets.iter_mut().filter(|w| w.item.kind == WidgetKind::Docky) {
                if started && !w.auto() && !w.pet.asleep() && !w.pet.busy() && !w.covered && !w.near {
                    w.pet.play(crate::docky::Act::Dance);
                }
                w.pet.set_music(playing);
                if new_song && w.auto() && !w.pet.asleep() {
                    w.auto.cue(crate::docky::Cue::Track, &mut w.pet);
                    w.dirty = true;
                }
            }
        }
    }

    /// The System widgets' items.
    fn system_items(&self) -> impl Iterator<Item = &WidgetItem> {
        self.widgets.iter().filter(|w| w.item.kind == WidgetKind::System).map(|w| &w.item)
    }

    fn sample(&mut self) {
        let drives = sensors::wanted_drives(self.system_items());
        self.sampler.sample(&drives, self.sources.as_ref().and_then(|s| s.gpus.as_ref()));
        self.data.sys = self.sampler.stats.clone();
    }

    /// Every second: clocks, playback progress, stats, and the date rolling over. True when Docky has started something and needs frames.
    pub fn on_tick(&mut self) -> bool {
        self.ticks += 1;
        let docky = self.tick_docky();
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
            let plugged = b.is_some_and(|b| b.charging) && self.data.battery.is_some_and(|o| !o.charging);
            if plugged {
                for w in self.widgets.iter_mut().filter(|w| w.item.kind == WidgetKind::Docky && !w.pet.asleep()) {
                    w.pet.play(crate::docky::Act::Love);
                    if w.auto() {
                        w.auto.cue(crate::docky::Cue::Charger, &mut w.pet);
                    }
                }
            }
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
            // Also "5m ago" and the uptime.
            self.mark(&[WidgetKind::Calendar, WidgetKind::UpNext, WidgetKind::Notifications, WidgetKind::User]);
        }
        self.pause_covered();
        if self.store.sweep() {
            self.mark(&[WidgetKind::Reminders]);
        }
        self.store.save_if_dirty();
        docky
    }

    /// The conversation changed: a Docky you spoke to says his reply, long
    /// enough to read it. True when any did.
    pub fn on_chat(&mut self) -> bool {
        if crate::docky_ai::chat_pending() {
            return false;
        }
        let Some(reply) = crate::docky_ai::last_reply() else { return false };
        let now = std::time::Instant::now();
        let mut any = false;
        for w in self.widgets.iter_mut().filter(|w| w.item.kind == WidgetKind::Docky && w.asked) {
            w.asked = false;
            // Brief: long enough to read at a glance (pointing at him keeps it up).
            let secs = (2.5 + reply.chars().count() as f32 / 30.0).min(9.0);
            w.reply = Some((reply.clone(), now, now + std::time::Duration::from_secs_f32(secs)));
            w.awake_until = now + std::time::Duration::from_secs(300);
            w.pet.play(crate::docky::Act::Happy);
            w.dirty = true;
            any = true;
        }
        any
    }

    /// Whether a Docky is on screen to watch the pointer for.
    pub fn watching(&self) -> bool {
        self.widgets.iter().any(|w| w.item.kind == WidgetKind::Docky && !w.covered)
    }

    /// Polled while a Docky is on screen (not behind a window): his eyes follow
    /// the pointer wherever it is, and he reacts to what you're doing. Returns
    /// (keep polling, needs frames).
    pub fn poll_docky(&mut self) -> (bool, bool) {
        use std::time::{Duration, Instant};
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId};
        if !self.watching() {
            return (false, false);
        }
        let now = Instant::now();
        let p = cursor_px();
        let input = crate::docky::last_input();
        let idle = crate::docky::idle_secs();
        let fg = unsafe { GetForegroundWindow() };
        let w8 = &mut self.watch;

        // A key typed (on the keyboard: clicks and the wheel don't count).
        let typed = crate::docky::keys_typed() && now.duration_since(w8.last_nod) > Duration::from_millis(80);
        if typed {
            w8.last_nod = now;
            if now.duration_since(w8.last_typed) > Duration::from_secs(3) {
                w8.typing_since = now;
            }
            w8.last_typed = now;
        }
        let since_typed = now.duration_since(w8.last_typed).as_secs_f32();
        let typing_for = if since_typed < 3.0 { now.duration_since(w8.typing_since).as_secs_f32() } else { 0.0 };
        // A shake: the pointer moving fast and turning back on itself again and again.
        let dx = p.x - w8.cursor.x;
        if dx.abs() > 28 {
            if w8.dir != 0 && dx.signum() != w8.dir {
                w8.turns.push(now);
            }
            w8.dir = dx.signum();
        }
        w8.turns.retain(|t| now.duration_since(*t) < Duration::from_millis(1100));
        let shaken = w8.turns.len() >= 5 && now >= w8.dizzy_after;
        if shaken {
            w8.turns.clear();
            w8.dizzy_after = now + Duration::from_secs(6);
        }
        // Back after a minute or more away.
        let back = w8.idle >= 60 && idle < 2;
        w8.idle = idle;
        // Another app came to the front (not one of ours): he glances at it.
        let own = |h: HWND| {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
            pid == std::process::id()
        };
        let switched = !fg.0.is_null() && fg.0 as isize != w8.fg && !own(fg);
        w8.fg = fg.0 as isize;
        let opened = switched.then(|| {
            let mut r = RECT::default();
            unsafe { GetWindowRect(fg, &mut r) }.ok().map(|_| ((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0))
        });
        w8.cursor = p;
        w8.input = input;
        let hello = now >= w8.hello_after;
        // The app switched to, by name, for an Auto Docky to remark on.
        let app = (switched && self.widgets.iter().any(|w| w.auto())).then(|| {
            let path = crate::win::apps::process_path(crate::win::apps::effective_pid(fg)).unwrap_or_default();
            crate::win::apps::file_description(&path).filter(|d| !d.is_empty()).unwrap_or_else(|| crate::win::apps::exe_name(&path))
        });
        let music = self.data.now.as_ref().is_some_and(|n| n.playing);
        let battery = self.data.battery.map(|b| (b.percent, b.charging));
        let next_event = self.data.events.iter().filter(|e| e.starts_in() >= 0).min_by_key(|e| e.start).map(|e| (e.title.clone(), e.starts_in()));

        let mut frames = false;
        let mut greeted = false;
        for w in self.widgets.iter_mut().filter(|w| w.item.kind == WidgetKind::Docky && !w.covered) {
            let s = w.surface.scale;
            let auto = w.auto();
            if auto {
                w.auto.set_pixel(w.item.docky_style() == "pixel-auto");
            }
            let (ex, ey) = views::docky_eyes(w.body(), &w.item, w.docky_look(), w.pet.position(now), auto);
            let (sx, sy) = (w.surface.rect.left as f32 + ex * s, w.surface.rect.top as f32 + ey * s);
            // A direction from his eyes, looking all the way once it's a little way off.
            let toward = |x: f32, y: f32| {
                let (dx, dy) = ((x - sx) / s, (y - sy) / s);
                let d = dx.hypot(dy).max(1.0);
                let m = (d / 140.0).min(1.0);
                ((dx / d * m), (dy / d * m * 0.85))
            };
            let dist = ((p.x as f32 - sx) / s).hypot((p.y as f32 - sy) / s);
            let mut changed = w.pet.aim(Some(toward(p.x as f32, p.y as f32)));
            let near = dist < 130.0;
            let arrived = near && !w.near;
            w.near = near;
            if w.auto() {
                let signals = crate::docky::Signals {
                    asleep: w.pet.asleep(),
                    typing_for,
                    since_typed,
                    near,
                    idle,
                    music,
                    battery,
                    next_event: next_event.as_ref().map(|(t, s)| (t.as_str(), *s)),
                };
                if w.auto.tick(now, &signals, &mut w.pet) || w.auto.busy(now) {
                    w.dirty = true;
                    changed = true;
                }
            }
            // Asleep, his eyes are shut: nothing more to draw.
            if w.pet.asleep() {
                if changed {
                    w.dirty = true;
                    frames = true;
                }
                continue;
            }
            changed |= w.pet.set_drowsy(idle >= 90);
            if typed {
                w.pet.nod();
                changed = true;
            }
            if shaken && dist < 600.0 {
                w.pet.play(crate::docky::Act::Dizzy);
                if auto {
                    w.auto.cue(crate::docky::Cue::Dizzy, &mut w.pet);
                }
                changed = true;
            } else if back {
                w.pet.play(crate::docky::Act::Wave);
                if auto {
                    w.auto.cue(crate::docky::Cue::Back, &mut w.pet);
                }
                changed = true;
            } else if arrived && hello && !w.pet.busy() {
                // The pointer came to see him.
                w.pet.play(crate::docky::Act::Happy);
                greeted = true;
                changed = true;
            } else if let (Some(Some((x, y))), true) = (opened, ((x_seed(p) + w.item.id) % 2) == 0) {
                // Half the time he glances at the app you switched to.
                w.pet.glance(toward(x, y));
                if let (true, Some(name)) = (auto, &app) {
                    w.auto.cue(crate::docky::Cue::Switched(name.clone()), &mut w.pet);
                }
                changed = true;
            }
            if changed {
                // Eyes and nods are his motion alone (the frame loop paces them);
                // what Auto changed (a look, a bubble) redraws the card itself.
                frames = true;
            }
        }
        if greeted {
            self.watch.hello_after = now + Duration::from_secs(20);
        }
        // A reply about to fade, or a speech box gone idle, needs frames.
        frames |= self.widgets.iter().any(|w| w.reply.as_ref().is_some_and(|(_, _, until)| until.saturating_duration_since(now).as_secs_f32() < REPLY_OUT + 0.1));
        frames |= self.speech.as_ref().is_some_and(|b| b.idle());
        (true, frames)
    }

    /// Puts Docky to bed (at night, or while you're away) and wakes him, and
    /// starts whatever he does next. True when he's started moving.
    fn tick_docky(&mut self) -> bool {
        if !self.has(WidgetKind::Docky) {
            return false;
        }
        let hour = chrono::Local::now().hour();
        let night = self.docky.sleep_at_night && !(7..23).contains(&hour);
        let away = self.docky.nap_when_away && crate::docky::idle_secs() >= 600;
        let now = std::time::Instant::now();
        // The computer flat out (85%+ for a few seconds): he gets hot. Not too often.
        let mut hot = false;
        if self.ticks % 2 == 0 {
            let busy = self.watch.cpu.sample().is_some_and(|c| c >= 85.0);
            self.watch.hot_streak = if busy { self.watch.hot_streak.saturating_add(1) } else { 0 };
            if self.watch.hot_streak >= 2 && now >= self.watch.hot_after {
                hot = true;
                self.watch.hot_after = now + std::time::Duration::from_secs(180);
            }
        }
        // Something on the calendar in the next five minutes: he waves (once for each).
        let soon = self.data.events.iter().find(|e| (0..=300).contains(&e.starts_in())).map(|e| e.start);
        let wave = soon.is_some_and(|s| s != self.watch.waved_event);
        if let (true, Some(s)) = (wave, soon) {
            self.watch.waved_event = s;
        }
        let mut moving = false;
        let idle = crate::docky::idle_secs();
        for w in self.widgets.iter_mut().filter(|w| w.item.kind == WidgetKind::Docky) {
            w.pet.set_energy(&self.docky.energy);
            // Asleep with nobody at the computer: he lies still (no frames at all).
            w.pet.set_still(w.pet.asleep() && idle >= 120);
            if !w.pet.asleep() && !w.pet.busy() && !w.covered {
                if hot {
                    w.pet.play(crate::docky::Act::Hot);
                    if w.auto() {
                        w.auto.cue(crate::docky::Cue::Hot, &mut w.pet);
                    }
                    w.dirty = true;
                } else if wave && !w.auto() {
                    // (An Auto Docky says so himself.)
                    w.pet.play(crate::docky::Act::Wave);
                    w.dirty = true;
                }
            }
            // Talking to him keeps him up for a while.
            if w.reply.as_ref().is_some_and(|(_, _, until)| *until <= now) {
                w.reply = None;
                w.dirty = true;
            }
            let chatting = now < w.awake_until || w.asked;
            if w.pet.set_asleep(!chatting && (w.item.docky_style() == "sleepy" || night || away)) {
                w.dirty = true;
            }
            if !w.covered && w.pet.update(now) {
                w.dirty = true;
                moving = true;
            }
        }
        moving
    }

    /// Moving widgets hidden behind app windows stop (their workers sleep);
    /// Music Wave stops listening when every one of them is hidden.
    fn pause_covered(&mut self) {
        let moving = |k| matches!(k, WidgetKind::Photo | WidgetKind::Video | WidgetKind::MusicWave | WidgetKind::Docky);
        if !self.widgets.iter().any(|w| moving(w.item.kind)) {
            return;
        }
        for w in self.widgets.iter_mut().filter(|w| moving(w.item.kind)) {
            let r = w.surface.rect;
            let s = w.surface.scale;
            let pad = (SHADOW * s).round() as i32;
            let body = RECT { left: r.left + pad, top: r.top + pad, right: r.right - pad, bottom: r.bottom - pad };
            w.covered = live::covered(body);
            if let Some(l) = &w.live {
                l.feed.set_paused(w.covered);
            }
        }
        let all_hidden = self.widgets.iter().filter(|w| w.item.kind == WidgetKind::MusicWave).all(|w| w.covered);
        if let Some(b) = self.sources.as_ref().and_then(|s| s.bands.as_ref()) {
            b.set_paused(all_hidden);
        }
    }

    // --- animation ---------------------------------------------------------

    /// Advances fades and springs; returns true while anything moves.
    pub fn animate(&mut self, dt: f32) -> bool {
        let mut moving = false;
        let now = std::time::Instant::now();
        let cursor = self.widgets.iter().any(|w| w.item.kind == WidgetKind::Docky && !w.covered && !w.pet.asleep()).then(cursor_px);
        for i in 0..self.widgets.len() {
            let w = &mut self.widgets[i];
            if let (WidgetKind::Docky, false, false, Some(p)) = (w.item.kind, w.covered, w.pet.asleep(), cursor) {
                // Sample the pointer at display refresh rate while frames already run,
                // so his eyes glide at full vsync without 40ms timer stair-steps.
                let s = w.surface.scale;
                let (ex, ey) = views::docky_eyes(w.body(), &w.item, w.docky_look(), w.pet.position(now), w.auto());
                let (dx, dy) = ((p.x as f32 - (w.surface.rect.left as f32 + ex * s)) / s, (p.y as f32 - (w.surface.rect.top as f32 + ey * s)) / s);
                let d = dx.hypot(dy).max(1.0);
                let m = (d / 140.0).min(1.0);
                w.pet.aim(Some((dx / d * m, dy / d * m * 0.85)));
            }
            if let Some((_, since, until)) = w.reply.as_mut() {
                // Pointing at him keeps his reply up; it pops in, then fades as time runs out.
                if w.tracking {
                    *until = (*until).max(now + std::time::Duration::from_secs_f32(1.2));
                }
                let popping = now.duration_since(*since).as_secs_f32() < REPLY_IN + 0.02;
                let left = until.saturating_duration_since(now).as_secs_f32();
                if popping || left < REPLY_OUT + 0.02 {
                    w.dirty = true;
                    moving = true;
                }
                if left <= 0.0 {
                    w.reply = None;
                    w.dirty = true;
                }
            }
            if w.item.kind == WidgetKind::Docky && !w.covered {
                let switching = w.auto() && w.auto.busy(now);
                if switching {
                    // A change of look or a speech bubble: the whole card.
                    w.dirty = true;
                    moving = true;
                }
                if w.pet.update(now) | w.pet.ease(dt) {
                    moving = true;
                    // Evenly paced: his motion is drawn at ~60 or ~30 fps, not
                    // at the display's full rate (on a 165 Hz screen, every 3rd
                    // or 6th frame), so he stays smooth for a fraction of the work.
                    let every = 1.0 / w.pet.fps(now);
                    if now.duration_since(w.docky_frame).as_secs_f32() >= every * 0.9 {
                        w.docky_frame = now;
                        w.docky_dirty = true;
                    }
                }
            }
            if w.appear.step(dt) {
                w.surface.set_opacity(w.appear.value);
                moving = true;
            }
            if w.motion.tick(dt) {
                w.dirty = true;
                moving = true;
            }
            if !w.at.0.settled() || !w.at.1.settled() {
                moving |= w.at.0.step(dt) | w.at.1.step(dt);
                self.follow(i);
            }
        }
        if let Some(b) = &mut self.speech {
            moving |= b.tick(dt);
            if b.idle() {
                b.close();
                moving = true;
            }
            if b.gone() {
                self.speech = None;
            }
        }
        self.leaving.retain_mut(|s| {
            let o = approach(s.opacity(), 0.0, dt, 20.0);
            s.set_opacity(o);
            o > 0.0
        });
        moving || !self.leaving.is_empty()
    }

    pub fn animating(&self) -> bool {
        !self.leaving.is_empty()
            || self.speech.as_ref().is_some_and(|b| b.moving())
            || self.widgets.iter().any(|w| {
                !w.appear.settled() || w.motion.moving() || !w.at.0.settled() || !w.at.1.settled() || ((w.pet.moving(std::time::Instant::now()) || w.pet.settling() || (w.auto() && w.auto.busy(std::time::Instant::now()))) && !w.covered)
            })
    }

    // --- drawing -----------------------------------------------------------

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        if let Some(b) = self.speech.as_mut().filter(|b| b.dirty) {
            b.render(gfx, theme);
        }
        let solid = self.cfg.backdrop == Backdrop::None;
        // Screenshot Mode paints demo data in place of anything personal.
        let private = crate::screenshot_mode::on().then(|| {
            let fahrenheit = sources::fahrenheit(&self.cfg.temperature_unit);
            let ids = self.widgets.iter().filter(|w| w.item.kind == WidgetKind::Notes).map(|w| w.item.id);
            (demo_data(&self.data, fahrenheit), demo_store(ids))
        });
        let Widgets { widgets, data, store, .. } = self;
        let (data, store) = match &private {
            Some((d, s)) => (d, s),
            None => (&*data, &*store),
        };
        for w in widgets.iter_mut().filter(|w| w.dirty || w.bars_dirty || w.docky_dirty) {
            // Docky moving: just where he was and where he is now.
            let docky_part = (w.item.kind == WidgetKind::Docky).then(|| {
                let auto = w.auto();
                let scene_full = auto && w.auto.scene(std::time::Instant::now()).say.is_some();
                let walk = w.pet.position(std::time::Instant::now());
                let now_at = views::docky_part(w.body(), &w.item, w.docky_look(), walk, auto).filter(|_| !scene_full);
                let part = match (now_at, w.drawn_at) {
                    (Some(a), Some(b)) => Some(a.union(b)),
                    (a, _) => a,
                };
                w.drawn_at = now_at;
                part
            });
            let part = if w.dirty {
                None
            } else if w.docky_dirty {
                docky_part.flatten()
            } else {
                Some(views::wave_area(w.body(), w.item.size))
            };
            w.docky_dirty = false;
            w.dirty = false;
            w.bars_dirty = false;
            if matches!(w.item.kind, WidgetKind::NowPlaying | WidgetKind::MusicWave) {
                let art = data.now.as_ref().and_then(|n| n.art.clone());
                if art.as_ref().map(|a| a.id) != w.art.as_ref().map(|(id, _)| *id) {
                    let rt = w.surface.canvas.target().clone();
                    w.art = art.and_then(|a| {
                        let px = IconPixels { width: a.size, height: a.size, data: a.data.clone() };
                        px.to_bitmap(&rt).map(|b| (a.id, b))
                    });
                }
            }
            match w.item.kind {
                WidgetKind::Photo | WidgetKind::Video => {
                    if let Some(f) = w.frame.take() {
                        let rt = w.surface.canvas.target().clone();
                        upload_frame(&rt, &mut w.art, f);
                    }
                }
                WidgetKind::User => {
                    let pic = data.user.as_ref().and_then(|u| u.picture.clone());
                    if pic.as_ref().map(|a| a.id) != w.art.as_ref().map(|(id, _)| *id) {
                        let rt = w.surface.canvas.target().clone();
                        w.art = pic.and_then(|a| IconPixels { width: a.size, height: a.size, data: a.data.clone() }.to_bitmap(&rt).map(|b| (a.id, b)));
                    }
                }
                WidgetKind::Notifications => {
                    let rt = w.surface.canvas.target().clone();
                    let icons: Vec<_> = data.notes.iter().filter_map(|n| n.icon.clone()).collect();
                    w.pictures.retain(|(id, _)| icons.iter().any(|a| a.id == *id));
                    for a in icons {
                        if !w.pictures.iter().any(|(id, _)| *id == a.id) {
                            if let Some(b) = (IconPixels { width: a.size, height: a.size, data: a.data.clone() }).to_bitmap(&rt) {
                                w.pictures.push((a.id, b));
                            }
                        }
                    }
                }
                _ => {}
            }
            let body = w.body();
            let mut hits = Vec::new();
            w.motion.sweep();
            let error = w.live.as_ref().and_then(|l| l.feed.error());
            // Demo notes, reminders and notifications can't be edited or opened:
            // their clicks would land on the real ones.
            let demo_only = private.is_some() && matches!(w.item.kind, WidgetKind::Notes | WidgetKind::Reminders | WidgetKind::Notifications);
            let ctx = views::Ctx {
                theme,
                data,
                store,
                item: &w.item,
                edit: w.edit.as_ref().filter(|_| !demo_only),
                art: w.art.as_ref().map(|(_, b)| b),
                hover: w.hover,
                motion: Some(&w.motion),
                pictures: &w.pictures,
                error: error.as_deref(),
                docky: (w.item.kind == WidgetKind::Docky).then(|| w.pet.pose(std::time::Instant::now())),
                docky_scene: w.auto().then(|| w.auto.scene(std::time::Instant::now())),
                docky_say: if w.asked && crate::docky_ai::chat_pending() {
                    Some(("hmm\u{2026}".to_string(), 1.0))
                } else {
                    // His reply pops in, and fades as its time runs out.
                    let now = std::time::Instant::now();
                    w.reply.as_ref().filter(|(_, _, until)| *until > now).map(|(t, since, until)| {
                        let pop = (now.duration_since(*since).as_secs_f32() / REPLY_IN).min(1.0);
                        let fade = (until.duration_since(now).as_secs_f32() / REPLY_OUT).min(1.0);
                        (t.clone(), (1.0 - (1.0 - pop).powi(3)).min(fade * fade * (3.0 - 2.0 * fade)))
                    })
                },
            };
            let result = w.surface.render_part(gfx, part, |p| paint(p, body, &ctx, solid, &mut hits));
            if let Err(e) = result {
                log!("widget render failed: {e}");
            }
            if demo_only {
                hits.clear();
            }
            w.hits = hits;
        }
    }

    /// Repaint everything (font or accent changed).
    pub fn invalidate(&mut self) {
        // Typing stops when Screenshot Mode covers the note being written.
        if crate::screenshot_mode::on() {
            for i in 0..self.widgets.len() {
                self.end_edit(i);
            }
        }
        for w in &mut self.widgets {
            w.dirty = true;
        }
    }

    pub fn needs_render(&self) -> bool {
        self.widgets.iter().any(|w| w.dirty || w.bars_dirty || w.docky_dirty) || self.speech.as_ref().is_some_and(|b| b.dirty)
    }

    // --- input -------------------------------------------------------------

    fn index(&self, hwnd: HWND) -> Option<usize> {
        self.widgets.iter().position(|w| w.surface.content == hwnd)
    }

    pub fn owns(&self, hwnd: HWND) -> bool {
        self.index(hwnd).is_some() || self.is_speech(hwnd)
    }

    /// The scale widget window `hwnd` draws at.
    pub fn scale_of(&self, hwnd: HWND) -> f32 {
        if let Some(b) = self.speech.as_ref().filter(|b| b.surface.content == hwnd) {
            return b.surface.scale;
        }
        self.index(hwnd).map_or(1.0, |i| self.widgets[i].surface.scale)
    }

    fn is_speech(&self, hwnd: HWND) -> bool {
        self.speech.as_ref().is_some_and(|b| b.surface.content == hwnd)
    }

    /// Opens Docky's speech box by widget `i` (or brings back the one that's
    /// there); returns its window, to give it the keyboard.
    fn open_speech(&mut self, i: usize, gfx: &Gfx) -> Option<HWND> {
        let id = self.widgets[i].item.id;
        if let Some(b) = self.speech.as_mut().filter(|b| b.id == id && !b.closing) {
            b.last_input = std::time::Instant::now();
            return Some(b.surface.content);
        }
        if let Some(b) = &mut self.speech {
            b.close();
        }
        let w = &self.widgets[i];
        let s = w.surface.scale;
        let pad = (SHADOW * s).round() as i32;
        let r = w.surface.rect;
        let anchor = RECT { left: r.left + pad, top: r.top + pad, right: r.right - pad, bottom: r.bottom - pad };
        let bottom = self.screen_of(&w.item).monitor.rect.bottom;
        let pixel = w.docky_look() == "pixel";
        match speech::SpeechBox::open(gfx, anchor, bottom, s, id, pixel) {
            Ok(b) => {
                unsafe {
                    SetWindowLongPtrW(b.surface.content, GWLP_USERDATA, TAG);
                }
                place_on_desktop(b.surface.root());
                let h = b.surface.content;
                self.speech = Some(b);
                Some(h)
            }
            Err(e) => {
                log!("docky speech box failed: {e}");
                None
            }
        }
    }

    /// Says what's in the speech box to its Docky, and lets the box go.
    fn speak(&mut self) {
        let Some(b) = &mut self.speech else { return };
        let text = b.field.string();
        if text.trim().is_empty() || !crate::docky_ai::send(&text, self.hwnd) {
            return;
        }
        b.close();
        let id = b.id;
        if let Some(w) = self.widgets.iter_mut().find(|w| w.item.id == id) {
            w.asked = true;
            w.reply = None;
            w.awake_until = std::time::Instant::now() + std::time::Duration::from_secs(300);
            w.pet.play(crate::docky::Act::Look);
            w.dirty = true;
        }
    }

    /// Pointer moved over a widget (or anywhere, while one is held).
    pub fn on_move(&mut self, hwnd: HWND, x: f32, y: f32) -> bool {
        if let Some(b) = self.speech.as_mut().filter(|b| b.surface.content == hwnd) {
            b.hover = true;
            let first = !b.tracking;
            b.tracking = true;
            return first;
        }
        let Some(i) = self.index(hwnd) else { return false };
        if let Some(d) = &mut self.drag {
            let p = cursor_px();
            let moved = (((p.x - d.start.x).pow(2) + (p.y - d.start.y).pow(2)) as f32).sqrt();
            if !d.moved && moved < DRAG_THRESHOLD * self.widgets[i].surface.scale {
                return false;
            }
            d.moved = true;
            let id = d.id;
            let (gx, gy) = d.grab;
            let Some(j) = self.widgets.iter().position(|w| w.item.id == id) else { return false };
            // It goes onto whichever display the pointer is on.
            let screen = match self.screens.iter().find(|s| s.monitor.contains(p.x, p.y)) {
                Some(s) => s.clone(),
                None => self.screen_of(&self.widgets[j].item).clone(),
            };
            let (cx, cy) = screen.local(p);
            let (w, h) = item_dims(&self.widgets[j].item);
            let display = screen.tag();
            let probe = WidgetItem { display: display.clone(), ..self.widgets[j].item.clone() };
            let others: Vec<Rect> = self
                .widgets
                .iter()
                .filter(|o| o.item.id != id && self.same_screen(&o.item, &probe))
                .map(|o| item_rect(&o.item))
                .collect();
            let (nx, ny) = snap(cx - gx, cy - gy, w, h, &others, screen.area(), screen.bounds());
            let wd = &mut self.widgets[j];
            // Whatever it was pressed on isn't being clicked any more.
            if wd.hover.take().is_some() {
                wd.dirty = true;
            }
            if (nx, ny) != (wd.item.x, wd.item.y) || display != wd.item.display {
                // It follows the pointer exactly (this also stops any glide).
                wd.item.x = nx;
                wd.item.y = ny;
                wd.item.display = display;
                wd.at.0.snap(nx);
                wd.at.1.snap(ny);
                self.follow(j);
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
        if let Some(b) = self.speech.as_mut().filter(|b| b.surface.content == hwnd) {
            b.hover = false;
            b.tracking = false;
            b.last_input = std::time::Instant::now();
            return;
        }
        if let Some(i) = self.index(hwnd) {
            let w = &mut self.widgets[i];
            w.tracking = false;
            if w.hover.take().is_some() {
                w.dirty = true;
            }
        }
    }

    pub fn on_down(&mut self, hwnd: HWND, x: f32, y: f32) {
        if self.is_speech(hwnd) {
            return;
        }
        let Some(i) = self.index(hwnd) else { return };
        let w = &self.widgets[i];
        // Any press may become a drag, even on text being typed into: a click
        // (released without moving) still places the caret or presses the button.
        let body = w.body();
        self.drag = Some(Drag { id: w.item.id, grab: (x - body.x, y - body.y), start: cursor_px(), moved: false });
    }

    /// The mouse capture went away mid-drag: keep where it got to.
    pub fn on_capture_lost(&mut self) -> WidgetEvent {
        match self.drag.take() {
            Some(d) if d.moved => WidgetEvent::Save(self.items()),
            _ => WidgetEvent::None,
        }
    }

    pub fn on_up(&mut self, hwnd: HWND, x: f32, y: f32, gfx: &Gfx, theme: &Theme) -> WidgetEvent {
        if self.is_speech(hwnd) {
            if self.speech.as_ref().is_some_and(|b| b.on_send(x, y)) {
                self.speak();
            }
            return WidgetEvent::None;
        }
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
            Some(Hit::Notification(n)) => match self.data.notes.get(n) {
                Some(note) => WidgetEvent::OpenMedia(note.app_id.clone()),
                None => WidgetEvent::None,
            },
            Some(Hit::Lock) => WidgetEvent::Action(Action::Lock),
            Some(Hit::SignOut) => WidgetEvent::Action(Action::LogOut),
            Some(Hit::Sleep) => WidgetEvent::Action(Action::Sleep),
            Some(Hit::Choose) => WidgetEvent::Action(Action::Widget(WidgetCmd::Choose(self.widgets[i].item.id, false))),
            None => match kind {
                WidgetKind::Notifications => WidgetEvent::Action(Action::Open("ms-actioncenter:".into())),
                WidgetKind::User => WidgetEvent::Action(Action::Open("ms-settings:yourinfo".into())),
                WidgetKind::MusicWave => match &self.data.now {
                    Some(n) => WidgetEvent::OpenMedia(n.app_id.clone()),
                    None => WidgetEvent::None,
                },
                WidgetKind::Clock | WidgetKind::WorldClock => WidgetEvent::Action(Action::Open("ms-clock:".into())),
                WidgetKind::Calendar | WidgetKind::UpNext => WidgetEvent::Action(Action::OpenCalendar),
                WidgetKind::Batteries => WidgetEvent::Action(Action::Open("ms-settings:bluetooth".into())),
                WidgetKind::System => WidgetEvent::Action(Action::Open("taskmgr.exe".into())),
                WidgetKind::Docky => {
                    // A click opens his speech box: he wakes up and listens.
                    let w = &mut self.widgets[i];
                    w.awake_until = std::time::Instant::now() + std::time::Duration::from_secs(300);
                    if w.pet.asleep() {
                        w.pet.set_asleep(false);
                    } else if w.pet.poke() && w.auto() {
                        w.auto.cue(crate::docky::Cue::Click, &mut w.pet);
                    }
                    w.dirty = true;
                    match self.open_speech(i, gfx) {
                        Some(h) => WidgetEvent::Focus(h),
                        None => WidgetEvent::None,
                    }
                }
                _ => WidgetEvent::None,
            },
        }
    }

    pub fn context_menu(&mut self, hwnd: HWND, x: f32, y: f32) -> Option<(Vec<Entry>, Placement)> {
        let i = self.index(hwnd)?;
        let item = self.widgets[i].item.clone();
        let sensor_entries = (item.kind == WidgetKind::System).then(|| self.sensor_entries(&item));
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
                Entry::item(label, Action::Widget(WidgetCmd::Resize(id, s))).checked(s == w.item.size && w.item.fit().is_none())
            })
            .collect();
        if kind == WidgetKind::Photo && w.item.fit().is_some() {
            entries.push(Entry::item("Fit to Picture", Action::Widget(WidgetCmd::EditPhoto(id))).checked(true));
        }
        entries.push(Entry::Separator);
        entries.push(Entry::submenu("Align with Neighbour", self.align_entries(i)));
        entries.push(Entry::submenu("Move To", self.move_entries(i)));
        if self.screens.len() > 1 {
            entries.push(Entry::submenu("Move to Display", self.display_entries(i)));
        }
        entries.push(Entry::Separator);
        if kind == WidgetKind::Weather {
            entries.push(Entry::item("Refresh Weather", Action::Widget(WidgetCmd::Refresh)));
            entries.push(Entry::Separator);
        }
        if let Some(s) = sensor_entries {
            entries.extend(s);
            entries.push(Entry::Separator);
        }
        match kind {
            WidgetKind::Photo => {
                if w.item.path.as_deref().is_some_and(|p| !p.is_empty()) {
                    entries.push(Entry::item("Edit Photo…", Action::Widget(WidgetCmd::EditPhoto(id))));
                }
                entries.push(Entry::item("Choose Picture or GIF…", Action::Widget(WidgetCmd::Choose(id, false))));
                entries.push(Entry::item("Choose Folder for a Slideshow…", Action::Widget(WidgetCmd::Choose(id, true))));
                entries.push(Entry::Separator);
            }
            WidgetKind::Clock => {
                let digital = w.item.digital();
                entries.push(Entry::item("Analog", Action::Widget(WidgetCmd::SetDigital(id, false))).checked(!digital));
                entries.push(Entry::item("Digital", Action::Widget(WidgetCmd::SetDigital(id, true))).checked(digital));
                entries.push(Entry::Separator);
            }
            WidgetKind::Video => {
                entries.push(Entry::item("Choose Video…", Action::Widget(WidgetCmd::Choose(id, false))));
                entries.push(Entry::Separator);
            }
            WidgetKind::Docky => {
                let style = w.item.docky_style();
                for (key, label) in crate::docky::STYLES {
                    entries.push(Entry::item(label, Action::Widget(WidgetCmd::SetDockyStyle(id, key))).checked(style == key));
                }
                entries.push(Entry::Separator);
                entries.push(Entry::item("Docky Settings…", Action::OpenSettings(Some(crate::settings::Page::Docky))));
            }
            WidgetKind::User => {
                for (key, label) in user::OPTIONS {
                    entries.push(Entry::item(label, Action::Widget(WidgetCmd::ToggleOption(id, key))).checked(user::shows(&w.item, key)));
                }
                entries.push(Entry::Separator);
            }
            WidgetKind::Notifications => {
                let any = !self.data.notes.is_empty();
                entries.push(if any {
                    Entry::item("Clear All Notifications", Action::Widget(WidgetCmd::ClearNotifications))
                } else {
                    Entry::disabled("Clear All Notifications")
                });
                entries.push(Entry::Separator);
            }
            _ => {}
        }
        entries.push(Entry::item("Edit Widgets…", Action::OpenSettings(Some(crate::settings::Page::Widgets))));
        entries.push(Entry::item("Reset Widgets…", Action::Widget(WidgetCmd::Reset)));
        entries.push(Entry::item(format!("Remove “{}”", title(kind)), Action::Widget(WidgetCmd::Remove(id))));
        let r = w.surface.rect;
        let s = w.surface.scale;
        Some((entries, Placement::Below { x: r.left as f32 / s + x, y: r.top as f32 / s + y }))
    }

    /// System: "Show ▸" with every sensor this PC has (ticked when shown), and
    /// "Graph ▸" to pick which usage leads (and so gets the graph).
    fn sensor_entries(&mut self, item: &WidgetItem) -> Vec<Entry> {
        use sensors::Sensor;
        let gpus = sensors::gpus();
        let first_gpu = gpus.first().map(|g| g.id.clone());
        self.first_gpu = first_gpu.clone();
        let shown: Vec<Sensor> = sensors::of(item).into_iter().map(|s| sensors::canonical(s, first_gpu.as_deref())).collect();
        let id = item.id;
        let toggle = |label: String, s: Sensor| {
            let on = shown.contains(&s);
            Entry::item(label, Action::Widget(WidgetCmd::ToggleSensor(id, s.key()))).checked(on)
        };
        let mut show = vec![toggle("CPU".into(), Sensor::Cpu), toggle("Memory".into(), Sensor::Memory)];
        // Menus open one level of submenu, so each GPU is a heading over its own items.
        for g in &gpus {
            let some = || Some(g.id.clone());
            show.push(Entry::Separator);
            show.push(Entry::disabled(g.name.clone()));
            show.push(toggle("GPU Usage".into(), Sensor::Gpu(some())));
            show.push(toggle("GPU Memory".into(), Sensor::GpuMemory(some())));
            show.push(toggle("GPU Temperature".into(), Sensor::GpuTemp(some())));
        }
        show.push(Entry::Separator);
        show.push(Entry::disabled("Drives"));
        for d in sensors::drives() {
            show.push(toggle(d.name, Sensor::Disk(Some(d.letter))));
        }
        show.push(Entry::Separator);
        show.push(Entry::disabled("Network"));
        show.push(toggle("All Adapters".into(), Sensor::Network(None)));
        let mut adapters = sensors::adapters();
        // Picked adapters stay listed while they're disconnected, so they can be unticked.
        for s in &shown {
            if let Sensor::Network(Some(n)) = s {
                if !adapters.iter().any(|a| a.eq_ignore_ascii_case(n)) {
                    adapters.push(n.clone());
                }
            }
        }
        for a in adapters {
            show.push(toggle(a.clone(), Sensor::Network(Some(a))));
        }
        show.push(Entry::Separator);
        let custom = item.sensors.is_some();
        show.push(if custom { Entry::item("Show Defaults", Action::Widget(WidgetCmd::DefaultSensors(id))) } else { Entry::disabled("Show Defaults") });

        let mut out = vec![Entry::submenu("Show", show)];
        let graphs: Vec<&Sensor> = shown.iter().filter(|s| s.graphable()).collect();
        if graphs.len() > 1 {
            let name = |s: &Sensor| match s {
                Sensor::Gpu(Some(g)) => gpus.iter().find(|x| x.id == *g).map_or_else(|| g.clone(), |x| x.name.clone()),
                _ => "CPU".to_string(),
            };
            let lead = graphs[0];
            out.push(Entry::submenu(
                "Graph",
                graphs.iter().map(|s| Entry::item(name(s), Action::Widget(WidgetCmd::LeadSensor(id, s.key()))).checked(*s == lead)).collect(),
            ));
        }
        out
    }

    /// Widget `i`'s rect and those of the others on its display.
    fn rects(&self, i: usize) -> (Rect, Vec<Rect>) {
        let me = &self.widgets[i].item;
        let others = self
            .widgets
            .iter()
            .enumerate()
            .filter(|(j, o)| *j != i && self.same_screen(&o.item, me))
            .map(|(_, o)| item_rect(&o.item))
            .collect();
        (item_rect(me), others)
    }

    /// "Move to Display": the other displays, by name.
    fn display_entries(&self, i: usize) -> Vec<Entry> {
        let item = &self.widgets[i].item;
        let here = self.screen_of(item).id.clone();
        let list = crate::win::displays::current();
        self.screens
            .iter()
            .map(|s| {
                let name = list.iter().find(|d| d.id == s.id).map_or_else(|| s.id.clone(), |d| d.name.clone());
                if s.id == here {
                    Entry::disabled(name).checked(true)
                } else {
                    Entry::item(name, Action::Widget(WidgetCmd::MoveToDisplay(item.id, s.id.clone())))
                }
            })
            .collect()
    }

    /// "Align with Neighbour": ticked when already lined up, dimmed when there's no room.
    fn align_entries(&self, i: usize) -> Vec<Entry> {
        let (r, others) = self.rects(i);
        let id = self.widgets[i].item.id;
        let mut entries = Vec::new();
        for how in Alignment::ALL {
            if how == Alignment::Top {
                entries.push(Entry::Separator);
            }
            entries.push(match aligned(r, &others, how, self.screen_of(&self.widgets[i].item).bounds()) {
                Some(p) if p == (r.x, r.y) => Entry::disabled(how.label()).checked(true),
                Some(_) => Entry::item(how.label(), Action::Widget(WidgetCmd::Align(id, how))),
                None => Entry::disabled(how.label()),
            });
        }
        entries
    }

    /// "Move To": the corners, the middles of the edges and the centre of the screen.
    fn move_entries(&self, i: usize) -> Vec<Entry> {
        const NAMES: [[&str; 3]; 3] =
            [["Top Left", "Top Centre", "Top Right"], ["Middle Left", "Centre", "Middle Right"], ["Bottom Left", "Bottom Centre", "Bottom Right"]];
        let (r, others) = self.rects(i);
        let id = self.widgets[i].item.id;
        let mut entries = Vec::new();
        for (row, names) in NAMES.iter().enumerate() {
            if row > 0 {
                entries.push(Entry::Separator);
            }
            for (col, name) in names.iter().enumerate() {
                let (x, y) = spot(col as u8, row as u8, r.w, r.h, self.screen_of(&self.widgets[i].item).area());
                let taken = others.iter().any(|o| overlaps(*o, Rect::new(x, y, r.w, r.h)));
                entries.push(if (x, y) == (r.x, r.y) {
                    Entry::disabled(*name).checked(true)
                } else if taken {
                    Entry::disabled(*name)
                } else {
                    Entry::item(*name, Action::Widget(WidgetCmd::MoveTo(id, col as u8, row as u8)))
                });
            }
        }
        entries
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
        // The speech box goes once you're elsewhere.
        if let Some(b) = self.speech.as_mut().filter(|b| b.surface.content == hwnd) {
            b.close();
            return;
        }
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
        if let Some(b) = self.speech.as_mut().filter(|b| b.surface.content == hwnd) {
            b.last_input = std::time::Instant::now();
            b.dirty = true;
            match vk {
                VK_RETURN => self.speak(),
                VK_ESCAPE => b.close(),
                0x56 if ctrl => {
                    if let Some(s) = crate::finder::fs::paste_text() {
                        b.field.insert(&s.replace(['\r', '\n'], " "));
                    }
                }
                _ => {
                    b.field.on_key(vk, ctrl, shift);
                }
            }
            return true;
        }
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
        if let Some(b) = self.speech.as_mut().filter(|b| b.surface.content == hwnd) {
            if !c.is_control() {
                b.field.insert(&c.to_string());
                b.last_input = std::time::Instant::now();
                b.dirty = true;
            }
            return;
        }
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

    /// Ticks or unticks sensor `key` on widget `id` (or, `lead`, puts it first).
    fn edit_sensors(&self, items: &mut [WidgetItem], id: u32, key: &str, lead: bool) -> Option<()> {
        let first = self.first_gpu.as_deref();
        let canon = |s| sensors::canonical(s, first);
        let i = items.iter().position(|w| w.id == id)?;
        let s = canon(sensors::Sensor::parse(key)?);
        let mut list: Vec<sensors::Sensor> = Vec::new();
        for x in sensors::of(&items[i]).into_iter().map(canon) {
            if !list.contains(&x) {
                list.push(x);
            }
        }
        match list.iter().position(|x| *x == s) {
            Some(p) if lead => {
                let x = list.remove(p);
                list.insert(0, x);
            }
            Some(p) => {
                list.remove(p);
            }
            None if lead => return None,
            None => list.push(s),
        }
        items[i].sensors = sensors::stored(&list, first);
        Some(())
    }

    /// The widget list after `cmd`, to be saved; None if nothing's to save.
    pub fn command(&mut self, cmd: WidgetCmd) -> Option<Vec<WidgetItem>> {
        let mut items = self.items();
        match cmd {
            WidgetCmd::Add(kind) => {
                // Onto the display being worked on (where Settings is).
                let p = cursor_px();
                let screen = self.screens.iter().find(|s| s.monitor.contains(p.x, p.y)).unwrap_or(&self.screens[0]).clone();
                let display = screen.tag();
                let probe = WidgetItem { id: 0, kind, size: sizes(kind)[0], x: 0.0, y: 0.0, zone: None, display: display.clone(), sensors: None, path: None, options: None, crop: None, fit: None };
                let here: Vec<WidgetItem> = items.iter().filter(|i| self.same_screen(i, &probe)).cloned().collect();
                let size = sizes(kind)[0];
                let (x, y) = free_spot(&here, dims(size), screen.area());
                let id = items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
                // A new Docky lives his own life (Auto).
                let options = (kind == WidgetKind::Docky).then(|| vec!["auto".to_string()]);
                items.push(WidgetItem { id, kind, size, x, y, zone: None, display, sensors: None, path: None, options, crop: None, fit: None });
            }
            WidgetCmd::MoveToDisplay(id, to) => {
                let screen = self.screens.iter().find(|s| s.id == to)?.clone();
                let i = items.iter().position(|w| w.id == id)?;
                let display = screen.tag();
                let probe = WidgetItem { display: display.clone(), ..items[i].clone() };
                let here: Vec<WidgetItem> = items.iter().filter(|o| o.id != id && self.same_screen(o, &probe)).cloned().collect();
                let (x, y) = free_spot(&here, item_dims(&items[i]), screen.area());
                items[i] = WidgetItem { x, y, display, ..items[i].clone() };
            }
            WidgetCmd::Remove(id) => {
                items.retain(|i| i.id != id);
                self.store.notes.remove(&id);
            }
            WidgetCmd::Resize(id, size) => {
                let (w, h) = dims(size);
                let bounds: Vec<Rect> = items.iter().map(|i| self.screen_of(i).bounds()).collect();
                for (i, b) in items.iter_mut().zip(bounds).filter(|(i, _)| i.id == id) {
                    i.size = size;
                    // A fixed size again: no longer the picture's shape.
                    i.fit = None;
                    i.x = i.x.min((b.right() - w).max(b.x));
                    i.y = i.y.min((b.bottom() - h).max(b.y));
                }
            }
            // Opened by the app (a panel of its own).
            WidgetCmd::EditPhoto(_) => return None,
            WidgetCmd::SetDockyStyle(id, style) => {
                let w = items.iter_mut().find(|w| w.id == id)?;
                w.options = (style != "classic").then(|| vec![style.to_string()]);
            }
            WidgetCmd::AddDocky(style) => {
                let kind = WidgetKind::Docky;
                let p = cursor_px();
                let screen = self.screens.iter().find(|s| s.monitor.contains(p.x, p.y)).unwrap_or(&self.screens[0]).clone();
                let display = screen.tag();
                let options = (style != "classic").then(|| vec![style.to_string()]);
                let probe = WidgetItem { id: 0, kind, size: WidgetSize::Small, x: 0.0, y: 0.0, zone: None, display: display.clone(), sensors: None, path: None, options, crop: None, fit: None };
                let here: Vec<WidgetItem> = items.iter().filter(|i| self.same_screen(i, &probe)).cloned().collect();
                let (x, y) = free_spot(&here, dims(WidgetSize::Small), screen.area());
                let id = items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
                items.push(WidgetItem { id, x, y, ..probe });
            }
            WidgetCmd::SetDigital(id, on) => {
                let w = items.iter_mut().find(|w| w.id == id)?;
                let mut o: Vec<String> = w.options.clone().unwrap_or_default().into_iter().filter(|s| s != "digital").collect();
                if on {
                    o.push("digital".into());
                }
                w.options = (!o.is_empty()).then_some(o);
            }
            WidgetCmd::SetPhoto { id, size, crop, fit } => {
                let bounds: Vec<Rect> = items.iter().map(|i| self.screen_of(i).bounds()).collect();
                let (i, b) = items.iter_mut().zip(bounds).find(|(i, _)| i.id == id)?;
                i.size = size;
                i.crop = crop.map(|c| c.to_vec());
                // Never bigger than its display.
                i.fit = fit.map(|(w, h)| {
                    let k = (b.w / w).min(b.h / h).min(1.0);
                    vec![(w * k).max(crate::config::FIT_MIN).round(), (h * k).max(crate::config::FIT_MIN).round()]
                });
                let (w, h) = item_dims(i);
                i.x = i.x.min((b.right() - w).max(b.x));
                i.y = i.y.min((b.bottom() - h).max(b.y));
            }
            WidgetCmd::Align(id, how) => {
                let i = self.widgets.iter().position(|w| w.item.id == id)?;
                let (r, others) = self.rects(i);
                let (x, y) = aligned(r, &others, how, self.screen_of(&items[i]).bounds())?;
                (items[i].x, items[i].y) = (x, y);
            }
            WidgetCmd::MoveTo(id, col, row) => {
                let i = self.widgets.iter().position(|w| w.item.id == id)?;
                let (r, others) = self.rects(i);
                let (x, y) = spot(col, row, r.w, r.h, self.screen_of(&items[i]).area());
                if others.iter().any(|o| overlaps(*o, Rect::new(x, y, r.w, r.h))) {
                    return None;
                }
                (items[i].x, items[i].y) = (x, y);
            }
            WidgetCmd::ToggleSensor(id, key) => self.edit_sensors(&mut items, id, &key, false)?,
            WidgetCmd::LeadSensor(id, key) => self.edit_sensors(&mut items, id, &key, true)?,
            WidgetCmd::Choose(id, folder) => {
                let i = items.iter().position(|w| w.id == id)?;
                let owner = self.widgets.iter().find(|w| w.item.id == id).map(|w| w.surface.content).unwrap_or_default();
                let picked = match (items[i].kind, folder) {
                    (WidgetKind::Video, _) => system::pick_video(owner),
                    (_, true) => system::pick_folder(owner),
                    _ => system::pick_photo(owner),
                }?;
                items[i].path = Some(picked);
                // A new picture: its own middle, at a fixed size.
                items[i].crop = None;
                items[i].fit = None;
            }
            WidgetCmd::ToggleOption(id, key) => {
                let w = items.iter_mut().find(|w| w.id == id)?;
                let mut on: Vec<String> = user::OPTIONS.iter().filter(|(k, _)| user::shows(w, k)).map(|(k, _)| k.to_string()).collect();
                match on.iter().position(|k| k == key) {
                    Some(p) => {
                        on.remove(p);
                    }
                    None => on.push(key.to_string()),
                }
                w.options = (on.len() != user::OPTIONS.len()).then_some(on);
            }
            WidgetCmd::ClearNotifications => {
                crate::island::notifications::remove(None);
                self.data.notes.clear();
                self.mark(&[WidgetKind::Notifications]);
                return None;
            }
            WidgetCmd::DefaultSensors(id) => {
                items.iter_mut().find(|w| w.id == id)?.sensors = None;
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

/// What the widgets show in Screenshot Mode: the live readings that say
/// nothing about you (time, system load, battery, the music's progress) with
/// everything personal swapped for demo data.
fn demo_data(real: &Data, fahrenheit: bool) -> Data {
    use crate::screenshot_mode as demo;
    Data {
        weather: demo::weather(fahrenheit, real.h24),
        events: demo::events(),
        devices: demo::devices(),
        battery: real.battery,
        sys: real.sys.clone(),
        now: real.now.as_ref().map(demo::now_playing),
        zones: demo::zones(),
        first_weekday: real.first_weekday,
        h24: real.h24,
        bands: real.bands.clone(),
        notes: demo::notes(),
        user: Some(demo::user()),
    }
}

/// Notes and reminders in Screenshot Mode (`notes`: the Notes widgets' ids).
fn demo_store(notes: impl Iterator<Item = u32>) -> Store {
    let mut s = Store::default();
    for id in notes {
        s.set_note(id, crate::screenshot_mode::NOTE.into());
    }
    for r in crate::screenshot_mode::REMINDERS {
        s.add_reminder(r);
    }
    s
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
        WidgetItem { id, kind: WidgetKind::Clock, size, x, y, zone: None, display: None, sensors: None, path: None, options: None, crop: None, fit: None }
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
                disks: vec![sensors::Disk { letter: 'C', used: 0.71, free: 140 << 30, total: 476 << 30 }],
                nets: vec![sensors::Net { name: "Wi-Fi".into(), down: 1.2e6, up: 45e3 }],
                down: 1.2e6,
                up: 45e3,
                gpus: vec![
                    sensors::Gpu {
                        id: "10de-2504".into(),
                        name: "NVIDIA GeForce RTX 4070 Laptop GPU".into(),
                        vendor: 0x10de,
                        usage: 0.58,
                        history: (0..60).map(|i| 0.5 + 0.3 * (i as f32 / 7.0).cos()).collect(),
                        mem_used: 5 << 30,
                        mem_total: 8 << 30,
                        dedicated: true,
                        temp: Some(64.0),
                    },
                    sensors::Gpu {
                        id: "8086-9a49".into(),
                        name: "Intel Iris Xe Graphics".into(),
                        vendor: 0x8086,
                        usage: 0.12,
                        history: (0..60).map(|i| 0.1 + 0.05 * (i as f32 / 3.0).sin()).collect(),
                        mem_used: 1 << 30,
                        mem_total: 8 << 30,
                        dedicated: false,
                        temp: None,
                    },
                ],            },
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
            h24: std::env::var_os("DOCKSIDE_GALLERY_H24").is_some(),
            bands: (0..live::BANDS).map(|i| (0.85 - i as f32 / 48.0) * (0.55 + 0.45 * (i as f32 * 0.9).sin().abs())).collect(),
            notes: {
                let note = |id: u32, mins: u64, app: &str, title: &str, body: &str| Note {
                    id,
                    at: std::time::SystemTime::now() - std::time::Duration::from_secs(mins * 60),
                    app: app.into(),
                    app_id: String::new(),
                    title: title.into(),
                    body: body.into(),
                    icon: None,
                };
                vec![
                    note(1, 0, "Messages", "Sam", "Are we still on for lunch tomorrow? I booked the place by the river."),
                    note(2, 12, "Outlook", "Design review moved", "The design review is now at 3 PM in room 4."),
                    note(3, 95, "Discord", "#general", "Alex: the build is green again"),
                ]
            },
            user: Some(UserInfo { name: "Ollie Box".into(), first: "Ollie".into(), picture: None, pc: "OLLIE-LAPTOP".into() }),
        };
        // DOCKSIDE_GALLERY_PRIVATE=1 paints what Screenshot Mode shows instead.
        let private = std::env::var_os("DOCKSIDE_GALLERY_PRIVATE").is_some();
        crate::screenshot_mode::set(private);
        let data = if private { demo_data(&data, false) } else { data };
        let mut store = Store::default();
        store.set_note(1, "Groceries\nMilk, eggs, coffee beans\nPick up the dry cleaning before 6pm on Thursday.".into());
        for r in ["Call the dentist", "Renew passport", "Send invoice to Alex", "Water the plants", "Book train tickets"] {
            store.add_reminder(r);
        }
        if private {
            store = demo_store(std::iter::once(1));
        }
        for dark in [false, true] {
            let mut cfg = crate::config::defaults();
            cfg.general.appearance = if dark { crate::config::Appearance::Dark } else { crate::config::Appearance::Light };
            let theme = Theme::new(&cfg, &gfx);
            let pad = 8.0;
            let (iw, mut placed) = (1100usize, Vec::new());
            let mut y = pad;
            // Every kind, then the Clock again with its digital face, and Docky's other looks.
            let rows = KINDS
                .iter()
                .map(|&k| (k, None))
                .chain(std::iter::once((WidgetKind::Clock, Some("digital"))))
                .chain(crate::docky::STYLES.iter().filter(|(s, _)| !matches!(*s, "classic" | "auto")).map(|(s, _)| (WidgetKind::Docky, Some(*s))))
                // Auto, saying something in each look.
                .chain(["say-classic", "say-peek", "say-pixel"].map(|s| (WidgetKind::Docky, Some(s))))
                // Talking to him: his speech box open, and a long reply.
                .chain(["chat-classic", "chat-pixel"].map(|s| (WidgetKind::Docky, Some(s))));
            for (kind, digital) in rows {
                let mut x = pad;
                let mut row_h: f32 = 0.0;
                for &size in sizes(kind) {
                    let (w, h) = dims(size);
                    placed.push((kind, size, x, y, digital));
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
            for (kind, size, x, y, digital) in placed {
                // System: a bit of everything, two GPUs (more than fits as rings).
                let sensors = (kind == WidgetKind::System).then(|| {
                    ["cpu", "gpu:10de-2504", "memory", "gpu_memory:10de-2504", "gpu_temp:10de-2504", "gpu:8086-9a49", "gpu_temp:8086-9a49", "disk:C", "network:Wi-Fi", "network"]
                        .map(String::from)
                        .to_vec()
                });
                let options = digital.map(|o| vec![o.to_string()]);
                let item = WidgetItem { id: 1, kind, size, x: 0.0, y: 0.0, zone: None, display: None, sensors, path: None, options, crop: None, fit: None };
                let (w, h) = dims(size);
                let (cw, ch) = ((w + 2.0 * SHADOW) as i32, (h + 2.0 * SHADOW) as i32);
                let mut canvas = Canvas::new(&gfx, cw, ch, 1.0).unwrap();
                // Photo: a made-up 3:2 picture, to see it cover the card.
                let photo = (kind == WidgetKind::Photo).then(|| {
                    let (pw, ph) = (300u32, 200u32);
                    let mut data = Vec::with_capacity((pw * ph * 4) as usize);
                    for yy in 0..ph {
                        for xx in 0..pw {
                            let sun = ((xx as f32 - 210.0).hypot(yy as f32 - 60.0) < 26.0) as u8;
                            let hill = (yy as f32 > 140.0 - 30.0 * (xx as f32 / 40.0).sin()) as u8;
                            let (b, g, r) = if sun == 1 { (90, 210, 255) } else if hill == 1 { (60, 140, 50) } else { (230, 170, 90 + yy as u8 / 3) };
                            data.extend([b, g, r, 255]);
                        }
                    }
                    IconPixels { width: pw, height: ph, data }.to_bitmap(canvas.target()).unwrap()
                });
                let typing = Edit { field: TextField::new("name my goldfish?"), scroll: 0.0 };
                let chatting = digital.is_some_and(|o| o.starts_with("chat-"));
                let item = WidgetItem {
                    options: item.options.as_ref().map(|o| o.iter().map(|s| s.replace("chat-", "")).collect()),
                    ..item
                };
                let ctx = views::Ctx {
                    theme: &theme,
                    data: &data,
                    store: &store,
                    item: &item,
                    edit: chatting.then_some(&typing),
                    art: photo.as_ref(),
                    hover: None,
                    motion: None,
                    pictures: &[],
                    error: None,
                    docky: None,
                    docky_say: digital.filter(|o| o.starts_with("chat-")).map(|_| {
                        ("ooh a fishy friend! how about bubbles, finn, or captain splash? hehe".to_string(), 1.0)
                    }),
                    docky_scene: digital.and_then(|o| o.strip_prefix("say-")).map(|look| {
                        let (look, said) = match look {
                            "peek" => ("peek", "Boo!"),
                            "pixel" => ("pixel", "> focus mode"),
                            _ => ("classic", "Ooh, Visual Studio Code!"),
                        };
                        crate::docky::Scene { look, from: None, say: Some((said.to_string(), 1.0)) }
                    }),
                };
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
    fn aligns_with_the_nearest_neighbour_that_fits() {
        let screen = Rect::new(0.0, 24.0, 1280.0, 776.0);
        let me = Rect::new(60.0, 300.0, 170.0, 170.0);
        // Above it, a little to the left; beside it, to the right.
        let above = Rect::new(24.0, 48.0, 364.0, 170.0);
        let beside = Rect::new(300.0, 310.0, 170.0, 170.0);
        let others = [above, beside];
        // Left edges with `beside` would overlap it, so `above` is used.
        assert_eq!(aligned(me, &others, Alignment::Left, screen), Some((24.0, 300.0)));
        assert_eq!(aligned(me, &others, Alignment::Centre, screen), Some((121.0, 300.0)));
        // Lining its top up with the one beside it doesn't collide.
        assert_eq!(aligned(me, &others, Alignment::Top, screen), Some((60.0, 310.0)));
        // Right edges with either would run into `beside`.
        assert_eq!(aligned(me, &others, Alignment::Right, screen), None);
        // Already lined up: stays put.
        assert_eq!(aligned(Rect::new(24.0, 300.0, 170.0, 170.0), &[above], Alignment::Left, screen), Some((24.0, 300.0)));
        assert_eq!(aligned(me, &[], Alignment::Left, screen), None);
    }

    #[test]
    fn spots_cover_the_area() {
        let area = Rect::new(24.0, 48.0, 1232.0, 728.0);
        assert_eq!(spot(0, 0, 170.0, 170.0, area), (24.0, 48.0));
        assert_eq!(spot(1, 1, 170.0, 170.0, area), (555.0, 327.0));
        assert_eq!(spot(2, 2, 170.0, 170.0, area), (1086.0, 606.0));
    }

    #[test]
    fn free_spot_skips_taken() {
        let area = Rect::new(24.0, 48.0, 1232.0, 728.0);
        assert_eq!(free_spot(&[], dims(WidgetSize::Small), area), (24.0, 48.0));
        let items = [item(1, WidgetSize::Small, 24.0, 48.0)];
        let (x, y) = free_spot(&items, dims(WidgetSize::Small), area);
        assert!(!overlaps(Rect::new(x, y, SMALL, SMALL), item_rect(&items[0])));
        assert_eq!(x, 24.0);
        assert_eq!(y, 48.0 + SMALL + GAP);
    }
}
