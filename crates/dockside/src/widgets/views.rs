//! What's drawn inside each widget. Every function paints into the widget's
//! body rect (in surface points) and records the rects it reacts to.

use std::f32::consts::TAU;

use chrono::{Datelike, Local, NaiveDate, Timelike};
use windows::Win32::Graphics::Direct2D::Common::D2D1_FIGURE_BEGIN_FILLED;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::DirectWrite::*;

use super::sensors::{self, Reading, Tint};
use super::sources::{self, WeatherState, Zone};
use super::store::Store;
use super::{glyphs, Data, Edit, Hit, RADIUS};
use crate::anim::{lerp, Motion};
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

/// A widget's springs: hover highlights and reminder checkmarks.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Anim {
    Hover(Hit),
    Done(usize),
}

pub struct Ctx<'a> {
    pub theme: &'a Theme,
    pub data: &'a Data,
    pub store: &'a Store,
    pub item: &'a WidgetItem,
    pub edit: Option<&'a Edit>,
    pub art: Option<&'a ID2D1Bitmap>,
    pub hover: Option<Hit>,
    /// Without springs (gallery renders), everything shows its end state.
    pub motion: Option<&'a Motion<Anim>>,
    /// Notification icons, by art id.
    pub pictures: &'a [(u64, ID2D1Bitmap)],
    /// Photo / Video: why nothing's showing.
    pub error: Option<&'a str>,
}

impl Ctx<'_> {
    /// 0..1 hover highlight for `hit`.
    fn hot(&self, hit: Hit) -> f32 {
        let on = self.hover == Some(hit);
        self.motion.map_or(if on { 1.0 } else { 0.0 }, |m| m.fade(Anim::Hover(hit), on))
    }

    fn anim(&self, key: Anim, target: f32, feel: (f32, f32)) -> f32 {
        self.motion.map_or(target, |m| m.get(key, target, feel))
    }

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
        WidgetKind::MusicWave => music_wave(p, r, c),
        WidgetKind::Photo | WidgetKind::Video => picture(p, r, c, hits),
        WidgetKind::Notifications => notifications(p, r, c, hits),
        WidgetKind::User => user(p, r, c, hits),
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
    let tw = p.text_tabular(&time, &tf, tr, c.theme.text);
    if !ampm.is_empty() {
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
    let busy: std::collections::HashSet<u32> = c
        .data
        .events
        .iter()
        .filter_map(|e| local_date(e.start))
        .filter(|d| d.year() == today.year() && d.month() == today.month())
        .map(|d| d.day())
        .collect();
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
            // A dot under days that have events (today's sits on the red circle).
            if busy.contains(d) {
                let dot = if *d == today.day() { WHITE } else { c.theme.text_secondary };
                p.fill_circle(cell.cx(), cell.cy() + fs * 0.62, (fs * 0.11).max(1.2), dot);
            }
        }
    }
}

fn local_date(unix: i64) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp(unix, 0).map(|t| t.with_timezone(&Local).date_naive())
}

/// "Tomorrow", "Friday", "Mon 3 Nov"; None for today.
fn day_label(d: NaiveDate) -> Option<String> {
    let days = (d - Local::now().date_naive()).num_days();
    match days {
        0 => None,
        1 => Some("Tomorrow".into()),
        2..=6 => Some(d.format("%A").to_string()),
        _ => Some(d.format("%a %-d %b").to_string()),
    }
}

/// "Tomorrow, 9:00 AM – 9:30 AM", or just the span for today. `short` drops
/// the end time (small widgets): "Tomorrow, 9:00 AM".
fn event_when(e: &CalEvent, short: bool) -> String {
    let end = e.span().rsplit(" – ").next().unwrap_or_default().to_string();
    if e.ongoing() {
        return if short { "Now".into() } else { format!("Now – {end}") };
    }
    let time = if short { e.clock() } else { e.span() };
    match local_date(e.start).and_then(day_label) {
        Some(day) => format!("{day}, {time}"),
        None => time,
    }
}

