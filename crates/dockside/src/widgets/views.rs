//! What's drawn inside each widget. Every function paints into the widget's
//! body rect (in surface points) and records the rects it reacts to.

use std::f32::consts::TAU;

use chrono::{Datelike, Local, NaiveDate, Timelike};
use windows::Win32::Graphics::Direct2D::Common::D2D1_FIGURE_BEGIN_FILLED;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::DirectWrite::*;

use super::sources::{self, WeatherState, Zone};
use super::store::Store;
use super::{glyphs, Data, Edit, Hit, RADIUS};
use crate::config::{WidgetItem, WidgetKind, WidgetSize};
use crate::geom::{Color, Rect};
use crate::gfx::{pt, Align, Gfx, Painter, Weight};
use crate::island::calendar::CalEvent;
use crate::island::glyphs as ig;
use crate::theme::Theme;

const PAD: f32 = 16.0;
const ORANGE: Color = Color::rgba(255, 149, 0, 1.0);
const GREEN: Color = Color::rgba(52, 199, 89, 1.0);
const PURPLE: Color = Color::rgba(191, 90, 242, 1.0);
const NOTES_YELLOW: Color = Color::rgba(230, 168, 0, 1.0);
const WHITE: Color = Color::WHITE;
/// Notes are laid out with a trailing zero-width space, so a caret after a
/// final line break lands on the new (empty) line.
const ZWSP: char = '\u{200B}';

pub struct Ctx<'a> {
    pub theme: &'a Theme,
    pub data: &'a Data,
    pub store: &'a Store,
    pub item: &'a WidgetItem,
    pub edit: Option<&'a Edit>,
    pub art: Option<&'a ID2D1Bitmap>,
    pub hover: Option<Hit>,
}

impl Ctx<'_> {
    fn red(&self) -> Color {
        if self.theme.dark { Color::rgba(255, 69, 58, 1.0) } else { Color::rgba(255, 59, 48, 1.0) }
    }
    fn faint(&self) -> Color {
        if self.theme.dark { Color::gray(255, 0.12) } else { Color::gray(0, 0.1) }
    }
}

fn text(p: &Painter, c: &Ctx, s: &str, size: f32, weight: Weight, align: Align, r: Rect, color: Color) {
    let f = p.gfx.format(&c.theme.font, size, weight, align);
    p.text(s, &f, r, color);
}

fn width(p: &Painter, c: &Ctx, s: &str, size: f32, weight: Weight) -> f32 {
    p.gfx.measure(s, &p.gfx.format(&c.theme.font, size, weight, Align::Leading))
}

fn degrees(t: f32) -> String {
    let t = t.round();
    format!("{}°", if t == 0.0 { 0.0 } else { t })
}

pub fn draw(p: &Painter, r: Rect, c: &Ctx, hits: &mut Vec<(Rect, Hit)>) {
    match c.item.kind {
        WidgetKind::Clock => clock(p, r, c),
        WidgetKind::WorldClock => world_clock(p, r, c),
        WidgetKind::Weather => weather(p, r, c),
        WidgetKind::Calendar => calendar(p, r, c),
        WidgetKind::UpNext => up_next(p, r, c),
        WidgetKind::Batteries => batteries(p, r, c),
        WidgetKind::Notes => notes(p, r, c, hits),
        WidgetKind::Reminders => reminders(p, r, c, hits),
        WidgetKind::NowPlaying => now_playing(p, r, c, hits),
        WidgetKind::System => system(p, r, c),
    }
}

// ---------------------------------------------------------------------------
// Clocks
// ---------------------------------------------------------------------------

fn analog(p: &Painter, c: &Ctx, cx: f32, cy: f32, rad: f32, (h, m, s): (u32, u32, u32), label: Option<&str>) {
    let dark = c.theme.dark;
    let face = if dark { Color::rgba(28, 28, 30, 1.0) } else { Color::WHITE };
    let ink = if dark { Color::WHITE } else { Color::rgba(28, 28, 30, 1.0) };
    p.fill_circle(cx, cy, rad, face);
    if !dark {
        p.stroke_circle(cx, cy, rad - 0.25, Color::gray(0, 0.08), 0.5);
    }
    let at = |a: f32, f: f32| (cx + a.sin() * rad * f, cy - a.cos() * rad * f);
    if rad >= 40.0 {
        for i in (0..60).filter(|i| i % 5 != 0) {
            let a = i as f32 * TAU / 60.0;
            let ((x0, y0), (x1, y1)) = (at(a, 0.9), at(a, 0.95));
            p.line(x0, y0, x1, y1, ink.alpha(0.25), 0.8);
        }
    }
    if rad >= 30.0 {
        let fs = rad * 0.24;
        let f = p.gfx.format(&c.theme.font, fs, Weight::Regular, Align::Center);
        for n in 1..=12 {
            let (x, y) = at(n as f32 * TAU / 12.0, 0.75);
            p.text(&n.to_string(), &f, Rect::new(x - fs, y - fs * 0.7, fs * 2.0, fs * 1.4), ink);
        }
    } else {
        for n in 0..12 {
            let a = n as f32 * TAU / 12.0;
            let ((x0, y0), (x1, y1)) = (at(a, 0.78), at(a, 0.9));
            p.line(x0, y0, x1, y1, ink.alpha(0.5), 1.2);
        }
    }
    if let Some(l) = label {
        let r = Rect::new(cx - rad * 0.6, cy - rad * 0.5, rad * 1.2, rad * 0.24);
        text(p, c, &l.to_uppercase(), rad * 0.15, Weight::Bold, Align::Center, r, ink.alpha(0.5));
    }
    let (hf, mf, sf) = ((h % 12) as f32 + m as f32 / 60.0, m as f32 + s as f32 / 60.0, s as f32);
    let hand = |a: f32, len: f32, w: f32, color: Color| {
        let (x, y) = at(a, len);
        p.line(cx, cy, x, y, color, w);
    };
    hand(hf * TAU / 12.0, 0.5, (rad * 0.075).max(2.2), ink);
    hand(mf * TAU / 60.0, 0.78, (rad * 0.05).max(1.6), ink);
    p.fill_circle(cx, cy, (rad * 0.06).max(2.0), ink);
    let a = sf * TAU / 60.0;
    let (tx, ty) = at(a + TAU / 2.0, 0.16);
    let (x, y) = at(a, 0.86);
    p.line(tx, ty, x, y, ORANGE, (rad * 0.02).max(1.0));
    p.fill_circle(cx, cy, (rad * 0.04).max(1.6), ORANGE);
    p.fill_circle(cx, cy, (rad * 0.015).max(0.7), face);
}

