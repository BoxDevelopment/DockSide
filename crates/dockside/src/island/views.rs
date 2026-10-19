//! What's drawn inside the pill: compact activities, alerts, the secondary
//! bubble and the expanded panels. Every function paints into the pill rect it
//! is given (which springs around), at opacity `a`, and records hit rects.

use std::collections::HashMap;
use std::path::PathBuf;

use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;

use super::calendar::CalEvent;
use super::claude::{Session, State};
use super::glyphs as g;
use super::media::NowPlaying;
use super::timer::Clock;
use super::{fmt_time, lerp, Alert, Download, Fonts, Hit, Persist, BLUE, CLAUDE, GREEN, ORANGE, PURPLE, RED, SECONDARY, WHITE};
use crate::geom::{Color, Rect};
use crate::gfx::Painter;

const PAD: f32 = 18.0;
/// Spacing of the desktop indicator's dots.
pub const DESK_DOT_GAP: f32 = 14.0;

pub struct View<'a> {
    pub fonts: &'a Fonts,
    pub now: Option<&'a NowPlaying>,
    pub art: Option<&'a ID2D1Bitmap>,
    pub old_art: Option<&'a ID2D1Bitmap>,
    pub art_fade: f32,
    pub wave_t: f32,
    pub energy: f32,
    pub hot: Option<Hit>,
    pub down: Option<Hit>,
    pub scrub: Option<f32>,
    pub bar_hover: f32,
    /// Desktop indicator: sliding position (desktop index) and its velocity.
    pub desk_pos: f32,
    pub desk_vel: f32,
    pub clock: Option<&'a Clock>,
    pub shelf: &'a [PathBuf],
    pub shelf_icons: &'a HashMap<PathBuf, ID2D1Bitmap>,
    pub drag_over: bool,
    pub events: &'a [CalEvent],
    pub downloads: &'a [Download],
    pub volume: (f32, bool),
    pub alert_img: Option<&'a ID2D1Bitmap>,
    pub accent: Color,
    pub time: f32,
    pub claude: Option<&'a Session>,
    pub claude_more: usize,
}

/// The spark turns while Claude works and holds still while it waits on you.
fn spark_angle(v: &View) -> f32 {
    match v.claude.map(|s| &s.state) {
        Some(State::Waiting(_)) | None => 0.0,
        _ => v.time * 1.6,
    }
}

type Hits = Vec<(Rect, Hit)>;

// ---------------------------------------------------------------------------
// Compact
// ---------------------------------------------------------------------------

pub fn compact(p: &Painter, r: Rect, which: Persist, v: &View, a: f32, hits: &mut Hits) {
    let _ = hits;
    let h = r.h;
    let icon_cx = r.x + h / 2.0 + 3.0;
    let inner = Rect::new(r.x + h + 4.0, r.y, r.w - 2.0 * h - 8.0, h);
    match which {
        Persist::Media => compact_media(p, r, v, a),
        Persist::Timer => {
            let Some(c) = v.clock else { return };
            if c.is_timer() {
                g::timer(p, icon_cx, r.cy(), h * 0.62, c.fraction(), ORANGE.alpha(a));
            } else {
                g::stopwatch(p, icon_cx, r.cy(), h * 0.72, c.fraction(), ORANGE.alpha(a));
            }
            let tr = Rect::new(r.x, r.y, r.w - h * 0.5 - 2.0, h);
            p.text_tabular(&c.label(), &v.fonts.alert_b, tr, ORANGE.alpha(if c.running { a } else { a * 0.6 }));
        }
        Persist::Download => {
            g::download(p, icon_cx, r.cy(), h * 0.66, BLUE.alpha(a));
            let d = v.downloads.first();
            let name = d.map(|d| d.name()).unwrap_or_default();
            p.text(&name, &v.fonts.alert, Rect::new(inner.x, r.y, inner.w - 54.0, h), WHITE.alpha(a));
            if let Some(d) = d {
                let tr = Rect::new(r.x, r.y, r.w - h * 0.5 - 22.0, h);
                p.text_tabular(&human_size(d.size), &v.fonts.small_r, tr, SECONDARY.alpha(a));
            }
            spinner(p, r.right() - h * 0.5 - 6.0, r.cy(), h * 0.26, v.time, WHITE.alpha(a));
        }
        Persist::Calendar => {
            let Some(e) = v.events.iter().find(|e| e.end > super::calendar::unix_now()) else { return };
            g::calendar(p, icon_cx, r.cy(), h * 0.68, WHITE.alpha(a), RED.alpha(a));
            let when = e.when();
            let ww = p.gfx.measure(&when, &v.fonts.alert_r) + 4.0;
            p.text(&e.title, &v.fonts.alert, Rect::new(inner.x, r.y, r.w - (inner.x - r.x) - ww - h * 0.5 - 6.0, h), WHITE.alpha(a));
            let tr = Rect::new(r.x, r.y, r.w - h * 0.5 - 2.0, h);
            p.text(&when, &v.fonts.alert_r, tr, if e.starts_in() <= 300 { ORANGE } else { SECONDARY }.alpha(a));
        }
        Persist::Claude => {
            let Some(s) = v.claude else { return };
            let waiting = matches!(s.state, State::Waiting(_));
            g::spark(p, icon_cx, r.cy(), h * 0.72, spark_angle(v), CLAUDE.alpha(a));
            let time = fmt_time(s.elapsed());
            let tw = p.gfx.measure(&time, &v.fonts.small_r) + 6.0;
            let color = if waiting { ORANGE } else { WHITE };
            p.text(&s.status(), &v.fonts.alert, Rect::new(inner.x, r.y, r.w - (inner.x - r.x) - tw - h * 0.5, h), color.alpha(a));
            p.text_tabular(&time, &v.fonts.small_r, Rect::new(r.x, r.y, r.w - h * 0.5 - 2.0, h), SECONDARY.alpha(a));
        }
        Persist::Shelf => {
            g::tray(p, icon_cx, r.cy(), h * 0.68, WHITE.alpha(a));
            let n = v.shelf.len();
            let label = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
            let tr = Rect::new(r.x, r.y, r.w - h * 0.5 - 2.0, h);
            p.text(&label, &v.fonts.alert_r, tr, SECONDARY.alpha(a));
        }
    }
}

