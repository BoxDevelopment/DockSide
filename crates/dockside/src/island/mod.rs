//! The Dynamic Island: an always-present black pill at the top center of the
//! screen that morphs between live activities.
//!
//! * **Idle**: a small pill in the menu bar. Click it for a home panel (time,
//!   next event, quick timers); drop files on it to hold them on the shelf.
//! * **Compact activities** (persistent): timer / stopwatch, Now Playing,
//!   downloads, an upcoming meeting and the file shelf. When two are running,
//!   the second splits off into a bubble beside the pill.
//! * **Alerts** (transient): volume / brightness, charging, Caps Lock, Do Not
//!   Disturb, screenshots, finished downloads, Bluetooth devices, mic / camera
//!   use and notifications. They queue, and pause while the pointer is on them.
//! * **Panels** (expanded): click an activity to open its player / controls.
//!
//! Everything moves on springs: the pill's size, its hover and press squish,
//! content cross-fades, the bubble splitting off, the waveform's energy. The
//! surface is resized to the pill (plus a shadow margin) every frame, so the
//! rest of the screen stays click-through.

mod bluetooth;
mod calendar;
pub mod claude;
mod glyphs;
mod ics;
pub mod media;
mod notifications;
mod shelf;
pub mod sources;
pub mod timer;
mod views;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::core::{w, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::DirectWrite::IDWriteTextFormat;
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindowVisible, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
};

use crate::anim::Spring;
use crate::config::{Backdrop, Config, ConfigValue, IslandCfg};
use crate::finder::dnd::Phase;
use crate::geom::{Color, Rect};
use crate::gfx::icons::{load_shell_image, IconPixels};
use crate::gfx::{Align, Gfx, Weight};
use crate::menu::{Action, Entry, Placement};
use crate::surface::Surface;
use crate::theme::Theme;
use crate::win::system::Battery;
use crate::win::Monitor;
use calendar::CalEvent;
use media::{Cmd, Media, NowPlaying};
use notifications::Note;
use sources::{Inbox, Privacy, Signal, Stop, VolumeKey};
use timer::Clock;
use views::View;

/// Transparent room around the pill for its shadow, in points.
const MARGIN_X: f32 = 28.0;
const MARGIN_BOTTOM: f32 = 40.0;
/// Icons inside alerts don't grow past a compact pill's height.
const COMPACT_MAX: f32 = 30.0;
/// How long after the Caps Lock alert appears its switch flips.
const CAPS_SWITCH_DELAY: f32 = 0.18;
/// Most desktops the switch indicator draws dots for.
const MAX_DESKTOP_DOTS: usize = 9;
const PANEL_RADIUS: f32 = 36.0;
/// Extra height at the bottom of a panel for its page dots.
const DOTS_H: f32 = 16.0;
/// Horizontal scroll needed for one page (a notch is 120), and the pause after
/// a switch so one swipe moves one page.
const SWIPE_STEP: f32 = 90.0;
const SWIPE_COOLDOWN: Duration = Duration::from_millis(250);
/// How far content slides when switching pages, in points.
const SLIDE: f32 = 42.0;

/// Pointer away from an expanded island this long collapses it.
const COLLAPSE_DELAY: f32 = 0.9;
/// Pointer resting on the island this long expands it (`expand_on_hover`).
const HOVER_EXPAND_DELAY: f32 = 0.35;
/// How far the pointer moves before a press on a file becomes a drag.
const DRAG_THRESHOLD: f32 = 5.0;

const GREEN: Color = Color::rgba(48, 209, 88, 1.0);
const RED: Color = Color::rgba(255, 69, 58, 1.0);
const ORANGE: Color = Color::rgba(255, 159, 10, 1.0);
const BLUE: Color = Color::rgba(10, 132, 255, 1.0);
const PURPLE: Color = Color::rgba(125, 122, 255, 1.0);
const CLAUDE: Color = Color::rgba(217, 119, 87, 1.0);
const WHITE: Color = Color::gray(255, 0.96);
const SECONDARY: Color = Color::gray(255, 0.55);

