//! The calendar under the menu bar clock: a month you can page through, with
//! dots for days that have events, and the chosen day's events below, from
//! every account in System Settings > Calendars.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;

use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};

use super::{Content, Ctx, DropEvent, Hit, PAD};
use crate::calsync::{self, Event};
use crate::config::Config;
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::menu::Action;

const W: f32 = 300.0;
const HEADER_H: f32 = 40.0;
const DOW_H: f32 = 20.0;
const CELL_H: f32 = 34.0;
const TIMED_H: f32 = 40.0;
const ALL_DAY_H: f32 = 26.0;
const LINK_H: f32 = 28.0;
/// Events listed for a day before "N more".
const MAX_LIST: usize = 6;

pub struct CalendarMenu {
    /// First of the month shown.
    month: NaiveDate,
    selected: NaiveDate,
    /// Monday (true) or Sunday first.
    monday: bool,
    /// Events overlapping the six weeks shown.
    events: Vec<Event>,
    gen: u64,
    accounts: usize,
}

fn days(d: NaiveDate) -> i32 {
    (d - NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default()).num_days() as i32
}

fn from_days(n: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default() + Duration::days(n as i64)
}

fn midnight(d: NaiveDate) -> i64 {
    crate::calsync::google::local_midnight(d)
}

fn local_date(t: i64) -> NaiveDate {
    Local.timestamp_opt(t, 0).single().map(|d| d.date_naive()).unwrap_or_default()
}

fn clock(t: i64) -> String {
    Local.timestamp_opt(t, 0).single().map(|d| d.format("%-I:%M %p").to_string()).unwrap_or_default()
}

fn first_of(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap_or(d)
}

fn add_months(d: NaiveDate, n: i32) -> NaiveDate {
    let m0 = d.year() * 12 + d.month0() as i32 + n;
    NaiveDate::from_ymd_opt(m0.div_euclid(12), m0.rem_euclid(12) as u32 + 1, 1).unwrap_or(d)
}

impl CalendarMenu {
    pub fn new(cfg: &Config) -> Self {
        let today = Local::now().date_naive();
        let monday = match cfg.calendar.week_starts.to_ascii_lowercase().as_str() {
            "monday" => true,
            "sunday" => false,
            // Windows: 0 = Monday … 6 = Sunday.
            _ => crate::win::system::intl_string("iFirstDayOfWeek").map(|v| v.trim() == "0").unwrap_or(false),
        };
        let mut m = Self { month: first_of(today), selected: today, monday, events: Vec::new(), gen: u64::MAX, accounts: 0 };
        m.reload();
        m
    }

    /// The first day in the grid (the week holding the 1st).
    fn grid_start(&self) -> NaiveDate {
        let wd = self.month.weekday();
        let back = if self.monday { wd.num_days_from_monday() } else { wd.num_days_from_sunday() };
        self.month - Duration::days(back as i64)
    }

    fn weeks(&self) -> usize {
        let start = self.grid_start();
        let next = add_months(self.month, 1);
        (((next - start).num_days() + 6) / 7) as usize
    }

    fn reload(&mut self) {
        let gen = calsync::gen();
        let start = self.grid_start();
        let from = midnight(start);
        let to = midnight(start + Duration::days(42));
        self.events = if crate::screenshot_mode::on() { demo_events() } else { calsync::events_between(from, to) };
        self.accounts = calsync::snapshot().accounts.len();
        self.gen = gen;
    }

    fn day_events(&self, d: NaiveDate) -> Vec<(usize, &Event)> {
        let (from, to) = (midnight(d), midnight(d + Duration::days(1)));
        self.events.iter().enumerate().filter(|(_, e)| e.start < to && e.end.max(e.start + 1) > from).collect()
    }

    fn list_height(&self) -> f32 {
        let list = self.day_events(self.selected);
        if list.is_empty() {
            return 30.0;
        }
        let shown = list.iter().take(MAX_LIST);
        let h: f32 = shown.map(|(_, e)| if e.all_day { ALL_DAY_H } else { TIMED_H }).sum();
        h + if list.len() > MAX_LIST { 22.0 } else { 0.0 }
    }