fn compact_media(p: &Painter, band: Rect, v: &View, a: f32) {
    let s = (band.h - 6.0).max(8.0);
    let inset = (band.h - s) / 2.0;
    let art = Rect::new(band.x + inset + 3.0, band.y + inset, s, s);
    artwork(p, art, s * 0.26, v, a);
    let ww = s * 1.05;
    let wave = Rect::new(band.right() - inset - 4.0 - ww, band.y + inset + s * 0.12, ww, s * 0.76);
    waveform(p, wave, v, a);
}

/// The secondary activity, as a small circle beside the pill.
pub fn bubble(p: &Painter, c: Rect, which: Persist, v: &View, a: f32) {
    let (cx, cy, d) = (c.cx(), c.cy(), c.w);
    match which {
        Persist::Media => {
            if v.art.is_some() {
                artwork(p, c.inset(d * 0.14, d * 0.14), d * 0.36, v, a);
            } else {
                waveform(p, Rect::new(cx - d * 0.26, cy - d * 0.2, d * 0.52, d * 0.4), v, a);
            }
        }
        Persist::Timer => match v.clock {
            Some(clock) if clock.is_timer() => g::timer(p, cx, cy, d * 0.62, clock.fraction(), ORANGE.alpha(a)),
            Some(clock) => g::stopwatch(p, cx, cy, d * 0.66, clock.fraction(), ORANGE.alpha(a)),
            None => {}
        },
        Persist::Download => spinner(p, cx, cy, d * 0.22, v.time, BLUE.alpha(a)),
        Persist::Calendar => g::calendar(p, cx, cy, d * 0.6, WHITE.alpha(a), RED.alpha(a)),
        Persist::Shelf => g::tray(p, cx, cy, d * 0.6, WHITE.alpha(a)),
        Persist::Claude => g::spark(p, cx, cy, d * 0.62, spark_angle(v), CLAUDE.alpha(a)),
    }
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

pub fn alert(p: &Painter, r: Rect, alert: &Alert, v: &View, a: f32, hits: &mut Hits) {
    let h = r.h.min(super::COMPACT_MAX);
    let icon_cx = r.x + h / 2.0 + 4.0;
    let cy = r.y + r.h / 2.0;
    let text_x = r.x + h + 6.0;
    let right_inset = h * 0.5;
    let f = v.fonts;
    // Label on the left, value on the right: most alerts are this shape.
    let pair = |label: &str, value: &str, vc: Color| {
        p.text(label, &f.alert, Rect::new(text_x, r.y, r.w * 0.6, r.h), WHITE.alpha(a));
        p.text(value, &f.alert_b, Rect::new(r.x, r.y, r.w - right_inset, r.h), vc.alpha(a));
    };
    match alert {
        Alert::Charging(pct) | Alert::LowBattery(pct) => {
            let low = matches!(alert, Alert::LowBattery(_));
            let color = if low { RED } else { GREEN };
            if !low {
                g::bolt(p, icon_cx, cy, h * 0.62, color.alpha(a));
            }
            let label = if low { "Low Battery" } else { "Charging" };
            p.text(label, &f.alert, Rect::new(if low { r.x + right_inset } else { text_x - 6.0 }, r.y, r.w / 2.0, r.h), WHITE.alpha(a));
            g::battery(p, r.right() - right_inset, cy, *pct, color, a);
            let tr = Rect::new(r.x, r.y, r.w - right_inset - 31.0, r.h);
            p.text_tabular(&format!("{pct}%"), &f.alert_b, tr, color.alpha(a));
        }
        Alert::Volume | Alert::Brightness(_) => {
            let (level, muted) = match alert {
                Alert::Brightness(b) => (*b as f32 / 100.0, false),
                _ => v.volume,
            };
            if matches!(alert, Alert::Volume) {
                g::speaker(p, icon_cx, cy, h * 0.7, level, muted, WHITE.alpha(a));
            } else {
                g::sun(p, icon_cx, cy, h * 0.72, WHITE.alpha(a));
            }
            let pct_w = 38.0;
            let bar = Rect::new(text_x + 2.0, cy - 3.0, r.right() - right_inset - pct_w - text_x - 4.0, 6.0);
            let thick = lerp(6.0, 8.0, v.bar_hover);
            let track = Rect::new(bar.x, cy - thick / 2.0, bar.w, thick);
            p.fill_round(track, thick / 2.0, Color::gray(255, 0.22 * a));
            let fill = if muted { 0.0 } else { level };
            if fill > 0.0 {
                p.fill_round(Rect::new(track.x, track.y, (track.w * fill).max(thick), thick), thick / 2.0, WHITE.alpha(a));
            }
            let pct = if muted { "Muted".to_string() } else { format!("{}", (level * 100.0).round() as u32) };
            p.text_tabular(&pct, &f.alert_r, Rect::new(r.x, r.y, r.w - right_inset, r.h), SECONDARY.alpha(a));
            if matches!(alert, Alert::Volume) {
                hits.push((bar.inset(-4.0, -8.0), Hit::Slider));
            }
        }
        Alert::Desktop { index, count, name, missing } => {
            g::desktops(p, icon_cx, cy, h * 0.66, WHITE.alpha(a));
            let n = (*count).clamp(1, super::MAX_DESKTOP_DOTS);
            let x0 = r.right() - right_inset - (n - 1) as f32 * DESK_DOT_GAP;
            let label_w = x0 - text_x - 16.0;
            match missing {
                Some(num) => p.text(&format!("No Desktop {num}"), &f.alert, Rect::new(text_x, r.y, label_w, r.h), RED.alpha(a)),
                None => p.text(name, &f.alert, Rect::new(text_x, r.y, label_w, r.h), WHITE.alpha(a)),
            }
            for i in 0..n {
                let on = i == *index;
                p.fill_circle(x0 + i as f32 * DESK_DOT_GAP, cy, 3.0, Color::gray(255, if on { 0.0 } else { 0.28 * a }));
            }
            // The current desktop is a pill that slides between dots and
            // stretches with its speed, like a drop of liquid.
            let pos = v.desk_pos.clamp(-0.6, (n - 1) as f32 + 0.6);
            let stretch = (v.desk_vel.abs() * 1.6).min(12.0);
            let pw = 9.0 + stretch;
            let px = x0 + pos * DESK_DOT_GAP;
            let color = if missing.is_some() { RED } else { WHITE };
            p.fill_round(Rect::new(px - pw / 2.0, cy - 4.5, pw, 9.0), 4.5, color.alpha(a));
        }
        Alert::Caps(on) => {
            g::caps(p, icon_cx, cy, h * 0.72, WHITE.alpha(a));
            pair("Caps Lock", if *on { "On" } else { "Off" }, if *on { GREEN } else { SECONDARY });
        }
        Alert::Dnd(on) => {
            g::moon(p, icon_cx, cy, h * 0.66, PURPLE.alpha(a), Color::BLACK);
            pair("Do Not Disturb", if *on { "On" } else { "Off" }, if *on { PURPLE } else { SECONDARY });
        }
        Alert::Screenshot(path) => {
            let th = r.h - 14.0;
            let mut tw = th;
            if let Some(img) = v.alert_img {
                let s = unsafe { img.GetSize() };
                tw = (th * s.width / s.height.max(1.0)).clamp(th * 0.6, th * 1.9);
                p.bitmap_round(img, Rect::new(r.x + 14.0, r.y + 7.0, tw, th), 7.0, a);
            }
            let tx = r.x + 14.0 + tw + 12.0;
            p.text("Screenshot", &f.title, Rect::new(tx, r.y + r.h / 2.0 - 19.0, r.right() - tx - 16.0, 20.0), WHITE.alpha(a));
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            p.text(&name, &f.body, Rect::new(tx, r.y + r.h / 2.0 + 1.0, r.right() - tx - 16.0, 18.0), SECONDARY.alpha(a));
            hits.push((r, Hit::AlertBody));
        }
        Alert::Downloaded(path) => {
            g::check(p, icon_cx, cy, h * 0.7, GREEN.alpha(a));
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            p.text("Downloaded", &f.alert_b, Rect::new(r.x, r.y, r.w - right_inset, r.h), GREEN.alpha(a));
            p.text(&name, &f.alert, Rect::new(text_x, r.y, r.w - (text_x - r.x) - 96.0, r.h), WHITE.alpha(a));
            hits.push((r, Hit::AlertBody));
        }
        Alert::Bluetooth { name, connected, battery } => {
            let big = r.h - 18.0;
            let ic = (r.x + 12.0 + big / 2.0, cy);
            p.fill_circle(ic.0, ic.1, big / 2.0, Color::gray(255, 0.12 * a));
            g::headphones(p, ic.0, ic.1, big * 0.6, WHITE.alpha(a));
            let tx = r.x + 24.0 + big;
            let tw = r.right() - tx - if battery.is_some() { 64.0 } else { 18.0 };
            p.text(name, &f.title, Rect::new(tx, cy - 19.0, tw, 20.0), WHITE.alpha(a));
            let state = if *connected { "Connected" } else { "Disconnected" };
            p.text(state, &f.body, Rect::new(tx, cy + 1.0, tw, 18.0), SECONDARY.alpha(a));
            if let Some(b) = battery {
                let (bx, rr) = (r.right() - 34.0, 14.0);
                let color = if *b <= 20 { RED } else { GREEN };
                p.stroke_circle(bx, cy, rr, Color::gray(255, 0.16 * a), 3.0);
                g::arc(p, bx, cy, rr, *b as f32 / 100.0, color.alpha(a), 3.0);
                p.text_tabular(&format!("{b}"), &f.small_c, Rect::new(bx - rr, cy - 8.0, rr * 2.0, 16.0), WHITE.alpha(a));
            }
            hits.push((r, Hit::AlertBody));
        }
        Alert::Privacy { camera, app } => {
            if *camera {
                g::camera(p, icon_cx, cy, h * 0.7, GREEN.alpha(a));
            } else {
                g::mic(p, icon_cx, cy, h * 0.7, ORANGE.alpha(a));
            }
            let label = if *camera { "Camera" } else { "Microphone" };
            p.text(label, &f.alert, Rect::new(text_x, r.y, r.w * 0.5, r.h), WHITE.alpha(a));
            p.text(app, &f.alert_r, Rect::new(r.x + r.w * 0.45, r.y, r.w * 0.55 - right_inset, r.h), SECONDARY.alpha(a));
        }
        Alert::Note(n) => {
            let is = 38.0;
            let icon = Rect::new(r.x + 16.0, cy - is / 2.0, is, is);
            match v.alert_img {
                Some(img) => p.bitmap_round(img, icon, 9.0, a),
                None => {
                    p.fill_round(icon, 9.0, Color::gray(255, 0.14 * a));
                    g::bell(p, icon.cx(), icon.cy(), 18.0, WHITE.alpha(a));
                }
            }
            let tx = icon.right() + 12.0;
            let tw = r.right() - tx - 18.0;
            p.text(&n.app, &f.small_l, Rect::new(tx, cy - 27.0, tw, 15.0), SECONDARY.alpha(a));
            p.text(&n.title, &f.alert_title, Rect::new(tx, cy - 12.0, tw, 19.0), WHITE.alpha(a));
            if !n.body.is_empty() {
                p.text(&n.body, &f.alert, Rect::new(tx, cy + 7.0, tw, 18.0), Color::gray(255, 0.75 * a));
            }
            hits.push((r, Hit::AlertBody));
        }
        Alert::ClaudeDone { project, secs, tools } => {
            g::spark(p, icon_cx, cy, h * 0.72, 0.0, CLAUDE.alpha(a));
            let what = if project.is_empty() { "Claude finished".to_string() } else { format!("{project} is done") };
            p.text(&what, &f.alert, Rect::new(text_x, r.y, r.w - (text_x - r.x) - 110.0, r.h), WHITE.alpha(a));
            let stat = if *tools > 0 { format!("{} · {} tools", fmt_time(*secs), tools) } else { fmt_time(*secs) };
            p.text_tabular(&stat, &f.alert_b, Rect::new(r.x, r.y, r.w - right_inset, r.h), GREEN.alpha(a));
            hits.push((r, Hit::AlertBody));
        }
        Alert::ClaudeNeeds { project, message } => {
            g::spark(p, icon_cx, cy, h * 0.72, 0.0, ORANGE.alpha(a));
            let msg = if message.is_empty() { "Claude needs your input" } else { message };
            let pw = if project.is_empty() { 0.0 } else { p.gfx.measure(project, &f.alert_r) + 10.0 };
            p.text(msg, &f.alert, Rect::new(text_x, r.y, r.w - (text_x - r.x) - pw - right_inset, r.h), WHITE.alpha(a));
            p.text(project, &f.alert_r, Rect::new(r.x, r.y, r.w - right_inset, r.h), SECONDARY.alpha(a));
            hits.push((r, Hit::AlertBody));
        }
        Alert::TimerDone => {
            g::bell(p, icon_cx, cy, h * 0.68, ORANGE.alpha(a));
            pair("Timer", "Done", ORANGE);
        }
    }
}

// ---------------------------------------------------------------------------
// Expanded panels
// ---------------------------------------------------------------------------

pub fn media_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) -> Rect {
    let Some(now) = v.now else { return Rect::default() };
    let f = v.fonts;
    let art = Rect::new(r.x + PAD, r.y + PAD, 56.0, 56.0);
    artwork(p, art, 12.0, v, a);
    let wave = Rect::new(r.right() - PAD - 22.0, art.cy() - 9.0, 22.0, 18.0);
    waveform(p, wave, v, a);
    let tx = art.right() + 14.0;
    let tw = wave.x - 12.0 - tx;
    let title = if now.title.is_empty() { "Not Playing" } else { &now.title };
    p.text(title, &f.title, Rect::new(tx, art.y + 7.0, tw, 20.0), WHITE.alpha(a));
    p.text(&now.artist, &f.body, Rect::new(tx, art.y + 28.0, tw, 19.0), SECONDARY.alpha(a));
    hits.push((Rect::new(art.x, art.y, wave.x - art.x, art.h), Hit::Art));

    // Progress: elapsed · bar · remaining.
    let row = r.y + 100.0;
    let pos = match v.scrub {
        Some(fr) => fr * now.duration,
        None => now.position_now(),
    };
    let frac = if now.duration > 0.0 { (pos / now.duration).clamp(0.0, 1.0) } else { 0.0 };
    let time_w = 42.0;
    let bar = Rect::new(r.x + PAD + time_w, row - 3.0, r.w - 2.0 * (PAD + time_w), 6.0);
    let thick = lerp(5.0, 8.0, v.bar_hover);
    let track = Rect::new(bar.x, row - thick / 2.0, bar.w, thick);
    p.fill_round(track, thick / 2.0, Color::gray(255, 0.2 * a));
    if now.duration > 0.0 {
        p.fill_round(Rect::new(track.x, track.y, (track.w * frac).max(thick), thick), thick / 2.0, Color::gray(255, 0.92 * a));
        let tc = SECONDARY.alpha(a);
        p.text_tabular(&fmt_time(pos), &f.small_l, Rect::new(r.x + PAD, row - 8.0, time_w, 16.0), tc);
        let left = format!("-{}", fmt_time(now.duration - pos));
        p.text_tabular(&left, &f.small_r, Rect::new(bar.right(), row - 8.0, time_w, 16.0), tc);
    }
    if now.can_seek {
        hits.push((bar.inset(-4.0, -8.0), Hit::Progress));
    }

    let cy = r.y + 138.0;
    let cx = r.cx();
    for (x, hit, enabled) in [(cx - 72.0, Hit::Prev, now.can_prev), (cx, Hit::Play, true), (cx + 72.0, Hit::Next, now.can_next)] {
        let (hot, pressed) = button_state(v, hit, enabled);
        if hot {
            p.fill_circle(x, cy, 21.0, Color::gray(255, if pressed { 0.16 } else { 0.09 } * a));
        }
        let c = Color::gray(255, 0.96 * a * if enabled { 1.0 } else { 0.32 });
        let k = if pressed { 0.88 } else { 1.0 };
        match hit {
            Hit::Play if now.playing => g::pause(p, x, cy, 20.0 * k, c),
            Hit::Play => g::play(p, x, cy, 21.0 * k, c),
            Hit::Prev => g::skip(p, x, cy, 17.0 * k, c, true),
            _ => g::skip(p, x, cy, 17.0 * k, c, false),
        }
        if enabled {
            hits.push((Rect::new(x - 24.0, cy - 22.0, 48.0, 44.0), hit));
        }
    }
    bar
}