fn local_hms() -> (u32, u32, u32) {
    let t = Local::now();
    (t.hour(), t.minute(), t.second())
}

fn clock(p: &Painter, r: Rect, c: &Ctx) {
    let now = Local::now();
    if c.item.size == WidgetSize::Small {
        let rad = r.w.min(r.h) / 2.0 - 12.0;
        return analog(p, c, r.cx(), r.cy(), rad, local_hms(), None);
    }
    let rad = r.h / 2.0 - PAD;
    analog(p, c, r.x + PAD + rad, r.cy(), rad, local_hms(), None);
    let x = r.x + 2.0 * PAD + 2.0 * rad + 8.0;
    let w = r.right() - PAD - x;
    text(p, c, &now.format("%A").to_string().to_uppercase(), 12.0, Weight::Bold, Align::Leading, Rect::new(x, r.y + 30.0, w, 18.0), c.red());
    let (time, ampm) = if c.data.h24 {
        (now.format("%H:%M").to_string(), String::new())
    } else {
        (now.format("%-I:%M").to_string(), now.format("%p").to_string())
    };
    let size = 44.0;
    let tr = Rect::new(x, r.y + 50.0, w, 56.0);
    let tf = p.gfx.format(&c.theme.font, size, Weight::Bold, Align::Leading);
    p.text_tabular(&time, &tf, tr, c.theme.text);
    if !ampm.is_empty() {
        let tw = p.gfx.measure(&time, &tf);
        text(p, c, &ampm, 15.0, Weight::Bold, Align::Leading, Rect::new(x + tw + 4.0, r.y + 72.0, 40.0, 24.0), c.theme.text_secondary);
    }
    text(p, c, &now.format("%B %-d").to_string(), 15.0, Weight::Regular, Align::Leading, Rect::new(x, r.y + 106.0, w, 22.0), c.theme.text_secondary);
}

