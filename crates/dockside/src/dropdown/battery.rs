//! The Battery menu: charge and power source like macOS, plus the Windows
//! power mode, Energy Saver, the charge level over the last day, battery
//! health, and the apps using significant energy.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;

use super::{Content, Ctx, DropEvent, Hit, PAD};
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::win::power::{self, Live, PowerMode, Watcher};

const W: f32 = 300.0;
const GRAPH_H: f32 = 64.0;
const ROW: f32 = 28.0;
const MODES: [(PowerMode, &str); 3] =
    [(PowerMode::Efficiency, "Efficiency"), (PowerMode::Balanced, "Balanced"), (PowerMode::Performance, "Performance")];
const GREEN: Color = Color::rgba(52, 199, 89, 1.0);
const RED: Color = Color::rgba(255, 59, 48, 1.0);

pub struct BatteryMenu {
    watcher: Watcher,
    live: Live,
    history: Vec<(i64, u8, bool)>,
}

impl BatteryMenu {
    pub fn new(msg_hwnd: isize) -> Self {
        let watcher = Watcher::start(msg_hwnd, crate::win::WM_APP_BATTERY);
        let live = watcher.snapshot();
        Self { watcher, live, history: power::history() }
    }

    fn hog_rows(&self) -> usize {
        match &self.live.hogs {
            Some(h) if !h.is_empty() => h.len(),
            _ => 1,
        }
    }
}

/// "3:05" from seconds.
fn hm(secs: u32) -> String {
    format!("{}:{:02}", secs / 3600, (secs % 3600) / 60)
}

impl Content for BatteryMenu {
    fn width(&self) -> f32 {
        W
    }

    fn key(&self, h: &mut DefaultHasher) {
        let d = &self.live.details;
        d.as_ref().map(|d| (d.percent, d.plugged, d.charging, d.left, d.to_full, d.health(), d.cycles, d.saver)).hash(h);
        self.live.mode.map(|m| m as u8).hash(h);
        self.live.hogs.as_ref().map(|v| v.iter().map(|x| (x.name.clone(), x.cpu as u32 / 5)).collect::<Vec<_>>()).hash(h);
        self.history.len().hash(h);
    }

    fn height(&self, _gfx: &Gfx, _font: &str) -> f32 {
        let header = 12.0 + 22.0 + 18.0 + 18.0 + 10.0;
        let graph = 1.0 + 10.0 + 16.0 + GRAPH_H + 16.0 + 6.0;
        let modes = 1.0 + 10.0 + 18.0 + 30.0 + 6.0 + ROW + 6.0;
        let hogs = 1.0 + 10.0 + 18.0 + self.hog_rows() as f32 * 24.0 + 8.0;
        let health = 1.0 + 6.0 + ROW + 4.0;
        let links = 1.0 + 6.0 + ROW + 8.0;
        header + graph + modes + hogs + health + links
    }