pub fn timer_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let Some(c) = v.clock else { return };
    let f = v.fonts;
    let label = if c.is_timer() { "Timer" } else { "Stopwatch" };
    p.text(label, &f.header, Rect::new(r.x + 24.0, r.y + 16.0, 160.0, 16.0), ORANGE.alpha(a));
    let digits = ORANGE.alpha(if c.running { a } else { a * 0.6 });
    p.text_tabular(&c.label(), &f.big, Rect::new(r.x + 22.0, r.y + 34.0, r.w - 150.0, 44.0), digits);
    let cy = r.y + r.h / 2.0 + 2.0;
    let (x1, x2) = (r.right() - 104.0, r.right() - 46.0);
    for (x, hit) in [(x1, Hit::TimerToggle), (x2, Hit::TimerCancel)] {
        let (hot, pressed) = button_state(v, hit, true);
        let k = if pressed { 0.92 } else { 1.0 };
        let (bg, fg) = match hit {
            Hit::TimerToggle => (ORANGE.alpha(if hot { 0.34 } else { 0.24 } * a), ORANGE.alpha(a)),
            _ => (Color::gray(255, if hot { 0.26 } else { 0.18 } * a), WHITE.alpha(a)),
        };
        p.fill_circle(x, cy, 23.0 * k, bg);
        match hit {
            Hit::TimerToggle if c.running => g::pause(p, x, cy, 15.0 * k, fg),
            Hit::TimerToggle => g::play(p, x, cy, 16.0 * k, fg),
            _ => g::xmark(p, x, cy, 11.0 * k, fg, 2.2),
        }
        hits.push((Rect::new(x - 25.0, cy - 25.0, 50.0, 50.0), hit));
    }
}