    fn set_month(&mut self, m: NaiveDate) {
        self.month = first_of(m);
        self.reload();
    }
}

/// Screenshot Mode's placeholder day.
fn demo_events() -> Vec<Event> {
    crate::screenshot_mode::events()
        .into_iter()
        .enumerate()
        .map(|(i, e)| Event::from_cal(e, false, "Work", [[0, 122, 255], [255, 149, 0], [52, 199, 89]][i % 3], 0))
        .collect()
}

impl Content for CalendarMenu {
    fn width(&self) -> f32 {
        W
    }

    fn key(&self, h: &mut DefaultHasher) {
        (self.month, self.selected, self.gen, self.accounts, self.events.len(), Local::now().date_naive()).hash(h);
    }

    fn height(&self, _gfx: &Gfx, _font: &str) -> f32 {
        let grid = 8.0 + HEADER_H + DOW_H + self.weeks() as f32 * CELL_H + 8.0;
        let list = 1.0 + 10.0 + 20.0 + self.list_height() + 6.0;
        let links = 1.0 + 6.0 + LINK_H * if self.accounts == 0 { 1.0 } else { 2.0 } + 8.0;
        grid + list + links
    }

    fn draw(&self, p: &Painter, ctx: &Ctx, hits: &mut Vec<(Rect, Hit)>) -> f32 {
        let pal = ctx.pal;
        let inner = W - 2.0 * PAD;
        let today = Local::now().date_naive();
        let mut y = 8.0;

        // "November 2026" with ‹ • › on the right.
        let month = self.month.format("%B").to_string();
        let mw = ctx.measure(p, &month, 16.0, Weight::Bold);
        ctx.text(p, &month, 16.0, Weight::Bold, Align::Leading, Rect::new(PAD, y, inner, HEADER_H), pal.text);
        ctx.text(p, &self.month.format("%Y").to_string(), 16.0, Weight::Regular, Align::Leading, Rect::new(PAD + mw + 5.0, y, 80.0, HEADER_H), pal.sub);
        let glyphs = p.gfx.format("Segoe Fluent Icons", 10.0, Weight::Regular, Align::Center);
        for (i, (glyph, hit)) in [("\u{E76B}", Hit::Prev), ("", Hit::Today), ("\u{E76C}", Hit::Next)].iter().enumerate() {
            let r = Rect::new(W - PAD - 84.0 + i as f32 * 28.0, y + 8.0, 26.0, 24.0);
            if ctx.hover == Some(*hit) {
                p.fill_round(r, 7.0, pal.chip);
            }
            if *hit == Hit::Today {
                p.fill_circle(r.cx(), r.cy(), 3.5, pal.sub);
            } else {
                p.text(glyph, &glyphs, r, pal.text);
            }
            hits.push((r, *hit));
        }
        y += HEADER_H;

        // Weekday initials.
        let cw = inner / 7.0;
        let names = if self.monday { ["M", "T", "W", "T", "F", "S", "S"] } else { ["S", "M", "T", "W", "T", "F", "S"] };
        for (i, n) in names.iter().enumerate() {
            ctx.text(p, n, 10.5, Weight::SemiBold, Align::Center, Rect::new(PAD + i as f32 * cw, y, cw, DOW_H), pal.sub);
        }
        y += DOW_H;

        // The days.
        let start = self.grid_start();
        for i in 0..self.weeks() * 7 {
            let d = start + Duration::days(i as i64);
            let (col, row) = (i % 7, i / 7);
            let cell = Rect::new(PAD + col as f32 * cw, y + row as f32 * CELL_H, cw, CELL_H);
            let hit = Hit::Day(days(d));
            let circle = (cell.cx(), cell.y + 14.0, 12.5);
            let in_month = d.month() == self.month.month();
            let is_today = d == today;
            let selected = d == self.selected;
            if is_today {
                p.fill_circle(circle.0, circle.1, circle.2, ctx.accent);
            } else if selected {
                p.fill_circle(circle.0, circle.1, circle.2, pal.chip.alpha((pal.chip.a * 1.8).min(1.0)));
            } else if ctx.hover == Some(hit) {
                p.fill_circle(circle.0, circle.1, circle.2, pal.chip);
            }
            let c = if is_today {
                Color::WHITE
            } else if !in_month {
                pal.sub.alpha(pal.sub.a * 0.6)
            } else {
                pal.text
            };
            let weight = if is_today || selected { Weight::SemiBold } else { Weight::Regular };
            ctx.text(p, &d.day().to_string(), 12.5, weight, Align::Center, Rect::new(cell.x, cell.y + 1.5, cell.w, 25.0), c);
            // Up to three dots: one per calendar with something that day.
            let mut colors: Vec<[u8; 3]> = Vec::new();
            for (_, e) in self.day_events(d) {
                if !colors.contains(&e.color) {
                    colors.push(e.color);
                }
            }
            let n = colors.len().min(3) as f32;
            for (k, rgb) in colors.iter().take(3).enumerate() {
                let dx = (k as f32 - (n - 1.0) / 2.0) * 5.5;
                let dot = Color::rgba(rgb[0], rgb[1], rgb[2], if in_month { 1.0 } else { 0.5 });
                p.fill_circle(cell.cx() + dx, cell.y + 30.0, 1.9, dot);
            }
            hits.push((cell, hit));
        }
        y += self.weeks() as f32 * CELL_H + 8.0;

        // The chosen day's events.
        ctx.separator(p, y, W);
        y += 1.0 + 10.0;
        let title = if self.selected == today {
            format!("Today \u{00B7} {}", self.selected.format("%A, %-d %B"))
        } else if self.selected == today + Duration::days(1) {
            format!("Tomorrow \u{00B7} {}", self.selected.format("%A, %-d %B"))
        } else {
            self.selected.format("%A, %-d %B %Y").to_string()
        };
        ctx.text(p, &title, 11.5, Weight::SemiBold, Align::Leading, Rect::new(PAD, y, inner, 18.0), pal.sub);
        y += 20.0;
        let list = self.day_events(self.selected);
        if list.is_empty() {
            let note = if self.accounts == 0 && !crate::screenshot_mode::on() { "No calendars added yet" } else { "No Events" };
            ctx.text(p, note, 13.0, Weight::Regular, Align::Leading, Rect::new(PAD, y, inner, 30.0), pal.sub);
            y += 30.0;
        }
        for (i, e) in list.iter().take(MAX_LIST) {
            let h = if e.all_day { ALL_DAY_H } else { TIMED_H };
            let row = Rect::new(PAD, y, inner, h);
            let hit = Hit::Event(*i as u16);
            if !e.link.is_empty() {
                ctx.highlight(p, Rect::new(6.0, y, W - 12.0, h), hit);
                hits.push((Rect::new(6.0, y, W - 12.0, h), hit));
            }
            let color = Color::rgba(e.color[0], e.color[1], e.color[2], 1.0);
            let past = e.end <= calsync::now() && !e.all_day;
            let fade = |c: Color| if past { c.alpha(c.a * 0.5) } else { c };
            p.fill_round(Rect::new(row.x, row.y + 5.0, 3.5, h - 10.0), 1.75, fade(color));
            let text_x = row.x + 12.0;
            if e.all_day {
                let tw = 56.0;
                ctx.text(p, &e.title, 13.0, Weight::Regular, Align::Leading, Rect::new(text_x, row.y, row.w - 12.0 - tw, h), fade(pal.text));
                ctx.text(p, "All-day", 11.0, Weight::Regular, Align::Trailing, Rect::new(row.x, row.y, row.w, h), pal.sub);
            } else {
                ctx.text(p, &e.title, 13.0, Weight::Regular, Align::Leading, Rect::new(text_x, row.y + 3.0, row.w - 12.0, 19.0), fade(pal.text));
                let mut detail = format!("{} \u{2013} {}", clock(e.start), clock(e.end));
                if local_date(e.start) != self.selected {
                    detail = format!("Until {}", clock(e.end));
                }
                if !e.location.is_empty() {
                    detail.push_str(" \u{00B7} ");
                    detail.push_str(&e.location);
                }
                ctx.text(p, &detail, 11.0, Weight::Regular, Align::Leading, Rect::new(text_x, row.y + 21.0, row.w - 12.0, 16.0), pal.sub);
            }
            y += h;
        }
        if list.len() > MAX_LIST {
            let more = format!("{} more", list.len() - MAX_LIST);
            ctx.text(p, &more, 11.0, Weight::Regular, Align::Leading, Rect::new(PAD + 12.0, y, inner, 22.0), pal.sub);
            y += 22.0;
        }
        y += 6.0;

        // Accounts.
        ctx.separator(p, y, W);
        y += 1.0 + 6.0;
        if self.accounts == 0 {
            ctx.link_row(p, hits, Rect::new(PAD, y, inner, LINK_H), "Add Calendar Account\u{2026}", Hit::Link(0));
            y += LINK_H;
        } else {
            ctx.link_row(p, hits, Rect::new(PAD, y, inner, LINK_H), "Add Account\u{2026}", Hit::Link(0));
            y += LINK_H;
            ctx.link_row(p, hits, Rect::new(PAD, y, inner, LINK_H), "Calendar Settings\u{2026}", Hit::Link(1));
            y += LINK_H;
        }
        y + 8.0
    }