    fn draw(&self, p: &Painter, ctx: &Ctx, hits: &mut Vec<(Rect, Hit)>) -> f32 {
        let pal = ctx.pal;
        let inner = W - 2.0 * PAD;
        let mut y = 12.0;
        let Some(d) = &self.live.details else {
            ctx.text(p, "Battery", 13.0, Weight::Bold, Align::Leading, Rect::new(PAD, y, inner, 22.0), pal.text);
            ctx.text(p, "No battery found.", 12.0, Weight::Regular, Align::Leading, Rect::new(PAD, y + 22.0, inner, 18.0), pal.sub);
            return y + 44.0;
        };

        // Header: like macOS, the name and the charge, then where power comes from.
        ctx.text(p, "Battery", 13.0, Weight::Bold, Align::Leading, Rect::new(PAD, y, inner, 22.0), pal.text);
        ctx.text(p, &format!("{}%", d.percent), 13.0, Weight::Regular, Align::Trailing, Rect::new(PAD, y, inner, 22.0), pal.sub);
        y += 22.0;
        let source = if d.plugged { "Power Source: Power Adapter" } else { "Power Source: Battery" };
        ctx.text(p, source, 12.0, Weight::Regular, Align::Leading, Rect::new(PAD, y, inner, 18.0), pal.sub);
        y += 18.0;
        let state = match (d.plugged, d.charging) {
            (true, true) => match d.to_full {
                Some(s) if s > 60 => format!("Charging: {} until full", hm(s)),
                _ => "Charging".to_string(),
            },
            (true, false) if d.percent >= 99 => "Fully Charged".to_string(),
            // Windows' charge limit (or a smart-charging setting) is holding it.
            (true, false) => "Battery Is Not Charging".to_string(),
            (false, _) => match d.left {
                Some(s) => format!("{} remaining", hm(s)),
                None => "Calculating Time Remaining\u{2026}".to_string(),
            },
        };
        ctx.text(p, &state, 12.0, Weight::Regular, Align::Leading, Rect::new(PAD, y, inner, 18.0), pal.sub);
        y += 18.0 + 10.0;

        // Charge level over the last day.
        ctx.separator(p, y, W);
        y += 1.0 + 10.0;
        ctx.text(p, "Charge Level", 11.0, Weight::SemiBold, Align::Leading, Rect::new(PAD, y, inner, 16.0), pal.sub);
        ctx.text(p, "Last 24 Hours", 11.0, Weight::Regular, Align::Trailing, Rect::new(PAD, y, inner, 16.0), pal.sub);
        y += 16.0;
        self.draw_graph(p, ctx, Rect::new(PAD, y + 4.0, inner, GRAPH_H - 4.0), d.percent, d.plugged);
        y += GRAPH_H;
        for (label, align) in [("24h ago", Align::Leading), ("12h", Align::Center), ("Now", Align::Trailing)] {
            ctx.text(p, label, 10.0, Weight::Regular, align, Rect::new(PAD, y + 1.0, inner - 26.0, 14.0), pal.sub);
        }
        y += 16.0 + 6.0;

        // Power mode and Energy Saver.
        ctx.separator(p, y, W);
        y += 1.0 + 10.0;
        ctx.text(p, "Power Mode", 11.0, Weight::SemiBold, Align::Leading, Rect::new(PAD, y, inner, 16.0), pal.sub);
        y += 18.0;
        let track = Rect::new(PAD, y, inner, 28.0);
        p.fill_round(track, 8.0, pal.track);
        let seg_w = inner / MODES.len() as f32;
        for (i, (mode, label)) in MODES.iter().enumerate() {
            let r = Rect::new(PAD + i as f32 * seg_w, y, seg_w, 28.0);
            let on = self.live.mode == Some(*mode);
            let hit = Hit::Mode(i as u8);
            if on {
                p.fill_round(r.inset(2.0, 2.0), 6.5, pal.fill);
            } else if ctx.hover == Some(hit) {
                p.fill_round(r.inset(2.0, 2.0), 6.5, pal.chip);
            }
            let c = if on && pal.fill.r > 0.9 { Color::gray(0, 0.86) } else { pal.text };
            ctx.text(p, label, 12.0, if on { Weight::SemiBold } else { Weight::Regular }, Align::Center, r, c);
            hits.push((r, hit));
        }
        y += 30.0 + 6.0;
        let row = Rect::new(PAD, y, inner, ROW);
        let hr = Rect::new(6.0, y, W - 12.0, ROW);
        ctx.highlight(p, hr, Hit::Saver);
        ctx.text(p, "Energy Saver", 13.0, Weight::Regular, Align::Leading, row, pal.text);
        crate::controlcenter::draw_switch(p, pal, Some(d.saver), Rect::new(row.right() - 34.0, row.cy() - 10.0, 34.0, 20.0), GREEN);
        hits.push((hr, Hit::Saver));
        y += ROW + 6.0;

        // Apps using significant energy.
        ctx.separator(p, y, W);
        y += 1.0 + 10.0;
        ctx.text(p, "Using Significant Energy", 11.0, Weight::SemiBold, Align::Leading, Rect::new(PAD, y, inner, 16.0), pal.sub);
        y += 18.0;
        match &self.live.hogs {
            None => ctx.text(p, "Measuring\u{2026}", 13.0, Weight::Regular, Align::Leading, Rect::new(PAD, y, inner, 24.0), pal.sub),
            Some(h) if h.is_empty() => {
                ctx.text(p, "No Apps Using Significant Energy", 13.0, Weight::Regular, Align::Leading, Rect::new(PAD, y, inner, 24.0), pal.sub)
            }
            Some(h) => {
                let glyphs = p.gfx.format("Segoe Fluent Icons", 13.0, Weight::Regular, Align::Center);
                for (i, hog) in h.iter().enumerate() {
                    let r = Rect::new(PAD, y + i as f32 * 24.0, inner, 24.0);
                    p.text("\u{E737}", &glyphs, Rect::new(r.x, r.y, 18.0, r.h), pal.sub);
                    let name = if crate::screenshot_mode::on() { format!("App {}", i + 1) } else { hog.name.clone() };
                    ctx.text(p, &name, 13.0, Weight::Regular, Align::Leading, Rect::new(r.x + 26.0, r.y, r.w - 70.0, r.h), pal.text);
                    let cpu = format!("{:.0}% CPU", hog.cpu);
                    ctx.text(p, &cpu, 11.0, Weight::Regular, Align::Trailing, r, pal.sub);
                }
            }
        }
        y += self.hog_rows() as f32 * 24.0 + 8.0;

        // Health.
        ctx.separator(p, y, W);
        y += 1.0 + 6.0;
        let row = Rect::new(PAD, y, inner, ROW);
        ctx.text(p, "Battery Health", 13.0, Weight::Regular, Align::Leading, row, pal.text);
        let health = match (d.health(), d.cycles) {
            (Some(hp), Some(c)) => format!("{} \u{00B7} {hp}% \u{00B7} {c} cycles", if hp >= 80 { "Normal" } else { "Service Recommended" }),
            (Some(hp), None) => format!("{} \u{00B7} {hp}%", if hp >= 80 { "Normal" } else { "Service Recommended" }),
            (None, Some(c)) => format!("{c} cycles"),
            (None, None) => "Not reported".to_string(),
        };
        let bad = d.health().is_some_and(|h| h < 80);
        ctx.text(p, &health, 12.0, Weight::Regular, Align::Trailing, row, if bad { RED } else { pal.sub });
        y += ROW + 4.0;

        ctx.separator(p, y, W);
        y += 1.0 + 6.0;
        ctx.link_row(p, hits, Rect::new(PAD, y, inner, ROW), "Battery Settings\u{2026}", Hit::Link(0));
        y + ROW + 8.0
    }