pub fn shelf_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let f = v.fonts;
    let hy = r.y + 14.0;
    g::tray(p, r.x + 30.0, hy + 9.0, 15.0, WHITE.alpha(a));
    p.text("Shelf", &f.header, Rect::new(r.x + 44.0, hy, 80.0, 18.0), WHITE.alpha(a));
    if !v.shelf.is_empty() {
        let n = v.shelf.len();
        let count = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
        p.text(&count, &f.small_l, Rect::new(r.x + 92.0, hy, 80.0, 18.0), SECONDARY.alpha(a));
        let clear = Rect::new(r.right() - 74.0, hy - 2.0, 54.0, 22.0);
        let hot = v.hot == Some(Hit::ShelfClear);
        p.fill_round(clear, 11.0, Color::gray(255, if hot { 0.22 } else { 0.12 } * a));
        p.text("Clear", &f.chip, clear, WHITE.alpha(a));
        hits.push((clear, Hit::ShelfClear));
    }
    let body = Rect::new(r.x + 16.0, r.y + 44.0, r.w - 32.0, r.h - 58.0);
    if v.shelf.is_empty() || v.drag_over {
        let glow = if v.drag_over { 0.55 } else { 0.22 };
        dashed_round(p, body, 14.0, Color::gray(255, glow * a));
        if v.drag_over {
            p.fill_round(body, 14.0, v.accent.alpha(0.14 * a));
        }
        let msg = if v.drag_over { "Drop to hold here" } else { "Drag files here to hold them" };
        p.text(msg, &f.chip, body, Color::gray(255, if v.drag_over { 0.9 } else { 0.5 } * a));
        if !v.drag_over {
            return;
        }
    }
    if v.drag_over {
        return;
    }
    let tile_w = 68.0;
    let max = ((body.w / tile_w) as usize).max(1);
    let overflow = v.shelf.len() > max;
    let shown = if overflow { max - 1 } else { v.shelf.len() };
    for (i, path) in v.shelf.iter().take(shown).enumerate() {
        let tile = Rect::new(body.x + i as f32 * tile_w, body.y, tile_w, body.h);
        let hot = matches!(v.hot, Some(Hit::ShelfItem(j)) | Some(Hit::ShelfRemove(j)) if j == i);
        if hot {
            p.fill_round(tile.inset(2.0, 0.0), 10.0, Color::gray(255, 0.1 * a));
        }
        let icon = Rect::new(tile.cx() - 20.0, tile.y + 6.0, 40.0, 40.0);
        match v.shelf_icons.get(path) {
            Some(bmp) => p.bitmap(bmp, icon, a),
            None => p.fill_round(icon, 8.0, Color::gray(255, 0.15 * a)),
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        p.text(&name, &f.small_c, Rect::new(tile.x + 3.0, icon.bottom() + 4.0, tile.w - 6.0, 15.0), Color::gray(255, 0.85 * a));
        hits.push((tile, Hit::ShelfItem(i)));
        if hot {
            let x = Rect::new(icon.right() - 6.0, icon.y - 6.0, 16.0, 16.0);
            p.fill_circle(x.cx(), x.cy(), 8.0, Color::gray(90, a));
            g::xmark(p, x.cx(), x.cy(), 6.0, WHITE.alpha(a), 1.6);
            hits.push((x.inset(-2.0, -2.0), Hit::ShelfRemove(i)));
        }
    }
    if overflow {
        let tile = Rect::new(body.x + shown as f32 * tile_w, body.y, tile_w, body.h);
        let more = format!("+{}", v.shelf.len() - shown);
        p.fill_round(Rect::new(tile.cx() - 20.0, tile.y + 6.0, 40.0, 40.0), 10.0, Color::gray(255, 0.12 * a));
        p.text(&more, &f.chip, Rect::new(tile.cx() - 20.0, tile.y + 6.0, 40.0, 40.0), WHITE.alpha(a));
        hits.push((tile, Hit::ShelfItem(usize::MAX)));
    }
}

