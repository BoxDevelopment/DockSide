//! Calendar: a macOS Calendar-style app window, opened from the menu bar
//! calendar's full screen button or by clicking a Calendar widget. It opens
//! maximized (between the menu bar and Dock) with Day, Week, Month and Year
//! views, a glass sidebar holding the calendars and a small month, and a
//! popover with an event's details. Events come from every account in
//! System Settings > Calendars, like the menu bar calendar's.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::{Datelike, Duration, Local, NaiveDate, Timelike, Weekday};
use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{lerp, Glide, Motion, Scroll, Spring};
use crate::calsync::{self, Event};
use crate::chrome::{self, Light, SidebarBackdrop, WindowAnim, SHADOW};
use crate::config::Config;
use crate::dropdown::calendar::{add_months, clock, days, demo_events, first_of, from_days, local_date, midnight, monday_first};
use crate::finder::{self, FinderWindow};
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Canvas, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::theme::Theme;
use crate::win::{self, Monitor};

/// Dock path for this window, which shows in the Dock like an app.
pub const CALENDAR_PATH: &str = "dockside:calendar";
pub const TITLE: &str = "Calendar";

/// Size when not maximized.
const W: f32 = 1120.0;
const H: f32 = 740.0;
const SIDEBAR: f32 = 232.0;
const TOOLBAR: f32 = 52.0;
/// The big "November 2026" under the toolbar.
const HEADER: f32 = 56.0;
/// Resize border inside the body's edge.
const EDGE: f32 = 6.0;
/// Week and Day: an hour's height, the hour labels' gutter, the day names' row.
const HOUR_H: f32 = 52.0;
const GUTTER: f32 = 60.0;
const DAY_HEAD: f32 = 40.0;
const ALL_DAY_ROW: f32 = 21.0;
/// Month: an event's row in a day.
const ROW_H: f32 = 19.0;
const POP_W: f32 = 290.0;
/// How far a page slides as it changes.
const SLIDE: f32 = 44.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum View {
    Day,
    Week,
    Month,
    Year,
}

impl View {
    /// `[calendar] default_view`: "day", "week", "month" or "year".
    fn from_config(s: &str) -> View {
        match s.to_ascii_lowercase().as_str() {
            "day" => View::Day,
            "week" => View::Week,
            "year" => View::Year,
            _ => View::Month,
        }
    }
}