/// Up to `max` events from `r.y` down; returns the y after the last one.
fn event_list(p: &Painter, r: Rect, c: &Ctx, max: usize) -> f32 {
    let mut y = r.y;
    let events: Vec<_> = c.data.events.iter().take(max).collect();
    if events.is_empty() {
        text(p, c, "No upcoming events", 12.0, Weight::Regular, Align::Leading, Rect::new(r.x, y, r.w, 18.0), c.theme.text_secondary);
        return y + 18.0;
    }
    for e in events {
        if y + 34.0 > r.bottom() {
            break;
        }
        p.fill_round(Rect::new(r.x, y + 2.0, 3.5, 30.0), 1.75, c.theme.accent);
        text(p, c, &e.title, 13.0, Weight::Bold, Align::Leading, Rect::new(r.x + 10.0, y, r.w - 10.0, 18.0), c.theme.text);
        text(p, c, &event_when(e, r.w < 200.0), 11.0, Weight::Regular, Align::Leading, Rect::new(r.x + 10.0, y + 17.0, r.w - 10.0, 16.0), c.theme.text_secondary);
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
        // Like macOS's default small Calendar: the date and what's next.
        WidgetSize::Small => {
            date_block(p, r.x + PAD, r.y + 14.0, r.w - 2.0 * PAD, c, 30.0);
            event_list(p, Rect::new(r.x + PAD, r.y + 70.0, r.w - 2.0 * PAD, r.h - 82.0), c, 2);
        }
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
        WidgetSize::Large => {
            // A row each: ring, name, percentage and a bar, like macOS.
            let rows = devices.len().clamp(4, 8) as f32;
            let row_h = (r.h - 2.0 * 14.0) / rows;
            let rad = (row_h * 0.34).min(19.0);
            for (i, d) in devices.iter().take(8).enumerate() {
                let y = r.y + 14.0 + row_h * i as f32;
                let cy = y + row_h / 2.0;
                ring(p, c, d, r.x + PAD + rad, cy, rad, rad * 0.2);
                let x = r.x + PAD + 2.0 * rad + 12.0;
                let pct_w = 64.0;
                let name_w = r.right() - PAD - pct_w - x;
                let ns = fitted(p, c, &d.name, 14.0, Weight::Bold, name_w - 8.0);
                text(p, c, &d.name, ns, Weight::Bold, Align::Leading, Rect::new(x, cy - 16.0, name_w, 20.0), c.theme.text);
                let bar = Rect::new(x, cy + 6.0, name_w - 12.0, 5.0);
                p.fill_round(bar, 2.5, c.faint());
                let low = d.percent <= 20 && !d.charging;
                let fill = Rect::new(bar.x, bar.y, (bar.w * d.percent as f32 / 100.0).max(bar.h), bar.h);
                p.fill_round(fill, 2.5, if low { c.red() } else { GREEN });
                percent_label(p, c, d, Rect::new(r.right() - PAD - pct_w, cy - 11.0, pct_w - 14.0, 22.0), 15.0);
                if i + 1 < devices.len().min(8) {
                    p.line(x, y + row_h, r.right() - PAD, y + row_h, c.faint(), 0.5);
                }
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
        let hot = c.hot(Hit::Reminder(i));
        // Checking one off pops the fill in; unchecking shrinks it away.
        let done = c.anim(Anim::Done(i), if rem.done { 1.0 } else { 0.0 }, (0.22, 0.8)).max(0.0);
        if done < 0.999 {
            let ring = c.theme.text_secondary.mix(c.theme.accent, hot);
            p.stroke_circle(cx, cy, 7.5, ring.alpha(ring.a * (1.0 - done.min(1.0))), 1.5);
        }
        if done > 0.01 {
            p.fill_circle(cx, cy, 8.0 * done, c.theme.accent);
            p.fill_circle(cx, cy, 3.5 * done, WHITE);
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
            let ink = c.theme.text_secondary.mix(c.theme.accent, c.hot(Hit::NewReminder));
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
        let hot = c.hot(hit);
        let r = Rect::new(x - gap / 2.0, cy - s, gap, 2.0 * s);
        if hot > 0.0 {
            let f = c.faint();
            p.fill_circle(x, cy, s * lerp(0.8, 0.95, hot), f.alpha(f.a * hot));
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

const TEAL: Color = Color::rgba(48, 176, 199, 1.0);
const PINK: Color = Color::rgba(255, 55, 95, 1.0);
/// Height of a gauge's row: ring, label and detail line.
const GAUGE_ROW: f32 = 84.0;

fn tint(c: &Ctx, t: Tint) -> Color {
    match t {
        Tint::Accent => c.theme.accent,
        Tint::Purple => PURPLE,
        Tint::Orange => ORANGE,
        Tint::Green => GREEN,
        Tint::Teal => TEAL,
        Tint::Pink => PINK,
    }
}

/// `s` at `size`, or smaller (down to 7pt) until it fits `w`.
fn fitted(p: &Painter, c: &Ctx, s: &str, size: f32, weight: Weight, w: f32) -> f32 {
    let mut sz = size;
    while sz > 7.0 && width(p, c, s, sz, weight) > w {
        sz -= 0.5;
    }
    sz
}

/// A ring centred on (`cx`, `cy`), its label below in `room` points of width
/// and, with `detail`, a line under that.
fn gauge(p: &Painter, c: &Ctx, cx: f32, cy: f32, rad: f32, g: &Reading, room: f32, detail: bool) {
    let color = tint(c, g.tint);
    p.stroke_circle(cx, cy, rad, c.faint(), rad * 0.22);
    ig::arc(p, cx, cy, rad, g.frac, color, rad * 0.22);
    let vs = fitted(p, c, &g.value, rad * 0.46, Weight::Bold, rad * 1.5);
    text(p, c, &g.value, vs, Weight::Bold, Align::Center, Rect::new(cx - rad, cy - vs * 0.87, 2.0 * rad, vs * 1.74), c.theme.text);
    let ls = fitted(p, c, &g.label, 10.0, Weight::Bold, room);
    text(p, c, &g.label, ls, Weight::Bold, Align::Center, Rect::new(cx - room / 2.0, cy + rad + 4.0, room, 14.0), c.theme.text_secondary);
    if let Some(d) = g.detail.as_deref().filter(|_| detail) {
        let ds = fitted(p, c, d, 10.0, Weight::Regular, room);
        text(p, c, d, ds, Weight::Regular, Align::Center, Rect::new(cx - room / 2.0, cy + rad + 18.0, room, 14.0), c.theme.text_secondary);
    }
}

/// A row of gauges spread evenly across `x`..`x + w`.
fn gauge_row(p: &Painter, c: &Ctx, x: f32, w: f32, cy: f32, rad: f32, row: &[&Reading], cells: usize, detail: bool) {
    let cell = w / cells.max(row.len()).max(1) as f32;
    let x0 = x + (w - cell * row.len() as f32) / 2.0;
    for (i, g) in row.iter().enumerate() {
        gauge(p, c, x0 + cell * (i as f32 + 0.5), cy, rad, g, cell - 4.0, detail);
    }
}

/// Down and up rates on two lines.
fn net(p: &Painter, c: &Ctx, x: f32, y: f32, w: f32, (down, up): (f64, f64)) {
    let rows = [("↓", sources::rate(down), GREEN), ("↑", sources::rate(up), c.theme.accent)];
    for (i, (arrow, rate, color)) in rows.iter().enumerate() {
        let ry = y + i as f32 * 18.0;
        text(p, c, arrow, 12.0, Weight::Bold, Align::Leading, Rect::new(x, ry, 14.0, 18.0), *color);
        text(p, c, rate, 12.0, Weight::Bold, Align::Leading, Rect::new(x + 14.0, ry, w - 14.0, 18.0), c.theme.text);
    }
}

/// An adapter on one line: its name, then down and up rates.
fn net_line(p: &Painter, c: &Ctx, x: f32, y: f32, w: f32, g: &Reading) {
    let (down, up) = g.net.unwrap_or_default();
    let col = 78.0;
    let name_w = w - 2.0 * col;
    let ns = fitted(p, c, &g.label, 12.0, Weight::Bold, name_w - 6.0);
    text(p, c, &g.label, ns, Weight::Bold, Align::Leading, Rect::new(x, y, name_w, 18.0), c.theme.text_secondary);
    for (i, (arrow, rate, color)) in [("↓", sources::rate(down), GREEN), ("↑", sources::rate(up), c.theme.accent)].iter().enumerate() {
        let cx = x + name_w + col * i as f32;
        text(p, c, arrow, 12.0, Weight::Bold, Align::Leading, Rect::new(cx, y, 14.0, 18.0), *color);
        text(p, c, rate, 12.0, Weight::Bold, Align::Leading, Rect::new(cx + 14.0, y, col - 14.0, 18.0), c.theme.text);
    }
}

fn sparkline(p: &Painter, r: Rect, values: &[f32], color: Color) {
    if values.len() < 2 {
        return;
    }
    let step = r.w / (sensors::HISTORY - 1) as f32;
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

/// Label, big percentage and usage over time, filling `r`.
fn graph(p: &Painter, c: &Ctx, r: Rect, g: &Reading) {
    let color = tint(c, g.tint);
    let ls = fitted(p, c, &g.label, 12.0, Weight::Bold, r.w);
    text(p, c, &g.label, ls, Weight::Bold, Align::Leading, Rect::new(r.x, r.y, r.w, 18.0), c.theme.text_secondary);
    text(p, c, &g.value, 28.0, Weight::Bold, Align::Leading, Rect::new(r.x, r.y + 16.0, r.w, 36.0), c.theme.text);
    let area = Rect::new(r.x, r.y + 62.0, r.w, (r.h - 62.0).max(8.0));
    p.line(area.x, area.bottom(), area.right(), area.bottom(), c.faint(), 0.8);
    sparkline(p, area, g.history.as_deref().unwrap_or(&[]), color);
}

/// One reading on a slim row: its label, a bar and its value (an adapter:
/// its name, then down and up rates).
fn bar_row(p: &Painter, c: &Ctx, r: Rect, g: &Reading) {
    let fs = (r.h * 0.6).clamp(9.0, 12.0);
    if let Some((down, up)) = g.net {
        let lw = (r.w * 0.3).min(80.0);
        let ls = fitted(p, c, &g.label, fs, Weight::Bold, lw - 4.0);
        text(p, c, &g.label, ls, Weight::Bold, Align::Leading, Rect::new(r.x, r.y, lw, r.h), c.theme.text_secondary);
        let half = (r.w - lw) / 2.0;
        for (i, (arrow, rate, color)) in [("↓", sources::rate(down), GREEN), ("↑", sources::rate(up), c.theme.accent)].iter().enumerate() {
            let x = r.x + lw + half * i as f32;
            let aw = fs + 1.0;
            text(p, c, arrow, fs, Weight::Bold, Align::Leading, Rect::new(x, r.y, aw, r.h), *color);
            let rs = fitted(p, c, rate, fs, Weight::Bold, half - aw - 2.0);
            text(p, c, rate, rs, Weight::Bold, Align::Leading, Rect::new(x + aw, r.y, half - aw, r.h), c.theme.text);
        }
        return;
    }
    // Narrow (a small widget): labels get more of the row.
    let lw = (r.w * if r.w < 160.0 { 0.44 } else { 0.36 }).min(110.0);
    let vw = width(p, c, "100%", fs, Weight::Bold) + 4.0;
    let ls = fitted(p, c, &g.label, fs, Weight::Bold, lw - 6.0);
    text(p, c, &g.label, ls, Weight::Bold, Align::Leading, Rect::new(r.x, r.y, lw, r.h), c.theme.text_secondary);
    let bar = Rect::new(r.x + lw, r.cy() - 2.5, (r.w - lw - vw - 6.0).max(4.0), 5.0);
    p.fill_round(bar, 2.5, c.faint());
    if g.frac > 0.0 {
        p.fill_round(Rect::new(bar.x, bar.y, (bar.w * g.frac).max(bar.h), bar.h), 2.5, tint(c, g.tint));
    }
    text(p, c, &g.value, fs, Weight::Bold, Align::Trailing, Rect::new(r.right() - vw, r.y, vw, r.h), c.theme.text);
}

/// Every reading as slim rows filling `area`, in up to `cols` columns
/// (whatever doesn't fit at the smallest row height is left out).
fn bar_rows(p: &Painter, c: &Ctx, area: Rect, items: &[&Reading], cols: usize) {
    const MIN_ROW: f32 = 12.0;
    if items.is_empty() {
        return;
    }
    let cols = cols.clamp(1, items.len());
    let fit = (area.h / MIN_ROW).floor().max(1.0) as usize;
    let per = items.len().div_ceil(cols).min(fit);
    let gap = 16.0;
    let cw = (area.w - gap * (cols - 1) as f32) / cols as f32;
    let h = (area.h / per as f32).min(24.0);
    // Fewer rows than room: keep them together, centred in the area.
    let top = area.y + (area.h - h * per as f32) / 2.0;
    for (k, g) in items.iter().enumerate().take(per * cols) {
        let (col, row) = (k / per, k % per);
        bar_row(p, c, Rect::new(area.x + col as f32 * (cw + gap), top + row as f32 * h, cw, h), g);
    }
}

fn system(p: &Painter, r: Rect, c: &Ctx) {
    let all = sensors::readings(&sensors::of(c.item), &c.data.sys);
    let nets: Vec<&Reading> = all.iter().filter(|g| g.is_net()).collect();
    let gauges: Vec<&Reading> = all.iter().filter(|g| !g.is_net()).collect();
    if all.is_empty() {
        header(p, c, r, "System", c.theme.text_secondary);
        let msg = if c.item.sensors.as_ref().is_some_and(|s| s.is_empty()) { "Right-click › Show to pick what to show" } else { "Reading…" };
        text(p, c, msg, 12.0, Weight::Regular, Align::Center, Rect::new(r.x + PAD, r.cy() - 16.0, r.w - 2.0 * PAD, 40.0), c.theme.text_secondary);
        return;
    }
    let inner = Rect::new(r.x + PAD, r.y, r.w - 2.0 * PAD, r.h);
    // Everything in reading order, for the slim rows.
    let every: Vec<&Reading> = all.iter().collect();
    match c.item.size {
        WidgetSize::Small => {
            header(p, c, r, "System", c.theme.text_secondary);
            match (gauges.len(), nets.len()) {
                (0..=3, 1) => {
                    gauge_row(p, c, inner.x, inner.w, r.y + 66.0, 20.0, &gauges, 3, false);
                    net(p, c, inner.x, r.bottom() - 48.0, inner.w, nets[0].net.unwrap_or_default());
                }
                (0..=3, 0) => gauge_row(p, c, inner.x, inner.w, r.y + 80.0, 22.0, &gauges, 3, true),
                (4..=6, 0) => {
                    gauge_row(p, c, inner.x, inner.w, r.y + 58.0, 17.0, &gauges[..3], 3, false);
                    gauge_row(p, c, inner.x, inner.w, r.y + 118.0, 17.0, &gauges[3..], 3, false);
                }
                // More than rings can hold: a row each.
                _ => bar_rows(p, c, Rect::new(inner.x, r.y + 34.0, inner.w, r.h - 34.0 - 10.0), &every, 1),
            }
        }
        WidgetSize::Medium => {
            let half = r.w / 2.0;
            let Some(gi) = gauges.iter().position(|g| g.history.is_some()) else {
                // No graph: gauges across and the adapter along the bottom, or two columns of rows.
                if gauges.len() <= 4 && nets.len() <= 1 {
                    gauge_row(p, c, inner.x, inner.w, r.y + 46.0, 24.0, &gauges, 4, true);
                    if let Some(n) = nets.first() {
                        net_line(p, c, inner.x, r.bottom() - PAD - 18.0, inner.w, n);
                    }
                } else {
                    bar_rows(p, c, Rect::new(inner.x, r.y + 12.0, inner.w, r.h - 24.0), &every, 2);
                }
                return;
            };
            graph(p, c, Rect::new(inner.x, r.y + 12.0, half - PAD - 8.0, r.h - 12.0 - PAD), gauges[gi]);
            let others: Vec<&Reading> = gauges.iter().enumerate().filter(|(i, _)| *i != gi).map(|(_, g)| *g).collect();
            let x = r.x + half + 4.0;
            let w = r.right() - PAD - x;
            match (others.len(), nets.len()) {
                (0..=2, 1) => {
                    gauge_row(p, c, x, w, r.y + 46.0, 24.0, &others, 2, true);
                    net(p, c, x + 8.0, r.bottom() - 50.0, w - 8.0, nets[0].net.unwrap_or_default());
                }
                (0..=2, 0) => gauge_row(p, c, x, w, r.y + 60.0, 26.0, &others, 2, true),
                (3..=4, 0) => {
                    gauge_row(p, c, x, w, r.y + 38.0, 18.0, &others[..2], 2, false);
                    gauge_row(p, c, x, w, r.y + 110.0, 18.0, &others[2..], 2, false);
                }
                _ => {
                    let rest: Vec<&Reading> = every.iter().copied().filter(|g| !std::ptr::eq(*g, gauges[gi])).collect();
                    bar_rows(p, c, Rect::new(x + 4.0, r.y + 12.0, w - 4.0, r.h - 24.0), &rest, 1);
                }
            }
        }
        WidgetSize::Large => {
            // Graphs for the first two usages, then rows of gauges, then adapters.
            let graphs: Vec<usize> = gauges.iter().enumerate().filter(|(_, g)| g.history.is_some()).map(|(i, _)| i).take(2).collect();
            let mut y = r.y + 12.0;
            if !graphs.is_empty() {
                let n = graphs.len() as f32;
                let gw = (inner.w - 16.0 * (n - 1.0)) / n;
                for (k, &i) in graphs.iter().enumerate() {
                    graph(p, c, Rect::new(inner.x + k as f32 * (gw + 16.0), y, gw, 104.0), gauges[i]);
                }
                y += 116.0;
            } else {
                y += 4.0;
            }
            let rest: Vec<&Reading> = gauges.iter().enumerate().filter(|(i, _)| !graphs.contains(i)).map(|(_, g)| *g).collect();
            let lines = nets.len();
            let floor = r.bottom() - PAD - lines as f32 * 20.0 - if lines > 0 { 4.0 } else { 0.0 };
            // The last row's detail line can sit in the bottom margin.
            let rows = ((floor - y + 8.0) / GAUGE_ROW).floor().max(0.0) as usize;
            if lines <= 4 && rest.len() <= rows * 4 {
                for (k, row) in rest.chunks(4).enumerate() {
                    gauge_row(p, c, inner.x, inner.w, y + k as f32 * GAUGE_ROW + 26.0, 20.0, row, 4, true);
                }
                for (k, n) in nets.iter().enumerate() {
                    net_line(p, c, inner.x, floor + 4.0 + k as f32 * 20.0, inner.w, n);
                }
            } else {
                // Too many for rings: everything below the graphs as rows.
                let below: Vec<&Reading> = rest.iter().chain(nets.iter()).copied().collect();
                let cols = if below.len() > 6 { 2 } else { 1 };
                bar_rows(p, c, Rect::new(inner.x, y, inner.w, r.bottom() - PAD - y), &below, cols);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Music Wave
// ---------------------------------------------------------------------------

/// Rounded bars mirrored about their middle line: a live spectrum. Silent,
/// they rest as dots.
fn wave_bars(p: &Painter, r: Rect, levels: &[f32], n: usize, color: Color) {
    let gap_ratio = 0.7;
    let bar = r.w / (n as f32 + (n as f32 - 1.0) * gap_ratio);
    let gap = bar * gap_ratio;
    for i in 0..n {
        // Pair up bands when fewer bars are drawn than worked out.
        let per = (levels.len() / n).max(1);
        let level = levels.get(i * per..((i + 1) * per).min(levels.len())).map_or(0.0, |s| s.iter().copied().fold(0.0, f32::max));
        let h = (r.h * level).max(bar);
        let x = r.x + i as f32 * (bar + gap);
        p.fill_round(Rect::new(x, r.cy() - h / 2.0, bar, h), bar / 2.0, color.alpha(0.55 + 0.45 * level));
    }
}

/// A little square of artwork (or a note on grey when there's none).
fn artwork(p: &Painter, c: &Ctx, r: Rect, radius: f32) {
    match c.art {
        Some(b) => p.bitmap_round(b, r, radius, 1.0),
        None => {
            p.fill_round(r, radius, c.faint());
            let f = p.gfx.format(ICONS, r.h * 0.42, Weight::Regular, Align::Center);
            p.text("\u{E8D6}", &f, r, c.theme.text_secondary);
        }
    }
}

fn music_wave(p: &Painter, r: Rect, c: &Ctx) {
    let np = c.data.now.as_ref();
    let tint = np.and_then(|n| n.art.as_ref()).map_or(c.theme.accent, |a| a.tint);
    // On a light card a pale artwork tint would vanish: the accent reads better.
    let color = if !c.theme.dark && tint.r.min(tint.g).min(tint.b) > 0.7 { c.theme.accent } else { tint };
    let (title, artist) = match np {
        Some(n) => (n.title.as_str(), n.artist.as_str()),
        None => ("Not Playing", ""),
    };
    match c.item.size {
        WidgetSize::Small => {
            let art = Rect::new(r.x + PAD, r.y + PAD, 40.0, 40.0);
            artwork(p, c, art, 8.0);
            let x = art.right() + 10.0;
            let w = r.right() - PAD - x;
            text(p, c, title, 13.0, Weight::Bold, Align::Leading, Rect::new(x, art.y, w, 20.0), c.theme.text);
            text(p, c, artist, 11.0, Weight::Regular, Align::Leading, Rect::new(x, art.y + 20.0, w, 18.0), c.theme.text_secondary);
            wave_bars(p, Rect::new(r.x + PAD, r.y + 70.0, r.w - 2.0 * PAD, r.h - 70.0 - PAD), &c.data.bands, 14, color);
        }
        _ => {
            let art = Rect::new(r.x + PAD, r.y + PAD, 56.0, 56.0);
            artwork(p, c, art, 10.0);
            let x = art.right() + 12.0;
            let w = r.right() - PAD - x;
            text(p, c, title, 15.0, Weight::Bold, Align::Leading, Rect::new(x, art.y + 6.0, w, 22.0), c.theme.text);
            text(p, c, artist, 13.0, Weight::Regular, Align::Leading, Rect::new(x, art.y + 28.0, w, 20.0), c.theme.text_secondary);
            wave_bars(p, Rect::new(r.x + PAD, r.y + 84.0, r.w - 2.0 * PAD, r.h - 84.0 - PAD), &c.data.bands, 32, color);
        }
    }
}

// ---------------------------------------------------------------------------
// Photo and Video
// ---------------------------------------------------------------------------

/// Segoe Fluent Icons, Windows 11's symbol font.
const ICONS: &str = "Segoe Fluent Icons";

fn picture(p: &Painter, r: Rect, c: &Ctx, hits: &mut Vec<(Rect, Hit)>) {
    if let Some(b) = c.art {
        p.bitmap_cover(b, r, RADIUS, 1.0);
        return;
    }
    let video = c.item.kind == WidgetKind::Video;
    let glyph = if video { "\u{E714}" } else { "\u{EB9F}" };
    let small = c.item.size == WidgetSize::Small;
    let gs = if small { 30.0 } else { 38.0 };
    let f = p.gfx.format(ICONS, gs, Weight::Regular, Align::Center);
    let top = r.cy() - if small { 44.0 } else { 50.0 };
    p.text(glyph, &f, Rect::new(r.x, top, r.w, gs + 8.0), c.theme.text_secondary);
    let msg = match (&c.item.path, c.error) {
        (None, _) => if video { "Choose a video to loop here" } else { "Choose a picture, GIF or folder" },
        (Some(_), Some(e)) => e,
        (Some(_), None) => "Loading…",
    };
    let line = Rect::new(r.x + PAD, top + gs + 12.0, r.w - 2.0 * PAD, 18.0);
    let ms = fitted(p, c, msg, 12.0, Weight::Regular, line.w);
    text(p, c, msg, ms, Weight::Regular, Align::Center, line, c.theme.text_secondary);
    if c.item.path.is_none() || c.error.is_some() {
        let label = "Choose…";
        let bw = width(p, c, label, 13.0, Weight::Bold) + 28.0;
        let b = Rect::new(r.cx() - bw / 2.0, line.bottom() + 10.0, bw, 28.0);
        let hot = c.hot(Hit::Choose);
        p.fill_round(b, 14.0, c.theme.accent.alpha(0.85 + 0.15 * hot));
        text(p, c, label, 13.0, Weight::Bold, Align::Center, b, Color::WHITE);
        hits.push((b, Hit::Choose));
    }
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// "now", "5m", "3h", "Yesterday", "Mon", "12 Oct".
fn ago(at: std::time::SystemTime) -> String {
    let secs = std::time::SystemTime::now().duration_since(at).map_or(0, |d| d.as_secs());
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => {
            let when: chrono::DateTime<Local> = at.into();
            let days = (Local::now().date_naive() - when.date_naive()).num_days();
            match days {
                ..=1 => "Yesterday".into(),
                2..=6 => when.format("%a").to_string(),
                _ => when.format("%-d %b").to_string(),
            }
        }
    }
}

/// One notification on its own rounded platter.
fn note_row(p: &Painter, c: &Ctx, r: Rect, n: &crate::island::notifications::Note, hit: Hit, hits: &mut Vec<(Rect, Hit)>, tall: bool) {
    let hot = c.hot(hit);
    let base = if c.theme.dark { Color::gray(255, 0.07) } else { Color::gray(0, 0.045) };
    p.fill_round(r, 12.0, base.alpha(base.a * (1.0 + hot)));
    let icon = Rect::new(r.x + 10.0, r.y + 10.0, 26.0, 26.0);
    match n.icon.as_ref().and_then(|a| c.pictures.iter().find(|(id, _)| *id == a.id)) {
        Some((_, b)) => p.bitmap_round(b, icon, 6.0, 1.0),
        None => {
            p.fill_round(icon, 6.0, c.faint());
            let f = p.gfx.format(ICONS, 13.0, Weight::Regular, Align::Center);
            p.text("\u{EA8F}", &f, icon, c.theme.text_secondary);
        }
    }
    let x = icon.right() + 10.0;
    let w = r.right() - 10.0 - x;
    let when = ago(n.at);
    let tw = width(p, c, &when, 11.0, Weight::Regular) + 2.0;
    text(p, c, &n.app.to_uppercase(), 10.0, Weight::Bold, Align::Leading, Rect::new(x, r.y + 8.0, w - tw - 6.0, 14.0), c.theme.text_secondary);
    text(p, c, &when, 11.0, Weight::Regular, Align::Trailing, Rect::new(r.right() - 10.0 - tw, r.y + 8.0, tw, 14.0), c.theme.text_secondary);
    text(p, c, &n.title, 13.0, Weight::Bold, Align::Leading, Rect::new(x, r.y + 22.0, w, 18.0), c.theme.text);
    if !n.body.is_empty() {
        if tall {
            // Room to wrap: the body runs on for a few lines.
            let f = p.gfx.format_block(&c.theme.font, 12.0, Weight::Regular, Align::Leading);
            p.text(&n.body, &f, Rect::new(x, r.y + 41.0, w, r.bottom() - 8.0 - (r.y + 41.0)), c.theme.text_secondary);
        } else {
            text(p, c, &n.body, 12.0, Weight::Regular, Align::Leading, Rect::new(x, r.y + 39.0, w, 16.0), c.theme.text_secondary);
        }
    }
    hits.push((r, hit));
}

fn notifications(p: &Painter, r: Rect, c: &Ctx, hits: &mut Vec<(Rect, Hit)>) {
    let notes = &c.data.notes;
    header(p, c, r, "Notifications", c.red());
    if !notes.is_empty() {
        let count = notes.len().to_string();
        let cw = (width(p, c, &count, 11.0, Weight::Bold) + 12.0).max(20.0);
        let b = Rect::new(r.right() - PAD - cw, r.y + 13.0, cw, 18.0);
        p.fill_round(b, 9.0, c.red());
        text(p, c, &count, 11.0, Weight::Bold, Align::Center, b, Color::WHITE);
    }
    let area = Rect::new(r.x + 10.0, r.y + 40.0, r.w - 20.0, r.h - 40.0 - 10.0);
    if notes.is_empty() {
        let f = p.gfx.format(ICONS, 28.0, Weight::Regular, Align::Center);
        p.text("\u{EA8F}", &f, Rect::new(r.x, area.cy() - 34.0, r.w, 36.0), c.theme.text_secondary);
        text(p, c, "No Notifications", 13.0, Weight::Regular, Align::Center, Rect::new(r.x, area.cy() + 6.0, r.w, 20.0), c.theme.text_secondary);
        return;
    }
    let gap = 6.0;
    let (rows, tall) = match c.item.size {
        // Just the newest, with room for its text.
        WidgetSize::Small => (1, true),
        WidgetSize::Medium => (2, false),
        WidgetSize::Large => (5, false),
    };
    let n = rows.min(notes.len());
    let row_h = if tall { area.h } else { ((area.h - gap * (rows as f32 - 1.0)) / rows as f32).min(62.0) };
    for (i, note) in notes.iter().take(n).enumerate() {
        let rr = Rect::new(area.x, area.y + i as f32 * (row_h + gap), area.w, row_h);
        note_row(p, c, rr, note, Hit::Notification(i), hits, tall);
    }
    if notes.len() > n && c.item.size == WidgetSize::Large {
        let more = format!("{} more", notes.len() - n);
        text(p, c, &more, 11.0, Weight::Regular, Align::Center, Rect::new(r.x, r.bottom() - 22.0, r.w, 16.0), c.theme.text_secondary);
    }
}

// ---------------------------------------------------------------------------
// User
// ---------------------------------------------------------------------------

/// The account picture in a circle (initials on grey without one).
fn avatar(p: &Painter, c: &Ctx, cx: f32, cy: f32, d: f32, name: &str) {
    let r = Rect::new(cx - d / 2.0, cy - d / 2.0, d, d);
    match c.art {
        Some(b) => p.bitmap_round(b, r, d / 2.0, 1.0),
        None => {
            p.fill_round_gradient(r, d / 2.0, Color::rgba(166, 170, 180, 1.0), Color::rgba(132, 136, 146, 1.0));
            text(p, c, &super::user::initials(name), d * 0.4, Weight::Bold, Align::Center, r, Color::WHITE);
        }
    }
    let rim = if c.theme.dark { Color::gray(255, 0.12) } else { Color::gray(0, 0.08) };
    p.stroke_circle(cx, cy, d / 2.0 - 0.25, rim, 0.5);
}

/// Lock / Sign Out / Sleep. `room` = Some(width): pills with words from `x`
/// when they fit there (round icons otherwise); None: round icons centred on `x`.
fn user_buttons(p: &Painter, c: &Ctx, x: f32, cy: f32, room: Option<f32>, hits: &mut Vec<(Rect, Hit)>) {
    let buttons = [("\u{E72E}", "Lock", Hit::Lock), ("\u{F3B1}", "Sign Out", Hit::SignOut), ("\u{E708}", "Sleep", Hit::Sleep)];
    let gap = 6.0;
    let pill = |l: &str| width(p, c, l, 12.0, Weight::Bold) + 36.0;
    let labelled: f32 = buttons.iter().map(|(_, l, _)| pill(l)).sum::<f32>() + gap * 2.0;
    let labels = room.is_some_and(|w| labelled <= w);
    let widths: Vec<f32> = buttons.iter().map(|(_, l, _)| if labels { pill(l) } else { 30.0 }).collect();
    let total: f32 = widths.iter().sum::<f32>() + gap * (buttons.len() - 1) as f32;
    let glyph_f = p.gfx.format(ICONS, if labels { 12.0 } else { 13.0 }, Weight::Regular, Align::Center);
    let mut x = if room.is_some() { x } else { x - total / 2.0 };
    let h = if labels { 28.0 } else { 30.0 };
    for ((glyph, label, hit), w) in buttons.iter().zip(widths) {
        let b = Rect::new(x, cy - h / 2.0, w, h);
        let hot = c.hot(*hit);
        let fill = if c.theme.dark { Color::gray(255, 0.1 + 0.08 * hot) } else { Color::gray(0, 0.06 + 0.05 * hot) };
        p.fill_round(b, h / 2.0, fill);
        if labels {
            p.text(glyph, &glyph_f, Rect::new(b.x + 9.0, b.y, 16.0, h), c.theme.text);
            text(p, c, label, 12.0, Weight::Bold, Align::Leading, Rect::new(b.x + 28.0, b.y, w - 32.0, h), c.theme.text);
        } else {
            p.text(glyph, &glyph_f, b, c.theme.text);
        }
        hits.push((b, *hit));
        x += w + gap;
    }
}

fn user(p: &Painter, r: Rect, c: &Ctx, hits: &mut Vec<(Rect, Hit)>) {
    use super::user::{greeting, shows, uptime};
    let Some(u) = &c.data.user else { return };
    let greet = shows(c.item, "greeting").then(|| format!("{},", greeting(Local::now().hour())));
    let device = shows(c.item, "device").then(|| {
        let up = format!("Up {}", uptime());
        if u.pc.is_empty() { up } else { format!("{} · {up}", u.pc) }
    });
    let buttons = shows(c.item, "buttons");
    match c.item.size {
        WidgetSize::Small => {
            // Stacked and centred: picture, name, the extras that are on.
            let d = if buttons && device.is_some() { 50.0 } else { 60.0 };
            let mut parts: Vec<f32> = vec![d, 8.0];
            if greet.is_some() {
                parts.push(15.0);
            }
            parts.push(20.0);
            if device.is_some() {
                parts.push(16.0);
            }
            if buttons {
                parts.extend([8.0, 30.0]);
            }
            let mut y = r.cy() - parts.iter().sum::<f32>() / 2.0;
            avatar(p, c, r.cx(), y + d / 2.0, d, &u.name);
            y += d + 8.0;
            let line = |y: f32, h: f32| Rect::new(r.x + 10.0, y, r.w - 20.0, h);
            if let Some(g) = &greet {
                text(p, c, g, 11.0, Weight::Regular, Align::Center, line(y, 15.0), c.theme.text_secondary);
                y += 15.0;
            }
            let ns = fitted(p, c, &u.first, 16.0, Weight::Bold, r.w - 20.0);
            text(p, c, if greet.is_some() { &u.first } else { &u.name }, ns, Weight::Bold, Align::Center, line(y, 20.0), c.theme.text);
            y += 20.0;
            if let Some(dv) = &device {
                let ds = fitted(p, c, dv, 10.0, Weight::Regular, r.w - 20.0);
                text(p, c, dv, ds, Weight::Regular, Align::Center, line(y, 16.0), c.theme.text_secondary);
                y += 16.0;
            }
            if buttons {
                user_buttons(p, c, r.cx(), y + 8.0 + 15.0, None, hits);
            }
        }
        _ => {
            let d = 84.0;
            avatar(p, c, r.x + PAD + 4.0 + d / 2.0, r.cy(), d, &u.name);
            let x = r.x + PAD + 4.0 + d + 16.0;
            let w = r.right() - PAD - x;
            let mut parts: Vec<f32> = Vec::new();
            if greet.is_some() {
                parts.push(18.0);
            }
            parts.push(30.0);
            if device.is_some() {
                parts.push(18.0);
            }
            if buttons {
                parts.extend([12.0, 28.0]);
            }
            let mut y = r.cy() - parts.iter().sum::<f32>() / 2.0;
            if let Some(g) = &greet {
                text(p, c, g, 13.0, Weight::Regular, Align::Leading, Rect::new(x, y, w, 18.0), c.theme.text_secondary);
                y += 18.0;
            }
            let ns = fitted(p, c, &u.name, 24.0, Weight::Bold, w);
            text(p, c, &u.name, ns, Weight::Bold, Align::Leading, Rect::new(x, y, w, 30.0), c.theme.text);
            y += 30.0;
            if let Some(dv) = &device {
                text(p, c, dv, 12.0, Weight::Regular, Align::Leading, Rect::new(x, y, w, 18.0), c.theme.text_secondary);
                y += 18.0;
            }
            if buttons {
                user_buttons(p, c, x, y + 12.0 + 14.0, Some(w), hits);
            }
        }
    }
}