pub fn calendar_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let f = v.fonts;
    p.text("Up Next", &f.header, Rect::new(r.x + 22.0, r.y + 14.0, 160.0, 16.0), RED.alpha(a));
    let now = super::calendar::unix_now();
    for (i, e) in v.events.iter().filter(|e| e.end > now).take(3).enumerate() {
        let row = Rect::new(r.x + 12.0, r.y + 38.0 + i as f32 * 44.0, r.w - 24.0, 40.0);
        if v.hot == Some(Hit::Event(i)) {
            p.fill_round(row, 10.0, Color::gray(255, 0.08 * a));
        }
        let ongoing = e.ongoing();
        p.fill_round(Rect::new(row.x + 8.0, row.y + 6.0, 4.0, row.h - 12.0), 2.0, if ongoing { RED } else { RED.alpha(0.6) }.alpha(a));
        let tx = row.x + 22.0;
        let when = e.when();
        let ww = p.gfx.measure(&when, &f.small_r) + 8.0;
        p.text(&e.title, &f.alert_title, Rect::new(tx, row.y + 3.0, row.w - 34.0 - ww, 18.0), WHITE.alpha(a));
        p.text(&when, &f.small_r, Rect::new(row.x, row.y + 3.0, row.w - 10.0, 18.0), if e.starts_in() <= 300 { ORANGE } else { SECONDARY }.alpha(a));
        let sub = if e.location.is_empty() { e.span() } else { format!("{} · {}", e.span(), e.location) };
        p.text(&sub, &f.small_l, Rect::new(tx, row.y + 21.0, row.w - 32.0, 15.0), SECONDARY.alpha(a));
        hits.push((row, Hit::Event(i)));
    }
}

