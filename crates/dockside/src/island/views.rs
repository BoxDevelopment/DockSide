//! What's drawn inside the pill: compact activities, alerts, the secondary
//! bubble and the expanded panels. Every function paints into the pill rect it
//! is given (which springs around), at opacity `a`, and records hit rects.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::DirectWrite::{IDWriteTextFormat, DWRITE_TEXT_METRICS};

use super::calendar::CalEvent;
use super::claude::{Agent, Card, Kind, Limit, Session, State};
use super::glyphs as g;
use super::holding::{self, Item};
use super::live::{Activity, Live};
use super::media::NowPlaying;
use super::notifications::{self, Note};
use super::timer::Clock;
use super::{fmt_time, lerp, Alert, Download, Fonts, Hit, NoteRow, Persist, Tile, BLUE, CLAUDE, GREEN, ORANGE, PURPLE, RED, SECONDARY, WHITE};
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
    /// How far the artwork and title have been swiped (or are flying), in points.
    pub art_shift: f32,
    pub wave_t: f32,
    pub energy: f32,
    pub hot: Option<Hit>,
    pub down: Option<Hit>,
    pub scrub: Option<f32>,
    pub bar_hover: f32,
    /// Desktop indicator: sliding position (desktop index) and its velocity.
    pub desk_pos: f32,
    pub desk_vel: f32,
    /// Caps Lock switch knob position: 0 off, 1 on (overshoots a little).
    pub caps_knob: f32,
    /// What the home panel shows: quick timer buttons and the next event.
    pub home_timers: bool,
    pub home_events: bool,
    pub clock: Option<&'a Clock>,
    pub shelf: &'a [Item],
    pub shelf_icons: &'a HashMap<PathBuf, ID2D1Bitmap>,
    /// Selected shelf files, how far the row has scrolled, and Quick Look's file and picture.
    pub selected: &'a [PathBuf],
    pub shelf_scroll: f32,
    pub preview: Option<&'a Path>,
    pub preview_bmp: Option<&'a ID2D1Bitmap>,
    /// How long held files last (`None`: no expiry badges).
    pub shelf_lifetime: Option<Duration>,
    pub wall: SystemTime,
    pub drag_over: bool,
    /// While a file drag is over: the Quick Action tiles (and whether each
    /// takes the files), the target under the pointer and the files.
    pub tiles: &'a [(Tile, bool)],
    pub drop_hot: Option<Tile>,
    pub drop_paths: &'a [PathBuf],
    pub events: &'a [CalEvent],
    pub downloads: &'a [Download],
    pub volume: (f32, bool),
    /// Now Playing's lyrics: whether the panel has the row, the line being
    /// sung and the one before it (crossfading by `lyric_fade`).
    pub lyrics_row: bool,
    pub lyric: Option<&'a str>,
    pub lyric_prev: Option<&'a str>,
    pub lyric_fade: f32,
    /// What's running, for the home page's row (at most a few).
    pub home_acts: &'a [Persist],
    /// The Widgets page's readings (`None` until it's first opened).
    pub weather: Option<&'a crate::widgets::sources::WeatherState>,
    pub devices: &'a [(String, u8)],
    pub sys: &'a crate::widgets::sensors::SysStats,
    pub zones: &'a [crate::widgets::sources::Zone],
    pub h24: bool,
    pub battery: Option<crate::win::system::Battery>,
    /// Discord voice, and members' pictures by user id.
    pub discord: &'a super::discord::Snapshot,
    pub discord_avatars: &'a HashMap<String, ID2D1Bitmap>,
    /// The sound mixer's apps and their icons.
    pub mixer: &'a [super::mixer::App],
    pub mixer_icons: &'a HashMap<PathBuf, ID2D1Bitmap>,
    pub alert_img: Option<&'a ID2D1Bitmap>,
    pub accent: Color,
    pub time: f32,
    /// The Claude session to show (working, else just finished), how many
    /// others are working, the panel's page and the Usage page's limits.
    pub claude: Option<&'a Session>,
    pub claude_more: usize,
    pub claude_view: u8,
    pub claude_flip: f32,
    pub usage: &'a [(Agent, Limit)],
    pub live: &'a Live,
    pub live_icons: &'a HashMap<PathBuf, ID2D1Bitmap>,
    pub notes: &'a [Note],
    pub note_icons: &'a HashMap<u64, ID2D1Bitmap>,
    pub note_rows: &'a [NoteRow],
    /// One app's stack is opened out.
    pub note_open: bool,
    /// A row being swiped away: (row, start, offset).
    pub note_drag: Option<(usize, f32, f32)>,
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
        Persist::Discord => {
            let Some(vc) = v.discord.voice.as_ref() else { return };
            voice_icon(p, icon_cx, r.cy(), h * 0.62, v.discord, a);
            p.text(&vc.channel, &v.fonts.alert, Rect::new(inner.x, r.y, inner.w - 24.0, h), WHITE.alpha(a));
            // A steady green dot while in the call, pulsing while someone talks.
            // (Speaking flickers between words, so swapping it for anything else jitters.)
            let (cx, cy) = (r.right() - h * 0.5 - 4.0, r.cy());
            if vc.members.iter().any(|m| m.speaking) {
                let pulse = 0.5 + 0.5 * (v.time * 6.0).sin();
                p.fill_circle(cx, cy, h * 0.3, GREEN.alpha(0.25 * pulse * a));
            }
            p.fill_circle(cx, cy, h * 0.15, GREEN.alpha(a));
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
        Persist::Live(k) => {
            let Some(act) = v.live.get(k) else { return };
            live_icon(p, act, v, icon_cx, r.cy(), h * 0.7, a);
            let trail = act.trailing();
            let tw = if trail.is_empty() { 0.0 } else { p.gfx.measure(&trail, &v.fonts.alert_b) + 8.0 };
            p.text(&act.title, &v.fonts.alert, Rect::new(inner.x, r.y, r.w - (inner.x - r.x) - tw - h * 0.5, h), WHITE.alpha(a));
            let color = act.color.unwrap_or(v.accent);
            p.text_tabular(&trail, &v.fonts.alert_b, Rect::new(r.x, r.y, r.w - h * 0.5 - 2.0, h), color.alpha(a));
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
        Persist::Discord => voice_icon(p, cx, cy, d * 0.58, v.discord, a),
        Persist::Claude => g::spark(p, cx, cy, d * 0.62, spark_angle(v), CLAUDE.alpha(a)),
        Persist::Live(k) => {
            if let Some(act) = v.live.get(k) {
                live_icon(p, act, v, cx, cy, d * 0.62, a);
            }
        }
    }
}