/// Long-running activities, in priority order: the first is the pill, the second the bubble.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Persist {
    Timer,
    Claude,
    Media,
    Download,
    Calendar,
    Shelf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Panel {
    Home,
    Claude,
    Media,
    Timer,
    Shelf,
    Calendar,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Content {
    Idle,
    Compact(Persist),
    Alert(u64),
    Panel(Panel),
}

pub enum Alert {
    Charging(u8),
    LowBattery(u8),
    /// Reads the live level from the island, so repeated key presses just update it.
    Volume,
    Brightness(u8),
    Caps,
    Dnd(bool),
    Screenshot(PathBuf),
    Downloaded(PathBuf),
    Bluetooth { name: String, connected: bool, battery: Option<u8> },
    Privacy { camera: bool, app: String },
    Note(Note),
    TimerDone,
    ClaudeDone { project: String, secs: f32, tools: u32 },
    ClaudeNeeds { project: String, message: String },
    /// A virtual desktop switch; `missing` is a desktop number that doesn't exist.
    Desktop { index: usize, count: usize, name: String, missing: Option<usize> },
}

impl Alert {
    /// Heads-up displays update in place and show over panels.
    fn is_hud(&self) -> bool {
        matches!(self, Alert::Volume | Alert::Brightness(_) | Alert::Desktop { .. })
    }

    fn same_kind(&self, other: &Alert) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

struct Showing {
    id: u64,
    alert: Alert,
    until: Instant,
    img: Option<ID2D1Bitmap>,
    img_tried: bool,
}

pub struct Download {
    /// The partial file (".crdownload", ".part", ...).
    pub path: PathBuf,
    pub size: u64,
}

impl Download {
    pub fn name(&self) -> String {
        sources::finished_name(&self.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Pill,
    Bubble,
    Art,
    Prev,
    Play,
    Next,
    Progress,
    Slider,
    TimerToggle,
    TimerCancel,
    /// Quick timer in seconds (0 = stopwatch).
    Chip(u32),
    ShelfItem(usize),
    ShelfRemove(usize),
    ShelfClear,
    Event(usize),
    AlertBody,
    /// A page dot under an expanded panel.
    Page(usize),
}

/// Commands from menus and Spotlight.
#[derive(Clone, Debug)]
pub enum IslandCmd {
    Timer(u32),
    Stopwatch,
    ToggleClock,
    CancelClock,
    ClearShelf,
}

pub enum IslandEvent {
    /// Bring the app that owns the media session (or a notification) to the front.
    OpenApp(String),
    Run(Action),
    Menu(Vec<Entry>, Placement),
    /// Drag files out of the island (modal; run outside the App borrow).
    DragOut(Vec<PathBuf>),
}

pub struct Fonts {
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    small_l: IDWriteTextFormat,
    small_r: IDWriteTextFormat,
    small_c: IDWriteTextFormat,
    alert: IDWriteTextFormat,
    alert_b: IDWriteTextFormat,
    alert_r: IDWriteTextFormat,
    alert_title: IDWriteTextFormat,
    big: IDWriteTextFormat,
    chip: IDWriteTextFormat,
    header: IDWriteTextFormat,
}

impl Fonts {
    fn new(gfx: &Gfx, font: &str) -> Self {
        Self {
            title: gfx.format(font, 15.0, Weight::Bold, Align::Leading),
            body: gfx.format(font, 13.5, Weight::Regular, Align::Leading),
            small_l: gfx.format(font, 11.0, Weight::Regular, Align::Leading),
            small_r: gfx.format(font, 11.0, Weight::Regular, Align::Trailing),
            small_c: gfx.format(font, 11.0, Weight::Regular, Align::Center),
            alert: gfx.format(font, 13.0, Weight::Regular, Align::Leading),
            alert_b: gfx.format(font, 13.0, Weight::Bold, Align::Trailing),
            alert_r: gfx.format(font, 13.0, Weight::Regular, Align::Trailing),
            alert_title: gfx.format(font, 13.5, Weight::Bold, Align::Leading),
            big: gfx.format(font, 34.0, Weight::Regular, Align::Leading),
            chip: gfx.format(font, 12.5, Weight::Bold, Align::Center),
            header: gfx.format(font, 12.5, Weight::Bold, Align::Leading),
        }
    }
}

pub struct Island {
    pub surface: Surface,
    cfg: IslandCfg,
    monitor: Monitor,
    accent: Color,
    /// Top of the pill and its compact height, in points.
    top: f32,
    compact_h: f32,

    // Sources.
    media: Media,
    inbox: Inbox,
    stop: Stop,
    _watcher: Option<notify::RecommendedWatcher>,

    // Activities.
    now: Option<NowPlaying>,
    paused_since: Option<Instant>,
    art: Option<(u64, ID2D1Bitmap)>,
    old_art: Option<ID2D1Bitmap>,
    art_fade: Spring,
    clock: Option<Clock>,
    shelf: Vec<PathBuf>,
    shelf_icons: HashMap<PathBuf, ID2D1Bitmap>,
    events: Vec<CalEvent>,
    downloads: Vec<Download>,
    privacy: Privacy,
    battery: Option<Battery>,
    volume: (f32, bool),
    caps: bool,
    dnd: Option<bool>,
    last_bt: Option<(String, Instant)>,
    claude: claude::Claude,

    // Alerts.
    alert: Option<Showing>,
    prev_alert: Option<Showing>,
    queue: VecDeque<(Alert, f32)>,
    next_alert_id: u64,

    // Shape and motion.
    expanded: Option<Panel>,
    drag_over: bool,
    dragging_out: bool,
    shown: Content,
    bubble_kind: Option<Persist>,
    w: Spring,
    h: Spring,
    presence: Spring,
    content: Spring,
    bubble: Spring,
    hover: Spring,
    press: Spring,
    energy: Spring,
    bar_hover: Spring,
    /// Desktop indicator position (desktop index, sliding).
    desk: Spring,
    last_desk: usize,
    /// Caps Lock switch knob: 0 off, 1 on. It starts at the old state and
    /// slides over once the alert has opened (`caps_delay` seconds).
    caps_knob: Spring,
    caps_delay: f32,
    /// Page switching: which activity leads, the slide direction and offset.
    preferred: Option<Persist>,
    page_dir: f32,
    slide: Spring,
    dot_pos: Spring,
    swipe: f32,
    swipe_at: Instant,
    wave_t: f32,
    time: f32,
    since_draw: f32,
    pulse_at: f32,

    // Pointer.
    pub tracking: bool,
    inside: bool,
    hover_since: Option<Instant>,
    leave_at: Option<Instant>,
    hot: Option<Hit>,
    down: Option<Hit>,
    down_at: (f32, f32),
    scrub: Option<f32>,
    hits: Vec<(Rect, Hit)>,
    pill_rect: Rect,

    fullscreen: bool,
    pub dirty: bool,
}

impl Island {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: Monitor, hwnd: isize, msg_media: u32, msg_signal: u32) -> Result<Self> {
        let bar_h = if cfg.menubar.enabled { cfg.menubar.height } else { 0.0 };
        let (top, compact_h) =
            if bar_h > 0.0 { let h = (bar_h - 2.0).clamp(18.0, 36.0); ((bar_h - h) / 2.0, h) } else { (6.0, 30.0) };
        let (iw, ih) = blob_size(compact_h);
        let rect = window_rect(&monitor, top, iw, ih);
        let surface = Surface::new(gfx, rect, monitor.scale, Backdrop::None, Color::TRANSPARENT)?;
        let ic = &cfg.island;
        let inbox = Inbox::new(hwnd, msg_signal);
        let stop = Stop::default();
        let watcher = if ic.screenshots || ic.downloads { sources::watch_folders(inbox.clone()) } else { None };
        if ic.volume_hud {
            sources::watch_brightness(inbox.clone(), stop.clone());
        }
        if ic.calendar {
            calendar::watch(inbox.clone(), stop.clone(), calendar::links(&ic.calendar_ics));
        }
        if ic.notifications {
            notifications::watch(inbox.clone(), stop.clone());
        }
        if ic.bluetooth {
            bluetooth::watch(inbox.clone(), stop.clone());
        }
        shelf::register(surface.content);

        let mut island = Self {
            surface,
            cfg: ic.clone(),
            monitor,
            accent: theme.accent,
            top,
            compact_h,
            media: Media::start(hwnd, msg_media),
            inbox,
            stop,
            _watcher: watcher,
            now: None,
            paused_since: None,
            art: None,
            old_art: None,
            art_fade: Spring::new(1.0, 0.45, 1.0),
            clock: None,
            shelf: Vec::new(),
            shelf_icons: HashMap::new(),
            events: Vec::new(),
            downloads: Vec::new(),
            privacy: sources::privacy(),
            battery: crate::win::system::battery(),
            volume: sources::volume().unwrap_or((0.5, false)),
            caps: sources::caps_lock(),
            dnd: sources::do_not_disturb(),
            last_bt: None,
            claude: claude::Claude::default(),
            alert: None,
            prev_alert: None,
            queue: VecDeque::new(),
            next_alert_id: 0,
            expanded: None,
            drag_over: false,
            dragging_out: false,
            shown: Content::Idle,
            bubble_kind: None,
            w: Spring::new(iw, 0.42, 0.72),
            h: Spring::new(ih, 0.42, 0.72),
            presence: Spring::new(0.0, 0.32, 1.0),
            content: Spring::new(1.0, 0.16, 1.0),
            bubble: Spring::new(0.0, 0.42, 0.66),
            hover: Spring::new(0.0, 0.3, 0.55),
            press: Spring::new(0.0, 0.18, 0.8),
            energy: Spring::new(0.0, 0.5, 1.0),
            bar_hover: Spring::new(0.0, 0.22, 1.0),
            desk: Spring::new(0.0, 0.32, 0.72),
            last_desk: 0,
            caps_knob: Spring::new(0.0, 0.3, 0.8),
            caps_delay: 0.0,
            preferred: None,
            page_dir: 0.0,
            slide: Spring::new(0.0, 0.36, 0.86),
            dot_pos: Spring::new(0.0, 0.32, 0.78),
            swipe: 0.0,
            swipe_at: Instant::now(),
            wave_t: 0.0,
            time: 0.0,
            since_draw: 0.0,
            pulse_at: 0.0,
            tracking: false,
            inside: false,
            hover_since: None,
            leave_at: None,
            hot: None,
            down: None,
            down_at: (0.0, 0.0),
            scrub: None,
            hits: Vec::new(),
            pill_rect: Rect::default(),
            fullscreen: false,
            dirty: true,
        };
        island.surface.set_hidden(true);
        island.retarget();
        Ok(island)
    }

    // --- what to show --------------------------------------------------------

    fn media_visible(&self) -> bool {
        let Some(now) = &self.now else { return false };
        if now.playing || self.cfg.hide_paused_after == 0 {
            return true;
        }
        self.paused_since.is_none_or(|t| t.elapsed().as_secs() < self.cfg.hide_paused_after as u64)
    }

    fn next_event(&self) -> Option<&CalEvent> {
        let now = calendar::unix_now();
        self.events.iter().find(|e| e.end > now)
    }

    /// An event starting soon (or just started) becomes a compact activity.
    fn event_soon(&self) -> bool {
        self.next_event().is_some_and(|e| {
            let s = e.starts_in();
            s <= self.cfg.calendar_lead as i64 * 60 && s > -300
        })
    }

    /// Running activities with the one you last looked at first.
    fn persistent(&self) -> Vec<Persist> {
        let mut list = self.natural();
        if let Some(i) = self.preferred.and_then(|p| list.iter().position(|x| *x == p)) {
            let p = list.remove(i);
            list.insert(0, p);
        }
        list
    }

    /// Panels you can page between while expanded, in a stable order.
    fn pages(&self) -> Vec<Panel> {
        self.natural().into_iter().filter_map(Self::panel_for).collect()
    }

    fn dots_for(&self, p: Panel) -> bool {
        let pages = self.pages();
        pages.len() > 1 && pages.contains(&p)
    }

    fn persist_for(p: Panel) -> Option<Persist> {
        match p {
            Panel::Media => Some(Persist::Media),
            Panel::Claude => Some(Persist::Claude),
            Panel::Timer => Some(Persist::Timer),
            Panel::Shelf => Some(Persist::Shelf),
            Panel::Calendar => Some(Persist::Calendar),
            Panel::Home => None,
        }
    }

    /// Moves to the next / previous activity: a panel page when expanded, the
    /// pill's lead activity (swapping with the bubble) when compact.
    fn switch_page(&mut self, dir: i32) -> bool {
        match self.wanted().0 {
            Content::Panel(cur) => {
                let pages = self.pages();
                let Some(i) = pages.iter().position(|p| *p == cur).filter(|_| pages.len() > 1) else { return false };
                let next = pages[(i as i32 + dir).rem_euclid(pages.len() as i32) as usize];
                self.go_to(next, dir);
            }
            Content::Compact(cur) => {
                let list = self.natural();
                let Some(i) = list.iter().position(|p| *p == cur).filter(|_| list.len() > 1) else { return false };
                self.preferred = Some(list[(i as i32 + dir).rem_euclid(list.len() as i32) as usize]);
                self.page_dir = dir as f32;
            }
            _ => return false,
        }
        self.retarget();
        self.dirty = true;
        true
    }

    fn go_to(&mut self, panel: Panel, dir: i32) {
        if self.expanded == Some(panel) {
            return;
        }
        self.page_dir = dir.signum() as f32;
        self.expanded = Some(panel);
        // Whatever you looked at last stays in front when the island folds up.
        self.preferred = Self::persist_for(panel).or(self.preferred);
        self.leave_at = None;
    }

    fn natural(&self) -> Vec<Persist> {
        let mut list = Vec::new();
        if self.clock.is_some() {
            list.push(Persist::Timer);
        }
        if self.cfg.claude_code && self.claude.current().is_some() {
            list.push(Persist::Claude);
        }
        if self.media_visible() {
            list.push(Persist::Media);
        }
        if !self.downloads.is_empty() {
            list.push(Persist::Download);
        }
        if self.event_soon() {
            list.push(Persist::Calendar);
        }
        if !self.shelf.is_empty() {
            list.push(Persist::Shelf);
        }
        list
    }

    fn panel_valid(&self, p: Panel) -> bool {
        match p {
            Panel::Media => self.media_visible(),
            Panel::Claude => self.claude.current().is_some(),
            Panel::Timer => self.clock.is_some(),
            Panel::Home | Panel::Shelf | Panel::Calendar => true,
        }
    }

    fn panel_for(p: Persist) -> Option<Panel> {
        match p {
            Persist::Timer => Some(Panel::Timer),
            Persist::Claude => Some(Panel::Claude),
            Persist::Media => Some(Panel::Media),
            Persist::Calendar => Some(Panel::Calendar),
            Persist::Shelf => Some(Panel::Shelf),
            Persist::Download => None,
        }
    }

    /// The pill's content and the bubble's activity.
    fn wanted(&self) -> (Content, Option<Persist>) {
        if self.drag_over {
            return (Content::Panel(Panel::Shelf), None);
        }
        let hud = self.alert.as_ref().filter(|s| s.alert.is_hud());
        if let Some(s) = hud {
            return (Content::Alert(s.id), None);
        }
        if let Some(p) = self.expanded.filter(|p| self.panel_valid(*p)) {
            return (Content::Panel(p), None);
        }
        if let Some(s) = &self.alert {
            return (Content::Alert(s.id), None);
        }
        let list = self.persistent();
        match list.first() {
            Some(&first) => (Content::Compact(first), list.get(1).copied()),
            None => (Content::Idle, None),
        }
    }

    fn alert_by_id(&self, id: u64) -> Option<&Showing> {
        [&self.alert, &self.prev_alert].into_iter().flatten().find(|s| s.id == id)
    }

    fn size_of(&self, c: Content) -> (f32, f32) {
        let ch = self.compact_h;
        match c {
            Content::Idle => (ch * 3.6, ch),
            Content::Compact(p) => match p {
                Persist::Media => (196.0, ch),
                Persist::Timer => (176.0, ch),
                Persist::Claude => (264.0, ch),
                Persist::Download => (240.0, ch),
                Persist::Calendar => (276.0, ch),
                Persist::Shelf => (152.0, ch),
            },
            Content::Alert(id) => match self.alert_by_id(id).map(|s| &s.alert) {
                Some(Alert::Charging(_) | Alert::LowBattery(_)) => (264.0, ch),
                Some(Alert::Volume | Alert::Brightness(_)) => (300.0, ch),
                Some(Alert::Desktop { count, missing, .. }) => {
                    let dots = (*count).min(MAX_DESKTOP_DOTS) as f32 * views::DESK_DOT_GAP;
                    ((if missing.is_some() { 178.0 } else { 150.0 }) + dots, ch)
                }
                Some(Alert::Caps) => (236.0, ch),
                Some(Alert::Dnd(_)) => (264.0, ch),
                Some(Alert::Screenshot(_)) => (320.0, 62.0),
                Some(Alert::Downloaded(_)) => (330.0, ch),
                Some(Alert::Bluetooth { .. }) => (320.0, 62.0),
                Some(Alert::Privacy { .. }) => (272.0, ch),
                Some(Alert::Note(_)) => (380.0, 74.0),
                Some(Alert::TimerDone) => (220.0, ch),
                Some(Alert::ClaudeDone { .. }) => (330.0, ch),
                Some(Alert::ClaudeNeeds { .. }) => (400.0, ch),
                None => (ch * 3.6, ch),
            },
            Content::Panel(p) => {
                let (w, h) = self.panel_size(p);
                (w, h + if self.dots_for(p) { DOTS_H } else { 0.0 })
            }
        }
    }

    fn panel_size(&self, p: Panel) -> (f32, f32) {
        match p {
                Panel::Media => (372.0, 166.0),
                Panel::Claude => (372.0, 126.0),
                Panel::Timer => (340.0, 96.0),
                Panel::Shelf => (420.0, 140.0),
                Panel::Calendar => {
                    let n = self.events.iter().filter(|e| e.end > calendar::unix_now()).take(3).count().max(1);
                    (360.0, 48.0 + 44.0 * n as f32)
                }
                Panel::Home => (372.0, 134.0),
        }
    }

    /// Points every spring at the state we should be in.
    fn retarget(&mut self) {
        if self.expanded.is_some_and(|p| !self.panel_valid(p)) {
            self.expanded = None;
        }
        let (want, bubble) = self.wanted();
        let present = want != Content::Idle || self.cfg.always_show;
        self.presence.set(if present { 1.0 } else { 0.0 });
        if present && self.presence.value < 0.02 {
            // Emerging from nothing: start as a small blob and spring outwards.
            let (bw, bh) = blob_size(self.compact_h);
            self.w.snap(bw);
            self.h.snap(bh);
            self.shown = want;
            self.content.snap(1.0);
        }
        let (tw, th) = self.size_of(want);
        if tw != self.w.target || th != self.h.target {
            // Growing is bouncy, shrinking a little firmer, like the real thing.
            let grow = th > self.h.value + 1.0 || tw > self.w.value + 1.0;
            let (response, damping) = if grow { (0.46, 0.68) } else { (0.4, 0.82) };
            self.w = self.w.with(response, damping);
            self.h = self.h.with(response, damping);
            self.w.set(tw);
            self.h.set(th);
        }
        // Cross-fade: the old content fades out, then the new one fades in as the pill reaches its size.
        if self.shown != want {
            if self.content.value < 0.05 {
                // Opening a panel puts the page capsule straight on its dot; paging glides it.
                if let (false, Content::Panel(p)) = (matches!(self.shown, Content::Panel(_)), want) {
                    if let Some(i) = self.pages().iter().position(|x| *x == p) {
                        self.dot_pos.snap(i as f32);
                    }
                }
                self.shown = want;
                self.content.set(1.0);
                // A page switch slides the new content in from the side it came from.
                if self.page_dir != 0.0 {
                    self.slide.snap(self.page_dir);
                    self.slide.set(0.0);
                    self.page_dir = 0.0;
                }
            } else {
                self.content.set(0.0);
            }
        } else {
            self.content.set(1.0);
        }
        let show_bubble = bubble.is_some() && matches!(want, Content::Compact(_));
        if show_bubble && self.bubble_kind != bubble {
            if self.bubble.value < 0.05 {
                self.bubble_kind = bubble;
                self.bubble.set(1.0);
            } else {
                self.bubble.set(0.0);
            }
        } else {
            self.bubble.set(if show_bubble { 1.0 } else { 0.0 });
        }

        let playing = self.now.as_ref().is_some_and(|n| n.playing);
        self.energy.set(if playing { 1.0 } else { 0.0 });
        self.press.set(if self.down == Some(Hit::Pill) { 1.0 } else { 0.0 });
        let compact = matches!(want, Content::Idle | Content::Compact(_));
        self.hover.set(if self.inside && compact { 1.0 } else { 0.0 });
        if let Content::Panel(p) = self.shown {
            if let Some(i) = self.pages().iter().position(|x| *x == p) {
                self.dot_pos.set(i as f32);
            }
        }
        let bar = matches!(self.hot, Some(Hit::Progress | Hit::Slider)) || self.scrub.is_some() || self.down == Some(Hit::Slider);
        self.bar_hover.set(if bar { 1.0 } else { 0.0 });
    }

    /// How close the pill is to the shown content's size (0-1): new content fades in as it fits.
    fn fit(&self) -> f32 {
        let (tw, th) = self.size_of(self.shown);
        let err = (self.w.value - tw).abs() / tw.max(1.0) + (self.h.value - th).abs() / th.max(1.0);
        (1.0 - err * 2.5).clamp(0.0, 1.0)
    }

    fn pill_size(&self) -> (f32, f32) {
        // Hover lift and press squish only apply to the compact pill.
        let compact = (1.0 - (self.h.value - self.compact_h) / 12.0).clamp(0.0, 1.0);
        let (hv, pr) = (self.hover.value * compact, self.press.value * compact);
        let w = self.w.value * (1.0 + 0.04 * hv - 0.035 * pr) + 6.0 * hv;
        let h = self.h.value * (1.0 + 0.07 * hv - 0.06 * pr);
        (w.max(1.0), h.max(1.0))
    }

    pub fn set_fullscreen(&mut self, on: bool) {
        self.fullscreen = on;
        self.sync_visibility();
        self.dirty = true;
    }

    fn sync_visibility(&mut self) {
        let idle = self.presence.target == 0.0 && self.presence.value < 0.01;
        let hide = self.fullscreen || idle;
        let was_hidden = !self.surface_visible();
        self.surface.set_hidden(hide);
        if was_hidden && !hide {
            // Stay above the menu bar, which may have been re-shown since.
            unsafe {
                let _ = SetWindowPos(self.surface.content, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    fn surface_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.surface.content).as_bool() }
    }

    /// A playful bump, like the island acknowledging a change.
    fn nudge(&mut self) {
        if self.expanded.is_none() && self.presence.value > 0.5 {
            self.hover.velocity += 7.0;
        }
    }

    // --- alerts ----------------------------------------------------------------

    fn show_alert(&mut self, alert: Alert, secs: f32) {
        let until = Instant::now() + Duration::from_secs_f32(secs);
        if let Some(s) = self.alert.as_mut() {
            // Volume / brightness / Caps Lock repeat: update the one on screen.
            if s.alert.same_kind(&alert) && (alert.is_hud() || matches!(alert, Alert::Caps | Alert::Dnd(_))) {
                s.alert = alert;
                s.until = until;
                self.dirty = true;
                return;
            }
        }
        let urgent = alert.is_hud() || matches!(alert, Alert::TimerDone | Alert::Caps | Alert::Dnd(_));
        if self.alert.is_some() && !urgent {
            self.queue.push_back((alert, secs));
            return;
        }
        if let Some(old) = self.alert.take() {
            let left = old.until.saturating_duration_since(Instant::now()).as_secs_f32();
            if !old.alert.is_hud() && left > 0.5 {
                // Interrupted alerts come back afterwards with what they had left.
                self.queue.push_front((old.alert, left));
            }
        }
        self.next_alert_id += 1;
        self.prev_alert = None;
        self.alert = Some(Showing { id: self.next_alert_id, alert, until, img: None, img_tried: false });
        self.nudge();
        self.retarget();
    }

    fn dismiss_alert(&mut self) {
        self.prev_alert = self.alert.take();
        if let Some((a, secs)) = self.queue.pop_front() {
            self.show_alert(a, secs);
        }
        self.retarget();
    }

    // --- inputs from the app -------------------------------------------------------

    /// Now Playing changed (posted by the media worker).
    pub fn on_media(&mut self) {
        let next = self.media.snapshot();
        if let (Some(a), Some(b)) = (&self.now, &next) {
            if !a.same_track(b) || a.playing != b.playing {
                self.nudge();
            }
        }
        match &next {
            Some(n) if !n.playing => {
                if self.paused_since.is_none() || self.now.as_ref().is_some_and(|o| o.playing) {
                    self.paused_since = Some(Instant::now());
                }
            }
            _ => self.paused_since = None,
        }
        self.now = next;
        self.retarget();
        self.dirty = true;
    }

    /// Signals from the background sources.
    pub fn on_signals(&mut self) {
        for s in self.inbox.drain() {
            match s {
                Signal::Brightness(b) => self.show_alert(Alert::Brightness(b), 1.6),
                Signal::Screenshot(p) => self.show_alert(Alert::Screenshot(p), 4.5),
                Signal::DownloadActive(p) => {
                    let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                    match self.downloads.iter_mut().find(|d| d.path == p) {
                        Some(d) => d.size = size,
                        None => {
                            self.downloads.push(Download { path: p, size });
                            self.nudge();
                        }
                    }
                }
                Signal::DownloadGone(p) => {
                    let had = self.downloads.iter().any(|d| d.path == p);
                    self.downloads.retain(|d| d.path != p);
                    let done = sources::finished_name(&p);
                    if had && done.is_file() && !sources::is_partial(&done) {
                        self.show_alert(Alert::Downloaded(done), 4.0);
                    }
                }
                Signal::Bluetooth { name, connected, battery } => {
                    // Classic and LE endpoints of one device both report; show it once.
                    let dup = self.last_bt.as_ref().is_some_and(|(n, t)| *n == name && t.elapsed() < Duration::from_secs(5));
                    if !dup {
                        self.last_bt = Some((name.clone(), Instant::now()));
                        self.show_alert(Alert::Bluetooth { name, connected, battery }, 4.0);
                    }
                }
                Signal::Notification(n) => self.show_alert(Alert::Note(n), 5.0),
                Signal::Calendar(events) => self.events = events,
            }
        }
        self.retarget();
        self.dirty = true;
    }

    /// Called every second: battery, mic / camera, Do Not Disturb, download sizes.
    pub fn on_tick(&mut self, b: Option<Battery>) -> bool {
        let prev = std::mem::replace(&mut self.battery, b);
        if let (true, Some(prev), Some(b)) = (self.cfg.battery_alerts, prev, b) {
            if b.charging && !prev.charging {
                self.show_alert(Alert::Charging(b.percent), 3.2);
            } else if !b.charging && b.percent < prev.percent && (b.percent == 20 || b.percent == 10) {
                self.show_alert(Alert::LowBattery(b.percent), 3.2);
            }
        }
        if self.cfg.privacy_indicators {
            let p = sources::privacy();
            let old = std::mem::replace(&mut self.privacy, p.clone());
            for app in p.camera.iter().filter(|a| !old.camera.contains(a)) {
                self.show_alert(Alert::Privacy { camera: true, app: app.clone() }, 3.0);
            }
            for app in p.mic.iter().filter(|a| !old.mic.contains(a)) {
                self.show_alert(Alert::Privacy { camera: false, app: app.clone() }, 3.0);
            }
        }
        if self.cfg.status_alerts {
            let d = sources::do_not_disturb();
            if let (Some(old), Some(new)) = (self.dnd, d) {
                if old != new {
                    self.show_alert(Alert::Dnd(new), 2.0);
                }
            }
            self.dnd = d.or(self.dnd);
        }
        for d in &mut self.downloads {
            d.size = std::fs::metadata(&d.path).map(|m| m.len()).unwrap_or(d.size);
        }
        self.claude.prune();
        // A missed rename event must not leave a download spinning forever.
        self.downloads.retain(|d| d.path.exists());
        self.retarget();
        self.dirty = true;
        self.presence.value > 0.0
    }

    /// A Claude Code hook event relayed by `dockside --claude-hook`.
    pub fn on_claude(&mut self, json: &str) {
        if !self.cfg.claude_code {
            return;
        }
        let was = self.claude.current().is_some();
        match self.claude.handle(json) {
            Some(claude::Event::Finished { project, secs, tools }) => {
                self.show_alert(Alert::ClaudeDone { project, secs, tools }, 4.0);
            }
            Some(claude::Event::Needs { project, message }) => {
                self.show_alert(Alert::ClaudeNeeds { project, message }, 6.0);
            }
            None => {}
        }
        if !was && self.claude.current().is_some() {
            self.nudge();
        }
        self.retarget();
        self.dirty = true;
    }

    pub fn on_volume_key(&mut self, key: VolumeKey) {
        if let Some(v) = sources::press_volume(key) {
            self.volume = v;
        }
        self.show_alert(Alert::Volume, 1.6);
    }

    /// The current virtual desktop changed: the indicator slides from the last one.
    pub fn on_desktop(&mut self, index: usize, count: usize, name: String) {
        let showing = matches!(self.alert.as_ref().map(|s| &s.alert), Some(Alert::Desktop { .. }));
        if !showing {
            self.desk.snap(self.last_desk.min(count.saturating_sub(1)) as f32);
        }
        self.desk.set(index as f32);
        self.last_desk = index;
        self.show_alert(Alert::Desktop { index, count, name, missing: None }, 1.4);
    }

    /// Alt+number for a desktop that doesn't exist: a little shake, and say so.
    pub fn on_desktop_missing(&mut self, number: usize, current: usize, count: usize) {
        self.desk.snap(current as f32);
        self.last_desk = current;
        self.desk.velocity += 6.0;
        self.show_alert(Alert::Desktop { index: current, count, name: String::new(), missing: Some(number) }, 1.4);
    }

    pub fn on_caps(&mut self) {
        self.caps = !self.caps;
        let on = if self.caps { 1.0 } else { 0.0 };
        if matches!(self.alert.as_ref().map(|s| &s.alert), Some(Alert::Caps)) {
            // Already showing: flip the switch right away.
            self.caps_delay = 0.0;
            self.caps_knob.set(on);
        } else {
            self.caps_knob.snap(1.0 - on);
            self.caps_delay = CAPS_SWITCH_DELAY;
        }
        if self.cfg.status_alerts {
            self.show_alert(Alert::Caps, 1.4);
        }
    }

    /// Horizontal scroll (two-finger swipe, tilt wheel, Shift+wheel) pages between activities.
    pub fn on_hwheel(&mut self, delta: i32) {
        // One page per gesture: after a switch, wait for the fingers (and any
        // momentum) to pause before accepting another.
        let now = Instant::now();
        if now.saturating_duration_since(self.swipe_at) > SWIPE_COOLDOWN {
            self.swipe = 0.0;
        }
        self.swipe_at = now;
        if self.swipe == f32::MAX {
            return;
        }
        self.swipe += delta as f32;
        if self.swipe.abs() >= SWIPE_STEP {
            let dir = if self.swipe > 0.0 { 1 } else { -1 };
            // Mark the gesture used up until the fingers pause.
            self.swipe = f32::MAX;
            self.switch_page(dir);
        }
    }

    pub fn on_wheel(&mut self, delta: i32) {
        if !self.cfg.volume_hud {
            return;
        }
        self.on_volume_key(if delta > 0 { VolumeKey::Up } else { VolumeKey::Down });
    }

    pub fn command(&mut self, cmd: IslandCmd) {
        match cmd {
            IslandCmd::Timer(secs) => self.clock = Some(Clock::timer(secs)),
            IslandCmd::Stopwatch => self.clock = Some(Clock::stopwatch()),
            IslandCmd::ToggleClock => {
                if let Some(c) = self.clock.as_mut() {
                    c.toggle();
                }
            }
            IslandCmd::CancelClock => self.clock = None,
            IslandCmd::ClearShelf => self.shelf.clear(),
        }
        if matches!(cmd, IslandCmd::Timer(_) | IslandCmd::Stopwatch) {
            self.expanded = None;
            self.nudge();
        }
        self.retarget();
        self.dirty = true;
    }

    /// A file drag over the island (OLE drop target). Returns whether it's accepted.
    pub fn on_drag(&mut self, phase: Phase, paths: &[PathBuf]) -> bool {
        if self.dragging_out {
            return false;
        }
        match phase {
            Phase::Over(..) => {
                if !self.drag_over {
                    self.drag_over = true;
                    self.leave_at = None;
                }
            }
            Phase::Leave => self.drag_over = false,
            Phase::Drop(..) => {
                self.drag_over = false;
                for p in paths {
                    if !self.shelf.contains(p) {
                        self.shelf.push(p.clone());
                    }
                }
                // Show what landed, then fold away unless the pointer stays.
                self.expanded = Some(Panel::Shelf);
                self.leave_at = Some(Instant::now() + Duration::from_millis(1200));
            }
        }
        self.retarget();
        self.dirty = true;
        true
    }

    /// A drag out of the island finished; shelf items that were dropped somewhere leave the shelf.
    pub fn drag_done(&mut self, paths: &[PathBuf], dropped: bool) {
        self.dragging_out = false;
        if dropped {
            self.shelf.retain(|p| !paths.contains(p));
        }
        self.shelf.retain(|p| p.exists());
        self.retarget();
        self.dirty = true;
    }

    // --- pointer ---------------------------------------------------------------

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    fn rect_of(&self, hit: Hit) -> Option<Rect> {
        self.hits.iter().find(|(_, h)| *h == hit).map(|(r, _)| *r)
    }

    pub fn on_move(&mut self, x: f32, y: f32) -> Option<IslandEvent> {
        let hit = self.hit_at(x, y);
        let inside = hit.is_some();
        if inside != self.inside {
            self.inside = inside;
            self.hover_since = inside.then(Instant::now);
        }
        let expanded = matches!(self.shown, Content::Panel(_));
        self.leave_at = if inside { None } else { self.leave_at.or_else(|| expanded.then(Instant::now)) };
        if self.scrub.is_some() {
            self.scrub = Some(frac_in(self.rect_of(Hit::Progress).map(|r| r.inset(4.0, 8.0)).unwrap_or_default(), x));
        }
        if self.down == Some(Hit::Slider) {
            self.set_volume_at(x);
        }
        self.hot = hit;
        self.retarget();
        self.dirty = true;

        // Pressing on a file and moving drags it out.
        let moved = (x - self.down_at.0).hypot(y - self.down_at.1) > DRAG_THRESHOLD;
        if moved {
            if let Some(paths) = self.down.and_then(|d| self.drag_paths(d)) {
                self.down = None;
                self.dragging_out = true;
                return Some(IslandEvent::DragOut(paths));
            }
        }
        None
    }

    fn drag_paths(&self, hit: Hit) -> Option<Vec<PathBuf>> {
        match hit {
            Hit::ShelfItem(usize::MAX) => Some(self.shelf.clone()),
            Hit::ShelfItem(i) => self.shelf.get(i).map(|p| vec![p.clone()]),
            Hit::AlertBody => match self.alert.as_ref().map(|s| &s.alert) {
                Some(Alert::Screenshot(p) | Alert::Downloaded(p)) => Some(vec![p.clone()]),
                _ => None,
            },
            _ => None,
        }
    }

    fn set_volume_at(&mut self, x: f32) {
        let Some(r) = self.rect_of(Hit::Slider).map(|r| r.inset(4.0, 8.0)) else { return };
        if let Some(v) = sources::set_volume(frac_in(r, x)) {
            self.volume = v;
        }
        if let Some(s) = self.alert.as_mut() {
            s.until = Instant::now() + Duration::from_secs_f32(1.6);
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        self.inside = false;
        self.hover_since = None;
        self.hot = None;
        if matches!(self.shown, Content::Panel(_)) && self.scrub.is_none() {
            self.leave_at.get_or_insert_with(Instant::now);
        }
        self.retarget();
        self.dirty = true;
    }

    pub fn on_down(&mut self, x: f32, y: f32) {
        let hit = self.hit_at(x, y);
        self.down = hit;
        self.down_at = (x, y);
        match hit {
            Some(Hit::Progress) if self.now.as_ref().is_some_and(|n| n.can_seek) => {
                self.scrub = Some(frac_in(self.rect_of(Hit::Progress).map(|r| r.inset(4.0, 8.0)).unwrap_or_default(), x));
            }
            Some(Hit::Slider) => self.set_volume_at(x),
            _ => {}
        }
        self.retarget();
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> Option<IslandEvent> {
        let down = self.down.take();
        let hit = self.hit_at(x, y);
        let mut event = None;
        if let Some(f) = self.scrub.take() {
            if let Some(n) = self.now.as_mut() {
                let secs = f * n.duration;
                n.position = secs;
                n.at = Instant::now();
                self.media.send(Cmd::Seek(secs));
            }
        } else if down.is_some() && down == hit {
            event = self.click(hit.unwrap_or(Hit::Pill));
        }
        self.retarget();
        self.dirty = true;
        event
    }

    fn click(&mut self, hit: Hit) -> Option<IslandEvent> {
        match hit {
            Hit::Pill => match self.shown {
                Content::Idle => self.expanded = Some(Panel::Home),
                Content::Compact(Persist::Download) => {
                    return sources::downloads_dir().map(|d| IslandEvent::Run(Action::OpenFinder(Some(d))));
                }
                Content::Compact(p) => {
                    self.preferred = Some(p);
                    self.expanded = Self::panel_for(p);
                }
                Content::Alert(_) => return self.alert_click(),
                Content::Panel(_) => self.expanded = None,
            },
            Hit::Bubble => {
                if let Some(p) = self.bubble_kind {
                    if p == Persist::Download {
                        return sources::downloads_dir().map(|d| IslandEvent::Run(Action::OpenFinder(Some(d))));
                    }
                    self.preferred = Some(p);
                    self.expanded = Self::panel_for(p);
                }
            }
            Hit::Page(i) => {
                let pages = self.pages();
                if let (Some(&next), Content::Panel(cur)) = (pages.get(i), self.shown) {
                    let from = pages.iter().position(|p| *p == cur).unwrap_or(0);
                    self.go_to(next, if i > from { 1 } else { -1 });
                }
            }
            Hit::Art => return self.now.as_ref().map(|n| IslandEvent::OpenApp(n.app_id.clone())),
            Hit::Play => {
                if let Some(n) = self.now.as_mut() {
                    // Flip immediately; the session confirms a moment later.
                    n.position = n.position_now();
                    n.at = Instant::now();
                    n.playing = !n.playing;
                    self.paused_since = (!n.playing).then(Instant::now);
                }
                self.media.send(Cmd::TogglePlay);
            }
            Hit::Next => self.media.send(Cmd::Next),
            Hit::Prev => self.media.send(Cmd::Prev),
            Hit::TimerToggle => self.command(IslandCmd::ToggleClock),
            Hit::TimerCancel => {
                self.command(IslandCmd::CancelClock);
                self.expanded = None;
            }
            Hit::Chip(0) => self.command(IslandCmd::Stopwatch),
            Hit::Chip(secs) => self.command(IslandCmd::Timer(secs)),
            Hit::ShelfItem(usize::MAX) => {}
            Hit::ShelfItem(i) => {
                return self.shelf.get(i).map(|p| IslandEvent::Run(Action::Open(p.to_string_lossy().into_owned())));
            }
            Hit::ShelfRemove(i) => {
                if i < self.shelf.len() {
                    self.shelf.remove(i);
                }
            }
            Hit::ShelfClear => self.shelf.clear(),
            Hit::Event(_) => return Some(IslandEvent::Run(Action::Open("outlookcal:".into()))),
            Hit::AlertBody => return self.alert_click(),
            Hit::Progress | Hit::Slider => {}
        }
        None
    }

    fn alert_click(&mut self) -> Option<IslandEvent> {
        let event = match self.alert.as_ref().map(|s| &s.alert) {
            Some(Alert::Volume | Alert::Brightness(_)) => return None,
            Some(Alert::Screenshot(p)) => Some(IslandEvent::Run(Action::Open(p.to_string_lossy().into_owned()))),
            Some(Alert::Downloaded(p)) => Some(IslandEvent::Run(Action::Reveal(p.to_string_lossy().into_owned()))),
            Some(Alert::Bluetooth { .. }) => Some(IslandEvent::Run(Action::Open("ms-settings:bluetooth".into()))),
            Some(Alert::Note(n)) if !n.app_id.is_empty() => Some(IslandEvent::OpenApp(n.app_id.clone())),
            _ => None,
        };
        self.dismiss_alert();
        event
    }

    /// Right-click: timers, the shelf and settings.
    pub fn context_menu(&self) -> IslandEvent {
        let mut entries = Vec::new();
        for (secs, label) in [(60, "1 Minute"), (300, "5 Minutes"), (600, "10 Minutes"), (1500, "25 Minutes"), (3600, "1 Hour")] {
            entries.push(Entry::item(format!("Timer: {label}"), Action::Island(IslandCmd::Timer(secs))));
        }
        entries.push(Entry::item("Start Stopwatch", Action::Island(IslandCmd::Stopwatch)));
        if let Some(c) = &self.clock {
            entries.push(Entry::Separator);
            entries.push(Entry::item(if c.running { "Pause" } else { "Resume" }, Action::Island(IslandCmd::ToggleClock)));
            let cancel = if c.is_timer() { "Cancel Timer" } else { "Reset Stopwatch" };
            entries.push(Entry::item(cancel, Action::Island(IslandCmd::CancelClock)));
        }
        if !self.shelf.is_empty() {
            entries.push(Entry::Separator);
            entries.push(Entry::item("Clear Shelf", Action::Island(IslandCmd::ClearShelf)));
        }
        entries.push(Entry::Separator);
        entries.push(Entry::item("Dynamic Island Settings…", Action::OpenSettings(Some(crate::settings::Page::MenuBar))));
        entries.push(Entry::item(
            "Hide Dynamic Island",
            Action::SetConfig { table: "island", key: "enabled", value: ConfigValue::Bool(false) },
        ));
        let s = self.surface.scale;
        let x = self.surface.rect.left as f32 / s + self.pill_rect.x;
        let y = self.surface.rect.top as f32 / s + self.pill_rect.bottom() + 6.0;
        IslandEvent::Menu(entries, Placement::Below { x, y })
    }

    // --- animation -----------------------------------------------------------

    /// Advances animations; returns true while frames are still needed.
    pub fn tick(&mut self, dt: f32) -> bool {
        let now = Instant::now();
        self.time += dt;

        // Alerts expire, but not while the pointer rests on them or a panel holds them back.
        let held = self.inside
            || self.alert.as_ref().is_some_and(|s| !s.alert.is_hud() && matches!(self.shown, Content::Panel(_)));
        if let Some(s) = self.alert.as_mut() {
            if held {
                s.until = s.until.max(now + Duration::from_millis(600));
            }
            if now >= s.until {
                self.dismiss_alert();
            }
        }
        if matches!(self.alert.as_ref().map(|s| &s.alert), Some(Alert::TimerDone)) && self.time - self.pulse_at > 1.0 {
            self.pulse_at = self.time;
            self.hover.velocity += 6.0;
        }
        if self.clock.is_some_and(|c| c.finished()) {
            self.clock = None;
            self.expanded = self.expanded.filter(|p| *p != Panel::Timer);
            self.show_alert(Alert::TimerDone, 8.0);
            play_sound();
        }
        if !self.drag_over && self.leave_at.is_some_and(|t| now.saturating_duration_since(t).as_secs_f32() > COLLAPSE_DELAY) {
            self.expanded = None;
            self.leave_at = None;
        }
        if self.cfg.expand_on_hover && self.expanded.is_none() && self.down.is_none() {
            let resting = self.hover_since.is_some_and(|t| (now - t).as_secs_f32() > HOVER_EXPAND_DELAY);
            if resting {
                if let Content::Compact(p) = self.shown {
                    self.expanded = Self::panel_for(p);
                }
            }
        }
        self.retarget();

        let mut moving = false;
        if self.caps_delay > 0.0 {
            self.caps_delay -= dt;
            if self.caps_delay <= 0.0 {
                self.caps_knob.set(if self.caps { 1.0 } else { 0.0 });
            }
            moving = true;
        }
        for s in [
            &mut self.w,
            &mut self.h,
            &mut self.presence,
            &mut self.content,
            &mut self.bubble,
            &mut self.hover,
            &mut self.press,
            &mut self.bar_hover,
            &mut self.art_fade,
            &mut self.slide,
            &mut self.dot_pos,
            &mut self.desk,
            &mut self.caps_knob,
        ] {
            moving |= s.step(dt);
        }
        // A finished fade-out swaps the content; retarget again to fade the new one in.
        self.retarget();
        let waving = self.energy.step(dt) || self.energy.value > 0.0;
        if self.energy.value > 0.0 {
            self.wave_t += dt;
        }
        let visible = self.presence.value > 0.0;
        self.sync_visibility();

        // Periodic redraws for things that change by themselves, throttled.
        let ticking = self.clock.is_some_and(|c| c.running) || !self.downloads.is_empty();
        let spinning = self.claude.current().is_some();
        let period = if waving || spinning { Some(1.0 / 30.0) } else if ticking { Some(0.1) } else { None };
        self.since_draw += dt;
        if moving || period.is_some_and(|p| visible && self.since_draw >= p) {
            self.dirty = true;
        }
        let waiting = self.alert.is_some()
            || self.leave_at.is_some()
            || (self.cfg.expand_on_hover && self.hover_since.is_some() && self.expanded.is_none());
        moving || (visible && period.is_some()) || waiting
    }

    // --- rendering -----------------------------------------------------------

    fn sync_bitmaps(&mut self) {
        let rt = self.surface.canvas.target().clone();
        let art = self.now.as_ref().and_then(|n| n.art.clone());
        let id = art.as_ref().map(|a| a.id);
        if id != self.art.as_ref().map(|(i, _)| *i) {
            let bmp = art.and_then(|a| {
                let px = IconPixels { width: a.size, height: a.size, data: a.data.clone() };
                px.to_bitmap(&rt).map(|b| (a.id, b))
            });
            self.old_art = self.art.take().map(|(_, b)| b);
            self.art = bmp;
            self.art_fade.snap(0.0);
            self.art_fade.set(1.0);
        }
        if let Some(s) = self.alert.as_mut().filter(|s| !s.img_tried) {
            s.img_tried = true;
            s.img = match &s.alert {
                Alert::Screenshot(p) => load_shell_image(&p.to_string_lossy(), 256, true).and_then(|px| px.to_bitmap(&rt)),
                Alert::Note(n) => n.icon.as_ref().and_then(|a| {
                    IconPixels { width: a.size, height: a.size, data: a.data.clone() }.to_bitmap(&rt)
                }),
                _ => None,
            };
        }
        self.shelf_icons.retain(|p, _| self.shelf.contains(p));
        for p in &self.shelf {
            if !self.shelf_icons.contains_key(p) {
                if let Some(b) = load_shell_image(&p.to_string_lossy(), 96, true).and_then(|px| px.to_bitmap(&rt)) {
                    self.shelf_icons.insert(p.clone(), b);
                }
            }
        }
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        self.since_draw = 0.0;
        if !self.surface_visible() && (self.fullscreen || self.presence.target == 0.0) {
            return;
        }
        self.sync_bitmaps();
        let ch = self.compact_h;
        let (pw, ph) = self.pill_size();
        let privacy_on = !self.privacy.camera.is_empty() || !self.privacy.mic.is_empty();
        let side = if self.bubble.value > 0.001 || self.bubble.target > 0.0 || privacy_on { ch + 26.0 } else { 0.0 };
        if let Err(e) = self.surface.set_rect(window_rect(&self.monitor, self.top, pw + 2.0 * side, ph)) {
            return log!("island resize failed: {e}");
        }
        let win_w = self.surface.width_pt();
        let pill = Rect::new((win_w - pw) / 2.0, 0.0, pw, ph);
        self.pill_rect = pill;
        let grown = ((ph - ch) / 30.0).clamp(0.0, 1.0);
        let radius = if ph <= ch * 1.05 { ph / 2.0 } else { (ph / 2.0).min(lerp(ch / 2.0, PANEL_RADIUS, grown)) };
        let presence = self.presence.value.clamp(0.0, 1.0);
        let fit = self.fit();
        let a = presence * self.content.value.clamp(0.0, 1.0) * fit;

        // The bubble splits off the pill's right end.
        let b = self.bubble.value.max(0.0);
        let bd = ch * (0.55 + 0.45 * b.min(1.0));
        let bcx = pill.right() - ch / 2.0 + (ch + 6.0) * b;
        let bubble_rect = Rect::new(bcx - bd / 2.0, ch / 2.0 - bd / 2.0, bd, bd);

        let fonts = Fonts::new(gfx, &theme.font);
        let view = View {
            fonts: &fonts,
            now: self.now.as_ref(),
            art: self.art.as_ref().map(|(_, b)| b),
            old_art: self.old_art.as_ref(),
            art_fade: self.art_fade.value.clamp(0.0, 1.0),
            wave_t: self.wave_t,
            energy: self.energy.value.clamp(0.0, 1.0),
            hot: self.hot,
            down: self.down,
            scrub: self.scrub,
            bar_hover: self.bar_hover.value.clamp(0.0, 1.0),
            desk_pos: self.desk.value,
            desk_vel: self.desk.velocity,
            caps_knob: self.caps_knob.value,
            clock: self.clock.as_ref(),
            shelf: &self.shelf,
            shelf_icons: &self.shelf_icons,
            drag_over: self.drag_over,
            events: &self.events,
            downloads: &self.downloads,
            volume: self.volume,
            alert_img: match self.shown {
                Content::Alert(id) => {
                    [&self.alert, &self.prev_alert].into_iter().flatten().find(|s| s.id == id).and_then(|s| s.img.as_ref())
                }
                _ => None,
            },
            accent: self.accent,
            time: self.time,
            claude: self.claude.current(),
            claude_more: self.claude.others(),
        };
        let shown = self.shown;
        let alert = match shown {
            Content::Alert(id) => [&self.alert, &self.prev_alert].into_iter().flatten().find(|s| s.id == id).map(|s| &s.alert),
            _ => None,
        };
        let bubble_kind = self.bubble_kind;
        // Outgoing content drifts away while fading; incoming slides in.
        let (want_now, _) = self.wanted();
        let shift = if self.shown != want_now { -self.page_dir * (1.0 - self.content.value) } else { self.slide.value } * SLIDE;
        let sliding = shift.abs() > 4.0;
        let pages = self.pages();
        let dots = match shown {
            Content::Panel(p) if pages.len() > 1 && pages.contains(&p) => pages.len(),
            _ => 0,
        };
        let dot_pos = self.dot_pos.value;
        let hot_page = match self.hot {
            Some(Hit::Page(i)) => Some(i),
            _ => None,
        };
        let dot = if !self.privacy.camera.is_empty() { Some(GREEN) } else if !self.privacy.mic.is_empty() { Some(ORANGE) } else { None };
        let mut hits = Vec::new();
        if presence > 0.5 {
            hits.push((pill, Hit::Pill));
            if b > 0.6 {
                hits.push((bubble_rect, Hit::Bubble));
            }
        }

        let r = self.surface.render(gfx, |p| {
            // Soft shadow that grows in with the pill.
            if grown > 0.01 {
                for i in 0..12 {
                    let g = i as f32 * 1.7;
                    let sa = 0.045 * grown * presence * (12 - i) as f32 / 12.0;
                    p.fill_round(pill.inset(-g, -g).offset(0.0, 7.0), radius + g, Color::BLACK.alpha(sa));
                }
                p.clear_round(pill, radius);
            }
            p.fill_round(pill, radius, Color::BLACK.alpha(presence));
            if grown > 0.01 {
                p.stroke_round(pill.inset(0.25, 0.25), radius, Color::gray(255, 0.07 * grown * presence), 0.5);
            }
            if b > 0.001 {
                p.fill_circle(bubble_rect.cx(), bubble_rect.cy(), bd / 2.0, Color::BLACK.alpha(presence));
                if let Some(k) = bubble_kind {
                    views::bubble(p, bubble_rect, k, &view, presence * ((b - 0.4) / 0.6).clamp(0.0, 1.0));
                }
            }
            if let Some(c) = dot {
                let x = if b > 0.001 { bubble_rect.right() } else { pill.right() } + 9.0;
                p.fill_circle(x, ch / 2.0, 6.0, c.alpha(0.22 * presence));
                p.fill_circle(x, ch / 2.0, 3.4, c.alpha(presence));
            }
            if a <= 0.0 {
                return;
            }
            let mut content_hits = Vec::new();
            let body = Rect::new(pill.x, pill.y, pill.w, pill.h - if dots > 0 { DOTS_H } else { 0.0 });
            p.clipped(pill, || match shown {
                Content::Idle => {}
                Content::Compact(k) => p.transformed(shift, 0.0, 1.0, || views::compact(p, pill, k, &view, a, &mut content_hits)),
                Content::Alert(_) => {
                    if let Some(al) = alert {
                        views::alert(p, pill, al, &view, a, &mut content_hits);
                    }
                }
                Content::Panel(panel) => {
                    // Panels scale up slightly as they settle in.
                    let s = lerp(0.94, 1.0, fit);
                    let (ax, ay) = (pill.cx(), pill.y);
                    p.transformed(ax * (1.0 - s) + shift, ay * (1.0 - s), s, || match panel {
                        Panel::Media => {
                            views::media_panel(p, body, &view, a, &mut content_hits);
                        }
                        Panel::Timer => views::timer_panel(p, body, &view, a, &mut content_hits),
                        Panel::Claude => views::claude_panel(p, body, &view, a, &mut content_hits),
                        Panel::Shelf => views::shelf_panel(p, body, &view, a, &mut content_hits),
                        Panel::Calendar => views::calendar_panel(p, body, &view, a, &mut content_hits),
                        Panel::Home => views::home_panel(p, body, &view, a, &mut content_hits),
                    });
                }
            });
            // Controls only become clickable once they're (nearly) fully in and still.
            if a > 0.85 && !sliding {
                hits.extend(content_hits);
            }
            // Page dots: dim dots with a white capsule gliding to the current page.
            if dots > 0 {
                let da = presence * fit;
                let (gap, r) = (13.0, 3.2);
                let cy = pill.bottom() - DOTS_H / 2.0 - 3.0;
                let x0 = pill.cx() - gap * (dots as f32 - 1.0) / 2.0;
                for i in 0..dots {
                    let x = x0 + i as f32 * gap;
                    let hot = hot_page == Some(i);
                    p.fill_circle(x, cy, r, Color::gray(255, if hot { 0.5 } else { 0.28 } * da));
                    hits.push((Rect::new(x - gap / 2.0, cy - 9.0, gap, 18.0), Hit::Page(i)));
                }
                let x = x0 + dot_pos.clamp(-0.5, dots as f32 - 0.5) * gap;
                let stretch = 1.0 + (dot_pos - dot_pos.round()).abs() * 2.4;
                let cap = Rect::new(x - r * 1.6 * stretch, cy - r, r * 3.2 * stretch, r * 2.0);
                p.fill_round(cap, r, Color::gray(255, 0.95 * da));
            }
        });
        if let Err(e) = r {
            log!("island render failed: {e}");
        }
        self.hits = hits;
    }
}

impl Drop for Island {
    fn drop(&mut self) {
        self.stop.stop();
        shelf::revoke(self.surface.content);
    }
}

/// Whether a `calendar_ics` setting holds only web links (empty is fine).
pub fn calendar_links_valid(setting: &str) -> bool {
    calendar::valid_links(setting)
}

/// "calendar.google.com" (or "2 calendars") for showing a secret link without revealing it.
pub fn calendar_links_summary(setting: &str) -> String {
    let links = calendar::links(setting);
    match links.as_slice() {
        [] => String::new(),
        [one] => format!("Subscribed: {}", one.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or("calendar")),
        many => format!("{} calendars subscribed", many.len()),
    }
}

fn play_sound() {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
    unsafe {
        if !PlaySoundW(w!("Notification.Reminder"), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT).as_bool() {
            let _ = PlaySoundW(w!("SystemExclamation"), None, SND_ALIAS | SND_ASYNC);
        }
    }
}

fn blob_size(compact_h: f32) -> (f32, f32) {
    (compact_h * 2.2, compact_h * 0.7)
}

fn window_rect(m: &Monitor, top: f32, w: f32, h: f32) -> RECT {
    let s = m.scale;
    let ww = ((w + 2.0 * MARGIN_X) * s).ceil() as i32;
    let wh = ((h + MARGIN_BOTTOM) * s).ceil() as i32;
    let left = (m.rect.left + m.rect.right - ww) / 2;
    let t = m.rect.top + (top * s).round() as i32;
    RECT { left, top: t, right: left + ww, bottom: t + wh }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn frac_in(r: Rect, x: f32) -> f32 {
    if r.w <= 0.0 {
        0.0
    } else {
        ((x - r.x) / r.w).clamp(0.0, 1.0)
    }
}

fn fmt_time(secs: f32) -> String {
    timer::hms(secs.max(0.0).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::fmt_time;

    #[test]
    fn times() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(65.4), "1:05");
        assert_eq!(fmt_time(3725.0), "1:02:05");
        assert_eq!(fmt_time(-3.0), "0:00");
    }
}