pub fn claude_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let _ = hits;
    let Some(s) = v.claude else { return };
    let f = v.fonts;
    let waiting = matches!(s.state, State::Waiting(_));
    g::spark(p, r.x + 30.0, r.y + 26.0, 18.0, spark_angle(v), CLAUDE.alpha(a));
    p.text("Claude Code", &f.header, Rect::new(r.x + 46.0, r.y + 17.0, 120.0, 18.0), WHITE.alpha(a));
    let more = if v.claude_more > 0 { format!("  +{} more", v.claude_more) } else { String::new() };
    p.text(&format!("{}{more}", s.project), &f.small_r, Rect::new(r.x, r.y + 18.0, r.w - 22.0, 16.0), SECONDARY.alpha(a));

    // What it's doing now.
    let (line, color) = match &s.state {
        State::Waiting(m) => (if m.is_empty() { "Needs your input".to_string() } else { m.clone() }, ORANGE),
        _ => (s.status(), WHITE),
    };
    p.text(&line, &f.title, Rect::new(r.x + 22.0, r.y + 46.0, r.w - 44.0, 22.0), color.alpha(a));
    if !s.prompt.is_empty() {
        let quoted = format!("\u{201C}{}\u{201D}", s.prompt);
        p.text(&quoted, &f.body, Rect::new(r.x + 22.0, r.y + 70.0, r.w - 44.0, 18.0), SECONDARY.alpha(a));
    }

    // Elapsed and tool count, with a shimmer bar while it works.
    let y = r.bottom() - 26.0;
    let stat = format!("{} · {} tool{}", fmt_time(s.elapsed()), s.tools, if s.tools == 1 { "" } else { "s" });
    p.text_tabular(&stat, &f.small_l, Rect::new(r.x + 22.0, y - 8.0, 140.0, 16.0), SECONDARY.alpha(a));
    let bar = Rect::new(r.x + 150.0, y - 2.0, r.w - 172.0, 4.0);
    p.fill_round(bar, 2.0, Color::gray(255, 0.12 * a));
    if !waiting {
        let t = (v.time * 0.6).fract();
        let w = bar.w * 0.28;
        let x = bar.x - w + (bar.w + w) * t;
        let seg = Rect::new(x.max(bar.x), bar.y, (x + w).min(bar.right()) - x.max(bar.x), bar.h);
        if seg.w > 0.0 {
            p.fill_round(seg, 2.0, CLAUDE.alpha(0.9 * a));
        }
    } else {
        p.fill_round(bar, 2.0, ORANGE.alpha(0.6 * a));
    }
}