const VIEWS: [(View, &str); 4] = [(View::Day, "Day"), (View::Week, "Week"), (View::Month, "Month"), (View::Year, "Year")];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Hit {
    Light(Light),
    View(View),
    Prev,
    Next,
    Today,
    MiniPrev,
    MiniNext,
    /// A day in the small month, as days since 1970-01-01.
    MiniDay(i32),
    /// A calendar's checkbox in the sidebar.
    Cal(u16),
    AddAccount,
    /// A day: a month cell, a week column, a day in the year.
    Day(i32),
    /// A month's name in the year view (0 = January).
    Month(u8),
    Event(u32),
    /// A month cell's "N more".
    More(i32),
    Popover,
    PopOpen,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Anim {
    Hover(Hit),
    Lights,
    /// A calendar's checkbox filling or emptying.
    Check(u16),
}

pub enum CalendarEvent {
    None,
    Close,
    Action(Action),
}

/// A calendar in the sidebar list.
#[derive(Debug)]
struct CalRow {
    group: String,
    account: u64,
    name: String,
    color: [u8; 3],
}

/// An event's details, shown beside it.
struct Pop {
    event: usize,
    anchor: Rect,
    open: bool,
}

pub struct CalendarApp {
    pub hwnd: HWND,
    /// In a cell so drawing can read the rest of the window.
    canvas: RefCell<Canvas>,
    scale: f32,
    view: View,
    /// The day the view is about (its week, month or year).
    cursor: NaiveDate,
    selected: NaiveDate,
    /// First of the month the sidebar's small month shows.
    mini: NaiveDate,
    monday: bool,
    events: Vec<Event>,
    /// Shown events by day (days since 1970), all-day first.
    by_day: HashMap<i32, Vec<usize>>,
    gen: u64,
    cals: Vec<CalRow>,
    hidden: HashSet<(u64, String)>,
    has_accounts: bool,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    last_click: Option<(Hit, Instant)>,
    popover: Option<Pop>,
    pop_t: Spring,
    /// Paging: the new page slides in from ±1 to 0.
    page: Spring,
    /// Changing views: the new one fades and grows in from 0 to 1.
    swap: Spring,
    /// Week and Day's timeline.
    scroll: Scroll,
    max_scroll: Cell<f32>,
    /// Month and Year: a touchpad's small wheel deltas, until they add up to a page.
    wheel_acc: i32,
    /// When a touchpad swipe last turned a page: the rest of that swipe (and its
    /// momentum) doesn't turn more.
    wheel_paged: Option<Instant>,
    /// Paging slides up and down (the wheel) rather than sideways.
    vertical: bool,
    /// Where a new view zooms out of (the day or month clicked), else its middle.
    swap_from: Option<(f32, f32)>,
    /// The sidebar's small month sliding to the next or last one.
    mini_page: Spring,
    /// What the last frame showed (view, dates, scroll): when that changes, a
    /// frame is drawn whole, never just a hover's corner of it.
    drawn: Option<(View, NaiveDate, NaiveDate, NaiveDate, i32)>,
    pill: RefCell<Glide>,
    sel: RefCell<Glide>,
    backdrop: Option<SidebarBackdrop>,
    backdrop_for: Option<((f32, f32, f32, f32), Rect)>,
    xf: (f32, f32, f32, f32),
    fullscreen: Option<WINDOWPLACEMENT>,
    /// Where the green button puts it, until it's first shown (maximized).
    normal: Option<RECT>,
    /// The minute last drawn (for the current time line).
    minute: u32,
    window: WindowAnim,
    motion: Motion<Anim>,
    shown: bool,
    pub focused: bool,
    pub tracking: bool,
    pub dirty: bool,
    /// Whether `cals` came from Screenshot Mode's demo events.
    cals_demo: bool,
    /// What the pending redraw must repaint, when that's not everything.
    part: Option<Rect>,
    /// Where hover highlights are still fading.
    fading: Option<Rect>,
    /// Pixels the last frame repainted (for the bench).
    last_px: u64,
}

fn rgb(c: [u8; 3]) -> Color {
    Color::rgba(c[0], c[1], c[2], 1.0)
}

#[allow(clippy::too_many_arguments)]
fn text(p: &Painter, theme: &Theme, s: &str, size: f32, weight: Weight, align: Align, r: Rect, c: Color) {
    let f = p.gfx.format(&theme.font, size, weight, align);
    p.text(s, &f, r, c);
}

fn measure(p: &Painter, theme: &Theme, s: &str, size: f32, weight: Weight) -> f32 {
    p.gfx.measure(s, &p.gfx.format(&theme.font, size, weight, Align::Leading))
}

fn hour_label(h: u32) -> String {
    match h {
        0 => "12 AM".into(),
        12 => "Noon".into(),
        h if h < 12 => format!("{h} AM"),
        h => format!("{} PM", h - 12),
    }
}

/// "9 AM" style when on the hour, else "9:30 AM".
fn short_clock(t: i64) -> String {
    let s = clock(t);
    s.replace(":00 ", " ")
}

/// The sidebar's calendars: every account's, then any others events came from
/// (`kept` from before, so they stay once seen). Screenshot Mode lists only the
/// demo events' calendars: no account or calendar name of the user's may show.
fn cal_rows(accounts: &[calsync::AccountView], events: &[Event], kept: Vec<CalRow>, demo: bool) -> Vec<CalRow> {
    let accounts = if demo { &[][..] } else { accounts };
    let mut cals: Vec<CalRow> = Vec::new();
    for a in accounts {
        if let calsync::Status::Ok { calendars, .. } = &a.status {
            for c in calendars {
                cals.push(CalRow { group: a.name.clone(), account: a.id, name: c.name.clone(), color: c.color });
            }
        }
    }
    let group_of = |id: u64| accounts.iter().find(|a| a.id == id).map_or_else(|| "Other".to_string(), |a| a.name.clone());
    let mut extra: Vec<CalRow> = kept.into_iter().filter(|c| !cals.iter().any(|k| k.account == c.account && k.name == c.name)).collect();
    for e in events {
        let known = |c: &CalRow| c.account == e.account && c.name == e.calendar;
        if !cals.iter().any(known) && !extra.iter().any(known) {
            let group = if demo { "iCloud".to_string() } else { group_of(e.account) };
            extra.push(CalRow { group, account: e.account, name: e.calendar.clone(), color: e.color });
        }
    }
    for c in extra {
        let at = cals.iter().rposition(|k| k.group == c.group).map_or(cals.len(), |i| i + 1);
        cals.insert(at, c);
    }
    cals
}

impl CalendarApp {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: &Monitor) -> Result<Self> {
        Self::create(gfx, theme, cfg, monitor, true)
    }

    fn create(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: &Monitor, show: bool) -> Result<Self> {
        let s = monitor.scale;
        let mon = monitor.rect;
        let (mw, mh) = ((mon.right - mon.left) as f32 / s, (mon.bottom - mon.top) as f32 / s);
        let (bw, bh) = (W.min(mw * 0.86), H.min(mh * 0.82));
        let (w, h) = (((bw + 2.0 * SHADOW) * s).round() as i32, ((bh + 2.0 * SHADOW) * s).round() as i32);
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + (mon.bottom - mon.top - h) / 2;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let hwnd = win::create_window(
            WS_EX_LAYERED | WS_EX_APPWINDOW,
            WS_POPUP | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX,
            rect,
            HWND::default(),
        )?;
        // Sized like a Finder window: the body fills the work area when maximized.
        finder::set_frame_state(hwnd, finder::FRAME_NORMAL);
        unsafe {
            let _ = SetWindowTextW(hwnd, w!("Calendar"));
            // Opens maximized; the green button makes it a window. Sized for that
            // now, so the first frame is drawn at that size before it shows.
            if show {
                let max = FinderWindow::screen_rect(&rect, false);
                let _ = SetWindowPos(hwnd, HWND::default(), max.left, max.top, max.right - max.left, max.bottom - max.top, SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(hwnd, &mut rc);
        }
        let today = Local::now().date_naive();
        let mut app = Self {
            hwnd,
            canvas: RefCell::new(Canvas::new(gfx, rc.right - rc.left, rc.bottom - rc.top, s)?),
            scale: s,
            view: View::from_config(&cfg.calendar.default_view),
            cursor: today,
            selected: today,
            mini: first_of(today),
            monday: monday_first(cfg),
            events: Vec::new(),
            by_day: HashMap::new(),
            gen: u64::MAX,
            cals: Vec::new(),
            hidden: HashSet::new(),
            has_accounts: false,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            last_click: None,
            popover: None,
            pop_t: Spring::new(0.0, 0.22, 0.86),
            page: Spring::new(0.0, 0.32, 1.0),
            swap: Spring::new(1.0, 0.26, 1.0),
            scroll: Scroll::default(),
            max_scroll: Cell::new(24.0 * HOUR_H),
            wheel_acc: 0,
            wheel_paged: None,
            vertical: false,
            swap_from: None,
            mini_page: Spring::new(0.0, 0.3, 1.0),
            drawn: None,
            pill: RefCell::new(Glide::selection()),
            sel: RefCell::new(Glide::selection()),
            backdrop: SidebarBackdrop::new(theme, cfg.finder.sidebar),
            backdrop_for: None,
            xf: (0.0, 0.0, 1.0, 1.0),
            fullscreen: None,
            normal: show.then_some(rect),
            minute: Local::now().minute(),
            window: WindowAnim::opening(),
            motion: Motion::default(),
            shown: false,
            focused: true,
            tracking: false,
            dirty: true,
            last_px: 0,
            cals_demo: false,
            part: None,
            fading: None,
        };
        app.reload();
        Ok(app)
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Brings it to the front (it was open already).
    pub fn show(&mut self) {
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.on_size();
    }

    pub fn on_size(&mut self) {
        let _ = self.window.restored(self.hwnd);
        self.redraw();
    }

    /// A calendar account synced, or Screenshot Mode changed.
    pub fn update(&mut self) {
        self.reload();
        self.redraw();
    }

    /// Once a second or so: redraw when the minute (or the day) turns.
    pub fn on_tick(&mut self) {
        let m = Local::now().minute();
        if m != self.minute {
            self.minute = m;
            self.redraw();
        }
    }

    pub fn tick(&mut self, dt: f32) -> bool {
        let mut full = self.window.tick(dt, self.hwnd);
        full |= self.page.step(dt) | self.swap.step(dt) | self.pop_t.step(dt);
        if self.popover.as_ref().is_some_and(|p| !p.open) && self.pop_t.settled() {
            self.popover = None;
            full = true;
        }
        // The rest move within one area: just that is repainted (and uploaded).
        let body = self.body();
        let content = Rect::new(body.x + SIDEBAR, body.y, body.w - SIDEBAR, body.h);
        let page = Rect::new(content.x, body.y + TOOLBAR, content.w, body.h - TOOLBAR);
        let mut part: Option<Rect> = None;
        let mut touch = |r: Rect| part = Some(part.map_or(r, |p| p.union(r)));
        if self.scroll.tick(dt) | self.sel.borrow_mut().tick(dt) {
            touch(page);
        }
        if self.pill.borrow_mut().tick(dt) {
            touch(Rect::new(content.x, body.y, content.w, TOOLBAR));
        }
        if self.mini_page.step(dt) {
            touch(Rect::new(body.x, body.y, SIDEBAR, body.h));
        }
        if self.motion.tick(dt) {
            match self.fading {
                Some(r) => touch(r),
                None => full = true,
            }
        } else if !self.dirty {
            // (A hover's fade starts when its frame is drawn, so not before.)
            self.fading = None;
        }
        if full {
            self.redraw();
        } else if let Some(r) = part {
            self.redraw_in(r);
        }
        full || part.is_some()
    }

    /// Everything is drawn again next frame.
    pub fn redraw(&mut self) {
        self.dirty = true;
        self.part = None;
    }

    /// Just `r` is drawn again (unless all of it is already due).
    fn redraw_in(&mut self, r: Rect) {
        if !self.dirty || self.part.is_some() {
            self.part = Some(self.part.map_or(r, |p| p.union(r)));
        }
        self.dirty = true;
    }

    /// Where a hover highlight for `h` draws (the traffic lights fade as one).
    fn hover_area(&self, h: Hit) -> Option<Rect> {
        let r = match h {
            Hit::Light(_) => self
                .hits
                .iter()
                .filter(|(_, h)| matches!(h, Hit::Light(_)))
                .map(|(r, _)| *r)
                .reduce(|a, b| a.union(b))?,
            h => self.hit_rect(h)?,
        };
        Some(r.inset(-4.0, -4.0))
    }

    /// The hover moved from `from` to `to`: those two areas fade.
    fn hover_changed(&mut self, from: Option<Hit>, to: Option<Hit>) {
        let mut area: Option<Rect> = None;
        for h in [from, to].into_iter().flatten() {
            match self.hover_area(h) {
                Some(r) => area = Some(area.map_or(r, |a| a.union(r))),
                None => return self.redraw(),
            }
        }
        if let Some(r) = area {
            self.fading = Some(self.fading.map_or(r, |f| f.union(r)));
            self.redraw_in(r);
        }
    }

    pub fn animating(&self) -> bool {
        self.window.moving()
            || self.motion.moving()
            || !self.page.settled()
            || !self.swap.settled()
            || !self.pop_t.settled()
            || !self.mini_page.settled()
            || self.scroll.moving()
            || self.pill.borrow().moving()
            || self.sel.borrow().moving()
    }

    // --- data --------------------------------------------------------------

    fn week_start(&self, d: NaiveDate) -> NaiveDate {
        let wd = d.weekday();
        d - Duration::days(if self.monday { wd.num_days_from_monday() } else { wd.num_days_from_sunday() } as i64)
    }

    /// The first day of a month's grid (the week holding the 1st) and its weeks.
    fn month_grid(&self, month: NaiveDate) -> (NaiveDate, usize) {
        let start = self.week_start(month);
        let weeks = ((add_months(month, 1) - start).num_days() + 6) / 7;
        (start, weeks as usize)
    }

    /// The days the main view covers.
    fn view_span(&self) -> (NaiveDate, NaiveDate) {
        match self.view {
            View::Day => (self.cursor, self.cursor + Duration::days(1)),
            View::Week => {
                let s = self.week_start(self.cursor);
                (s, s + Duration::days(7))
            }
            View::Month => {
                let (s, weeks) = self.month_grid(first_of(self.cursor));
                (s, s + Duration::days(weeks as i64 * 7))
            }
            View::Year => {
                let y = self.cursor.year();
                (NaiveDate::from_ymd_opt(y, 1, 1).unwrap_or(self.cursor), NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap_or(self.cursor))
            }
        }
    }

    fn reload(&mut self) {
        let (a, b) = self.view_span();
        let (ms, _) = self.month_grid(self.mini);
        let (from, to) = (a.min(ms), b.max(ms + Duration::days(42)));
        self.gen = calsync::gen();
        let demo = crate::screenshot_mode::on();
        self.events = if demo { demo_events() } else { calsync::events_between(midnight(from), midnight(to)) };
        let snap = calsync::snapshot();
        self.has_accounts = !snap.accounts.is_empty() || demo;
        let switched = demo != self.cals_demo;
        self.cals_demo = demo;
        let kept = if switched { Vec::new() } else { std::mem::take(&mut self.cals) };
        let cals = cal_rows(&snap.accounts, &self.events, kept, demo);
        self.cals = cals;
        self.index();
        if let Some(p) = &self.popover {
            if p.event >= self.events.len() {
                self.popover = None;
            }
        }
    }

    /// Files shown events under each day they touch.
    fn index(&mut self) {
        self.by_day.clear();
        for (i, e) in self.events.iter().enumerate() {
            if self.hidden.contains(&(e.account, e.calendar.clone())) {
                continue;
            }
            let first = local_date(e.start);
            let last = local_date((e.end - 1).max(e.start));
            let mut d = first;
            while d <= last && (d - first).num_days() < 400 {
                self.by_day.entry(days(d)).or_default().push(i);
                d += Duration::days(1);
            }
        }
        let events = &self.events;
        for list in self.by_day.values_mut() {
            list.sort_by(|&a, &b| {
                let (x, y) = (&events[a], &events[b]);
                (!x.all_day, x.start, &x.title).cmp(&(!y.all_day, y.start, &y.title))
            });
        }
    }

    fn day_events(&self, d: NaiveDate) -> &[usize] {
        self.by_day.get(&days(d)).map_or(&[], |v| v.as_slice())
    }

    // --- navigation ----------------------------------------------------------

    fn slide(&mut self, dir: f32) {
        self.vertical = false;
        self.page.snap(dir.signum());
        self.page.set(0.0);
        self.sel.borrow_mut().reset();
        self.close_popover();
    }

    /// Back or forward a day, week, month or year.
    fn step(&mut self, dir: i32) {
        self.step_on(dir, false);
    }

    /// [`Self::step`], sliding up and down if `vertical` (the wheel).
    fn step_on(&mut self, dir: i32, vertical: bool) {
        self.cursor = match self.view {
            View::Day => self.cursor + Duration::days(dir as i64),
            View::Week => self.cursor + Duration::days(7 * dir as i64),
            View::Month => add_months(first_of(self.cursor), dir),
            View::Year => NaiveDate::from_ymd_opt(self.cursor.year() + dir, self.cursor.month(), 1).unwrap_or(self.cursor),
        };
        if matches!(self.view, View::Day | View::Week) {
            self.selected = self.cursor;
        }
        self.set_mini(first_of(self.cursor));
        self.slide(dir as f32);
        self.vertical = vertical;
        self.reload();
        self.redraw();
    }

    /// Shows `month` in the sidebar's small month, sliding over to it.
    fn set_mini(&mut self, month: NaiveDate) {
        if month != self.mini {
            self.mini_page.snap(if month > self.mini { 1.0 } else { -1.0 });
            self.mini_page.set(0.0);
            self.mini = month;
        }
    }

    /// Shows `d` (sliding the way it lies), in `view` if given.
    fn go(&mut self, d: NaiveDate, view: Option<View>) {
        let view = view.unwrap_or(self.view);
        let before = self.view_span();
        let changed = view != self.view;
        self.view = view;
        self.cursor = d;
        self.selected = d;
        self.set_mini(first_of(d));
        let from = self.swap_from.take();
        if changed {
            self.swap_from = from;
            self.swap.snap(0.0);
            self.swap.set(1.0);
            self.sel.borrow_mut().reset();
            self.close_popover();
            self.scroll_to_morning();
        } else {
            let (a, b) = self.view_span();
            if a >= before.1 {
                self.slide(1.0);
            } else if b <= before.0 {
                self.slide(-1.0);
            }
        }
        self.reload();
        self.redraw();
    }

    /// Opens `d` in `view`, zooming out of where `hit` is (a day or a month).
    fn zoom_into(&mut self, d: NaiveDate, view: View, hit: Hit) {
        self.swap_from = self.hit_rect(hit).map(|r| (r.cx(), r.cy()));
        self.go(d, Some(view));
    }

    fn set_view(&mut self, v: View) {
        if v != self.view {
            // The selected day, if it's in view; else where the view is.
            let (a, b) = self.view_span();
            let d = if self.selected >= a && self.selected < b { self.selected } else { self.cursor };
            self.go(d, Some(v));
        }
    }

    /// Week and Day open on the working day (or just before now, today).
    fn scroll_to_morning(&mut self) {
        let now = Local::now();
        let (a, b) = self.view_span();
        let today = now.date_naive();
        let hour = if today >= a && today < b { (now.hour() as f32 + now.minute() as f32 / 60.0 - 1.5).max(0.0) } else { 7.5 };
        self.scroll.jump((hour * HOUR_H).min(self.max_scroll.get()));
    }

    fn close_popover(&mut self) {
        if let Some(p) = &mut self.popover {
            p.open = false;
            self.pop_t.set(0.0);
        }
    }

    // --- window ----------------------------------------------------------------

    fn size_pt(&self) -> (f32, f32) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        ((rc.right - rc.left) as f32 / self.scale, (rc.bottom - rc.top) as f32 / self.scale)
    }

    fn body(&self) -> Rect {
        let (w, h) = self.size_pt();
        Rect::new(SHADOW, SHADOW, w - 2.0 * SHADOW, h - 2.0 * SHADOW)
    }

    fn radius(&self) -> f32 {
        if self.fullscreen.is_some() {
            0.0
        } else {
            chrome::RADIUS
        }
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen.is_some()
    }

    fn is_zoomed(&self) -> bool {
        unsafe { IsZoomed(self.hwnd).as_bool() }
    }

    /// The green button: maximize between the menu bar and Dock, or back to a window.
    fn toggle_zoom(&mut self) {
        if self.fullscreen.is_some() {
            return self.toggle_fullscreen();
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, if self.is_zoomed() { SW_RESTORE } else { SW_MAXIMIZE });
        }
        self.redraw();
    }

    /// F11: the whole display, menu bar and Dock out of the way (like Finder's).
    pub fn toggle_fullscreen(&mut self) {
        unsafe {
            match self.fullscreen.take() {
                Some(placement) => {
                    finder::set_frame_state(self.hwnd, finder::FRAME_NORMAL);
                    finder::mark_fullscreen(self.hwnd, false);
                    let _ = SetWindowPlacement(self.hwnd, &placement);
                }
                None => {
                    let mut placement = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                    let _ = GetWindowPlacement(self.hwnd, &mut placement);
                    if self.is_zoomed() {
                        let _ = ShowWindow(self.hwnd, SW_RESTORE);
                    }
                    let mut current = RECT::default();
                    let _ = GetWindowRect(self.hwnd, &mut current);
                    self.fullscreen = Some(placement);
                    finder::set_frame_state(self.hwnd, finder::FRAME_FULL);
                    let rc = FinderWindow::screen_rect(&current, true);
                    let _ = SetWindowPos(self.hwnd, HWND_TOP, rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_SHOWWINDOW);
                    finder::mark_fullscreen(self.hwnd, true);
                }
            }
        }
        self.redraw();
    }

    /// First show: maximized, already at that size, restoring to `normal`.
    fn show_maximized(&self, normal: RECT) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWMAXIMIZED);
            // The restore rect is the size it was created at (the maximized one):
            // put the window size there. It's in workspace coordinates, offset
            // from the screen's by the display's reserved edges.
            let mut pl = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            let mut rc = RECT::default();
            if GetWindowPlacement(self.hwnd, &mut pl).is_ok() && GetWindowRect(self.hwnd, &mut rc).is_ok() {
                let (dx, dy) = (rc.left - pl.rcNormalPosition.left, rc.top - pl.rcNormalPosition.top);
                pl.rcNormalPosition = RECT { left: normal.left - dx, top: normal.top - dy, right: normal.right - dx, bottom: normal.bottom - dy };
                pl.showCmd = SW_SHOWMAXIMIZED.0 as u32;
                let _ = SetWindowPlacement(self.hwnd, &pl);
            }
        }
    }

    /// Fades and zooms the window (and its glass sidebar) away; the App drops
    /// it once [`Self::closed`].
    pub fn begin_close(&mut self) {
        self.window.close();
        self.redraw();
        unsafe {
            let ex = GetWindowLongW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_TRANSPARENT.0 as i32);
        }
    }

    pub fn closed(&self) -> bool {
        self.window.closed()
    }

    fn sync_backdrop(&mut self) {
        let body = self.body();
        self.backdrop_for = Some((self.xf, body));
        let (ox, oy, k, alpha) = self.xf;
        if let Some(bd) = &mut self.backdrop {
            let r = chrome::sidebar_pane(body, SIDEBAR);
            let r = Rect::new(ox + (r.x - ox) * k, oy + (r.y - oy) * k, r.w * k, r.h * k);
            bd.sync(self.hwnd, r, chrome::PANE_RADIUS * k, self.scale);
            bd.set_opacity(alpha);
        }
    }

    pub fn on_window_pos(&mut self) {
        self.sync_backdrop();
        self.redraw();
    }

    // --- input -----------------------------------------------------------------

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    fn hit_rect(&self, hit: Hit) -> Option<Rect> {
        self.hits.iter().rev().find(|(_, h)| *h == hit).map(|(r, _)| *r)
    }

    /// WM_NCHITTEST, from window-local points.
    pub fn nc_hit(&self, x: f32, y: f32) -> u32 {
        let b = self.body();
        if !b.inset(-2.0, -2.0).contains(x, y) {
            return HTNOWHERE;
        }
        if self.fullscreen.is_some() {
            return HTCLIENT;
        }
        let caption = y < b.y + TOOLBAR && self.hit_at(x, y).is_none();
        if self.is_zoomed() {
            return if caption { HTCAPTION } else { HTCLIENT };
        }
        let (l, r) = (x < b.x + EDGE, x > b.right() - EDGE);
        let (t, bt) = (y < b.y + EDGE, y > b.bottom() - EDGE);
        match (l, r, t, bt) {
            (true, _, true, _) => HTTOPLEFT,
            (_, true, true, _) => HTTOPRIGHT,
            (true, _, _, true) => HTBOTTOMLEFT,
            (_, true, _, true) => HTBOTTOMRIGHT,
            (true, ..) => HTLEFT,
            (_, true, ..) => HTRIGHT,
            (_, _, true, _) => HTTOP,
            (_, _, _, true) => HTBOTTOM,
            _ if caption => HTCAPTION,
            _ => HTCLIENT,
        }
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        let h = self.hit_at(x, y);
        if h != self.hover {
            let from = std::mem::replace(&mut self.hover, h);
            self.hover_changed(from, h);
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if let Some(h) = self.hover.take() {
            self.hover_changed(Some(h), None);
        }
    }

    pub fn on_down(&mut self, x: f32, y: f32) {
        self.pressed = self.hit_at(x, y);
        // A click anywhere else puts the popover away.
        if self.popover.as_ref().is_some_and(|p| p.open) && !matches!(self.pressed, Some(Hit::Popover | Hit::PopOpen | Hit::Event(_))) {
            self.close_popover();
        }
        self.redraw();
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> CalendarEvent {
        let pressed = self.pressed.take();
        self.redraw();
        let Some(hit) = self.hit_at(x, y).filter(|h| Some(*h) == pressed) else { return CalendarEvent::None };
        let double = self.last_click.is_some_and(|(h, t)| h == hit && t.elapsed().as_millis() < unsafe { GetDoubleClickTime() } as u128);
        self.last_click = if double { None } else { Some((hit, Instant::now())) };
        match hit {
            Hit::Light(Light::Close) => return CalendarEvent::Close,
            Hit::Light(Light::Minimize) => self.window.minimize(),
            Hit::Light(Light::Zoom) => self.toggle_zoom(),
            Hit::View(v) => self.set_view(v),
            Hit::Prev => self.step(-1),
            Hit::Next => self.step(1),
            Hit::Today => self.go(Local::now().date_naive(), None),
            Hit::MiniPrev | Hit::MiniNext => {
                self.set_mini(add_months(self.mini, if hit == Hit::MiniPrev { -1 } else { 1 }));
                self.reload();
            }
            Hit::MiniDay(n) => self.go(from_days(n), None),
            Hit::Cal(i) => {
                if let Some(c) = self.cals.get(i as usize) {
                    let key = (c.account, c.name.clone());
                    if !self.hidden.remove(&key) {
                        self.hidden.insert(key);
                    }
                    self.close_popover();
                    self.index();
                }
            }
            Hit::AddAccount => {
                calsync::setup(Some(calsync::Step::Pick));
                return CalendarEvent::Action(Action::OpenSettings(Some(crate::settings::Page::Calendars)));
            }
            Hit::Day(n) => {
                let d = from_days(n);
                if double {
                    self.zoom_into(d, View::Day, hit);
                } else {
                    self.selected = d;
                    if self.view == View::Month && d.month() != self.cursor.month() {
                        self.go(d, None);
                    }
                }
            }
            Hit::Month(m) => {
                let d = NaiveDate::from_ymd_opt(self.cursor.year(), m as u32 + 1, 1).unwrap_or(self.cursor);
                self.zoom_into(d, View::Month, hit);
            }
            Hit::More(n) => self.zoom_into(from_days(n), View::Day, Hit::Day(n)),
            Hit::Event(i) => {
                let anchor = self.hit_rect(hit).unwrap_or_default();
                let same = self.popover.as_ref().is_some_and(|p| p.open && p.event == i as usize);
                if same {
                    self.close_popover();
                } else {
                    if self.popover.as_ref().is_none_or(|p| !p.open) {
                        self.pop_t.snap(0.0);
                    }
                    self.popover = Some(Pop { event: i as usize, anchor, open: true });
                    self.pop_t.set(1.0);
                }
            }
            Hit::PopOpen => {
                if let Some(e) = self.popover.as_ref().and_then(|p| self.events.get(p.event)).filter(|e| !e.link.is_empty()) {
                    return CalendarEvent::Action(Action::Open(e.link.clone()));
                }
            }
            Hit::Popover => {}
        }
        CalendarEvent::None
    }

    /// Mouse wheel: the timeline scrolls; months and years page.
    pub fn on_wheel(&mut self, x: f32, delta: i32, horizontal: bool) {
        let body = self.body();
        if x < body.x + SIDEBAR {
            return;
        }
        let touchpad = delta % 120 != 0;
        if !horizontal && matches!(self.view, View::Day | View::Week) {
            self.scroll.wheel_smooth(delta, HOUR_H * 1.5, self.max_scroll.get());
            self.close_popover();
            // A touchpad's moves land at once (no glide follows to draw them).
            let page = self.page_rect();
            self.redraw_in(page);
            return;
        }
        // A notch turns a page. A touchpad turns one per swipe: once the fingers
        // have moved far enough, the rest of that swipe (and its coasting) is spent.
        let now = Instant::now();
        if let Some(t) = self.wheel_paged {
            if touchpad && now.duration_since(t).as_millis() < 280 {
                self.wheel_paged = Some(now);
                self.wheel_acc = 0;
                return;
            }
            self.wheel_paged = None;
        }
        // Horizontal wheels and touchpads swipe the other way round.
        self.wheel_acc += if horizontal { -delta } else { delta };
        while self.wheel_acc.abs() >= 120 {
            let dir = if self.wheel_acc > 0 { -1 } else { 1 };
            self.wheel_acc = if touchpad { 0 } else { self.wheel_acc + dir * 120 };
            self.step_on(dir, !horizontal);
            if touchpad {
                self.wheel_paged = Some(now);
            }
        }
    }

    /// The area under the toolbar: the title and the view.
    fn page_rect(&self) -> Rect {
        let body = self.body();
        Rect::new(body.x + SIDEBAR, body.y + TOOLBAR, body.w - SIDEBAR, body.h - TOOLBAR)
    }

    pub fn on_key(&mut self, vk: u16, ctrl: bool) -> CalendarEvent {
        const ESC: u16 = 0x1B;
        const LEFT: u16 = 0x25;
        const UP: u16 = 0x26;
        const RIGHT: u16 = 0x27;
        const DOWN: u16 = 0x28;
        const F11: u16 = 0x7A;
        match vk {
            ESC if self.popover.as_ref().is_some_and(|p| p.open) => self.close_popover(),
            ESC if self.fullscreen.is_some() => self.toggle_fullscreen(),
            0x57 if ctrl => return CalendarEvent::Close,
            F11 => self.toggle_fullscreen(),
            // Ctrl+1…4 like macOS's ⌘1…4.
            0x31..=0x34 if ctrl => self.set_view(VIEWS[(vk - 0x31) as usize].0),
            0x54 => self.go(Local::now().date_naive(), None),
            LEFT => self.step(-1),
            RIGHT => self.step(1),
            UP | DOWN if matches!(self.view, View::Day | View::Week) => {
                let to = self.scroll.target() + if vk == UP { -HOUR_H } else { HOUR_H };
                self.scroll.to(to.clamp(0.0, self.max_scroll.get()));
            }
            _ => return CalendarEvent::None,
        }
        self.redraw();
        CalendarEvent::None
    }

    // --- drawing -----------------------------------------------------------------

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        let mut part = self.part.take();
        // (Screenshot Mode too: names must never show once it's on.)
        if calsync::gen() != self.gen || crate::screenshot_mode::on() != self.cals_demo {
            self.reload();
            part = None;
        }
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        if (self.canvas.get_mut().width, self.canvas.get_mut().height) != (rc.right - rc.left, rc.bottom - rc.top) {
            part = None;
        }
        // Only the same view of the same days may be patched, and the whole
        // page once the timeline has scrolled.
        let shows = Some((self.view, self.cursor, self.selected, self.mini, (self.scroll.offset_px(self.scale) * self.scale).round() as i32));
        if shows != self.drawn {
            let scrolled_only = matches!((shows, self.drawn), (Some(a), Some(b)) if (a.0, a.1, a.2, a.3) == (b.0, b.1, b.2, b.3));
            self.drawn = shows;
            part = if scrolled_only { part.map(|r| r.union(self.page_rect())) } else { None };
        }
        if let Err(e) = self.canvas.get_mut().resize(rc.right - rc.left, rc.bottom - rc.top) {
            return log!("calendar resize failed: {e}");
        }
        if !self.window.begin_render(self.hwnd) {
            return;
        }
        self.motion.sweep();
        let body = self.body();
        self.xf = self.window.transform(body);
        let (zx, zy, zoom, alpha) = self.xf;
        let mut pal = chrome::Palette::new(theme);
        if let Some(bd) = &mut self.backdrop {
            bd.set_theme(theme);
            pal.sidebar = SidebarBackdrop::fill();
        }
        if self.backdrop_for != Some((self.xf, body)) {
            self.sync_backdrop();
        }
        let floating = self.backdrop.is_some();
        let radius = self.radius();
        let mut hits = Vec::new();
        let this = &*self;
        let mut canvas = this.canvas.borrow_mut();
        // Just the area that changed when that's all (and the window isn't zooming or fading).
        let part = part.filter(|_| this.shown && (zoom, alpha) == (1.0, 1.0)).and_then(|r| canvas.part_px(r));
        let drawn = canvas.draw_part(gfx, part, |p| {
            p.scaled_about(zx, zy, zoom, || {
                if floating {
                    chrome::window_floating(p, body, SIDEBAR, theme, &pal, radius);
                } else {
                    chrome::window(p, body, SIDEBAR, theme, &pal, radius);
                }
                this.paint(p, theme, &pal, body, floating, &mut hits);
            })
        });
        let presented = drawn.and_then(|_| canvas.present_part(this.hwnd, rc.left, rc.top, alpha, part));
        let px = part.map_or(canvas.width as u64 * canvas.height as u64, |r| ((r.right - r.left) * (r.bottom - r.top)) as u64);
        drop(canvas);
        self.last_px = px;
        self.hits = hits;
        if let Err(e) = presented {
            log!("calendar render failed: {e}");
        }
        if !self.shown {
            self.shown = true;
            self.scroll_to_morning();
            if let Some(normal) = self.normal.take() {
                self.show_maximized(normal);
            }
            unsafe {
                let _ = SetForegroundWindow(self.hwnd);
            }
        }
    }

    fn hv(&self, h: Hit) -> f32 {
        self.motion.fade(Anim::Hover(h), self.hover == Some(h))
    }

    fn paint(&self, p: &Painter, theme: &Theme, pal: &chrome::Palette, body: Rect, floating: bool, hits: &mut Vec<(Rect, Hit)>) {
        let dark = theme.dark;
        let soft = if dark { Color::gray(255, 0.08) } else { Color::gray(0, 0.055) };
        let side = if floating { chrome::sidebar_pane(body, SIDEBAR) } else { Rect::new(body.x, body.y, SIDEBAR, body.h) };
        self.paint_sidebar(p, theme, side, soft, hits);

        let content = Rect::new(body.x + SIDEBAR, body.y, body.w - SIDEBAR, body.h);
        self.paint_toolbar(p, theme, content, soft, hits);

        // The page: title and view, sliding while paging and fading in on a new view.
        let page = Rect::new(content.x, body.y + TOOLBAR, content.w, body.h - TOOLBAR);
        let k = self.page.value;
        let s = self.swap.value.clamp(0.0, 1.0);
        let opacity = (1.0 - k.abs() * 0.85).clamp(0.0, 1.0) * s;
        p.clipped(page, || {
            p.faded_in(page, opacity, || {
                let (dx, dy) = if self.vertical { (0.0, k * SLIDE) } else { (k * SLIDE, 0.0) };
                // A new view grows out of the day or month clicked, else settles in place.
                let ((ox, oy), from) = self.swap_from.map_or(((page.cx(), page.y), 0.985), |o| (o, 0.9));
                p.transformed(dx, dy, 1.0, || {
                    p.scaled_about(ox, oy, lerp(from, 1.0, s), || {
                        self.paint_title(p, theme, Rect::new(page.x + 22.0, page.y, page.w - 44.0, HEADER));
                        let area = Rect::new(page.x, page.y + HEADER, page.w, page.h - HEADER);
                        match self.view {
                            View::Month => self.paint_month(p, theme, pal, area, soft, hits),
                            View::Week | View::Day => self.paint_timeline(p, theme, pal, area, soft, hits),
                            View::Year => self.paint_year(p, theme, area, hits),
                        }
                    })
                })
            })
        });

        self.paint_popover(p, theme, body, hits);

        // Traffic lights last so they win hit-testing.
        let group = self.motion.fade(Anim::Lights, matches!(self.hover, Some(Hit::Light(_))));
        let lights = Rect::new(body.x + 4.0, body.y + TOOLBAR / 2.0 - 20.0, body.w, body.h);
        for (r, light) in chrome::traffic_lights(p, lights, group, true) {
            hits.push((r, Hit::Light(light)));
        }
    }

    fn paint_sidebar(&self, p: &Painter, theme: &Theme, side: Rect, soft: Color, hits: &mut Vec<(Rect, Hit)>) {
        let sub = theme.text_secondary;
        // The small month sits at the bottom; the calendars fill the rest.
        let cell_h = 24.0;
        let mini_h = 30.0 + 18.0 + 6.0 * cell_h;
        let mini = Rect::new(side.x + 10.0, side.bottom() - mini_h - 12.0, side.w - 20.0, mini_h);
        let list = Rect::new(side.x, side.y + TOOLBAR, side.w, mini.y - side.y - TOOLBAR - 8.0);
        p.clipped(list, || {
            let mut y = list.y;
            if !self.has_accounts {
                text(p, theme, "No calendars added yet", 12.0, Weight::Regular, Align::Leading, Rect::new(side.x + 16.0, y, side.w - 32.0, 22.0), sub);
                y += 26.0;
                let r = Rect::new(side.x + 8.0, y, side.w - 16.0, 28.0);
                let t = self.hv(Hit::AddAccount);
                if t > 0.0 {
                    p.fill_round(r, 7.0, soft.alpha(soft.a * t));
                }
                text(p, theme, "Add Calendar Account\u{2026}", 13.0, Weight::Regular, Align::Leading, r.inset(8.0, 0.0), theme.accent);
                hits.push((r, Hit::AddAccount));
                return;
            }
            let mut group: Option<&str> = None;
            for (i, c) in self.cals.iter().enumerate() {
                if group != Some(c.group.as_str()) {
                    group = Some(c.group.as_str());
                    y += 4.0;
                    text(p, theme, &c.group, 11.0, Weight::SemiBold, Align::Leading, Rect::new(side.x + 16.0, y, side.w - 32.0, 22.0), sub);
                    y += 24.0;
                }
                let hit = Hit::Cal(i as u16);
                let r = Rect::new(side.x + 8.0, y, side.w - 16.0, 26.0);
                let t = self.hv(hit);
                if t > 0.0 {
                    p.fill_round(r, 7.0, soft.alpha(soft.a * t));
                }
                let color = rgb(c.color);
                let shown = !self.hidden.contains(&(c.account, c.name.clone()));
                let b = Rect::new(r.x + 8.0, r.cy() - 7.0, 14.0, 14.0);
                // The box fills (with a little pop) and the tick draws itself in.
                let k = self.motion.get(Anim::Check(i as u16), if shown { 1.0 } else { 0.0 }, (0.24, 0.72));
                let fill = k.clamp(0.0, 1.0);
                p.stroke_round(b.inset(0.75, 0.75), 3.5, color.alpha(1.0 - fill), 1.5);
                if k > 0.01 {
                    let g = b.inset(7.0 * (1.0 - k.min(1.0)), 7.0 * (1.0 - k.min(1.0))).inset(-(k - 1.0).max(0.0) * 6.0, -(k - 1.0).max(0.0) * 6.0);
                    p.fill_round(g, 4.0 * g.w / b.w, color.alpha(fill));
                    // The tick: down to the corner, then up, as far as it's got.
                    let (a, m, e) = ((b.x + 3.5, b.cy() + 0.2), (b.x + 6.0, b.cy() + 2.8), (b.x + 10.8, b.cy() - 2.6));
                    let t = ((k - 0.25) / 0.75).clamp(0.0, 1.0);
                    let t1 = (t / 0.35).min(1.0);
                    p.line(a.0, a.1, lerp(a.0, m.0, t1), lerp(a.1, m.1, t1), Color::WHITE, 1.6);
                    if t > 0.35 {
                        let t2 = (t - 0.35) / 0.65;
                        p.line(m.0, m.1, lerp(m.0, e.0, t2), lerp(m.1, e.1, t2), Color::WHITE, 1.6);
                    }
                }
                text(p, theme, &c.name, 13.0, Weight::Regular, Align::Leading, Rect::new(b.right() + 9.0, r.y, r.w - 40.0, r.h), theme.text);
                hits.push((r, hit));
                y += 26.0;
            }
        });
        self.paint_mini(p, theme, mini, soft, hits);
    }

    fn paint_mini(&self, p: &Painter, theme: &Theme, r: Rect, soft: Color, hits: &mut Vec<(Rect, Hit)>) {
        let sub = theme.text_secondary;
        let accent = theme.accent;
        let today = Local::now().date_naive();
        let mw = measure(p, theme, &self.mini.format("%B").to_string(), 13.0, Weight::SemiBold);
        text(p, theme, &self.mini.format("%B").to_string(), 13.0, Weight::SemiBold, Align::Leading, Rect::new(r.x + 6.0, r.y, r.w, 28.0), theme.text);
        text(p, theme, &self.mini.format("%Y").to_string(), 13.0, Weight::Regular, Align::Leading, Rect::new(r.x + 10.0 + mw, r.y, 60.0, 28.0), sub);
        let glyphs = p.gfx.format("Segoe Fluent Icons", 9.0, Weight::Regular, Align::Center);
        for (i, (g, hit)) in [("\u{E76B}", Hit::MiniPrev), ("\u{E76C}", Hit::MiniNext)].into_iter().enumerate() {
            let b = Rect::new(r.right() - 52.0 + i as f32 * 26.0, r.y + 3.0, 24.0, 22.0);
            let t = self.hv(hit);
            if t > 0.0 {
                p.fill_round(b, 6.0, soft.alpha(soft.a * t * 1.4));
            }
            p.text(g, &glyphs, b, theme.text);
            hits.push((b, hit));
        }
        let cw = r.w / 7.0;
        let y0 = r.y + 30.0;
        for (i, d) in self.dow_names(1).iter().enumerate() {
            text(p, theme, d, 10.0, Weight::SemiBold, Align::Center, Rect::new(r.x + i as f32 * cw, y0, cw, 16.0), sub);
        }
        let (start, weeks) = self.month_grid(self.mini);
        // The week or day the main view shows, as a soft band.
        let (va, vb) = self.view_span();
        let band = matches!(self.view, View::Week | View::Day);
        // A new month slides in from the side it lies on.
        let k = self.mini_page.value;
        let grid = Rect::new(r.x, y0 + 18.0, r.w, 6.0 * 24.0);
        p.clipped(grid.inset(0.0, -2.0), || p.faded_in(grid, (1.0 - k.abs()).clamp(0.0, 1.0), || p.transformed(k * 36.0, 0.0, 1.0, || {
        for i in 0..weeks.max(6) * 7 {
            let d = start + Duration::days(i as i64);
            let cell = Rect::new(r.x + (i % 7) as f32 * cw, y0 + 18.0 + (i / 7) as f32 * 24.0, cw, 24.0);
            let hit = Hit::MiniDay(days(d));
            if band && d >= va && d < vb {
                let first = d == va || i % 7 == 0;
                let last = d + Duration::days(1) == vb || i % 7 == 6;
                let b = Rect::new(cell.x + if first { 2.0 } else { 0.0 }, cell.y + 2.0, cell.w - if first { 2.0 } else { 0.0 } - if last { 2.0 } else { 0.0 }, cell.h - 4.0);
                p.fill_round(b, if first || last { 10.0 } else { 0.0 }, soft);
            }
            let (cx, cy, rad) = (cell.cx(), cell.cy(), 10.5);
            let is_today = d == today;
            if is_today {
                p.fill_circle(cx, cy, rad, accent);
            } else if d == self.selected && !band {
                p.fill_circle(cx, cy, rad, soft.alpha((soft.a * 2.0).min(1.0)));
            } else {
                let t = self.hv(hit);
                if t > 0.0 {
                    p.fill_circle(cx, cy, rad, soft.alpha(soft.a * t * 1.5));
                }
            }
            let c = if is_today {
                Color::WHITE
            } else if d.month() != self.mini.month() {
                sub.alpha(sub.a * 0.55)
            } else {
                theme.text
            };
            let weight = if is_today { Weight::SemiBold } else { Weight::Regular };
            text(p, theme, &d.day().to_string(), 11.0, weight, Align::Center, cell, c);
            hits.push((cell, hit));
        }
        })));
    }

    fn paint_toolbar(&self, p: &Painter, theme: &Theme, content: Rect, soft: Color, hits: &mut Vec<(Rect, Hit)>) {
        let dark = theme.dark;
        // Day | Week | Month | Year, with a sliding thumb.
        let seg_w = 64.0;
        let track = Rect::new(content.cx() - seg_w * 2.0, content.y + 12.0, seg_w * 4.0, 28.0);
        p.fill_round(track, 8.0, soft);
        let at = VIEWS.iter().position(|(v, _)| *v == self.view).unwrap_or(2);
        let thumb_to = Rect::new(track.x + 2.0 + at as f32 * seg_w, track.y + 2.0, seg_w - 4.0, track.h - 4.0);
        if let Some((r, a)) = self.pill.borrow_mut().update(Some(thumb_to)) {
            let fill = if dark { Color::gray(255, 0.2) } else { Color::WHITE };
            p.fill_round(r.offset(0.0, 0.5), 6.5, Color::gray(0, 0.08 * a));
            p.fill_round(r, 6.5, fill.alpha(fill.a * a));
        }
        for (i, (v, label)) in VIEWS.iter().enumerate() {
            let r = Rect::new(track.x + i as f32 * seg_w, track.y, seg_w, track.h);
            let hit = Hit::View(*v);
            if *v != self.view {
                let t = self.hv(hit);
                if t > 0.0 {
                    p.fill_round(r.inset(2.0, 2.0), 6.5, soft.alpha(soft.a * t));
                }
            }
            let weight = if *v == self.view { Weight::SemiBold } else { Weight::Regular };
            text(p, theme, label, 12.5, weight, Align::Center, r, theme.text);
            hits.push((r, hit));
        }

        // ‹ Today › at the right.
        let glyphs = p.gfx.format("Segoe Fluent Icons", 10.0, Weight::Regular, Align::Center);
        let tw = measure(p, theme, "Today", 12.5, Weight::Regular) + 22.0;
        let group = Rect::new(content.right() - 18.0 - (30.0 + tw + 30.0), content.y + 12.0, 30.0 + tw + 30.0, 28.0);
        p.fill_round(group, 8.0, soft);
        let parts = [(Rect::new(group.x, group.y, 30.0, group.h), Hit::Prev), (Rect::new(group.x + 30.0, group.y, tw, group.h), Hit::Today), (Rect::new(group.x + 30.0 + tw, group.y, 30.0, group.h), Hit::Next)];
        for (r, hit) in parts {
            let t = self.hv(hit);
            let pressed = self.pressed == Some(hit) && self.hover == Some(hit);
            if t > 0.0 {
                p.fill_round(r.inset(2.0, 2.0), 6.5, soft.alpha(soft.a * t * if pressed { 2.4 } else { 1.4 }));
            }
            match hit {
                Hit::Prev => p.text("\u{E76B}", &glyphs, r, theme.text),
                Hit::Next => p.text("\u{E76C}", &glyphs, r, theme.text),
                _ => text(p, theme, "Today", 12.5, Weight::Regular, Align::Center, r, theme.text),
            }
            hits.push((r, hit));
        }
        p.line(group.x + 30.0, group.y + 7.0, group.x + 30.0, group.bottom() - 7.0, soft.alpha(soft.a * 2.0), 1.0);
        p.line(group.x + 30.0 + tw, group.y + 7.0, group.x + 30.0 + tw, group.bottom() - 7.0, soft.alpha(soft.a * 2.0), 1.0);
    }

    fn paint_title(&self, p: &Painter, theme: &Theme, r: Rect) {
        let (bold, rest) = match self.view {
            View::Day => (self.cursor.format("%-d %B %Y").to_string(), self.cursor.format("%A").to_string()),
            View::Week => {
                let s = self.week_start(self.cursor);
                let e = s + Duration::days(6);
                if s.month() == e.month() {
                    (s.format("%B").to_string(), s.format("%Y").to_string())
                } else if s.year() == e.year() {
                    (format!("{} \u{2013} {}", s.format("%b"), e.format("%b")), s.format("%Y").to_string())
                } else {
                    (format!("{} \u{2013} {}", s.format("%b %Y"), e.format("%b")), e.format("%Y").to_string())
                }
            }
            View::Month => (self.cursor.format("%B").to_string(), self.cursor.format("%Y").to_string()),
            View::Year => (self.cursor.format("%Y").to_string(), String::new()),
        };
        let bw = measure(p, theme, &bold, 26.0, Weight::Bold);
        text(p, theme, &bold, 26.0, Weight::Bold, Align::Leading, Rect::new(r.x, r.y + 6.0, r.w, r.h - 10.0), theme.text);
        text(p, theme, &rest, 26.0, Weight::Regular, Align::Leading, Rect::new(r.x + bw + 8.0, r.y + 6.0, r.w, r.h - 10.0), theme.text_secondary);
    }

    /// Weekday names in the week's order: 1 letter, 3 letters, or full.
    fn dow_names(&self, len: usize) -> Vec<String> {
        let mut wd = if self.monday { Weekday::Mon } else { Weekday::Sun };
        let mut v = Vec::new();
        for _ in 0..7 {
            let full = match wd {
                Weekday::Mon => "Monday",
                Weekday::Tue => "Tuesday",
                Weekday::Wed => "Wednesday",
                Weekday::Thu => "Thursday",
                Weekday::Fri => "Friday",
                Weekday::Sat => "Saturday",
                Weekday::Sun => "Sunday",
            };
            v.push(if len >= 9 { full.to_string() } else { full.chars().take(len).collect() });
            wd = wd.succ();
        }
        v
    }

    fn weekend(d: NaiveDate) -> bool {
        matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
    }

    /// An event's colour, text colour and fill for a tinted block.
    fn tints(theme: &Theme, e: &Event) -> (Color, Color, Color) {
        let c = rgb(e.color);
        let ink = if theme.dark { c.mix(Color::WHITE, 0.45) } else { c.mix(Color::BLACK, 0.35) };
        (c, ink, c.alpha(if theme.dark { 0.3 } else { 0.18 }))
    }

    fn paint_month(&self, p: &Painter, theme: &Theme, pal: &chrome::Palette, area: Rect, soft: Color, hits: &mut Vec<(Rect, Hit)>) {
        let sub = theme.text_secondary;
        let month = first_of(self.cursor);
        let (start, weeks) = self.month_grid(month);
        let dow_h = 26.0;
        let grid = Rect::new(area.x, area.y + dow_h, area.w, area.h - dow_h);
        let (cw, ch) = (grid.w / 7.0, grid.h / weeks as f32);
        let names = self.dow_names(if cw > 110.0 { 3 } else { 1 });
        let weekend_fill = soft.alpha(soft.a * 0.45);
        for (i, n) in names.iter().enumerate() {
            let col = Rect::new(grid.x + i as f32 * cw, area.y, cw, dow_h);
            text(p, theme, n, 12.0, Weight::Regular, Align::Trailing, Rect::new(col.x, col.y, col.w - 10.0, col.h), sub);
            if Self::weekend(start + Duration::days(i as i64)) {
                p.fill_rect(Rect::new(col.x, grid.y, cw, grid.h), weekend_fill);
            }
        }
        // Grid lines.
        for r in 0..=weeks {
            let y = grid.y + r as f32 * ch;
            p.line(grid.x, y, grid.right(), y, pal.divider, 1.0);
        }
        for c in 1..7 {
            let x = grid.x + c as f32 * cw;
            p.line(x, grid.y, x, grid.bottom(), pal.divider, 1.0);
        }
        // The selected day's tint glides from day to day.
        let sel_cell = (self.selected >= start && self.selected < start + Duration::days(weeks as i64 * 7)).then(|| {
            let i = (self.selected - start).num_days() as usize;
            Rect::new(grid.x + (i % 7) as f32 * cw, grid.y + (i / 7) as f32 * ch, cw, ch).inset(0.5, 0.5)
        });
        if let Some((r, a)) = self.sel.borrow_mut().update(sel_cell) {
            p.fill_rect(r, theme.accent.alpha(0.09 * a));
        }
        let today = Local::now().date_naive();
        let now = calsync::now();
        for i in 0..weeks * 7 {
            let d = start + Duration::days(i as i64);
            let cell = Rect::new(grid.x + (i % 7) as f32 * cw, grid.y + (i / 7) as f32 * ch, cw, ch);
            let hit = Hit::Day(days(d));
            let t = self.hv(hit);
            if t > 0.0 {
                p.fill_rect(cell.inset(0.5, 0.5), soft.alpha(soft.a * 0.6 * t));
            }
            hits.push((cell, hit));
            let in_month = d.month() == month.month();
            let draw = |hits: &mut Vec<(Rect, Hit)>| {
                // The day's number, top right ("Nov 1" on the 1st), today in a pill.
                let label = if d.day() == 1 { d.format("%b %-d").to_string() } else { d.day().to_string() };
                let num = Rect::new(cell.x, cell.y + 5.0, cell.w - 8.0, 22.0);
                if d == today {
                    let pw = (measure(p, theme, &label, 13.0, Weight::SemiBold) + 14.0).max(22.0);
                    let pill = Rect::new(num.right() - pw + 4.0, num.y, pw, 22.0);
                    p.fill_round(pill, 11.0, theme.accent);
                    text(p, theme, &label, 13.0, Weight::SemiBold, Align::Center, pill, Color::WHITE);
                } else {
                    let c = if in_month { theme.text } else { sub };
                    text(p, theme, &label, 13.0, Weight::Regular, Align::Trailing, Rect::new(num.x, num.y, num.w - 3.0, num.h), c);
                }
                // Its events, as many as fit.
                let list = self.day_events(d);
                let fit = (((ch - 32.0) / ROW_H).floor().max(0.0)) as usize;
                let shown = if list.len() > fit { fit.saturating_sub(1) } else { list.len() };
                let mut y = cell.y + 30.0;
                for &ei in &list[..shown] {
                    let e = &self.events[ei];
                    let r = Rect::new(cell.x + 4.0, y, cell.w - 8.0, ROW_H - 2.0);
                    let ehit = Hit::Event(ei as u32);
                    let (c, ink, fill) = Self::tints(theme, e);
                    let lit = self.hv(ehit).max(if self.popover.as_ref().is_some_and(|p| p.open && p.event == ei) { 1.0 } else { 0.0 });
                    let past = !e.all_day && e.end <= now;
                    p.faded_in(r.inset(-1.0, -1.0), if past { 0.55 } else { 1.0 }, || {
                        if e.all_day {
                            p.fill_round(r, 4.0, fill.alpha((fill.a * (1.0 + lit)).min(1.0)));
                            text(p, theme, &e.title, 11.5, Weight::SemiBold, Align::Leading, r.inset(6.0, 0.0), ink);
                        } else {
                            if lit > 0.0 {
                                p.fill_round(r, 4.0, fill.alpha(fill.a * lit));
                            }
                            p.fill_circle(r.x + 6.0, r.cy(), 3.2, c);
                            let time = short_clock(e.start);
                            let tw = if cell.w > 120.0 { measure(p, theme, &time, 11.0, Weight::Regular) + 6.0 } else { 0.0 };
                            text(p, theme, &e.title, 11.5, Weight::Regular, Align::Leading, Rect::new(r.x + 14.0, r.y, r.w - 16.0 - tw, r.h), theme.text);
                            if tw > 0.0 {
                                text(p, theme, &time, 11.0, Weight::Regular, Align::Trailing, Rect::new(r.x, r.y, r.w - 4.0, r.h), sub);
                            }
                        }
                    });
                    hits.push((r, ehit));
                    y += ROW_H;
                }
                if list.len() > shown && fit > 0 {
                    let r = Rect::new(cell.x + 4.0, y, cell.w - 8.0, ROW_H - 2.0);
                    let more = Hit::More(days(d));
                    let t = self.hv(more);
                    if t > 0.0 {
                        p.fill_round(r, 4.0, soft.alpha(soft.a * t * 1.4));
                    }
                    text(p, theme, &format!("{} more", list.len() - shown), 11.0, Weight::SemiBold, Align::Leading, r.inset(6.0, 0.0), sub);
                    hits.push((r, more));
                }
            };
            if in_month {
                draw(hits);
            } else {
                p.faded_in(cell, 0.6, || draw(hits));
            }
        }
    }

    /// Lays out one day's timed events side by side where they overlap:
    /// (event, start minute, end minute, column, columns).
    fn columns(&self, d: NaiveDate) -> Vec<(usize, f32, f32, usize, usize)> {
        let day0 = midnight(d);
        let mut items: Vec<(usize, f32, f32)> = self
            .day_events(d)
            .iter()
            .filter(|&&i| !self.events[i].all_day)
            .map(|&i| {
                let e = &self.events[i];
                let s = ((e.start - day0) as f32 / 60.0).clamp(0.0, 1440.0);
                let end = ((e.end - day0) as f32 / 60.0).clamp(0.0, 1440.0);
                (i, s, end.max(s + 22.0))
            })
            .collect();
        items.sort_by(|a, b| a.1.total_cmp(&b.1).then(b.2.total_cmp(&a.2)));
        let mut out = Vec::new();
        let mut cluster: Vec<(usize, f32, f32, usize)> = Vec::new();
        let mut cols_end: Vec<f32> = Vec::new();
        let mut cluster_end = f32::MIN;
        let flush = |cluster: &mut Vec<(usize, f32, f32, usize)>, cols: &mut Vec<f32>, out: &mut Vec<(usize, f32, f32, usize, usize)>| {
            let n = cols.len().max(1);
            for (i, s, e, c) in cluster.drain(..) {
                out.push((i, s, e, c, n));
            }
            cols.clear();
        };
        for (i, s, e) in items {
            if s >= cluster_end && !cluster.is_empty() {
                flush(&mut cluster, &mut cols_end, &mut out);
            }
            let col = match cols_end.iter().position(|&end| end <= s) {
                Some(c) => {
                    cols_end[c] = e;
                    c
                }
                None => {
                    cols_end.push(e);
                    cols_end.len() - 1
                }
            };
            cluster.push((i, s, e, col));
            cluster_end = if cluster.len() == 1 { e } else { cluster_end.max(e) };
        }
        flush(&mut cluster, &mut cols_end, &mut out);
        out
    }

    fn paint_timeline(&self, p: &Painter, theme: &Theme, pal: &chrome::Palette, area: Rect, soft: Color, hits: &mut Vec<(Rect, Hit)>) {
        let sub = theme.text_secondary;
        let (first, n) = match self.view {
            View::Day => (self.cursor, 1),
            _ => (self.week_start(self.cursor), 7),
        };
        let dates: Vec<NaiveDate> = (0..n).map(|i| first + Duration::days(i as i64)).collect();
        let cw = (area.w - GUTTER) / n as f32;
        let col_x = |i: usize| area.x + GUTTER + i as f32 * cw;
        let today = Local::now().date_naive();
        let mut y = area.y;

        // Day names (the week), with today's date in a pill.
        if n > 1 {
            let names = self.dow_names(if cw > 90.0 { 3 } else { 1 });
            for (i, d) in dates.iter().enumerate() {
                let r = Rect::new(col_x(i), y, cw, DAY_HEAD);
                let num = d.day().to_string();
                let name = &names[i];
                let nw = measure(p, theme, name, 13.0, Weight::Regular);
                let dw = measure(p, theme, &num, 13.0, Weight::SemiBold);
                let total = nw + 6.0 + dw.max(14.0) + 10.0;
                let x0 = r.cx() - total / 2.0;
                text(p, theme, name, 13.0, Weight::Regular, Align::Leading, Rect::new(x0, r.y, nw + 4.0, r.h), sub);
                let pill = Rect::new(x0 + nw + 6.0, r.cy() - 11.0, dw.max(14.0) + 10.0, 22.0);
                if *d == today {
                    p.fill_round(pill, 11.0, theme.accent);
                }
                let c = if *d == today { Color::WHITE } else { theme.text };
                text(p, theme, &num, 13.0, Weight::SemiBold, Align::Center, pill, c);
                hits.push((r, Hit::Day(days(*d))));
            }
            y += DAY_HEAD;
        }

        // All-day events in a strip.
        let most = dates.iter().map(|d| self.day_events(*d).iter().filter(|&&i| self.events[i].all_day).count()).max().unwrap_or(0);
        let strip_h = (most as f32 * ALL_DAY_ROW + 8.0).max(26.0);
        p.line(area.x, y, area.right(), y, pal.divider, 1.0);
        text(p, theme, "all-day", 11.0, Weight::Regular, Align::Trailing, Rect::new(area.x, y, GUTTER - 10.0, 26.0), sub);
        for (i, d) in dates.iter().enumerate() {
            let mut ry = y + 4.0;
            for &ei in self.day_events(*d).iter().filter(|&&i| self.events[i].all_day) {
                let e = &self.events[ei];
                let (_, ink, fill) = Self::tints(theme, e);
                let r = Rect::new(col_x(i) + 2.0, ry, cw - 4.0, ALL_DAY_ROW - 2.0);
                let hit = Hit::Event(ei as u32);
                let lit = self.hv(hit);
                p.fill_round(r, 4.0, fill.alpha((fill.a * (1.0 + lit)).min(1.0)));
                text(p, theme, &e.title, 11.5, Weight::SemiBold, Align::Leading, r.inset(6.0, 0.0), ink);
                hits.push((r, hit));
                ry += ALL_DAY_ROW;
            }
        }
        y += strip_h;
        p.line(area.x, y, area.right(), y, pal.divider, 1.0);

        // The hours.
        let tl = Rect::new(area.x, y, area.w, area.bottom() - y);
        let max = (24.0 * HOUR_H + 12.0 - tl.h).max(0.0);
        self.max_scroll.set(max);
        let off = self.scroll.offset_px(self.scale).min(max.max(0.0) + 40.0);
        let top = tl.y - off + 6.0;
        p.clipped(tl, || {
            for (i, d) in dates.iter().enumerate() {
                let col = Rect::new(col_x(i), tl.y, cw, tl.h);
                if n > 1 && Self::weekend(*d) {
                    p.fill_rect(col, soft.alpha(soft.a * 0.45));
                }
                hits.push((col, Hit::Day(days(*d))));
            }
            for h in 0..=24u32 {
                let ly = top + h as f32 * HOUR_H;
                p.line(area.x + GUTTER - 6.0, ly, area.right(), ly, pal.divider, 1.0);
                if h > 0 && h < 24 {
                    text(p, theme, &hour_label(h), 11.0, Weight::Regular, Align::Trailing, Rect::new(area.x, ly - 8.0, GUTTER - 12.0, 16.0), sub);
                }
            }
            for i in 1..n {
                let x = col_x(i);
                p.line(x, tl.y, x, tl.bottom(), pal.divider, 1.0);
            }
            p.line(col_x(0), tl.y, col_x(0), tl.bottom(), pal.divider, 1.0);
            let now = calsync::now();
            for (i, d) in dates.iter().enumerate() {
                let x0 = col_x(i);
                for (ei, s, e, c, cols) in self.columns(*d) {
                    let ev = &self.events[ei];
                    let (color, ink, fill) = Self::tints(theme, ev);
                    let w = (cw - 6.0) / cols as f32;
                    let r = Rect::new(x0 + 2.0 + c as f32 * w, top + s / 60.0 * HOUR_H + 1.0, w - 2.0, ((e - s) / 60.0 * HOUR_H - 2.0).max(16.0));
                    let hit = Hit::Event(ei as u32);
                    let lit = self.hv(hit).max(if self.popover.as_ref().is_some_and(|p| p.open && p.event == ei) { 1.0 } else { 0.0 });
                    let past = ev.end <= now;
                    p.faded_in(r.inset(-1.0, -1.0), if past { 0.6 } else { 1.0 }, || {
                        p.fill_round(r, 5.0, fill.alpha((fill.a * (1.0 + 0.8 * lit)).min(1.0)));
                        p.fill_round(Rect::new(r.x, r.y, 3.5, r.h), 1.75, color);
                        p.clipped(r, || {
                            let tx = Rect::new(r.x + 8.0, r.y + 3.0, r.w - 10.0, 16.0);
                            if r.h < 32.0 {
                                let line = format!("{}  {}", ev.title, short_clock(ev.start));
                                text(p, theme, &line, 11.5, Weight::SemiBold, Align::Leading, Rect::new(tx.x, r.y, tx.w, r.h.min(20.0)), ink);
                            } else {
                                text(p, theme, &ev.title, 12.0, Weight::SemiBold, Align::Leading, tx, ink);
                                let mut detail = format!("{} \u{2013} {}", short_clock(ev.start), short_clock(ev.end));
                                if !ev.location.is_empty() && r.h < 52.0 {
                                    detail = format!("{detail} \u{00B7} {}", ev.location);
                                }
                                text(p, theme, &detail, 11.0, Weight::Regular, Align::Leading, Rect::new(tx.x, tx.y + 16.0, tx.w, 15.0), ink.alpha(0.85));
                                if !ev.location.is_empty() && r.h >= 52.0 {
                                    text(p, theme, &ev.location, 11.0, Weight::Regular, Align::Leading, Rect::new(tx.x, tx.y + 31.0, tx.w, 15.0), ink.alpha(0.85));
                                }
                            }
                        });
                    });
                    hits.push((r, hit));
                }
            }
            // Now: a red line across today with the time in the gutter.
            if let Some(i) = dates.iter().position(|d| *d == today) {
                let t = Local::now();
                let ny = top + (t.hour() as f32 + t.minute() as f32 / 60.0) * HOUR_H;
                let red = Color::rgba(255, 59, 48, 1.0);
                p.line(col_x(0), ny, area.right(), ny, red.alpha(0.35), 1.0);
                p.line(col_x(i), ny, col_x(i) + cw, ny, red, 1.8);
                p.fill_circle(col_x(i), ny, 4.0, red);
                let label = t.format("%-I:%M").to_string();
                let lw = measure(p, theme, &label, 11.0, Weight::SemiBold) + 10.0;
                let pill = Rect::new(area.x + GUTTER - 8.0 - lw, ny - 9.0, lw, 18.0);
                p.fill_round(pill, 9.0, red);
                text(p, theme, &label, 11.0, Weight::SemiBold, Align::Center, pill, Color::WHITE);
            }
        });
    }

    fn paint_year(&self, p: &Painter, theme: &Theme, area: Rect, hits: &mut Vec<(Rect, Hit)>) {
        let sub = theme.text_secondary;
        let accent = theme.accent;
        let year = self.cursor.year();
        let today = Local::now().date_naive();
        let cols = if area.w >= 1000.0 { 4 } else if area.w >= 640.0 { 3 } else { 2 };
        let rows = 12 / cols;
        let (mw, mh) = (area.w / cols as f32, area.h / rows as f32);
        let initials = self.dow_names(1);
        for m in 0..12u32 {
            let Some(first) = NaiveDate::from_ymd_opt(year, m + 1, 1) else { continue };
            let box_ = Rect::new(area.x + (m as usize % cols) as f32 * mw, area.y + (m as usize / cols) as f32 * mh, mw, mh).inset(16.0, 6.0);
            let name = first.format("%B").to_string();
            let current = today.year() == year && today.month() == m + 1;
            let name_r = Rect::new(box_.x + 2.0, box_.y, measure(p, theme, &name, 16.0, Weight::Bold) + 8.0, 26.0);
            let hit = Hit::Month(m as u8);
            let t = self.hv(hit);
            let c = if current { accent } else { theme.text.mix(accent, 0.5 * t) };
            text(p, theme, &name, 16.0, Weight::Bold, Align::Leading, name_r, c);
            hits.push((name_r, hit));
            let cw = box_.w / 7.0;
            let ch = ((box_.h - 26.0 - 16.0) / 6.0).max(10.0);
            let rad = (cw.min(ch) / 2.0 - 1.0).clamp(5.0, 13.0);
            let small = (rad * 0.95).clamp(9.0, 12.0);
            for (i, n) in initials.iter().enumerate() {
                text(p, theme, n, 9.5, Weight::SemiBold, Align::Center, Rect::new(box_.x + i as f32 * cw, box_.y + 26.0, cw, 14.0), sub);
            }
            let (start, _) = self.month_grid(first);
            for i in 0..42 {
                let d = start + Duration::days(i as i64);
                if d.month() != m + 1 {
                    continue;
                }
                let cell = Rect::new(box_.x + (i % 7) as f32 * cw, box_.y + 42.0 + (i / 7) as f32 * ch, cw, ch);
                let (cx, cy) = (cell.cx(), cell.cy());
                let hit = Hit::Day(days(d));
                let n = self.day_events(d).len();
                if d == today {
                    p.fill_circle(cx, cy, rad, accent);
                } else if d == self.selected {
                    p.stroke_circle(cx, cy, rad - 0.5, accent, 1.5);
                }
                if n > 0 && d != today {
                    // Busier days glow stronger.
                    p.fill_circle(cx, cy, rad, accent.alpha((0.1 + 0.07 * n as f32).min(0.38)));
                }
                let t = self.hv(hit);
                if t > 0.0 && d != today {
                    p.fill_circle(cx, cy, rad, theme.text.alpha(0.08 * t));
                }
                let col = if d == today { Color::WHITE } else if Self::weekend(d) { sub } else { theme.text };
                text(p, theme, &d.day().to_string(), small, if d == today { Weight::SemiBold } else { Weight::Regular }, Align::Center, cell, col);
                hits.push((cell, hit));
            }
        }
    }

    fn paint_popover(&self, p: &Painter, theme: &Theme, body: Rect, hits: &mut Vec<(Rect, Hit)>) {
        let Some(pop) = &self.popover else { return };
        let Some(e) = self.events.get(pop.event) else { return };
        let t = self.pop_t.value.clamp(0.0, 1.2);
        if t <= 0.01 {
            return;
        }
        let sub = theme.text_secondary;
        let date = local_date(e.start);
        let when = if e.all_day {
            let last = local_date((e.end - 1).max(e.start));
            if last == date {
                format!("{}  \u{00B7}  All-day", date.format("%A, %-d %B %Y"))
            } else {
                format!("{} \u{2013} {}  \u{00B7}  All-day", date.format("%-d %b"), last.format("%-d %b %Y"))
            }
        } else {
            format!("{}\n{} \u{2013} {}", date.format("%A, %-d %B %Y"), clock(e.start), clock(e.end))
        };
        let lines = if e.all_day { 1.0 } else { 2.0 };
        let has_loc = !e.location.is_empty();
        let has_link = !e.link.is_empty();
        let h = 16.0 + 24.0 + if has_loc { 18.0 } else { 0.0 } + 10.0 + lines * 18.0 + 10.0 + 20.0 + if has_link { 38.0 } else { 0.0 } + 14.0;
        let a = pop.anchor;
        let right = a.right() + 10.0 + POP_W <= body.right() - 8.0;
        let x = if right { a.right() + 10.0 } else { (a.x - 10.0 - POP_W).max(body.x + 8.0) };
        let y = (a.cy() - 40.0).clamp(body.y + 8.0, (body.bottom() - h - 8.0).max(body.y + 8.0));
        let r = Rect::new(x, y, POP_W, h);
        let origin_x = if right { r.x } else { r.right() };
        let bg = if theme.dark { Color::rgba(44, 44, 46, 0.985) } else { Color::rgba(252, 252, 253, 0.985) };
        p.faded_in(r.inset(-40.0, -40.0), t.min(1.0), || {
            p.scaled_about(origin_x, a.cy().clamp(r.y, r.bottom()), lerp(0.9, 1.0, t), || {
                p.soft_shadow(r, 12.0, 10, 1.8, 0.04, 6.0, theme.shadow);
                p.fill_round(r, 12.0, bg);
                p.stroke_round(r.inset(0.25, 0.25), 12.0, theme.menu_border, 0.5);
                let color = rgb(e.color);
                let inner = r.inset(16.0, 0.0);
                let mut y = r.y + 14.0;
                p.fill_round(Rect::new(inner.x, y + 3.0, 4.0, 20.0), 2.0, color);
                text(p, theme, &e.title, 16.0, Weight::Bold, Align::Leading, Rect::new(inner.x + 12.0, y, inner.w - 12.0, 26.0), theme.text);
                y += 26.0;
                if has_loc {
                    text(p, theme, &e.location, 12.0, Weight::Regular, Align::Leading, Rect::new(inner.x + 12.0, y, inner.w - 12.0, 18.0), sub);
                    y += 18.0;
                }
                y += 8.0;
                p.line(inner.x, y, inner.right(), y, theme.menu_separator, 1.0);
                y += 8.0;
                for (k, line) in when.lines().enumerate() {
                    text(p, theme, line, 12.5, Weight::Regular, Align::Leading, Rect::new(inner.x, y + k as f32 * 18.0, inner.w, 18.0), theme.text);
                }
                y += lines * 18.0 + 8.0;
                p.fill_circle(inner.x + 5.0, y + 10.0, 4.5, color);
                text(p, theme, &e.calendar, 12.5, Weight::Regular, Align::Leading, Rect::new(inner.x + 16.0, y, inner.w - 16.0, 20.0), sub);
                y += 20.0;
                hits.push((r, Hit::Popover));
                if has_link {
                    y += 8.0;
                    let b = Rect::new(inner.x, y, inner.w, 28.0);
                    let lt = self.hv(Hit::PopOpen);
                    p.fill_round(b, 7.0, theme.accent.alpha(0.12 + 0.1 * lt));
                    text(p, theme, "Open in Browser", 12.5, Weight::SemiBold, Align::Center, b, theme.accent);
                    hits.push((b, Hit::PopOpen));
                }
            })
        });
    }
}