fn world_clock(p: &Painter, r: Rect, c: &Ctx) {
    let pinned = c.item.zone.as_deref().and_then(sources::parse_zone);
    let zones: Vec<&Zone> = match &pinned {
        Some(z) => std::iter::once(z).chain(c.data.zones.iter()).collect(),
        None => c.data.zones.iter().collect(),
    };
    if zones.is_empty() {
        let msg = "Add time zones in System Settings › Widgets";
        let f = p.gfx.format_block(&c.theme.font, 13.0, Weight::Regular, Align::Center);
        p.text(msg, &f, Rect::new(r.x + PAD, r.cy() - 20.0, r.w - 2.0 * PAD, 40.0), c.theme.text_secondary);
        return;
    }
    match c.item.size {
        WidgetSize::Small => {
            let z = zones[0];
            let (h, m, s, rel) = sources::zone_time(z);
            let rad = 52.0;
            analog(p, c, r.cx(), r.y + 12.0 + rad, rad, (h, m, s), None);
            text(p, c, &z.label, 13.0, Weight::Bold, Align::Center, Rect::new(r.x + 8.0, r.y + 120.0, r.w - 16.0, 18.0), c.theme.text);
            text(p, c, &rel, 11.0, Weight::Regular, Align::Center, Rect::new(r.x + 8.0, r.y + 138.0, r.w - 16.0, 16.0), c.theme.text_secondary);
        }
        WidgetSize::Medium => {
            let n = zones.len().min(4);
            let col = (r.w - 2.0 * 8.0) / n as f32;
            for (i, z) in zones.iter().take(n).enumerate() {
                let (h, m, s, rel) = sources::zone_time(z);
                let cx = r.x + 8.0 + col * (i as f32 + 0.5);
                analog(p, c, cx, r.y + 58.0, 38.0, (h, m, s), None);
                let tr = Rect::new(cx - col / 2.0 + 2.0, r.y + 104.0, col - 4.0, 18.0);
                text(p, c, &z.label, 12.0, Weight::Bold, Align::Center, tr, c.theme.text);
                let day = rel.split(", ").collect::<Vec<_>>();
                for (j, part) in day.iter().enumerate() {
                    let rr = Rect::new(tr.x, tr.bottom() + j as f32 * 14.0, tr.w, 14.0);
                    text(p, c, part, 10.0, Weight::Regular, Align::Center, rr, c.theme.text_secondary);
                }
            }
        }
        // Up to four: big faces, two by two.
        WidgetSize::Large if zones.len() <= 4 => {
            let (cw, ch) = (r.w / 2.0, r.h / 2.0);
            for (i, z) in zones.iter().enumerate() {
                let (h, m, s, rel) = sources::zone_time(z);
                let (cx, top) = (r.x + cw * ((i % 2) as f32 + 0.5), r.y + ch * (i / 2) as f32);
                analog(p, c, cx, top + 72.0, 58.0, (h, m, s), None);
                let tr = Rect::new(cx - cw / 2.0 + 8.0, top + 136.0, cw - 16.0, 18.0);
                text(p, c, &z.label, 13.0, Weight::Bold, Align::Center, tr, c.theme.text);
                text(p, c, &rel, 11.0, Weight::Regular, Align::Center, tr.offset(0.0, 17.0), c.theme.text_secondary);
            }
        }
        WidgetSize::Large => {
            let n = zones.len().min(6);
            let row = (r.h - 2.0 * 10.0) / 6.0;
            for (i, z) in zones.iter().take(n).enumerate() {
                let (h, m, s, rel) = sources::zone_time(z);
                let y = r.y + 10.0 + row * i as f32;
                if i > 0 {
                    p.line(r.x + PAD, y, r.right() - PAD, y, c.faint(), 0.8);
                }
                analog(p, c, r.x + PAD + 20.0, y + row / 2.0, 20.0, (h, m, s), None);
                let x = r.x + PAD + 52.0;
                text(p, c, &z.label, 15.0, Weight::Bold, Align::Leading, Rect::new(x, y + row / 2.0 - 18.0, 170.0, 20.0), c.theme.text);
                text(p, c, &rel, 11.0, Weight::Regular, Align::Leading, Rect::new(x, y + row / 2.0 + 2.0, 170.0, 16.0), c.theme.text_secondary);
                let t = if c.data.h24 {
                    format!("{h:02}:{m:02}")
                } else {
                    format!("{}:{m:02} {}", if h % 12 == 0 { 12 } else { h % 12 }, if h < 12 { "AM" } else { "PM" })
                };
                let f = p.gfx.format(&c.theme.font, 22.0, Weight::Regular, Align::Trailing);
                p.text_tabular(&t, &f, Rect::new(r.right() - PAD - 130.0, y, 130.0, row), c.theme.text);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Weather
// ---------------------------------------------------------------------------

fn sky(code: u8, is_day: bool) -> (Color, Color) {
    let cloudy = matches!(code, 3 | 45 | 48 | 51..=67 | 71..=86 | 95..=99);
    match (is_day, cloudy) {
        (true, false) => (Color::rgba(38, 108, 198, 1.0), Color::rgba(100, 165, 226, 1.0)),
        (true, true) => (Color::rgba(86, 108, 134, 1.0), Color::rgba(132, 150, 172, 1.0)),
        (false, false) => (Color::rgba(14, 22, 52, 1.0), Color::rgba(44, 62, 108, 1.0)),
        (false, true) => (Color::rgba(34, 40, 54, 1.0), Color::rgba(66, 74, 92, 1.0)),
    }
}

fn weather(p: &Painter, r: Rect, c: &Ctx) {
    let w = match &c.data.weather {
        WeatherState::Ready(w) => w,
        other => {
            let (top, bottom) = sky(0, true);
            p.fill_round_gradient(r, RADIUS, top, bottom);
            let msg = match other {
                WeatherState::Failed(e) => e.as_str(),
                _ => "Loading weather…",
            };
            text(p, c, msg, 13.0, Weight::Bold, Align::Center, Rect::new(r.x, r.cy() - 12.0, r.w, 24.0), WHITE.alpha(0.85));
            return;
        }
    };
    let (top, bottom) = sky(w.code, w.is_day);
    p.fill_round_gradient(r, RADIUS, top, bottom);
    let soft = WHITE.alpha(0.75);
    let x = r.x + PAD;
    let inner = r.w - 2.0 * PAD;

    let place_w = width(p, c, &w.place, 14.0, Weight::Bold).min(inner - 16.0);
    text(p, c, &w.place, 14.0, Weight::Bold, Align::Leading, Rect::new(x, r.y + 12.0, place_w + 1.0, 20.0), WHITE);
    if w.located {
        glyphs::location(p, x + place_w + 9.0, r.y + 22.0, 9.0, WHITE);
    }
    text(p, c, &degrees(w.temp), 40.0, Weight::Regular, Align::Leading, Rect::new(x - 2.0, r.y + 30.0, inner, 50.0), WHITE);
    let hl = format!("H:{} L:{}", degrees(w.high), degrees(w.low));
    let cond = sources::condition(w.code, w.is_day);

    if c.item.size == WidgetSize::Small {
        glyphs::weather(p, x + 11.0, r.bottom() - 58.0, 22.0, w.code, w.is_day);
        text(p, c, cond, 12.0, Weight::Bold, Align::Leading, Rect::new(x, r.bottom() - 46.0, inner, 18.0), WHITE);
        text(p, c, &hl, 12.0, Weight::Bold, Align::Leading, Rect::new(x, r.bottom() - 29.0, inner, 18.0), WHITE);
        return;
    }
    let right = Rect::new(r.right() - PAD - 160.0, r.y, 160.0, 0.0);
    glyphs::weather(p, right.right() - 12.0, r.y + 26.0, 24.0, w.code, w.is_day);
    text(p, c, cond, 12.0, Weight::Bold, Align::Trailing, Rect::new(right.x, r.y + 42.0, right.w, 18.0), WHITE);
    text(p, c, &hl, 12.0, Weight::Bold, Align::Trailing, Rect::new(right.x, r.y + 59.0, right.w, 18.0), WHITE);

    // Hourly forecast.
    let n = w.hours.len().max(1);
    let col = inner / n as f32;
    let y = r.y + 88.0;
    for (i, h) in w.hours.iter().enumerate() {
        let cx = x + col * (i as f32 + 0.5);
        let cr = Rect::new(cx - col / 2.0, y, col, 16.0);
        text(p, c, &h.label, 11.0, Weight::Bold, Align::Center, cr, soft);
        glyphs::weather(p, cx, y + 34.0, 20.0, h.code, h.is_day);
        text(p, c, &degrees(h.temp), 13.0, Weight::Bold, Align::Center, cr.offset(0.0, 50.0), WHITE);
    }
    if c.item.size != WidgetSize::Large {
        return;
    }

    // Daily forecast, with each day's range on a bar spanning the week's.
    let sep = r.y + 172.0;
    p.line(x, sep, r.right() - PAD, sep, WHITE.alpha(0.25), 0.8);
    let days: Vec<_> = w.days.iter().take(5).collect();
    let lo = days.iter().map(|d| d.low).fold(f32::MAX, f32::min);
    let hi = days.iter().map(|d| d.high).fold(f32::MIN, f32::max);
    let row = (r.bottom() - 10.0 - sep - 6.0) / 5.0;
    for (i, d) in days.iter().enumerate() {
        let y = sep + 6.0 + row * i as f32;
        let rr = |x: f32, w: f32| Rect::new(x, y, w, row);
        text(p, c, &d.label, 14.0, Weight::Bold, Align::Leading, rr(x, 60.0), WHITE);
        glyphs::weather(p, x + 84.0, y + row / 2.0, 20.0, d.code, true);
        text(p, c, &degrees(d.low), 14.0, Weight::Bold, Align::Trailing, rr(x + 104.0, 40.0), soft);
        text(p, c, &degrees(d.high), 14.0, Weight::Bold, Align::Trailing, rr(r.right() - PAD - 40.0, 40.0), WHITE);
        let bar = Rect::new(x + 156.0, y + row / 2.0 - 2.5, r.right() - PAD - 52.0 - (x + 156.0), 5.0);
        p.fill_round(bar, 2.5, Color::gray(0, 0.18));
        let span = (hi - lo).max(1.0);
        let (a, b) = ((d.low - lo) / span, (d.high - lo) / span);
        let seg = Rect::new(bar.x + bar.w * a, bar.y, (bar.w * (b - a)).max(5.0), bar.h);
        p.fill_round(seg, 2.5, Color::rgba(255, 204, 0, 1.0));
    }
}

// ---------------------------------------------------------------------------
// Calendar and Up Next
// ---------------------------------------------------------------------------

fn month_grid(p: &Painter, r: Rect, c: &Ctx, today: NaiveDate, header: bool, fs: f32) {
    let mut y = r.y;
    if header {
        text(p, c, &today.format("%B").to_string().to_uppercase(), fs, Weight::Bold, Align::Leading, Rect::new(r.x + 4.0, y, r.w, fs + 6.0), c.red());
        y += fs + 8.0;
    }
    let col = r.w / 7.0;
    let first = c.data.first_weekday;
    let mut wd = first;
    for i in 0..7 {
        let letter = wd.to_string().chars().next().unwrap_or(' ').to_string();
        let cr = Rect::new(r.x + col * i as f32, y, col, fs + 4.0);
        text(p, c, &letter, fs * 0.85, Weight::Bold, Align::Center, cr, c.theme.text_secondary);
        wd = wd.succ();
    }
    y += fs + 6.0;
    let weeks = sources::month_weeks(today, first);
    let row = ((r.bottom() - y) / weeks.len() as f32).min(fs * 2.4);
    let num = p.gfx.format(&c.theme.font, fs, Weight::Bold, Align::Center);
    for (wi, week) in weeks.iter().enumerate() {
        for (di, day) in week.iter().enumerate() {
            let Some(d) = day else { continue };
            let cell = Rect::new(r.x + col * di as f32, y + row * wi as f32, col, row);
            let mut ink = c.theme.text;
            if *d == today.day() {
                p.fill_circle(cell.cx(), cell.cy(), (col.min(row) * 0.46).max(fs * 0.8), c.red());
                ink = WHITE;
            }
            p.text_tabular(&d.to_string(), &num, cell, ink);
        }
    }
}

/// "Tomorrow, 9:00 AM – 9:30 AM", or just the span for today.
fn event_when(e: &CalEvent) -> String {
    if e.ongoing() {
        return format!("Now – {}", e.span().rsplit(" – ").next().unwrap_or_default());
    }
    let day = chrono::DateTime::from_timestamp(e.start, 0).map(|t| t.with_timezone(&Local).date_naive());
    match day {
        Some(d) if d != Local::now().date_naive() => format!("{}, {}", d.format("%A"), e.span()),
        _ => e.span(),
    }
}

/// Up to `max` events from `r.y` down; returns the y after the last one.
fn event_list(p: &Painter, r: Rect, c: &Ctx, max: usize) -> f32 {
    let mut y = r.y;
    let events: Vec<_> = c.data.events.iter().take(max).collect();
    if events.is_empty() {
        text(p, c, "No more events today", 12.0, Weight::Regular, Align::Leading, Rect::new(r.x, y, r.w, 18.0), c.theme.text_secondary);
        return y + 18.0;
    }
    for e in events {
        if y + 34.0 > r.bottom() {
            break;
        }
        p.fill_round(Rect::new(r.x, y + 2.0, 3.5, 30.0), 1.75, c.theme.accent);
        text(p, c, &e.title, 13.0, Weight::Bold, Align::Leading, Rect::new(r.x + 10.0, y, r.w - 10.0, 18.0), c.theme.text);
        text(p, c, &event_when(e), 11.0, Weight::Regular, Align::Leading, Rect::new(r.x + 10.0, y + 17.0, r.w - 10.0, 16.0), c.theme.text_secondary);
        y += 40.0;
    }
    y
}

/// Weekday (red) over a big day number.
fn date_block(p: &Painter, x: f32, y: f32, w: f32, c: &Ctx, big: f32) {
    let now = Local::now();
    text(p, c, &now.format("%A").to_string().to_uppercase(), 12.0, Weight::Bold, Align::Leading, Rect::new(x, y, w, 16.0), c.red());
    text(p, c, &now.day().to_string(), big, Weight::Regular, Align::Leading, Rect::new(x - 1.0, y + 14.0, w, big * 1.25), c.theme.text);
}

fn calendar(p: &Painter, r: Rect, c: &Ctx) {
    let today = Local::now().date_naive();
    match c.item.size {
        WidgetSize::Small => month_grid(p, r.inset(14.0, 14.0), c, today, true, 11.0),
        WidgetSize::Medium => {
            let left = Rect::new(r.x + PAD, r.y + 14.0, 150.0, r.h - 28.0);
            date_block(p, left.x, left.y, left.w, c, 36.0);
            event_list(p, Rect::new(left.x, left.y + 62.0, left.w, left.h - 62.0), c, 2);
            month_grid(p, Rect::new(r.x + 182.0, r.y + 14.0, r.right() - 14.0 - (r.x + 182.0), r.h - 28.0), c, today, true, 11.0);
        }
        WidgetSize::Large => {
            month_grid(p, Rect::new(r.x + PAD, r.y + PAD, r.w - 2.0 * PAD, 190.0), c, today, true, 13.0);
            let y = r.y + PAD + 200.0;
            p.line(r.x + PAD, y, r.right() - PAD, y, c.faint(), 0.8);
            event_list(p, Rect::new(r.x + PAD, y + 10.0, r.w - 2.0 * PAD, r.bottom() - y - 20.0), c, 3);
        }
    }
}

fn up_next(p: &Painter, r: Rect, c: &Ctx) {
    match c.item.size {
        WidgetSize::Small => {
            date_block(p, r.x + PAD, r.y + 14.0, r.w - 2.0 * PAD, c, 30.0);
            event_list(p, Rect::new(r.x + PAD, r.y + 70.0, r.w - 2.0 * PAD, r.h - 82.0), c, 2);
        }
        WidgetSize::Medium => {
            date_block(p, r.x + PAD, r.y + 14.0, 110.0, c, 40.0);
            event_list(p, Rect::new(r.x + 140.0, r.y + 18.0, r.right() - PAD - (r.x + 140.0), r.h - 30.0), c, 3);
        }
        WidgetSize::Large => {
            date_block(p, r.x + PAD, r.y + 14.0, r.w - 2.0 * PAD, c, 40.0);
            let y = r.y + 82.0;
            p.line(r.x + PAD, y, r.right() - PAD, y, c.faint(), 0.8);
            event_list(p, Rect::new(r.x + PAD, y + 12.0, r.w - 2.0 * PAD, r.bottom() - y - 22.0), c, 7);
        }
    }
}

// ---------------------------------------------------------------------------
// Batteries
// ---------------------------------------------------------------------------

pub struct Device {
    pub name: String,
    pub percent: u8,
    pub charging: bool,
    pub laptop: bool,
}

fn device_glyph(p: &Painter, d: &Device, cx: f32, cy: f32, s: f32, ink: Color) {
    let n = d.name.to_lowercase();
    if d.laptop {
        glyphs::laptop(p, cx, cy, s, ink);
        return;
    }
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    if has(&["airpods", "buds", "headphone", "headset", "beats", "wh-", "wf-", "jabra", "bose", "earbud", "speaker"]) {
        ig::headphones(p, cx, cy, s, ink);
    } else if has(&["controller", "gamepad", "xbox", "dualsense", "dualshock", "joy-con"]) {
        glyphs::gamepad(p, cx, cy, s, ink);
    } else if has(&["keyboard", "keys"]) {
        glyphs::keyboard(p, cx, cy, s, ink);
    } else if has(&["mouse", "mx master", "trackpad"]) {
        glyphs::mouse(p, cx, cy, s, ink);
    } else {
        glyphs::bluetooth(p, cx, cy, s, ink);
    }
}

fn ring(p: &Painter, c: &Ctx, d: &Device, cx: f32, cy: f32, rad: f32, stroke: f32) {
    let low = d.percent <= 20 && !d.charging;
    let color = if low { c.red() } else { GREEN };
    p.stroke_circle(cx, cy, rad, c.faint(), stroke);
    ig::arc(p, cx, cy, rad, d.percent as f32 / 100.0, color, stroke);
    device_glyph(p, d, cx, cy, rad * 0.8, c.theme.text);
}

fn percent_label(p: &Painter, c: &Ctx, d: &Device, r: Rect, size: f32) {
    let s = format!("{}%", d.percent);
    text(p, c, &s, size, Weight::Bold, Align::Center, r, c.theme.text);
    if d.charging {
        let w = width(p, c, &s, size, Weight::Bold);
        ig::bolt(p, r.cx() + w / 2.0 + size * 0.45, r.cy(), size * 0.8, c.theme.text);
    }
}

fn batteries(p: &Painter, r: Rect, c: &Ctx) {
    let devices = super::devices(c.data);
    if devices.is_empty() {
        ig::battery(p, r.cx() + 14.0, r.cy() - 12.0, 0, c.theme.text_secondary, 1.0);
        text(p, c, "No batteries", 13.0, Weight::Regular, Align::Center, Rect::new(r.x, r.cy() + 4.0, r.w, 20.0), c.theme.text_secondary);
        return;
    }
    match c.item.size {
        WidgetSize::Small if devices.len() == 1 => {
            let d = &devices[0];
            ring(p, c, d, r.cx(), r.y + 66.0, 44.0, 7.0);
            percent_label(p, c, d, Rect::new(r.x, r.y + 120.0, r.w, 30.0), 20.0);
        }
        WidgetSize::Small => {
            for (i, d) in devices.iter().take(4).enumerate() {
                let cx = r.x + r.w * if i % 2 == 0 { 0.28 } else { 0.72 };
                let cy = r.y + if i < 2 { 44.0 } else { 116.0 };
                ring(p, c, d, cx, cy, 25.0, 5.0);
                percent_label(p, c, d, Rect::new(cx - 40.0, cy + 28.0, 80.0, 16.0), 11.0);
            }
        }
        _ => {
            let col = (r.w - 16.0) / 4.0;
            for (i, d) in devices.iter().take(4).enumerate() {
                let cx = r.x + 8.0 + col * (i as f32 + 0.5);
                ring(p, c, d, cx, r.y + 58.0, 32.0, 6.0);
                percent_label(p, c, d, Rect::new(cx - col / 2.0, r.y + 98.0, col, 22.0), 15.0);
                text(p, c, &d.name, 11.0, Weight::Regular, Align::Center, Rect::new(cx - col / 2.0 + 4.0, r.y + 122.0, col - 8.0, 16.0), c.theme.text_secondary);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Notes and Reminders
// ---------------------------------------------------------------------------

/// Where a note's text goes inside the widget body.
pub fn note_area(body: Rect) -> Rect {
    Rect::new(body.x + PAD, body.y + 38.0, body.w - 2.0 * PAD, body.h - 38.0 - 12.0)
}

/// The wrapped layout of a note (first line bold, like a note's title).
pub fn note_layout(gfx: &Gfx, theme: &Theme, chars: &[char], w: f32) -> Option<IDWriteTextLayout> {
    let s: String = chars.iter().chain(std::iter::once(&ZWSP)).collect();
    let f = gfx.format_block(&theme.font, 13.0, Weight::Regular, Align::Leading);
    let layout = gfx.layout(&s, &f, w, 10_000.0)?;
    let title = chars.iter().position(|&ch| ch == '\n').unwrap_or(chars.len());
    let len: u32 = chars[..title].iter().map(|ch| ch.len_utf16() as u32).sum();
    unsafe {
        let _ = layout.SetFontWeight(DWRITE_FONT_WEIGHT_BOLD, DWRITE_TEXT_RANGE { startPosition: 0, length: len });
        // Notes stay readable rather than trailing off with an ellipsis.
        let none = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_NONE, delimiter: 0, delimiterCount: 0 };
        let _ = layout.SetTrimming(&none, None);
    }
    Some(layout)
}

pub fn utf16_index(chars: &[char], i: usize) -> u32 {
    chars[..i.min(chars.len())].iter().map(|ch| ch.len_utf16() as u32).sum()
}

pub fn char_index(chars: &[char], u: u32) -> usize {
    let mut acc = 0;
    for (i, ch) in chars.iter().enumerate() {
        if acc >= u {
            return i;
        }
        acc += ch.len_utf16() as u32;
    }
    chars.len()
}

/// Caret position (x, top, height) in layout coordinates.
pub fn caret_pos(layout: &IDWriteTextLayout, chars: &[char], i: usize) -> (f32, f32, f32) {
    let (mut x, mut y) = (0.0, 0.0);
    let mut m = DWRITE_HIT_TEST_METRICS::default();
    unsafe {
        let _ = layout.HitTestTextPosition(utf16_index(chars, i), false, &mut x, &mut y, &mut m);
    }
    (x, y, m.height.max(15.0))
}

/// Character index nearest a point in layout coordinates.
pub fn index_at(layout: &IDWriteTextLayout, chars: &[char], x: f32, y: f32) -> usize {
    let (mut trailing, mut inside) = (Default::default(), Default::default());
    let mut m = DWRITE_HIT_TEST_METRICS::default();
    unsafe {
        if layout.HitTestPoint(x, y, &mut trailing, &mut inside, &mut m).is_err() {
            return chars.len();
        }
    }
    let i = m.textPosition + if trailing.as_bool() { m.length } else { 0 };
    char_index(chars, i).min(chars.len())
}

fn header(p: &Painter, c: &Ctx, r: Rect, title: &str, color: Color) {
    text(p, c, title, 13.0, Weight::Bold, Align::Leading, Rect::new(r.x + PAD, r.y + 12.0, r.w - 2.0 * PAD, 20.0), color);
}

fn notes(p: &Painter, r: Rect, c: &Ctx, hits: &mut Vec<(Rect, Hit)>) {
    header(p, c, r, "Notes", NOTES_YELLOW);
    let area = note_area(r);
    hits.push((r, Hit::Note));
    let (chars, editing): (Vec<char>, _) = match c.edit {
        Some(e) => (e.field.text.clone(), Some(e)),
        None => (c.store.note(c.item.id).chars().collect(), None),
    };
    if chars.is_empty() && editing.is_none() {
        let f = p.gfx.format_block(&c.theme.font, 13.0, Weight::Regular, Align::Leading);
        p.text("Click to write a note", &f, area, c.theme.text_secondary);
        return;
    }
    let Some(layout) = note_layout(p.gfx, c.theme, &chars, area.w) else { return };
    let scroll = editing.map_or(0.0, |e| e.scroll);
    p.clipped(area, || {
        let (ox, oy) = (area.x, area.y - scroll);
        if let Some(e) = editing {
            let (a, b) = e.field.range();
            if a != b {
                let mut buf = [DWRITE_HIT_TEST_METRICS::default(); 64];
                let mut n = 0;
                let (ua, ub) = (utf16_index(&chars, a), utf16_index(&chars, b));
                unsafe {
                    let _ = layout.HitTestTextRange(ua, ub - ua, ox, oy, Some(&mut buf), &mut n);
                }
                for m in &buf[..(n as usize).min(buf.len())] {
                    p.fill_rect(Rect::new(m.left, m.top, m.width.max(3.0), m.height), c.theme.accent.alpha(0.3));
                }
            }
        }
        p.text_layout(&layout, ox, oy, c.theme.text);
        if let Some(e) = editing {
            let (x, y, h) = caret_pos(&layout, &chars, e.field.caret);
            p.fill_rect(Rect::new(ox + x - 0.5, oy + y, 1.5, h), c.theme.accent);
        }
    });
}

/// Reminder rows laid out from the top of the list area.
pub const REMINDER_ROW: f32 = 26.0;

fn reminders(p: &Painter, r: Rect, c: &Ctx, hits: &mut Vec<(Rect, Hit)>) {
    header(p, c, r, "Reminders", c.theme.accent);
    let open = c.store.open_reminders();
    text(p, c, &open.to_string(), 22.0, Weight::Bold, Align::Trailing, Rect::new(r.right() - PAD - 60.0, r.y + 8.0, 60.0, 28.0), c.theme.accent);
    let x = r.x + PAD;
    let mut y = r.y + 40.0;
    let bottom = r.bottom() - 10.0;
    // Room for the "New Reminder" row.
    let rows = (((bottom - y) / REMINDER_ROW) as usize).saturating_sub(1);
    let total = c.store.reminders.len();
    for (i, rem) in c.store.reminders.iter().enumerate().take(rows) {
        let row = Rect::new(x, y, r.w - 2.0 * PAD, REMINDER_ROW);
        let (cx, cy) = (x + 8.0, row.cy());
        let hot = c.hover == Some(Hit::Reminder(i));
        if rem.done {
            p.fill_circle(cx, cy, 8.0, c.theme.accent);
            p.fill_circle(cx, cy, 3.5, WHITE);
        } else {
            p.stroke_circle(cx, cy, 7.5, if hot { c.theme.accent } else { c.theme.text_secondary }, 1.5);
        }
        hits.push((Rect::new(x - 4.0, row.y, 26.0, row.h), Hit::Reminder(i)));
        let ink = if rem.done { c.theme.text_secondary } else { c.theme.text };
        text(p, c, &rem.text, 13.0, Weight::Regular, Align::Leading, Rect::new(x + 24.0, row.y, row.w - 24.0, row.h), ink);
        y += REMINDER_ROW;
        if i + 1 < total.min(rows) {
            p.line(x + 24.0, y, row.right(), y, c.faint(), 0.6);
        }
    }
    // The new-reminder row (with a count of what didn't fit).
    let row = Rect::new(x, y, r.w - 2.0 * PAD, REMINDER_ROW);
    if total > rows && c.edit.is_none() {
        let more = format!("+{} more", total - rows);
        text(p, c, &more, 11.0, Weight::Regular, Align::Trailing, row, c.theme.text_secondary);
    }
    hits.push((row, Hit::NewReminder));
    match c.edit {
        Some(e) => {
            p.stroke_circle(x + 8.0, row.cy(), 7.5, c.theme.text_disabled, 1.5);
            let f = p.gfx.format(&c.theme.font, 13.0, Weight::Regular, Align::Leading);
            let tr = Rect::new(x + 24.0, row.y, row.w - 24.0, row.h);
            let s = e.field.string();
            if s.is_empty() {
                p.text("New Reminder", &f, tr, c.theme.text_disabled);
            }
            p.clipped(tr, || {
                let before: String = e.field.text[..e.field.caret].iter().collect();
                let cx = p.gfx.measure(&before, &f);
                let shift = (cx - (tr.w - 4.0)).max(0.0);
                let (a, b) = e.field.range();
                if a != b {
                    let xa = p.gfx.measure(&e.field.text[..a].iter().collect::<String>(), &f);
                    let xb = p.gfx.measure(&e.field.text[..b].iter().collect::<String>(), &f);
                    p.fill_rect(Rect::new(tr.x + xa - shift, tr.cy() - 9.0, xb - xa, 18.0), c.theme.accent.alpha(0.3));
                }
                p.text(&s, &f, Rect::new(tr.x - shift, tr.y, tr.w + shift + 400.0, tr.h), c.theme.text);
                p.fill_rect(Rect::new(tr.x + cx - shift, tr.cy() - 8.0, 1.5, 16.0), c.theme.accent);
            });
        }
        None => {
            let hot = c.hover == Some(Hit::NewReminder);
            let ink = if hot { c.theme.accent } else { c.theme.text_secondary };
            glyphs::plus(p, x + 8.0, row.cy(), 9.0, ink);
            text(p, c, "New Reminder", 13.0, Weight::Regular, Align::Leading, Rect::new(x + 24.0, row.y, row.w - 24.0, row.h), ink);
        }
    }
}

// ---------------------------------------------------------------------------
// Now Playing
// ---------------------------------------------------------------------------

fn clock_text(secs: f32) -> String {
    let s = secs.max(0.0) as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

fn controls(p: &Painter, c: &Ctx, cx: f32, cy: f32, gap: f32, s: f32, playing: bool, hits: &mut Vec<(Rect, Hit)>) {
    for (dx, hit) in [(-gap, Hit::Prev), (0.0, Hit::Play), (gap, Hit::Next)] {
        let x = cx + dx;
        let hot = c.hover == Some(hit);
        let r = Rect::new(x - gap / 2.0, cy - s, gap, 2.0 * s);
        if hot {
            p.fill_circle(x, cy, s * 0.95, c.faint());
        }
        match hit {
            Hit::Prev => ig::skip(p, x, cy, s * 0.62, c.theme.text, true),
            Hit::Next => ig::skip(p, x, cy, s * 0.62, c.theme.text, false),
            _ if playing => ig::pause(p, x, cy, s * 0.8, c.theme.text),
            _ => ig::play(p, x, cy, s * 0.8, c.theme.text),
        }
        hits.push((r, hit));
    }
}

fn art(p: &Painter, c: &Ctx, r: Rect, radius: f32) {
    match c.art {
        Some(bmp) => p.bitmap_round(bmp, r, radius, 1.0),
        None => {
            p.fill_round(r, radius, c.faint());
            glyphs::music(p, r.cx(), r.cy(), r.w * 0.4, c.theme.text_secondary);
        }
    }
}

fn now_playing(p: &Painter, r: Rect, c: &Ctx, hits: &mut Vec<(Rect, Hit)>) {
    let Some(np) = &c.data.now else {
        glyphs::music(p, r.cx(), r.cy() - 14.0, 30.0, c.theme.text_secondary);
        text(p, c, "Not Playing", 13.0, Weight::Bold, Align::Center, Rect::new(r.x, r.cy() + 8.0, r.w, 20.0), c.theme.text_secondary);
        return;
    };
    let artist = if np.artist.is_empty() { "" } else { np.artist.as_str() };
    if c.item.size == WidgetSize::Small {
        let a = Rect::new(r.x + 14.0, r.y + 14.0, 62.0, 62.0);
        art(p, c, a, 10.0);
        hits.push((a, Hit::Art));
        text(p, c, &np.title, 13.0, Weight::Bold, Align::Leading, Rect::new(r.x + 14.0, r.y + 84.0, r.w - 28.0, 18.0), c.theme.text);
        text(p, c, artist, 12.0, Weight::Regular, Align::Leading, Rect::new(r.x + 14.0, r.y + 101.0, r.w - 28.0, 16.0), c.theme.text_secondary);
        controls(p, c, r.cx(), r.bottom() - 26.0, 46.0, 14.0, np.playing, hits);
        return;
    }
    let a = Rect::new(r.x + 14.0, r.y + 14.0, r.h - 28.0, r.h - 28.0);
    art(p, c, a, 12.0);
    hits.push((a, Hit::Art));
    let x = a.right() + 16.0;
    let w = r.right() - PAD - x;
    text(p, c, &np.title, 15.0, Weight::Bold, Align::Leading, Rect::new(x, r.y + 18.0, w, 20.0), c.theme.text);
    text(p, c, artist, 13.0, Weight::Regular, Align::Leading, Rect::new(x, r.y + 39.0, w, 18.0), c.theme.text_secondary);
    if np.duration > 0.0 {
        let pos = np.position_now();
        let bar = Rect::new(x, r.y + 76.0, w, 4.0);
        p.fill_round(bar, 2.0, c.faint());
        p.fill_round(Rect::new(bar.x, bar.y, bar.w * (pos / np.duration).clamp(0.0, 1.0), bar.h), 2.0, c.theme.text_secondary);
        let tr = Rect::new(x, bar.bottom() + 3.0, w, 14.0);
        text(p, c, &clock_text(pos), 10.0, Weight::Regular, Align::Leading, tr, c.theme.text_secondary);
        text(p, c, &format!("-{}", clock_text(np.duration - pos)), 10.0, Weight::Regular, Align::Trailing, tr, c.theme.text_secondary);
    }
    controls(p, c, x + w / 2.0, r.bottom() - 34.0, 56.0, 16.0, np.playing, hits);
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

fn gauge(p: &Painter, c: &Ctx, cx: f32, cy: f32, rad: f32, frac: f32, color: Color, label: &str) {
    p.stroke_circle(cx, cy, rad, c.faint(), rad * 0.22);
    ig::arc(p, cx, cy, rad, frac, color, rad * 0.22);
    let pct = format!("{:.0}%", frac * 100.0);
    text(p, c, &pct, rad * 0.46, Weight::Bold, Align::Center, Rect::new(cx - rad, cy - rad * 0.4, 2.0 * rad, rad * 0.8), c.theme.text);
    text(p, c, label, 10.0, Weight::Bold, Align::Center, Rect::new(cx - 40.0, cy + rad + 4.0, 80.0, 14.0), c.theme.text_secondary);
}

fn net(p: &Painter, c: &Ctx, x: f32, y: f32, w: f32) {
    let s = &c.data.sys;
    let rows = [("↓", sources::rate(s.down), GREEN), ("↑", sources::rate(s.up), c.theme.accent)];
    for (i, (arrow, rate, color)) in rows.iter().enumerate() {
        let ry = y + i as f32 * 18.0;
        text(p, c, arrow, 12.0, Weight::Bold, Align::Leading, Rect::new(x, ry, 14.0, 18.0), *color);
        text(p, c, rate, 12.0, Weight::Bold, Align::Leading, Rect::new(x + 14.0, ry, w - 14.0, 18.0), c.theme.text);
    }
}

fn sparkline(p: &Painter, r: Rect, values: &[f32], color: Color) {
    if values.len() < 2 {
        return;
    }
    let step = r.w / (sources::HISTORY - 1) as f32;
    let x0 = r.right() - step * (values.len() - 1) as f32;
    let at = |i: usize| pt(x0 + step * i as f32, r.bottom() - r.h * values[i].clamp(0.0, 1.0));
    let area = p.gfx.path(|g| unsafe {
        g.BeginFigure(pt(x0, r.bottom()), D2D1_FIGURE_BEGIN_FILLED);
        for i in 0..values.len() {
            g.AddLine(at(i));
        }
        g.AddLine(pt(r.right(), r.bottom()));
        g.EndFigure(D2D1_FIGURE_END_CLOSED);
    });
    if let Some(g) = area {
        p.fill_geometry(&g, color.alpha(0.22));
    }
    let line = p.gfx.path(|g| unsafe {
        g.BeginFigure(at(0), D2D1_FIGURE_BEGIN_HOLLOW);
        for i in 1..values.len() {
            g.AddLine(at(i));
        }
        g.EndFigure(D2D1_FIGURE_END_OPEN);
    });
    if let Some(g) = line {
        p.stroke_geometry(&g, color, 1.6);
    }
}

fn system(p: &Painter, r: Rect, c: &Ctx) {
    let s = &c.data.sys;
    let accent = c.theme.accent;
    if c.item.size == WidgetSize::Small {
        header(p, c, r, "System", c.theme.text_secondary);
        let cy = r.y + 66.0;
        for (i, (frac, color, label)) in [(s.cpu, accent, "CPU"), (s.mem, PURPLE, "MEMORY"), (s.disk, ORANGE, "DISK")].into_iter().enumerate() {
            gauge(p, c, r.x + r.w * (0.2 + 0.3 * i as f32), cy, 20.0, frac, color, label);
        }
        net(p, c, r.x + PAD, r.bottom() - 48.0, r.w - 2.0 * PAD);
        return;
    }
    let half = r.w / 2.0;
    text(p, c, "CPU", 12.0, Weight::Bold, Align::Leading, Rect::new(r.x + PAD, r.y + 12.0, 60.0, 18.0), c.theme.text_secondary);
    text(p, c, &format!("{:.0}%", s.cpu * 100.0), 28.0, Weight::Bold, Align::Leading, Rect::new(r.x + PAD, r.y + 28.0, half, 36.0), c.theme.text);
    let graph = Rect::new(r.x + PAD, r.y + 74.0, half - PAD - 8.0, r.h - 74.0 - PAD);
    p.line(graph.x, graph.bottom(), graph.right(), graph.bottom(), c.faint(), 0.8);
    let hist: Vec<f32> = s.cpu_history.iter().copied().collect();
    sparkline(p, graph, &hist, accent);

    let x = r.x + half + 4.0;
    let w = r.right() - PAD - x;
    gauge(p, c, x + w * 0.25, r.y + 46.0, 24.0, s.mem, PURPLE, "MEMORY");
    gauge(p, c, x + w * 0.75, r.y + 46.0, 24.0, s.disk, ORANGE, "DISK");
    let detail = [
        format!("{} of {}", sources::gigabytes(s.mem_used), sources::gigabytes(s.mem_total)),
        format!("{} free", sources::gigabytes(s.disk_free)),
    ];
    for (i, d) in detail.iter().enumerate() {
        let cx = x + w * (0.25 + 0.5 * i as f32);
        text(p, c, d, 10.0, Weight::Regular, Align::Center, Rect::new(cx - w / 4.0, r.y + 90.0, w / 2.0, 14.0), c.theme.text_secondary);
    }
    net(p, c, x + 8.0, r.bottom() - 50.0, w - 8.0);
}