pub const CHIPS: [(u32, &str); 4] = [(300, "5 min"), (600, "10 min"), (1500, "25 min"), (0, "Stopwatch")];

pub fn home_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let f = v.fonts;
    let now = chrono::Local::now();
    p.text(&now.format("%-I:%M").to_string(), &f.big, Rect::new(r.x + 22.0, r.y + 12.0, 150.0, 44.0), WHITE.alpha(a));
    p.text(&now.format("%A, %b %-d").to_string(), &f.body, Rect::new(r.x + 24.0, r.y + 54.0, 160.0, 18.0), SECONDARY.alpha(a));

    // Next event on the right.
    let card = Rect::new(r.x + r.w * 0.5, r.y + 18.0, r.w * 0.5 - 20.0, 50.0);
    g::calendar(p, card.x + 10.0, card.y + 12.0, 16.0, WHITE.alpha(a), RED.alpha(a));
    let unix = super::calendar::unix_now();
    match v.events.iter().find(|e| e.end > unix) {
        Some(e) => {
            p.text(&e.title, &f.alert_title, Rect::new(card.x + 24.0, card.y + 2.0, card.w - 24.0, 20.0), WHITE.alpha(a));
            let when = if e.ongoing() { "Now".to_string() } else { format!("{} · {}", e.when(), e.clock()) };
            p.text(&when, &f.small_l, Rect::new(card.x + 24.0, card.y + 22.0, card.w - 24.0, 16.0), SECONDARY.alpha(a));
            hits.push((card, Hit::Event(0)));
        }
        None => {
            p.text("No more events", &f.body, Rect::new(card.x + 24.0, card.y + 2.0, card.w - 24.0, 20.0), SECONDARY.alpha(a));
        }
    }

    // Quick timers.
    let y = r.bottom() - 44.0;
    let gap = 8.0;
    let w = (r.w - 36.0 - gap * (CHIPS.len() as f32 - 1.0)) / CHIPS.len() as f32;
    for (i, (secs, label)) in CHIPS.iter().enumerate() {
        let chip = Rect::new(r.x + 18.0 + i as f32 * (w + gap), y, w, 28.0);
        let (hot, pressed) = button_state(v, Hit::Chip(*secs), true);
        let bg = if pressed { 0.26 } else if hot { 0.2 } else { 0.12 };
        p.fill_round(chip, 14.0, Color::gray(255, bg * a));
        p.text(label, &f.chip, chip, if *secs == 0 { WHITE } else { ORANGE }.alpha(a));
        hits.push((chip, Hit::Chip(*secs)));
    }
}

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