impl Drop for CalendarApp {
    fn drop(&mut self) {
        if self.fullscreen.is_some() {
            finder::mark_fullscreen(self.hwnd, false);
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(title: &str, day: NaiveDate, from: (u32, u32), to: (u32, u32), color: [u8; 3], cal: &str) -> Event {
        let at = |(h, m): (u32, u32)| midnight(day) + (h * 3600 + m * 60) as i64;
        Event {
            title: title.into(),
            location: if title.contains("Lunch") { "Café Macs".into() } else { String::new() },
            start: at(from),
            end: at(to),
            all_day: false,
            calendar: cal.into(),
            color,
            link: if title.contains("Standup") { "https://example.com".into() } else { String::new() },
            account: 1,
        }
    }

    fn all_day(title: &str, day: NaiveDate, n: i64, color: [u8; 3], cal: &str) -> Event {
        Event { title: title.into(), location: String::new(), start: midnight(day), end: midnight(day + Duration::days(n)), all_day: true, calendar: cal.into(), color, link: String::new(), account: 1 }
    }

    /// Screenshot Mode's sidebar shows no account or calendar name of the user's,
    /// neither listed nor kept from before.
    #[test]
    fn screenshot_mode_hides_calendar_names() {
        let today = Local::now().date_naive();
        let accounts = vec![calsync::AccountView {
            id: 7,
            provider: calsync::Provider::Google,
            method: calsync::Method::SignIn,
            name: "jane.doe@example.com".into(),
            status: calsync::Status::Ok { calendars: vec![calsync::Calendar { name: "Therapy".into(), color: [1, 2, 3] }], at: 0 },
        }];
        let mut real = ev("Private", today, (9, 0), (10, 0), [1, 2, 3], "Side project");
        real.account = 7;
        let shown = cal_rows(&accounts, &[real], Vec::new(), false);
        assert!(shown.iter().any(|c| c.name == "Therapy") && shown.iter().any(|c| c.name == "Side project"));
        // Switching on drops what was kept (reload passes nothing kept then).
        let demo = cal_rows(&accounts, &demo_events(), Vec::new(), true);
        assert!(!demo.is_empty());
        for c in &demo {
            assert!(!["Therapy", "Side project"].contains(&c.name.as_str()) && !c.group.contains("example.com"), "{c:?}");
        }
    }

    /// Renders each view, light and dark, into the folder named by
    /// `DOCKSIDE_SHOTS`, for a look at the design without opening the window.
    #[test]
    #[ignore]
    fn shots() {
        use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        let Ok(dir) = std::env::var("DOCKSIDE_SHOTS") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let _ = crate::win::register_class(Some(proc_));
        let gfx = Gfx::new().unwrap();
        let today = Local::now().date_naive();
        let (blue, orange, green, purple) = ([0, 122, 255], [255, 149, 0], [52, 199, 89], [175, 82, 222]);
        let mut events = vec![
            all_day("Conference", today + Duration::days(2), 3, purple, "Work"),
            all_day("Mum's birthday", today - Duration::days(4), 1, orange, "Family"),
            ev("Standup", today, (9, 30), (9, 45), blue, "Work"),
            ev("Design review", today, (10, 0), (11, 30), blue, "Work"),
            ev("1:1 with Sam", today, (11, 0), (11, 30), green, "Home"),
            ev("Lunch with Alex", today, (12, 30), (13, 30), orange, "Family"),
            ev("Gym", today + Duration::days(1), (18, 0), (19, 0), green, "Home"),
            ev("Dentist", today - Duration::days(1), (15, 0), (16, 0), orange, "Family"),
        ];
        for i in [3, 5, 9, 12, 17, 23] {
            events.push(ev("Team sync", today + Duration::days(i - 10), (14, 0), (15, 0), blue, "Work"));
        }
        for i in 0..5 {
            events.push(ev(&format!("Busy thing {i}"), today + Duration::days(6), (8 + i, 0), (9 + i, 0), [blue, green, orange][i as usize % 3], "Work"));
        }
        for dark in [false, true] {
            let mut cfg = crate::config::defaults();
            cfg.general.appearance = if dark { crate::config::Appearance::Dark } else { crate::config::Appearance::Light };
            cfg.finder.sidebar = crate::config::Backdrop::None;
            let theme = Theme::new(&cfg, &gfx);
            let mut c = CalendarApp::create(&gfx, &theme, &cfg, &Monitor::primary(), false).unwrap();
            c.shown = true;
            c.has_accounts = true;
            let shot = |c: &mut CalendarApp, name: &str| {
                c.events = events.clone();
                c.cals = ["Work", "Home", "Family"].iter().zip([blue, green, orange]).map(|(n, col)| CalRow { group: "iCloud".into(), account: 1, name: n.to_string(), color: col }).collect();
                c.index();
                for _ in 0..40 {
                    c.tick(0.05);
                }
                c.redraw();
                c.render(&gfx, &theme);
                let canvas = c.canvas.borrow();
                let look = if dark { "dark" } else { "light" };
                crate::gfx::write_png(&gfx, canvas.width as u32, canvas.height as u32, &canvas.pixels(), &format!("{dir}/calendar-{look}-{name}.png"));
            };
            for (view, name) in VIEWS {
                c.go(today, Some(view));
                c.scroll.jump(8.0 * HOUR_H);
                shot(&mut c, name);
                // Twice: the first frame lays out (and the hit rects) for the next.
                shot(&mut c, name);
            }
            c.go(today, Some(View::Week));
            c.scroll.jump(8.0 * HOUR_H);
            shot(&mut c, "week");
            if let Some(i) = c.events.iter().position(|e| e.title == "Design review") {
                if let Some(r) = c.hit_rect(Hit::Event(i as u32)) {
                    c.pressed = Some(Hit::Event(i as u32));
                    let _ = c.on_up(r.cx(), r.cy());
                    shot(&mut c, "popover");
                }
            }
        }
    }

    /// Frames drawn while idle and after a hover, and what each costs, at the
    /// maximized size. `cargo test --profile fast bench_calendar -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_calendar() {
        use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let _ = crate::win::register_class(Some(proc_));
        let gfx = Gfx::new().unwrap();
        let mut cfg = crate::config::defaults();
        cfg.finder.sidebar = crate::config::Backdrop::None;
        let theme = Theme::new(&cfg, &gfx);
        let mut c = CalendarApp::create(&gfx, &theme, &cfg, &Monitor::primary(), false).unwrap();
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(c.hwnd, &mut rc);
            let max = FinderWindow::screen_rect(&rc, false);
            let _ = SetWindowPos(c.hwnd, HWND::default(), max.left, max.top, max.right - max.left, max.bottom - max.top, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        c.shown = true;
        let today = Local::now().date_naive();
        c.events = (0..30).map(|i| ev(&format!("Thing {i}"), today + Duration::days(i % 20 - 10), (9 + (i % 8) as u32, 0), (10 + (i % 8) as u32, 0), [0, 122, 255], "Work")).collect();
        c.index();
        // Settles, counting frames; returns (frames, pixels drawn, ms drawing).
        let run = |c: &mut CalendarApp, ticks: u32| {
            let (mut frames, mut px, mut ms) = (0, 0u64, 0.0f64);
            for _ in 0..ticks {
                c.tick(1.0 / 60.0);
                if c.dirty {
                    let t = Instant::now();
                    c.render(&gfx, &theme);
                    ms += t.elapsed().as_secs_f64() * 1000.0;
                    frames += 1;
                    px += c.last_px;
                }
            }
            (frames, px, ms)
        };
        for (view, name) in VIEWS {
            c.go(today, Some(view));
            run(&mut c, 120);
            let canvas_px = { let cv = c.canvas.borrow(); cv.width as u64 * cv.height as u64 };
            let idle = run(&mut c, 120);
            // Hover across a few things, then let it settle.
            let targets: Vec<Rect> = c.hits.iter().filter(|(_, h)| matches!(h, Hit::Day(_) | Hit::Month(_))).map(|(r, _)| *r).take(6).collect();
            let mut hover = (0, 0u64, 0.0);
            for r in targets {
                c.on_move(r.cx(), r.cy());
                let (f, p, m) = run(&mut c, 30);
                hover = (hover.0 + f, hover.1 + p, hover.2 + m);
            }
            // A touchpad scroll (it lands at once), then a hover: no stale strip.
            let x = c.page_rect().cx();
            c.on_wheel(x, -40, false);
            run(&mut c, 2);
            if let Some(r) = c.hits.iter().find(|(_, h)| matches!(h, Hit::Event(_))).map(|(r, _)| *r) {
                c.on_move(r.cx(), r.cy());
                run(&mut c, 3);
            }
            // Stop mid-fade, then compare against a full repaint of the same state.
            if let Some(r) = c.hits.iter().find(|(_, h)| matches!(h, Hit::Day(_))).map(|(r, _)| *r) {
                c.on_move(r.cx(), r.cy());
                run(&mut c, 4);
            }
            let partial = c.canvas.borrow().pixels();
            c.redraw();
            c.render(&gfx, &theme);
            let full = c.canvas.borrow().pixels();
            let worst = partial.iter().zip(&full).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
            assert!(worst <= 2, "{name}: partial frames left stale pixels (off by {worst})");
            println!(
                "{name}: idle {} frames; hover {} frames, {:.0}% of the window each, {:.2} ms each",
                idle.0,
                hover.0,
                100.0 * hover.1 as f64 / (hover.0.max(1) as u64 * canvas_px) as f64,
                hover.2 / hover.0.max(1) as f64
            );
        }
    }
}