    fn click(&mut self, hit: Hit) -> DropEvent {
        match hit {
            Hit::Mode(i) => {
                if let Some((mode, _)) = MODES.get(i as usize) {
                    crate::win::controls::set_power_mode(*mode);
                    self.watcher.refresh();
                    self.live = self.watcher.snapshot();
                }
                DropEvent::None
            }
            Hit::Saver => {
                let on = self.live.details.as_ref().is_some_and(|d| d.saver);
                crate::win::controls::set_low_power_mode(!on);
                self.watcher.refresh();
                self.live = self.watcher.snapshot();
                DropEvent::None
            }
            Hit::Link(_) => DropEvent::Run(Action::Open("ms-settings:batterysaver".into())),
            _ => DropEvent::None,
        }
    }

    fn update(&mut self) {
        self.live = self.watcher.snapshot();
    }
}

impl BatteryMenu {
    /// Bars for each half hour of the last day (gaps where DockSide wasn't running),
    /// green while on the adapter, with 0 / 50 / 100% guides on the right.
    fn draw_graph(&self, p: &Painter, ctx: &Ctx, r: Rect, now_pct: u8, now_plugged: bool) {
        let pal = ctx.pal;
        let plot = Rect::new(r.x, r.y, r.w - 26.0, r.h);
        for (pct, label) in [(100.0, "100%"), (50.0, "50%"), (0.0, "0%")] {
            let gy = plot.bottom() - plot.h * pct / 100.0;
            p.line(plot.x, gy, plot.right(), gy, pal.sep, 1.0);
            ctx.text(p, label, 9.0, Weight::Regular, Align::Trailing, Rect::new(plot.right(), gy - 7.0, 26.0, 14.0), pal.sub);
        }
        const BUCKETS: usize = 48;
        let now = crate::island::calendar::unix_now();
        let span = 24 * 3600;
        let mut sums = [(0u32, 0u32, false); BUCKETS];
        let samples = self.history.iter().copied().chain(std::iter::once((now, now_pct, now_plugged)));
        for (t, pct, plugged) in samples {
            let age = now - t;
            if !(0..span).contains(&age) {
                continue;
            }
            let i = BUCKETS - 1 - (age * BUCKETS as i64 / span) as usize;
            sums[i].0 += pct as u32;
            sums[i].1 += 1;
            sums[i].2 |= plugged;
        }
        let bw = plot.w / BUCKETS as f32;
        for (i, (sum, n, plugged)) in sums.iter().enumerate() {
            if *n == 0 {
                continue;
            }
            let pct = (*sum / *n) as f32;
            let bh = (plot.h * pct / 100.0).max(1.5);
            let c = if *plugged {
                GREEN
            } else if pct <= 20.0 {
                RED
            } else {
                pal.text.alpha(pal.text.a * 0.55)
            };
            p.fill_round(Rect::new(plot.x + i as f32 * bw + 0.75, plot.bottom() - bh, (bw - 1.5).max(1.0), bh), 1.0, c);
        }
    }
}