    fn click(&mut self, hit: Hit) -> DropEvent {
        match hit {
            Hit::Prev => self.set_month(add_months(self.month, -1)),
            Hit::Next => self.set_month(add_months(self.month, 1)),
            Hit::Today => {
                let today = Local::now().date_naive();
                self.selected = today;
                self.set_month(today);
            }
            Hit::Day(n) => {
                let d = from_days(n);
                self.selected = d;
                if d.month() != self.month.month() || d.year() != self.month.year() {
                    self.set_month(d);
                }
            }
            Hit::Event(i) => {
                if let Some(e) = self.events.get(i as usize).filter(|e| !e.link.is_empty()) {
                    return DropEvent::Run(Action::Open(e.link.clone()));
                }
            }
            Hit::Link(0) => {
                calsync::setup(Some(calsync::Step::Pick));
                return DropEvent::Run(Action::OpenSettings(Some(crate::settings::Page::Calendars)));
            }
            Hit::Link(_) => return DropEvent::Run(Action::OpenSettings(Some(crate::settings::Page::Calendars))),
            _ => {}
        }
        DropEvent::None
    }

    fn wheel(&mut self, delta: i32) -> bool {
        self.set_month(add_months(self.month, if delta > 0 { -1 } else { 1 }));
        true
    }

    fn key_down(&mut self, vk: u16) -> bool {
        const LEFT: u16 = 0x25;
        const UP: u16 = 0x26;
        const RIGHT: u16 = 0x27;
        const DOWN: u16 = 0x28;
        let step = match vk {
            LEFT => -1,
            RIGHT => 1,
            UP => -7,
            DOWN => 7,
            _ => return false,
        };
        self.selected += Duration::days(step);
        if first_of(self.selected) != self.month {
            self.set_month(self.selected);
        }
        true
    }

    fn update(&mut self) {
        if calsync::gen() != self.gen {
            self.reload();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months() {
        let d = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        assert_eq!(add_months(d, -1), NaiveDate::from_ymd_opt(2025, 12, 1).unwrap());
        assert_eq!(add_months(d, 13), NaiveDate::from_ymd_opt(2027, 2, 1).unwrap());
        assert_eq!(from_days(days(d)), d);
    }
}