fn button_state(v: &View, hit: Hit, enabled: bool) -> (bool, bool) {
    let hot = enabled && v.hot == Some(hit);
    (hot, hot && v.down == Some(hit))
}

fn artwork(p: &Painter, r: Rect, radius: f32, v: &View, a: f32) {
    if let (Some(old), true) = (v.old_art, v.art_fade < 1.0) {
        p.bitmap_round(old, r, radius, a * (1.0 - v.art_fade));
    }
    match v.art {
        Some(bmp) => p.bitmap_round(bmp, r, radius, a * v.art_fade),
        None => {
            p.fill_round(r, radius, Color::gray(255, 0.14 * a));
            let (cx, cy, s) = (r.cx(), r.cy(), r.w / 2.0);
            let c = Color::gray(255, 0.7 * a);
            p.fill_circle(cx - s * 0.28, cy + s * 0.32, s * 0.2, c);
            p.line(cx - s * 0.1, cy + s * 0.32, cx - s * 0.1, cy - s * 0.42, c, (s * 0.1).max(1.0));
            p.line(cx - s * 0.1, cy - s * 0.42, cx + s * 0.32, cy - s * 0.24, c, (s * 0.1).max(1.0));
        }
    }
}

/// Bars that dance while playing and settle into dots when paused.
fn waveform(p: &Painter, r: Rect, v: &View, a: f32) {
    const N: usize = 5;
    const SPEED: [f32; N] = [5.1, 6.7, 4.3, 7.9, 5.8];
    const PHASE: [f32; N] = [0.0, 1.7, 3.1, 0.9, 2.4];
    let bar_w = r.w / (N as f32 * 2.0 - 1.0);
    let tint = v.now.and_then(|n| n.art.as_ref()).map(|art| art.tint).unwrap_or(Color::gray(255, 0.92));
    let color = tint.alpha(a);
    for i in 0..N {
        let t = v.wave_t;
        let n = 0.5 * (t * SPEED[i] + PHASE[i]).sin()
            + 0.3 * (t * SPEED[i] * 1.93 + PHASE[i] * 2.1).sin()
            + 0.2 * (t * SPEED[i] * 0.47 + 1.3).sin();
        // Middle bars run taller, like a real spectrum.
        let shape = 1.0 - ((i as f32 - (N as f32 - 1.0) / 2.0).abs() / N as f32) * 0.6;
        let level = (0.22 + 0.78 * (n * 0.5 + 0.5)) * shape * v.energy;
        let h = (r.h * level).max(bar_w);
        let x = r.x + i as f32 * bar_w * 2.0;
        p.fill_round(Rect::new(x, r.cy() - h / 2.0, bar_w, h), bar_w / 2.0, color);
    }
}

/// The classic ring of fading dots.
fn spinner(p: &Painter, cx: f32, cy: f32, r: f32, t: f32, c: Color) {
    let head = (t * 10.0) as usize % 8;
    for i in 0..8 {
        let ang = i as f32 * std::f32::consts::FRAC_PI_4;
        let age = (head + 8 - i) % 8;
        let k = 1.0 - age as f32 / 8.0;
        p.fill_circle(cx + ang.sin() * r, cy - ang.cos() * r, r * 0.26, c.alpha(c.a * (0.2 + 0.8 * k)));
    }
}

fn dashed_round(p: &Painter, r: Rect, radius: f32, c: Color) {
    // A dash pattern along the straight edges; corners stay solid.
    p.stroke_round(r, radius, c.alpha(c.a * 0.35), 1.2);
    let (dash, gap) = (8.0, 6.0);
    let mut x = r.x + radius;
    while x + dash < r.right() - radius {
        p.line(x, r.y, x + dash, r.y, c, 1.4);
        p.line(x, r.bottom(), x + dash, r.bottom(), c, 1.4);
        x += dash + gap;
    }
}

pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b < 1e6 {
        format!("{:.0} KB", (b / 1e3).max(1.0))
    } else if b < 1e9 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.2} GB", b / 1e9)
    }
}