/// A Live Activity's mark: its icon, else a ring filling with its progress, else a dot in its color.
fn live_icon(p: &Painter, act: &Activity, v: &View, cx: f32, cy: f32, s: f32, a: f32) {
    let color = act.color.unwrap_or(v.accent);
    match (act.icon.as_ref().and_then(|i| v.live_icons.get(i)), act.progress) {
        (Some(img), _) => p.bitmap_round(img, Rect::new(cx - s / 2.0, cy - s / 2.0, s, s), s * 0.24, a),
        (None, Some(f)) => g::timer(p, cx, cy, s * 0.9, f, color.alpha(a)),
        (None, None) => {
            p.fill_circle(cx, cy, s * 0.36, color.alpha(a));
            p.fill_circle(cx, cy, s * 0.13, WHITE.alpha(a));
        }
    }
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

/// An iOS-style switch ending at `right`: the track fills with `on_color` and
/// the knob slides across as `t` goes from 0 (off) to 1 (on).
fn toggle(p: &Painter, right: f32, cy: f32, t: f32, on_color: Color, a: f32) {
    const W: f32 = 36.0;
    const H: f32 = 21.0;
    let fill = t.clamp(0.0, 1.0);
    let off = Color::gray(255, 0.22);
    let mix = |x: f32, y: f32| x + (y - x) * fill;
    let track = Color {
        r: mix(off.r, on_color.r),
        g: mix(off.g, on_color.g),
        b: mix(off.b, on_color.b),
        a: mix(off.a, on_color.a),
    };
    let r = Rect::new(right - W, cy - H / 2.0, W, H);
    p.fill_round(r, H / 2.0, track.alpha(track.a * a));
    let knob_r = H / 2.0 - 2.0;
    let kx = r.x + H / 2.0 + t.clamp(-0.06, 1.06) * (W - H);
    p.fill_circle(kx, cy + 0.75, knob_r + 0.5, Color::gray(0, 0.25 * a));
    p.fill_circle(kx, cy, knob_r, WHITE.alpha(a));
}

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
        Alert::Caps => {
            g::caps(p, icon_cx, cy, h * 0.72, WHITE.alpha(a));
            p.text("Caps Lock", &f.alert, Rect::new(text_x, r.y, r.w * 0.6, r.h), WHITE.alpha(a));
            toggle(p, r.right() - right_inset, cy, v.caps_knob, GREEN, a);
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
        Alert::LiveDone { title, message, color } => {
            let c = color.unwrap_or(GREEN);
            g::check(p, icon_cx, cy, h * 0.6, c.alpha(a));
            let mw = p.gfx.measure(message, &f.alert_b) + 8.0;
            p.text(title, &f.alert, Rect::new(text_x, r.y, r.w - (text_x - r.x) - mw - right_inset, r.h), WHITE.alpha(a));
            p.text(message, &f.alert_b, Rect::new(r.x, r.y, r.w - right_inset, r.h), c.alpha(a));
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
        Alert::TextCopied(text) if text.is_empty() => {
            g::text_scan(p, icon_cx, cy, h * 0.66, SECONDARY.alpha(a));
            pair("Copy Text", "No text found", SECONDARY);
        }
        Alert::TextCopied(text) => {
            g::text_scan(p, icon_cx, cy, h * 0.66, GREEN.alpha(a));
            let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
            p.text("Copied", &f.alert_b, Rect::new(r.x, r.y, r.w - right_inset, r.h), GREEN.alpha(a));
            p.text(first, &f.alert, Rect::new(text_x, r.y, r.w - (text_x - r.x) - 80.0, r.h), WHITE.alpha(a));
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
    let wave = Rect::new(r.right() - PAD - 22.0, art.cy() - 9.0, 22.0, 18.0);
    waveform(p, wave, v, a);
    let tx = art.right() + 14.0;
    let tw = wave.x - 12.0 - tx;
    // The artwork and title move together as a card when swiped, fading as they go.
    let fade = a * (1.0 - v.art_shift.abs() / 150.0).clamp(0.0, 1.0);
    let card = Rect::new(r.x, art.y - 4.0, wave.x - 6.0 - r.x, art.h + 8.0);
    p.clipped(card, || {
        p.transformed(v.art_shift, 0.0, 1.0, || {
            artwork(p, art, 12.0, v, fade);
            let title = if now.title.is_empty() { "Not Playing" } else { &now.title };
            p.text(title, &f.title, Rect::new(tx, art.y + 7.0, tw, 20.0), WHITE.alpha(fade));
            p.text(&now.artist, &f.body, Rect::new(tx, art.y + 28.0, tw, 19.0), SECONDARY.alpha(fade));
        });
    });
    hits.push((Rect::new(art.x, art.y, wave.x - art.x, art.h), Hit::Art));

    // The line being sung: the new one rises in as the old one rises away.
    let ly = if v.lyrics_row { LYRICS_H } else { 0.0 };
    if v.lyrics_row {
        let line = Rect::new(r.x + PAD, art.bottom() + 8.0, r.w - 2.0 * PAD, 22.0);
        let k = v.lyric_fade.clamp(0.0, 1.0);
        p.clipped(line.inset(0.0, -2.0), || {
            if let Some(prev) = v.lyric_prev.filter(|_| k < 1.0) {
                lyric_line(p, line.offset(0.0, -line.h * k), prev, f, a * (1.0 - k) * (1.0 - k));
            }
            lyric_line(p, line.offset(0.0, line.h * 0.6 * (1.0 - k)), v.lyric.unwrap_or(""), f, a * k * k);
        });
    }

    // Progress: elapsed · bar · remaining.
    let row = r.y + 100.0 + ly;
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

    let cy = r.y + 138.0 + ly;
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

/// Width of one file on the shelf's row, in points.
pub const TILE_W: f32 = 76.0;
/// The row's inset from the panel's sides.
pub const SHELF_INSET: f32 = 16.0;

pub fn shelf_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    if let (Some(path), false) = (v.preview, v.drag_over) {
        return quick_look(p, r, path, v, a, hits);
    }
    let f = v.fonts;
    let hy = r.y + 14.0;
    g::tray(p, r.x + 30.0, hy + 9.0, 15.0, WHITE.alpha(a));
    p.text("Shelf", &f.header, Rect::new(r.x + 44.0, hy, 80.0, 18.0), WHITE.alpha(a));
    if !v.shelf.is_empty() {
        // The count: click selects everything, drag takes everything.
        let n = v.shelf.len();
        let picked = v.selected.len();
        let count = match (picked, n) {
            (0, 1) => "1 file".to_string(),
            (0, _) => format!("{n} files"),
            _ => format!("{picked} of {n} selected"),
        };
        let cw = p.gfx.measure(&count, &f.small_l) + 12.0;
        let cr = Rect::new(r.x + 86.0, hy - 1.0, cw, 20.0);
        let hot = v.hot == Some(Hit::ShelfAll);
        if hot {
            p.fill_round(cr, 10.0, Color::gray(255, 0.1 * a));
        }
        p.text(&count, &f.small_l, Rect::new(cr.x + 6.0, hy, cw, 18.0), Color::gray(255, if hot { 0.85 } else { 0.55 } * a));
        hits.push((cr, Hit::ShelfAll));
        let clear = Rect::new(r.right() - 74.0, hy - 2.0, 54.0, 22.0);
        let hot = v.hot == Some(Hit::ShelfClear);
        p.fill_round(clear, 11.0, Color::gray(255, if hot { 0.22 } else { 0.12 } * a));
        p.text("Clear", &f.chip, clear, WHITE.alpha(a));
        hits.push((clear, Hit::ShelfClear));
    }
    let body = Rect::new(r.x + SHELF_INSET, r.y + 44.0, r.w - 2.0 * SHELF_INSET, r.h - 58.0);
    if v.drag_over {
        return drop_targets(p, body, v, a, hits);
    }
    if v.shelf.is_empty() {
        dashed_round(p, body, 14.0, Color::gray(255, 0.22 * a));
        p.text("Drag files to the top of the screen to hold them", &f.chip, body, Color::gray(255, 0.5 * a));
        return;
    }
    hits.push((body, Hit::ShelfBody));
    // The hovered badge's tooltip, drawn over everything after the tiles.
    let mut tip: Option<(f32, f32, String)> = None;
    p.clipped(body, || {
        for (i, item) in v.shelf.iter().enumerate() {
            let x = body.x + i as f32 * TILE_W - v.shelf_scroll;
            if x + TILE_W <= body.x || x >= body.right() {
                continue;
            }
            let tile = Rect::new(x, body.y, TILE_W, body.h);
            // Only the part in view takes the pointer.
            let (x0, x1) = (tile.x.max(body.x), tile.right().min(body.right()));
            if x1 - x0 > 8.0 {
                hits.push((Rect::new(x0, tile.y, x1 - x0, tile.h), Hit::ShelfItem(i)));
            }
            let whole = tile.x >= body.x - 0.5 && tile.right() <= body.right() + 0.5;
            if let Some(t) = shelf_tile(p, tile, i, item, v, a, whole, hits) {
                tip = Some(t);
            }
        }
    });
    // A slim scroller under the row while it holds more than fits.
    let content = v.shelf.len() as f32 * TILE_W;
    if content > body.w + 0.5 {
        let track = Rect::new(body.x + 40.0, body.bottom() + 5.0, body.w - 80.0, 3.0);
        let thumb_w = (track.w * body.w / content).max(18.0);
        let k = (v.shelf_scroll / (content - body.w)).clamp(0.0, 1.0);
        p.fill_round(track, 1.5, Color::gray(255, 0.08 * a));
        p.fill_round(Rect::new(track.x + (track.w - thumb_w) * k, track.y, thumb_w, track.h), 1.5, Color::gray(255, 0.35 * a));
    }
    if let Some((cx, y, label)) = tip {
        let w = p.gfx.measure(&label, &f.small_c) + 18.0;
        let x = (cx - w / 2.0).clamp(r.x + 8.0, r.right() - 8.0 - w);
        let bubble = Rect::new(x, y, w, 20.0);
        p.fill_round(bubble.offset(0.0, 1.0), 10.0, Color::BLACK.alpha(0.4 * a));
        p.fill_round(bubble, 10.0, Color::gray(62, a));
        p.stroke_round(bubble.inset(0.25, 0.25), 10.0, Color::gray(255, 0.12 * a), 0.5);
        p.text(&label, &f.small_c, bubble, WHITE.alpha(a));
    }
}

/// One file on the shelf: its thumbnail and name (Finder's selection look),
/// a pin or expiry badge, and a remove button on hover. `whole`: entirely in
/// view, so its small buttons can take clicks. Returns the badge's tooltip
/// while it's hovered.
#[allow(clippy::too_many_arguments)]
fn shelf_tile(p: &Painter, tile: Rect, i: usize, item: &Item, v: &View, a: f32, whole: bool, hits: &mut Hits) -> Option<(f32, f32, String)> {
    let f = v.fonts;
    let selected = v.selected.contains(&item.path);
    let hot = matches!(v.hot, Some(Hit::ShelfItem(j) | Hit::ShelfRemove(j) | Hit::ShelfTimer(j)) if j == i);
    let icon = Rect::new(tile.cx() - 26.0, tile.y + 3.0, 52.0, 52.0);
    if selected || (hot && !item.pending) {
        p.fill_round(icon.inset(-5.0, -3.0), 10.0, Color::gray(255, if selected { 0.17 } else { 0.08 } * a));
    }
    match v.shelf_icons.get(&item.path) {
        Some(bmp) if !item.pending => p.bitmap(bmp, fit(bmp, icon, 1.0), a),
        _ => p.fill_round(icon.inset(6.0, 6.0), 8.0, Color::gray(255, 0.15 * a)),
    }
    if item.pending {
        spinner(p, icon.cx(), icon.cy(), 9.0, v.time, WHITE.alpha(a));
    }
    let name = item.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let line = Rect::new(tile.x + 3.0, icon.bottom() + 4.0, tile.w - 6.0, 15.0);
    if selected {
        let w = (p.gfx.measure(&name, &f.small_c) + 10.0).min(tile.w - 4.0);
        p.fill_round(Rect::new(tile.cx() - w / 2.0, line.y, w, line.h), line.h / 2.0, v.accent.alpha(a));
        p.text(&name, &f.small_c, line, WHITE.alpha(a));
    } else {
        let fade = if item.pending { 0.5 } else { 0.85 };
        p.text(&name, &f.small_c, line, Color::gray(255, fade * a));
    }
    if item.pending {
        return None;
    }
    let mut tip = None;
    // Badge: a pin, or a ring that empties as the file's time runs out.
    let (cx, cy) = (icon.x + 3.0, icon.bottom() - 3.0);
    let badge = Rect::new(cx - 9.0, cy - 9.0, 18.0, 18.0);
    let badge_hot = v.hot == Some(Hit::ShelfTimer(i));
    if item.pinned {
        p.fill_circle(cx, cy, 8.0, Color::gray(if badge_hot { 78 } else { 52 }, a));
        g::pin(p, cx, cy, 11.0, WHITE.alpha(a));
        if badge_hot {
            tip = Some((cx, badge.bottom() + 4.0, "Pinned".to_string()));
        }
    } else if let Some(life) = v.shelf_lifetime {
        let left = (item.added + life).duration_since(v.wall).unwrap_or_default();
        let frac = left.as_secs_f32() / life.as_secs_f32().max(1.0);
        let urgent = frac < 0.1 || left.as_secs() < 300;
        p.fill_circle(cx, cy, 8.0, Color::gray(if badge_hot { 78 } else { 52 }, a));
        g::timer(p, cx, cy, 15.0, frac, if urgent { ORANGE } else { WHITE }.alpha(a));
        if badge_hot {
            tip = Some((cx, badge.bottom() + 4.0, format!("Expires in {}", holding::remaining_label(left.as_secs()))));
        }
    }
    if whole && (item.pinned || v.shelf_lifetime.is_some()) {
        hits.push((badge, Hit::ShelfTimer(i)));
    }
    if hot && whole {
        let x = Rect::new(icon.right() - 8.0, icon.y - 4.0, 16.0, 16.0);
        p.fill_circle(x.cx(), x.cy(), 8.0, Color::gray(90, a));
        g::xmark(p, x.cx(), x.cy(), 6.0, WHITE.alpha(a), 1.6);
        hits.push((x.inset(-2.0, -2.0), Hit::ShelfRemove(i)));
    }
    tip
}

/// Quick Look (Space on the shelf): the file big, with its name, Open, and
/// arrows to step through the shelf.
fn quick_look(p: &Painter, r: Rect, path: &Path, v: &View, a: f32, hits: &mut Hits) {
    let f = v.fonts;
    let hy = r.y + 14.0;
    let close = Rect::new(r.x + 18.0, hy - 2.0, 22.0, 22.0);
    let hot = v.hot == Some(Hit::PreviewClose);
    p.fill_circle(close.cx(), close.cy(), 11.0, Color::gray(255, if hot { 0.22 } else { 0.12 } * a));
    g::xmark(p, close.cx(), close.cy(), 7.0, WHITE.alpha(a), 1.6);
    hits.push((close, Hit::PreviewClose));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    p.text(&name, &f.header, Rect::new(close.right() + 10.0, hy, r.w - 150.0, 18.0), WHITE.alpha(a));
    let open = Rect::new(r.right() - 74.0, hy - 2.0, 54.0, 22.0);
    let hot = v.hot == Some(Hit::PreviewOpen) && v.down.is_none();
    p.fill_round(open, 11.0, Color::gray(255, if hot { 0.22 } else { 0.12 } * a));
    p.text("Open", &f.chip, open, WHITE.alpha(a));
    hits.push((open, Hit::PreviewOpen));

    let area = Rect::new(r.x + 16.0, r.y + 46.0, r.w - 32.0, r.h - 46.0 - 40.0);
    match v.preview_bmp {
        Some(bmp) => {
            let img = fit(bmp, area, 1.0);
            p.bitmap_round(bmp, img, 10.0, a);
            // Click opens it; drag takes the file.
            hits.push((img, Hit::PreviewOpen));
        }
        None => {
            p.fill_round(area.inset(area.w / 2.0 - 40.0, area.h / 2.0 - 40.0), 14.0, Color::gray(255, 0.12 * a));
            hits.push((area, Hit::PreviewOpen));
        }
    }

    // Footer: where this file is in the shelf, its size, and the arrows.
    let files: Vec<&Item> = v.shelf.iter().filter(|i| !i.pending).collect();
    let at = files.iter().position(|i| i.path == path).unwrap_or(0);
    let cy = r.bottom() - 22.0;
    let size = std::fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| human_size(m.len()));
    let label = match size {
        Some(sz) => format!("{} of {}  ·  {sz}", at + 1, files.len()),
        None => format!("{} of {}", at + 1, files.len()),
    };
    p.text(&label, &f.small_c, Rect::new(r.x + 60.0, cy - 8.0, r.w - 120.0, 16.0), SECONDARY.alpha(a));
    for (dir, x) in [(-1, r.x + 34.0), (1, r.right() - 34.0)] {
        let can = if dir < 0 { at > 0 } else { at + 1 < files.len() };
        let hot = can && v.hot == Some(Hit::PreviewStep(dir));
        let c = Color::gray(255, if !can { 0.2 } else if hot { 1.0 } else { 0.7 } * a);
        if hot {
            p.fill_circle(x, cy, 13.0, Color::gray(255, 0.12 * a));
        }
        g::chevron(p, x, cy, 12.0, dir < 0, c);
        if can {
            hits.push((Rect::new(x - 15.0, cy - 15.0, 30.0, 30.0), Hit::PreviewStep(dir)));
        }
    }
}

/// `bmp` fitted inside `r`, keeping its shape and never scaled past `max`.
fn fit(bmp: &ID2D1Bitmap, r: Rect, max: f32) -> Rect {
    let s = unsafe { bmp.GetSize() };
    let k = (r.w / s.width.max(1.0)).min(r.h / s.height.max(1.0)).min(max);
    let (w, h) = (s.width * k, s.height * k);
    Rect::new(r.cx() - w / 2.0, r.cy() - h / 2.0, w, h)
}

/// While files are dragged over the shelf: a zone that holds them, and a tile
/// per Quick Action. The one under the pointer lights up; tiles that can't take
/// these files are dimmed.
fn drop_targets(p: &Painter, body: Rect, v: &View, a: f32, hits: &mut Hits) {
    const ACTION_W: f32 = 76.0;
    const GAP: f32 = 8.0;
    let f = v.fonts;
    let hold = Rect::new(body.x, body.y, body.w - v.tiles.len() as f32 * (ACTION_W + GAP), body.h);
    let hot = v.drop_hot == Some(Tile::Hold);
    if hot {
        p.fill_round(hold, 14.0, v.accent.alpha(0.16 * a));
    }
    dashed_round(p, hold, 14.0, Color::gray(255, if hot { 0.6 } else { 0.3 } * a));
    let ink = Color::gray(255, if hot { 0.95 } else { 0.6 } * a);
    g::tray(p, hold.cx(), hold.cy() - 9.0, 22.0, ink);
    let n = v.drop_paths.len();
    let label = if n > 1 { format!("Hold {n} files") } else { "Hold".to_string() };
    p.text(&label, &f.chip, Rect::new(hold.x, hold.cy() + 8.0, hold.w, 18.0), ink);
    hits.push((hold, Hit::Drop(Tile::Hold)));
    for (i, &(tile, ok)) in v.tiles.iter().enumerate() {
        let t = Rect::new(hold.right() + GAP + i as f32 * (ACTION_W + GAP), body.y, ACTION_W, body.h);
        let hot = ok && v.drop_hot == Some(tile);
        let fade = if ok { 1.0 } else { 0.35 };
        // The tile under the pointer swells a little, like a Dock icon taking a drop.
        p.scaled_about(t.cx(), t.cy(), if hot { 1.05 } else { 1.0 }, || {
            p.fill_round(t, 14.0, if hot { v.accent.alpha(0.9 * a) } else { Color::gray(255, 0.1 * fade * a) });
            let c = Color::gray(255, if hot { 1.0 } else { 0.85 } * fade * a);
            let (cx, cy) = (t.cx(), t.cy() - 9.0);
            match tile {
                Tile::Convert => g::convert(p, cx, cy, 22.0, c),
                Tile::Text => g::text_scan(p, cx, cy, 22.0, c),
                Tile::Zip => g::archive(p, cx, cy, 22.0, c),
                Tile::Hold => g::tray(p, cx, cy, 22.0, c),
            }
            p.text(tile.label(), &f.small_c, Rect::new(t.x + 2.0, t.cy() + 8.0, t.w - 4.0, 16.0), c);
        });
        hits.push((t, Hit::Drop(tile)));
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

/// Droppy's Claude colors (sampled from Droppy itself).
const CL_ORANGE: Color = Color::rgba(232, 138, 94, 1.0);
const CL_MASCOT: Color = Color::rgba(214, 120, 100, 1.0);
const CL_MINT: Color = Color::rgba(96, 224, 150, 1.0);
const CL_GREEN: Color = Color::rgba(88, 212, 140, 1.0);
const CL_RED: Color = Color::rgba(255, 105, 97, 1.0);
const CL_TILE: Color = Color::rgba(217, 105, 82, 1.0);
const CODEX_BLUE: Color = Color::rgba(90, 155, 245, 1.0);

/// White at `k` of full strength, faded by `a`.
fn ink(k: f32, a: f32) -> Color {
    Color::gray(255, k * a)
}

/// Claude Code, open, laid out as Droppy's: a Progress page (the turn's steps
/// and the latest tool call in detail) and a Usage page (how much of each plan
/// is left), with a round button for each in the corner. Points are measured
/// off Droppy's 549 × 231 panel.
pub fn claude_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    const SLIDE: f32 = 28.0;
    let f = v.fonts;
    let hy = r.y + 37.0;
    // 0 on Progress, 1 on Usage, gliding between: the pages cross-fade and slide.
    let t = v.claude_flip.clamp(0.0, 1.0);
    let shown = [1.0 - t, t];
    for (page, k) in shown.into_iter().enumerate() {
        if k < 0.01 {
            continue;
        }
        let (dx, c) = ((page as f32 - t) * SLIDE * 0.4, ink(0.6, a * k));
        p.transformed(dx, 0.0, 1.0, || {
            if page == 0 {
                g::checklist(p, r.x + 31.5, hy, 17.0, c);
            } else {
                g::gauge(p, r.x + 31.5, hy, 17.0, c);
            }
            let name = if page == 0 { "Progress" } else { "Usage" };
            p.text(name, &f.cl_head, Rect::new(r.x + 49.5, hy - 12.0, 160.0, 24.0), c);
        });
    }
    for i in 0..2u8 {
        let cx = r.right() - if i == 0 { 74.0 } else { 36.75 };
        let hit = Hit::ClaudeView(i);
        let on = shown[i as usize];
        let (hot, pressed) = button_state(v, hit, v.claude_view != i);
        let off = ink(if pressed { 0.2 } else if hot { 0.17 } else { 0.125 }, a);
        p.fill_circle(cx, hy, 15.25, off.mix(CL_MINT.alpha(0.25 * a), on));
        let c = ink(0.6, a).mix(CL_MINT.alpha(a), on);
        if i == 0 {
            g::checklist(p, cx, hy, 16.0, c);
        } else {
            g::gauge(p, cx, hy, 16.0, c);
        }
        hits.push((Rect::new(cx - 17.0, hy - 17.0, 34.0, 34.0), hit));
    }
    if shown[0] > 0.01 {
        p.transformed(-t * SLIDE, 0.0, 1.0, || claude_progress(p, r, v, a * shown[0]));
    }
    if shown[1] > 0.01 {
        p.transformed((1.0 - t) * SLIDE, 0.0, 1.0, || claude_usage(p, r, v, a * shown[1], t));
    }
}

/// The Progress page's rows: the turn's last few steps, then what it's doing between them.
fn step_rows(s: &Session) -> Vec<(&str, bool)> {
    let mut rows: Vec<(&str, bool)> = s.steps.iter().map(|x| (x.label.as_str(), x.done)).collect();
    let busy = s.steps.iter().any(|x| !x.done);
    match &s.state {
        State::Waiting(_) => rows.push(("Waiting for you", false)),
        State::Thinking if !busy => rows.push(("Thinking...", false)),
        State::Idle if rows.is_empty() => rows.push(("Finished", true)),
        _ => {}
    }
    let skip = rows.len().saturating_sub(3);
    rows.split_off(skip)
}

fn claude_progress(p: &Painter, r: Rect, v: &View, a: f32) {
    let Some(s) = v.claude else { return };
    let f = v.fonts;
    // Who, and what the session is about.
    let ty = r.y + 86.0;
    g::clawd(p, r.x + 40.0, r.y + 86.5, 19.0, CL_MASCOT.alpha(a));
    let nx = r.x + 68.5;
    p.text("Claude", &f.cl_name, Rect::new(nx, ty - 15.0, 120.0, 30.0), ink(1.0, a));
    let dot = nx + p.gfx.measure("Claude", &f.cl_name) + 16.0;
    p.fill_circle(dot, ty, 4.0, CL_ORANGE.alpha(a));
    let more = if v.claude_more > 0 { format!("+{} more", v.claude_more) } else { String::new() };
    let mw = if more.is_empty() { 0.0 } else { p.gfx.measure(&more, &f.cl_status) + 12.0 };
    let tr = Rect::new(dot + 10.5, ty - 12.0, r.right() - 24.0 - mw - dot - 10.5, 24.0);
    p.text(s.heading(), &f.cl_title, tr, CL_ORANGE.alpha(a));
    if !more.is_empty() {
        p.text(&more, &f.cl_status, Rect::new(r.right() - 24.0 - mw + 12.0, ty - 10.0, mw, 20.0), ink(0.6, a));
    }

    // Its steps on the left; the latest in detail on the right.
    let card = Rect::new(r.x + 283.5, r.y + 113.0, 244.0, 93.0);
    for (i, (label, done)) in step_rows(s).into_iter().enumerate() {
        let cy = r.y + 123.5 + i as f32 * 28.75;
        let waiting = !done && matches!(s.state, State::Waiting(_));
        let mark = if waiting { ORANGE } else { CL_ORANGE }.alpha(a);
        if done {
            g::tick(p, r.x + 29.75, cy, 16.0, mark);
        } else {
            g::play(p, r.x + 30.75, cy, 12.5, mark);
        }
        let c = if waiting { ORANGE.alpha(a) } else { ink(0.89, a) };
        // Long names keep both ends ("Write Releas...tesPage.swift").
        let w = card.x - 24.0 - r.x - 49.5;
        p.text(&middle_fit(p, label, &f.cl_step, w, "..."), &f.cl_step, Rect::new(r.x + 49.5, cy - 12.0, w + 4.0, 24.0), c);
    }
    claude_card(p, card, s, v, a);
}

/// The card beside the steps: the tool running (or just finished), else what
/// Claude last said, what it's waiting on, or how the turn went.
fn claude_card(p: &Painter, c: Rect, s: &Session, v: &View, a: f32) {
    let f = v.fonts;
    p.fill_round(c, 20.0, ink(0.04, a));
    let tool = s.card.as_ref().filter(|k| s.working() && k.end.map_or(true, |e| e.elapsed().as_secs_f32() < 1.2));
    let text = c.inset(40.0, 12.0);
    match (&s.state, tool) {
        (State::Waiting(m), _) => {
            let m = if m.is_empty() { "Claude needs your input" } else { m };
            centered_block(p, m, &f.cl_said, text, ORANGE.alpha(a));
        }
        (State::Idle, _) => {
            let said = if s.said.is_empty() { "All done" } else { &s.said };
            centered_block(p, said, &f.cl_said, Rect::new(text.x, c.y + 10.0, text.w, c.h - 36.0), ink(0.81, a));
            let n = s.tools;
            let stat = format!("Done in {} \u{B7} {n} tool{}", fmt_time(s.elapsed()), if n == 1 { "" } else { "s" });
            p.text(&stat, &f.small_c, Rect::new(c.x, c.bottom() - 26.0, c.w, 16.0), ink(0.5, a));
        }
        (_, Some(k)) => tool_card(p, c, k, v, a),
        _ => {
            let t = if !s.said.is_empty() {
                s.said.clone()
            } else if !s.prompt.is_empty() {
                s.prompt.clone()
            } else {
                "Thinking...".to_string()
            };
            centered_block(p, &t, &f.cl_said, text, ink(0.81, a));
        }
    }
}

/// A tool call: its icon and target (with lines added and removed), how it's
/// going, and a bar that fills as it finishes.
fn tool_card(p: &Painter, c: Rect, k: &Card, v: &View, a: f32) {
    let f = v.fonts;
    let (ix, iy) = (c.x + 24.75, c.y + 29.25);
    let hole = Color::BLACK.alpha(0.8 * a);
    let accent = CL_ORANGE.alpha(a);
    match k.kind {
        Kind::Terminal => g::terminal(p, ix + 2.75, iy, 17.0, accent, hole),
        Kind::File => g::page(p, ix, iy, 15.5, accent, hole),
        Kind::Search => g::magnifier(p, ix, iy, 16.0, accent),
        Kind::Web => g::globe(p, ix, iy, 16.0, accent),
        Kind::Agent | Kind::Other => {
            let turn = if k.end.is_none() { v.time * 1.6 } else { 0.0 };
            g::spark(p, ix, iy, 17.0, turn, accent);
        }
    }
    // Lines in and out, at the right.
    let mut right = c.x + 226.0;
    let removed = k.removed.filter(|&n| n > 0);
    let added = k.added.filter(|&n| n > 0 || removed.is_none());
    for (n, color, sign) in [(removed, CL_RED, "\u{2212}"), (added, CL_GREEN, "+")] {
        let Some(n) = n else { continue };
        let t = format!("{sign}{n}");
        let w = p.gfx.measure(&t, &f.mono_b);
        p.text(&t, &f.mono_b, Rect::new(right - w, iy - 12.0, w + 2.0, 24.0), color.alpha(a));
        right -= w + 9.0;
    }
    let tx = c.x + if k.kind == Kind::Terminal { 48.0 } else { 43.0 };
    let tw = right - 8.0 - tx;
    let name = if k.target.is_empty() { k.tool.clone() } else { k.target.clone() };
    // Paths keep both ends ("content/R…eNotes.md"); commands trail off.
    let name = if k.kind == Kind::File { middle_fit(p, &name, &f.mono_b, tw, "…") } else { name };
    let tc = if k.kind == Kind::File { accent } else { ink(1.0, a) };
    p.text(&name, &f.mono_b, Rect::new(tx, iy - 12.0, tw, 24.0), tc);

    let sy = c.y + 53.5;
    let (label, value) = k.status();
    let lf = if k.kind == Kind::Terminal { &f.mono } else { &f.cl_status };
    p.text_tabular(&label, lf, Rect::new(c.x + 17.5, sy - 10.0, c.w - 35.0, 20.0), ink(0.615, a));
    if let Some(value) = value {
        let x = c.x + 17.5 + p.gfx.measure(&label, lf) + 6.0;
        p.text_tabular(&value, &f.cl_value, Rect::new(x, sy - 10.0, c.right() - 18.0 - x, 20.0), ink(1.0, a));
    }
    let bar = Rect::new(c.x + 17.0, c.y + 70.0, 209.0, 4.0);
    p.fill_round(bar, 2.0, CL_ORANGE.alpha(0.18 * a));
    let fw = bar.w * k.progress();
    if fw > 0.5 {
        p.fill_round(Rect::new(bar.x, bar.y, fw.max(bar.h), bar.h), 2.0, accent);
    }
}

/// `text` shortened in the middle (keeping both ends, joined by `dots`) to fit `w`.
fn middle_fit(p: &Painter, text: &str, fmt: &IDWriteTextFormat, w: f32, dots: &str) -> String {
    if p.gfx.measure(text, fmt) <= w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let cut = |n: usize| {
        let head = n / 2;
        let mut s: String = chars[..head].iter().collect();
        s.push_str(dots);
        s.extend(&chars[chars.len() - (n - head)..]);
        s
    };
    // The most characters that fit.
    let (mut lo, mut hi) = (0, chars.len() - 1);
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if p.gfx.measure(&cut(mid), fmt) <= w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    cut(lo)
}

/// Wrapping text centered both ways in `r` (clipped to it).
fn centered_block(p: &Painter, text: &str, fmt: &IDWriteTextFormat, r: Rect, c: Color) {
    let Some(layout) = p.gfx.shared_layout(text, fmt, r.w, r.h) else { return };
    let mut m = DWRITE_TEXT_METRICS::default();
    let h = if unsafe { layout.GetMetrics(&mut m) }.is_ok() { m.height.min(r.h) } else { r.h };
    p.clipped(r, || p.text_layout(&layout, r.x, r.y + (r.h - h) / 2.0, c));
}

/// The Usage page: a ring per agent, filled with how much of its 5-hour limit
/// is left (`fill` 0–1 sweeps the rings in as the page appears).
fn claude_usage(p: &Painter, r: Rect, v: &View, a: f32, fill: f32) {
    const PITCH: f32 = 106.5;
    const RING: f32 = 36.5;
    const TILE: f32 = 31.5;
    let f = v.fonts;
    if v.usage.is_empty() {
        p.text("No usage reported yet", &f.ring_name, Rect::new(r.x, r.y + 104.0, r.w, 22.0), ink(0.82, a));
        let sub = "Claude's shows up once Claude Code runs, Codex's once Codex has.";
        p.text(sub, &f.ring_sub, Rect::new(r.x, r.y + 128.0, r.w, 18.0), ink(0.5, a));
        return;
    }
    let n = v.usage.len() as f32;
    let x0 = r.cx() - PITCH * (n - 1.0) / 2.0;
    let cy = r.y + 115.0;
    for (i, (agent, limit)) in v.usage.iter().enumerate() {
        let x = x0 + i as f32 * PITCH;
        let left = limit.left();
        let color = match agent {
            Agent::Claude => CL_ORANGE,
            Agent::Codex => CODEX_BLUE,
        };
        p.stroke_circle(x, cy, RING, color.alpha(0.18 * a), 5.5);
        // The rings sweep round to their level as the page comes in.
        let sweep = 1.0 - (1.0 - fill.clamp(0.0, 1.0)).powi(3);
        g::arc(p, x, cy, RING, left * sweep, color.alpha(a), 5.5);
        let tile = Rect::new(x - TILE / 2.0, cy - TILE / 2.0, TILE, TILE);
        match agent {
            Agent::Claude => {
                p.fill_round(tile, 7.5, CL_TILE.alpha(a));
                g::claude_mark(p, x, cy, 21.0, ink(1.0, a));
            }
            Agent::Codex => g::codex_tile(p, tile, 7.5, a),
        }
        let col = Rect::new(x - PITCH / 2.0, 0.0, PITCH, 20.0);
        p.text(agent.name(), &f.ring_name, Rect::new(col.x, cy + 43.5, col.w, 20.0), ink(0.82, a));
        let sub = format!("{:.0}% left", left * 100.0);
        p.text_tabular(&sub, &f.ring_sub, Rect::new(col.x, cy + 62.0, col.w, 19.0), ink(0.5, a));
    }
}

// An expanded Live Activity: its icon, title and subtitle, the time or value,
/// and a progress bar. Clicking the top opens what it links to.
pub fn live_panel(p: &Painter, r: Rect, key: u32, v: &View, a: f32, hits: &mut Hits) {
    let Some(act) = v.live.get(key) else { return };
    let f = v.fonts;
    let color = act.color.unwrap_or(v.accent);
    live_icon(p, act, v, r.x + 40.0, r.y + 38.0, 40.0, a);
    let trail = act.trailing();
    let tw = if trail.is_empty() { 0.0 } else { p.gfx.measure(&trail, &f.title) + 12.0 };
    let tx = r.x + 72.0;
    let text_w = r.right() - 22.0 - tw - tx;
    p.text(&act.title, &f.title, Rect::new(tx, r.y + 18.0, text_w, 22.0), WHITE.alpha(a));
    if !act.subtitle.is_empty() {
        p.text(&act.subtitle, &f.body, Rect::new(tx, r.y + 40.0, text_w, 18.0), SECONDARY.alpha(a));
    }
    if !trail.is_empty() {
        p.text_tabular(&trail, &f.alert_b, Rect::new(r.x, r.y + 18.0, r.w - 22.0, 22.0), color.alpha(a));
    }
    if let Some(frac) = act.progress {
        let bar = Rect::new(r.x + 22.0, r.bottom() - 30.0, r.w - 44.0, 6.0);
        p.fill_round(bar, 3.0, Color::gray(255, 0.14 * a));
        if frac > 0.0 {
            p.fill_round(Rect::new(bar.x, bar.y, (bar.w * frac).max(bar.h), bar.h), 3.0, color.alpha(a));
        }
    }
    if act.open.is_some() {
        hits.push((Rect::new(r.x, r.y, r.w, 64.0), Hit::LiveOpen(key)));
    }
}

/// The notification stack: a row per app (its notes stacked behind the newest),
/// or one app's notes opened out. Click to open, × or swipe sideways to dismiss.
pub fn notes_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let f = v.fonts;
    let more = v.note_rows.len().saturating_sub(super::NOTE_ROWS);
    p.text("Notifications", &f.header, Rect::new(r.x + 22.0, r.y + 14.0, 120.0, 18.0), WHITE.alpha(a));
    if more > 0 {
        p.text(&format!("+{more} more"), &f.small_l, Rect::new(r.x + 122.0, r.y + 15.0, 100.0, 16.0), SECONDARY.alpha(a));
    }
    let (label, hit) = if v.note_open { ("Show Less", Hit::NotesBack) } else { ("Clear All", Hit::NotesClear) };
    let b = Rect::new(r.right() - 20.0 - 78.0, r.y + 12.0, 78.0, 22.0);
    let (hot, pressed) = button_state(v, hit, true);
    p.fill_round(b, 11.0, Color::gray(255, if pressed { 0.26 } else if hot { 0.2 } else { 0.12 } * a));
    p.text(label, &f.small_c, b, WHITE.alpha(a));
    hits.push((b, hit));

    let now = SystemTime::now();
    for (i, row) in v.note_rows.iter().take(super::NOTE_ROWS).enumerate() {
        let Some(n) = v.notes.get(row.note()) else { continue };
        let dx = v.note_drag.filter(|d| d.0 == i).map_or(0.0, |d| d.2);
        let ra = a * (1.0 - (dx.abs() / (super::NOTE_SWIPE * 1.8)).min(0.85));
        let y = r.y + 46.0 + i as f32 * super::NOTE_ROW_H;
        let card = Rect::new(r.x + 12.0 + dx, y, r.w - 24.0, super::NOTE_ROW_H - 6.0);
        let count = match row {
            NoteRow::Stack { count, .. } => *count,
            NoteRow::One(_) => 1,
        };
        // More from the same app peek out underneath.
        if count > 1 {
            p.fill_round(Rect::new(card.x + 10.0, card.y + 5.0, card.w - 20.0, card.h), 12.0, Color::gray(255, 0.05 * ra));
        }
        let hot = v.hot == Some(Hit::NoteRow(i)) || v.hot == Some(Hit::NoteClose(i));
        p.fill_round(card, 12.0, Color::rgba(38, 38, 40, ra));
        if hot {
            p.fill_round(card, 12.0, Color::gray(255, 0.05 * ra));
        }
        let icon = Rect::new(card.x + 10.0, card.cy() - 15.0, 30.0, 30.0);
        match n.icon.as_ref().and_then(|i| v.note_icons.get(&i.id)) {
            Some(img) => p.bitmap_round(img, icon, 7.0, ra),
            None => {
                p.fill_round(icon, 7.0, Color::gray(255, 0.14 * ra));
                g::bell(p, icon.cx(), icon.cy(), 15.0, WHITE.alpha(ra));
            }
        }
        let tx = icon.right() + 10.0;
        let close = Rect::new(card.right() - 30.0, card.cy() - 12.0, 24.0, 24.0);
        let badge_w = if count > 1 { 26.0 } else { 0.0 };
        let tw = close.x - tx - 6.0 - badge_w;
        let age = notifications::age(n.at, now);
        p.text(&format!("{} \u{B7} {age}", n.app), &f.small_l, Rect::new(tx, card.y + 4.0, tw, 14.0), SECONDARY.alpha(ra));
        p.text(&n.title, &f.alert_title, Rect::new(tx, card.y + 17.0, tw, 17.0), WHITE.alpha(ra));
        if !n.body.is_empty() {
            p.text(&n.body, &f.small_l, Rect::new(tx, card.y + 33.0, tw, 15.0), Color::gray(255, 0.7 * ra));
        }
        if count > 1 {
            let badge = Rect::new(close.x - badge_w - 2.0, card.cy() - 9.0, badge_w, 18.0);
            p.fill_round(badge, 9.0, Color::gray(255, 0.16 * ra));
            p.text(&count.to_string(), &f.small_c, badge, WHITE.alpha(ra));
        }
        let close_hot = v.hot == Some(Hit::NoteClose(i));
        if close_hot {
            p.fill_circle(close.cx(), close.cy(), 11.0, Color::gray(255, 0.14 * ra));
        }
        g::xmark(p, close.cx(), close.cy(), 8.0, Color::gray(255, if hot { 0.8 } else { 0.4 } * ra), 1.5);
        hits.push((card, Hit::NoteRow(i)));
        hits.push((close, Hit::NoteClose(i)));
    }
}

pub const CHIPS: [(u32, &str); 4] = [(300, "5 min"), (600, "10 min"), (1500, "25 min"), (0, "Stopwatch")];

pub fn home_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let f = v.fonts;
    let now = chrono::Local::now();
    p.text(&now.format("%-I:%M").to_string(), &f.big, Rect::new(r.x + 22.0, r.y + 12.0, 150.0, 44.0), WHITE.alpha(a));
    p.text(&now.format("%A, %b %-d").to_string(), &f.body, Rect::new(r.x + 24.0, r.y + 54.0, 160.0, 18.0), SECONDARY.alpha(a));

    // Next event on the right.
    if v.home_events {
        next_event_card(p, r, v, a, hits);
    }
    if !v.home_acts.is_empty() {
        running_row(p, Rect::new(r.x + 18.0, r.y + 84.0, r.w - 36.0, 32.0), v, a, hits);
    }
    if !v.home_timers {
        return;
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

/// The home page's row of what's running: a chip each (icon and a few words);
/// click one for its panel.
fn running_row(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let f = v.fonts;
    let n = v.home_acts.len();
    let gap = 8.0;
    let w = (r.w - gap * (n as f32 - 1.0)) / n as f32;
    for (i, &which) in v.home_acts.iter().enumerate() {
        let chip = Rect::new(r.x + i as f32 * (w + gap), r.y, w, r.h);
        let hit = Hit::HomeAct(i as u8);
        let (hot, pressed) = button_state(v, hit, true);
        p.fill_round(chip, r.h / 2.0, Color::gray(255, if pressed { 0.24 } else if hot { 0.18 } else { 0.1 } * a));
        let d = r.h - 8.0;
        let icon = Rect::new(chip.x + 4.0, chip.y + 4.0, d, d);
        p.fill_circle(icon.cx(), icon.cy(), d / 2.0, Color::BLACK.alpha(a));
        bubble(p, icon, which, v, a);
        let label = running_label(v, which);
        p.text(&label, &f.chip, Rect::new(icon.right() + 7.0, chip.y, chip.right() - icon.right() - 14.0, chip.h), WHITE.alpha(a));
        hits.push((chip, hit));
    }
}

/// A few words for a running activity.
fn running_label(v: &View, which: Persist) -> String {
    match which {
        Persist::Media => v.now.map(|n| if n.title.is_empty() { "Now Playing".to_string() } else { n.title.clone() }).unwrap_or_default(),
        Persist::Timer => v.clock.map(|c| c.label()).unwrap_or_default(),
        Persist::Claude => v.claude.map(|s| if s.project.is_empty() { "Claude Code".to_string() } else { s.project.clone() }).unwrap_or_default(),
        Persist::Discord => v.discord.voice.as_ref().map(|vc| vc.channel.clone()).unwrap_or_default(),
        Persist::Download => v.downloads.first().map(|d| d.name()).unwrap_or_default(),
        Persist::Calendar => {
            let unix = super::calendar::unix_now();
            v.events.iter().find(|e| e.end > unix).map(|e| e.title.clone()).unwrap_or_default()
        }
        Persist::Shelf => "Shelf".to_string(),
        Persist::Live(k) => v.live.get(k).map(|act| act.title.clone()).unwrap_or_default(),
    }
}

/// The open island's page buttons, in a pill floating under it: Home, Shelf, Widgets
/// (and Notifications while there are some). The current page's is highlighted.
pub fn tab_buttons(p: &Painter, pill: Rect, tabs: &[super::Panel], current: super::Panel, v: &View, a: f32, hits: &mut Hits) {
    use super::Panel;
    // Droppy's tab bar: a black capsule under the island, the current page's
    // icon on a lighter capsule of its own.
    const H: f32 = 36.0;
    const PAD: f32 = 3.5;
    const TAB_W: f32 = 37.0;
    const TAB_H: f32 = 29.0;
    const GAP: f32 = 1.5;
    // An activity's panel (Claude, media...) counts as part of Home, as in Droppy.
    let current = if tabs.contains(&current) { current } else { Panel::Home };
    let n = tabs.len() as f32;
    let w = 2.0 * PAD + n * TAB_W + (n - 1.0) * GAP;
    let bar = Rect::new(pill.cx() - w / 2.0, pill.bottom() + 16.5, w, H);
    p.fill_round(bar, H / 2.0, Color::BLACK.alpha(a));
    for (i, &tab) in tabs.iter().enumerate() {
        let t = Rect::new(bar.x + PAD + i as f32 * (TAB_W + GAP), bar.cy() - TAB_H / 2.0, TAB_W, TAB_H);
        let on = tab == current;
        let hit = Hit::Tab(i as u8);
        let (hot, pressed) = button_state(v, hit, !on);
        if on || hot {
            p.fill_round(t, TAB_H / 2.0, Color::gray(255, if on { 0.19 } else if pressed { 0.14 } else { 0.09 } * a));
        }
        let ink = Color::gray(255, if on { 1.0 } else if hot { 0.75 } else { 0.52 } * a);
        let (x, y) = (t.cx(), t.cy());
        match tab {
            Panel::Home => g::house_fill(p, x, y, 15.0, ink),
            Panel::Shelf => g::tray_fill(p, x, y, 15.0, ink),
            Panel::Notes => g::bell(p, x, y, 15.0, ink),
            _ => g::grid_fill(p, x, y, 13.5, ink),
        }
        // Reaches up to the panel: a gap would read as leaving and fold it away at once.
        let r = t.inset(-GAP / 2.0, 0.0);
        hits.push((Rect::new(r.x, pill.bottom(), r.w, bar.bottom() - pill.bottom()), hit));
    }
}

// The Widgets page: weather, the system, world clocks and batteries, in a 2x2 grid.
pub fn widgets_panel(p: &Painter, r: Rect, v: &View, a: f32) {
    let pad = 16.0;
    let gap = 12.0;
    let cw = (r.w - 2.0 * pad - gap) / 2.0;
    let ch = (r.h - 2.0 * pad - gap) / 2.0;
    let cell = |i: usize, j: usize| Rect::new(r.x + pad + i as f32 * (cw + gap), r.y + pad + j as f32 * (ch + gap), cw, ch);
    for (i, j) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        p.fill_round(cell(i, j), 16.0, Color::gray(255, 0.07 * a));
    }
    weather_cell(p, cell(0, 0).inset(12.0, 10.0), v, a);
    system_cell(p, cell(1, 0).inset(12.0, 10.0), v, a);
    clocks_cell(p, cell(0, 1).inset(12.0, 10.0), v, a);
    batteries_cell(p, cell(1, 1).inset(12.0, 10.0), v, a);
}

fn cell_title(p: &Painter, r: Rect, title: &str, v: &View, a: f32) {
    p.text(title, &v.fonts.small_l, Rect::new(r.x, r.y, r.w, 15.0), SECONDARY.alpha(a));
}

fn weather_cell(p: &Painter, r: Rect, v: &View, a: f32) {
    use crate::widgets::sources::WeatherState;
    let f = v.fonts;
    match v.weather {
        Some(WeatherState::Ready(w)) => {
            p.text(&w.place, &f.small_l, Rect::new(r.x, r.y, r.w - 4.0, 15.0), SECONDARY.alpha(a));
            p.faded(a, || crate::widgets::glyphs::weather(p, r.right() - 18.0, r.y + 28.0, 30.0, w.code, w.is_day));
            p.text(&format!("{}°", w.temp.round() as i32), &f.big, Rect::new(r.x, r.y + 13.0, r.w - 40.0, 40.0), WHITE.alpha(a));
            // Bottom line: the sky on the left, today's range on the right.
            let line = Rect::new(r.x, r.bottom() - 15.0, r.w, 15.0);
            let range = format!("H:{}°  L:{}°", w.high.round() as i32, w.low.round() as i32);
            let rw = p.gfx.measure(&range, &f.small_r) + 8.0;
            let cond = crate::widgets::sources::condition(w.code, w.is_day);
            p.text(cond, &f.small_l, Rect::new(line.x, line.y, line.w - rw, line.h), WHITE.alpha(0.9 * a));
            p.text(&range, &f.small_r, line, SECONDARY.alpha(a));
        }
        Some(WeatherState::Failed(_)) => {
            cell_title(p, r, "Weather", v, a);
            p.text("Couldn't load the weather", &f.small_l, Rect::new(r.x, r.cy() - 8.0, r.w, 16.0), SECONDARY.alpha(a));
        }
        _ => {
            cell_title(p, r, "Weather", v, a);
            p.text("Loading\u{2026}", &f.small_l, Rect::new(r.x, r.cy() - 8.0, r.w, 16.0), SECONDARY.alpha(a));
        }
    }
}

/// A labelled bar: name, fill and a value on the right.
fn meter(p: &Painter, r: Rect, label: &str, frac: f32, value: &str, v: &View, a: f32) {
    let f = v.fonts;
    p.text(label, &f.small_l, Rect::new(r.x, r.y, 44.0, r.h), Color::gray(255, 0.85 * a));
    let bar = Rect::new(r.x + 44.0, r.cy() - 2.5, r.w - 44.0 - 40.0, 5.0);
    p.fill_round(bar, 2.5, Color::gray(255, 0.16 * a));
    let c = if frac > 0.9 { RED } else if frac > 0.75 { ORANGE } else { GREEN };
    p.fill_round(Rect::new(bar.x, bar.y, (bar.w * frac.clamp(0.0, 1.0)).max(5.0), bar.h), 2.5, c.alpha(a));
    p.text_tabular(value, &f.small_r, Rect::new(bar.right(), r.y, 40.0, r.h), SECONDARY.alpha(a));
}

fn system_cell(p: &Painter, r: Rect, v: &View, a: f32) {
    use crate::widgets::sources::rate;
    let s = v.sys;
    cell_title(p, r, "System", v, a);
    let row = |i: usize| Rect::new(r.x, r.y + 19.0 + i as f32 * 17.0, r.w, 16.0);
    let pct = |x: f32| format!("{}%", (x * 100.0).round() as i32);
    meter(p, row(0), "CPU", s.cpu, &pct(s.cpu), v, a);
    meter(p, row(1), "Memory", s.mem, &pct(s.mem), v, a);
    let disk = s.disks.first().map_or(0.0, |d| d.used);
    meter(p, row(2), "Disk", disk, &pct(disk), v, a);
    let net = format!("\u{2193} {}   \u{2191} {}", rate(s.down), rate(s.up));
    p.text(&net, &v.fonts.small_l, row(3), SECONDARY.alpha(a));
}

fn clocks_cell(p: &Painter, r: Rect, v: &View, a: f32) {
    use crate::widgets::sources::zone_time;
    let f = v.fonts;
    cell_title(p, r, "World Clock", v, a);
    if v.zones.is_empty() {
        p.text("Add zones under Widgets", &f.small_l, Rect::new(r.x, r.cy() - 8.0, r.w, 16.0), SECONDARY.alpha(a));
        return;
    }
    for (i, z) in v.zones.iter().take(3).enumerate() {
        let y = r.y + 19.0 + i as f32 * 22.0;
        let (h, m, _, _) = zone_time(z);
        let time = if v.h24 {
            format!("{h:02}:{m:02}")
        } else {
            let (h12, ampm) = match h {
                0 => (12, "AM"),
                1..=11 => (h, "AM"),
                12 => (12, "PM"),
                _ => (h - 12, "PM"),
            };
            format!("{h12}:{m:02} {ampm}")
        };
        p.text(&z.label, &f.body, Rect::new(r.x, y, r.w - 70.0, 19.0), WHITE.alpha(a));
        p.text_tabular(&time, &f.small_r, Rect::new(r.x, y + 1.0, r.w, 17.0), Color::gray(255, 0.85 * a));
    }
}

fn batteries_cell(p: &Painter, r: Rect, v: &View, a: f32) {
    use crate::widgets::glyphs as wg;
    let f = v.fonts;
    cell_title(p, r, "Batteries", v, a);
    let mut rows: Vec<(bool, String, u8, bool)> = Vec::new();
    if let Some(b) = v.battery {
        rows.push((true, "This PC".into(), b.percent, b.charging));
    }
    rows.extend(v.devices.iter().map(|(name, pct)| (false, name.clone(), *pct, false)));
    if rows.is_empty() {
        p.text("No batteries", &f.small_l, Rect::new(r.x, r.cy() - 8.0, r.w, 16.0), SECONDARY.alpha(a));
        return;
    }
    for (i, (pc, name, pct, charging)) in rows.iter().take(3).enumerate() {
        let y = r.y + 19.0 + i as f32 * 22.0;
        let ic = (r.x + 8.0, y + 9.5);
        if *pc {
            wg::laptop(p, ic.0, ic.1, 14.0, WHITE.alpha(a));
        } else {
            wg::bluetooth(p, ic.0, ic.1, 13.0, WHITE.alpha(a));
        }
        p.text(name, &f.small_l, Rect::new(r.x + 22.0, y + 1.0, r.w - 96.0, 17.0), WHITE.alpha(a));
        p.text_tabular(&format!("{pct}%"), &f.small_r, Rect::new(r.x, y + 1.0, r.w - 34.0, 17.0), SECONDARY.alpha(a));
        let c = if *charging { GREEN } else if *pct <= 20 { RED } else { WHITE };
        g::battery(p, r.right(), y + 9.5, *pct, c, a);
    }
}

fn next_event_card(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let f = v.fonts;
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
}

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

fn button_state(v: &View, hit: Hit, enabled: bool) -> (bool, bool) {
    let hot = enabled && v.hot == Some(hit);
    (hot, hot && v.down == Some(hit))
}

/// Discord's blurple, for voice.
const BLURPLE: Color = Color::rgba(88, 101, 242, 1.0);

/// Your voice state: a mic, red and struck through when muted, or headphones
/// when deafened.
fn voice_icon(p: &Painter, cx: f32, cy: f32, s: f32, d: &super::discord::Snapshot, a: f32) {
    if d.deaf {
        g::headphones(p, cx, cy, s, RED.alpha(a));
        g::slash(p, cx, cy, s, RED.alpha(a), Color::BLACK.alpha(a));
    } else if d.mute {
        g::mic(p, cx, cy, s, RED.alpha(a));
        g::slash(p, cx, cy, s, RED.alpha(a), Color::BLACK.alpha(a));
    } else {
        g::mic(p, cx, cy, s, BLURPLE.alpha(a));
    }
}

/// Discord voice: the channel, who's in it (a green ring while they talk),
/// and Mute, Deafen and Leave.
pub fn discord_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let Some(vc) = v.discord.voice.as_ref() else { return };
    let f = v.fonts;
    let hy = r.y + 14.0;
    voice_icon(p, r.x + 30.0, hy + 9.0, 16.0, v.discord, a);
    p.text(&vc.channel, &f.header, Rect::new(r.x + 46.0, hy, r.w - 150.0, 18.0), WHITE.alpha(a));
    let count = match vc.members.len() {
        1 => "1 in voice".to_string(),
        n => format!("{n} in voice"),
    };
    p.text(&count, &f.small_r, Rect::new(r.x, hy, r.w - 22.0, 18.0), SECONDARY.alpha(a));

    // Members: up to six, then "+N".
    const MAX: usize = 6;
    let gap = (r.w - 2.0 * PAD) / MAX as f32;
    let shown = if vc.members.len() > MAX { MAX - 1 } else { vc.members.len() };
    let ay = r.y + 48.0;
    // Centered when there are fewer than fill the row.
    let x0 = r.cx() - gap * vc.members.len().min(MAX) as f32 / 2.0;
    for (i, m) in vc.members.iter().take(shown).enumerate() {
        let cx = x0 + gap * (i as f32 + 0.5);
        let rad = 17.0;
        match v.discord_avatars.get(&m.id) {
            Some(bmp) => p.bitmap_round(bmp, Rect::new(cx - rad, ay, rad * 2.0, rad * 2.0), rad, a),
            None => {
                p.fill_circle(cx, ay + rad, rad, BLURPLE.alpha(0.85 * a));
                let initial: String = m.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                p.text(&initial, &f.chip, Rect::new(cx - rad, ay, rad * 2.0, rad * 2.0), WHITE.alpha(a));
            }
        }
        if m.speaking {
            p.stroke_circle(cx, ay + rad, rad + 2.5, GREEN.alpha(a), 2.5);
        }
        if m.muted || m.deafened {
            let (bx, by) = (cx + rad * 0.72, ay + rad * 1.72);
            p.fill_circle(bx, by, 7.5, Color::BLACK.alpha(a));
            p.fill_circle(bx, by, 6.0, RED.alpha(a));
            if m.deafened {
                g::headphones(p, bx, by, 7.0, WHITE.alpha(a));
            } else {
                g::mic(p, bx, by, 7.5, WHITE.alpha(a));
            }
        }
        p.text(&m.name, &f.small_c, Rect::new(cx - gap / 2.0 + 2.0, ay + rad * 2.0 + 4.0, gap - 4.0, 15.0), Color::gray(255, 0.8 * a));
    }
    if vc.members.len() > MAX {
        let cx = x0 + gap * (MAX as f32 - 0.5);
        p.fill_circle(cx, ay + 17.0, 17.0, Color::gray(255, 0.12 * a));
        p.text(&format!("+{}", vc.members.len() - shown), &f.chip, Rect::new(cx - 17.0, ay, 34.0, 34.0), WHITE.alpha(a));
    }

    // Mute, Deafen, Leave.
    let cy = r.bottom() - 32.0;
    let cx = r.cx();
    for (x, hit, on) in [(cx - 72.0, Hit::DiscordMute, v.discord.mute), (cx, Hit::DiscordDeaf, v.discord.deaf), (cx + 72.0, Hit::DiscordLeave, true)] {
        let (hot, pressed) = button_state(v, hit, true);
        let leave = hit == Hit::DiscordLeave;
        let fill = if leave || on { RED } else { Color::gray(255, 1.0) };
        let base = if leave || on { 0.9 } else if hot { 0.2 } else { 0.12 };
        let lift = if (leave || on) && hot { 1.0 } else { base };
        let k = if pressed { 0.92 } else { 1.0 };
        p.fill_circle(x, cy, 21.0 * k, fill.alpha(lift * a));
        let bg = if leave || on { RED.alpha(lift * a) } else { Color::BLACK.alpha(a) };
        match hit {
            Hit::DiscordMute => {
                g::mic(p, x, cy, 17.0 * k, WHITE.alpha(a));
                if on {
                    g::slash(p, x, cy, 17.0 * k, WHITE.alpha(a), bg);
                }
            }
            Hit::DiscordDeaf => {
                g::headphones(p, x, cy, 17.0 * k, WHITE.alpha(a));
                if on {
                    g::slash(p, x, cy, 17.0 * k, WHITE.alpha(a), bg);
                }
            }
            _ => g::hangup(p, x, cy, 18.0 * k, WHITE.alpha(a)),
        }
        hits.push((Rect::new(x - 24.0, cy - 24.0, 48.0, 48.0), hit));
    }
}

/// The Now Playing panel's extra height for its lyrics row.
pub const LYRICS_H: f32 = 28.0;

/// One lyric line; an empty one (a break in the singing) shows a note.
fn lyric_line(p: &Painter, r: Rect, text: &str, f: &Fonts, a: f32) {
    if text.is_empty() {
        p.text("♪", &f.body, r, Color::gray(255, 0.4 * a));
    } else {
        p.text(text, &f.body, r, Color::gray(255, 0.92 * a));
    }
}

/// The sound mixer: the master volume, then each app making sound.
pub const MIX_ROW: f32 = 40.0;
pub const MIX_ROWS: usize = 5;

pub fn mixer_panel(p: &Painter, r: Rect, v: &View, a: f32, hits: &mut Hits) {
    let f = v.fonts;
    p.text("Sound", &f.header, Rect::new(r.x + 22.0, r.y + 14.0, 160.0, 18.0), WHITE.alpha(a));
    let rows = std::iter::once(None).chain(v.mixer.iter().take(MIX_ROWS).map(Some));
    for (k, app) in rows.enumerate() {
        let cy = r.y + 46.0 + MIX_ROW * (k as f32 + 0.5);
        let ic = r.x + PAD + 12.0;
        let (name, level, muted) = match app {
            None => ("Volume", v.volume.0, v.volume.1),
            Some(app) => (app.name.as_str(), app.level, app.muted),
        };
        match app.and_then(|a| a.exe.as_ref()).and_then(|e| v.mixer_icons.get(e)) {
            Some(bmp) => p.bitmap(bmp, Rect::new(ic - 12.0, cy - 12.0, 24.0, 24.0), a),
            None => g::speaker(p, ic, cy, 16.0, if muted { 0.0 } else { level }, muted, WHITE.alpha(a)),
        }
        let dim = if app.is_some_and(|x| !x.active) { 0.55 } else { 0.92 };
        p.text(name, &f.body, Rect::new(ic + 20.0, cy - 9.5, 104.0, 19.0), Color::gray(255, dim * a));
        let hit = Hit::MixSlider(k);
        let track = Rect::new(r.x + 150.0, cy - 3.0, r.w - 150.0 - PAD - 40.0, 6.0);
        let hot = v.hot == Some(hit) || v.down == Some(hit);
        let thick = if hot { 8.0 } else { 6.0 };
        let bar = Rect::new(track.x, cy - thick / 2.0, track.w, thick);
        p.fill_round(bar, thick / 2.0, Color::gray(255, 0.2 * a));
        let shown = if muted { 0.0 } else { level.clamp(0.0, 1.0) };
        p.fill_round(Rect::new(bar.x, bar.y, (bar.w * shown).max(thick), thick), thick / 2.0, Color::gray(255, if muted { 0.35 } else { 0.92 } * a));
        let pct = if muted { "Muted".to_string() } else { format!("{}%", (level * 100.0).round() as i32) };
        p.text_tabular(&pct, &f.small_r, Rect::new(bar.right(), cy - 8.0, PAD + 40.0 - PAD, 16.0), SECONDARY.alpha(a));
        hits.push((bar.inset(-4.0, -12.0), hit));
    }
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
