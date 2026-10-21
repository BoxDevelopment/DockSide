//! The top menu bar: logo menu, bold app name, standard menus, and status items.

use std::fmt::Write as _;

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::UI::Shell::ABE_TOP;

use crate::config::{Backdrop, Config, MenuBarCfg};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconStore;
use crate::gfx::{pt, Align, Gfx, Painter, Weight};
use crate::menu::{Action, Placement, MENU_TITLES};
use crate::surface::Surface;
use crate::theme::Theme;
use crate::win::system::{self, Battery, Shortcut};
use crate::win::{appbar, Monitor, WM_APP_APPBAR};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Logo,
    AppName,
    Title(usize),
    Battery,
    Wifi,
    Search,
    ControlCenter,
    Clock,
    /// Every app's notification-area icon, in a menu.
    Tray,
}

impl Hit {
    pub fn opens_menu(self) -> bool {
        matches!(self, Hit::Logo | Hit::AppName | Hit::Title(_) | Hit::Tray)
    }

    /// The neighbouring menu for Left/Right arrow navigation.
    pub fn step(self, dir: i32) -> Hit {
        let order: Vec<Hit> =
            [Hit::Logo, Hit::AppName].into_iter().chain((0..MENU_TITLES.len()).map(Hit::Title)).collect();
        let i = order.iter().position(|h| *h == self).unwrap_or(0) as i32;
        order[(i + dir).rem_euclid(order.len() as i32) as usize]
    }
}

const FONT_SIZE: f32 = 13.0;
/// The bar is laid out at this height and scaled up to the configured one.
const DESIGN_H: f32 = 24.0;

pub struct MenuBar {
    pub surface: Surface,
    cfg: MenuBarCfg,
    monitor: Monitor,
    pub app_name: String,
    pub target: HWND,
    pub target_exe: Option<String>,
    clock: String,
    battery: Option<Battery>,
    hits: Vec<(Rect, Hit)>,
    pub open: Option<Hit>,
    pressed: Option<Hit>,
    logo: Option<ID2D1Bitmap>,
    reserved: bool,
    /// With a clear bar: whether the wallpaper under it is dark (light text).
    on_dark: Option<bool>,
    pub dirty: bool,
}

impl MenuBar {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: Monitor) -> Result<Self> {
        let m = &cfg.menubar;
        let height = (m.height * monitor.scale).round() as i32;
        let rect = RECT {
            left: monitor.rect.left,
            top: monitor.rect.top,
            right: monitor.rect.right,
            bottom: monitor.rect.top + height,
        };
        // With its space reserved, only the desktop is ever behind the menu bar.
        let source = if m.reserve_space { crate::win::glass::Source::Wallpaper } else { crate::win::glass::Source::Screen };
        let surface = Surface::with_layer(gfx, rect, monitor.scale, m.backdrop, theme.menubar_tint, true, source)?;
        if m.reserve_space {
            appbar::register(surface.content, WM_APP_APPBAR, ABE_TOP, monitor.rect, height);
        }
        let mut bar = Self {
            surface,
            cfg: m.clone(),
            monitor,
            app_name: m.desktop_app_name.clone(),
            target: HWND::default(),
            target_exe: None,
            clock: String::new(),
            battery: None,
            hits: Vec::new(),
            open: None,
            pressed: None,
            logo: None,
            reserved: m.reserve_space,
            on_dark: (m.backdrop == Backdrop::Clear).then(|| wallpaper_top_is_dark(gfx).unwrap_or(theme.dark)),
            dirty: true,
        };
        bar.tick();
        Ok(bar)
    }

    /// Refreshes clock and battery; returns true when something visible changed.
    pub fn tick(&mut self) -> bool {
        let now = chrono::Local::now();
        let fmt = &self.cfg.clock_format;
        let valid = !chrono::format::StrftimeItems::new(fmt).any(|i| matches!(i, chrono::format::Item::Error));
        let mut clock = String::new();
        if !valid || write!(clock, "{}", now.format(fmt)).is_err() {
            clock = now.format("%a %-I:%M %p").to_string();
        }
        let battery = if self.cfg.show_battery { system::battery() } else { None };
        let changed = clock != self.clock || battery != self.battery;
        self.clock = clock;
        self.battery = battery;
        self.dirty |= changed;
        changed
    }

    pub fn set_app(&mut self, name: String, hwnd: HWND, exe: Option<String>) {
        self.target = hwnd;
        self.target_exe = exe;
        if name != self.app_name {
            self.app_name = name;
            self.dirty = true;
        }
    }

    /// Content scale: a taller bar draws everything proportionally bigger.
    fn zoom(&self) -> f32 {
        (self.surface.height_pt() / DESIGN_H).max(0.75)
    }

    pub fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        let k = self.zoom();
        self.hits.iter().find(|(r, _)| r.contains(x / k, y / k)).map(|(_, h)| *h)
    }

    pub fn menu_placement(&self, hit: Hit) -> Option<Placement> {
        let (r, _) = self.hits.iter().find(|(_, h)| *h == hit)?;
        let s = self.monitor.scale;
        Some(Placement::Below {
            x: self.monitor.rect.left as f32 / s + r.x * self.zoom(),
            y: self.monitor.rect.top as f32 / s + self.surface.height_pt(),
        })
    }

    pub fn set_open(&mut self, hit: Option<Hit>) {
        if self.open != hit {
            self.open = hit;
            self.dirty = true;
        }
    }

    pub fn set_pressed(&mut self, hit: Option<Hit>) {
        if self.pressed != hit {
            self.pressed = hit;
            self.dirty = true;
        }
    }

    pub fn status_action(hit: Hit, spotlight: bool) -> Option<Action> {
        const WIN_S: Shortcut = Shortcut::new("w", b'S' as u16, "S");
        const WIN_A: Shortcut = Shortcut::new("w", b'A' as u16, "A");
        const WIN_N: Shortcut = Shortcut::new("w", b'N' as u16, "N");
        match hit {
            Hit::Search if spotlight => Some(Action::ToggleSpotlight),
            Hit::Search => Some(Action::Keys(WIN_S)),
            Hit::ControlCenter | Hit::Wifi => Some(Action::Keys(WIN_A)),
            Hit::Clock => Some(Action::Keys(WIN_N)),
            Hit::Battery => Some(Action::Open("ms-settings:batterysaver".into())),
            _ => None,
        }
    }

    fn layout(&mut self, gfx: &Gfx, theme: &Theme) {
        let k = self.zoom();
        let h = self.surface.height_pt() / k;
        let w = self.surface.width_pt() / k;
        let regular = gfx.format(&theme.font, FONT_SIZE, Weight::Regular, Align::Center);
        let bold = gfx.format(&theme.font, FONT_SIZE, Weight::Bold, Align::Center);
        let mut hits = Vec::new();

        let mut x = 10.0;
        hits.push((Rect::new(x, 0.0, 34.0, h), Hit::Logo));
        x += 34.0;
        let aw = gfx.measure(&self.app_name, &bold) + 20.0;
        hits.push((Rect::new(x, 0.0, aw, h), Hit::AppName));
        x += aw;
        for (i, t) in MENU_TITLES.iter().enumerate() {
            let tw = gfx.measure(t, &regular) + 20.0;
            hits.push((Rect::new(x, 0.0, tw, h), Hit::Title(i)));
            x += tw;
        }

        let mut rx = w - 8.0;
        let cw = gfx.measure(&self.clock, &regular) + 16.0;
        rx -= cw;
        hits.push((Rect::new(rx, 0.0, cw, h), Hit::Clock));
        for (show, hit) in [
            (self.cfg.show_control_center, Hit::ControlCenter),
            (self.cfg.show_search, Hit::Search),
            (self.cfg.show_wifi, Hit::Wifi),
        ] {
            if show {
                rx -= 30.0;
                hits.push((Rect::new(rx, 0.0, 30.0, h), hit));
            }
        }
        if let Some(b) = self.battery {
            let mut bw = 44.0;
            if self.cfg.show_battery_percent {
                bw += gfx.measure(&format!("{}%", b.percent), &regular) + 4.0;
            }
            rx -= bw;
            hits.push((Rect::new(rx, 0.0, bw, h), Hit::Battery));
        }
        if self.cfg.show_tray {
            rx -= 30.0;
            hits.push((Rect::new(rx, 0.0, 30.0, h), Hit::Tray));
        }
        self.hits = hits;
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme, icons: &mut IconStore) {
        self.dirty = false;
        self.layout(gfx, theme);

        let logo_kind = self.cfg.logo.to_lowercase();
        if logo_kind != "apple" && logo_kind != "windows" && self.logo.is_none() {
            if let Some(px) = icons.get(&gfx.wic, &self.cfg.logo, Some(&self.cfg.logo)) {
                self.logo = px.to_bitmap(self.surface.canvas.target());
            }
        }

        let regular = gfx.format(&theme.font, FONT_SIZE, Weight::Regular, Align::Center);
        let bold = gfx.format(&theme.font, FONT_SIZE, Weight::Bold, Align::Center);
        let trailing = gfx.format(&theme.font, FONT_SIZE, Weight::Regular, Align::Trailing);
        let k = self.zoom();
        let w = self.surface.width_pt() / k;
        let h = self.surface.height_pt() / k;
        let has_backdrop = self.surface.backdrop.is_some();
        let clear = self.on_dark.is_some();
        let (hits, open, pressed) = (&self.hits, self.open, self.pressed);
        let (app_name, clock, battery, logo) = (&self.app_name, &self.clock, self.battery, &self.logo);
        let show_pct = self.cfg.show_battery_percent;
        // A clear bar takes its colours from the wallpaper, not the app theme.
        let (c, highlight) = match self.on_dark {
            Some(true) => (Color::gray(255, 0.96), Color::gray(255, 0.22)),
            Some(false) => (Color::gray(0, 0.86), Color::gray(0, 0.10)),
            None => (theme.text, theme.menubar_highlight),
        };

        let r = self.surface.render(gfx, |p| p.transformed(0.0, 0.0, k, || {
            // Paint every pixel so the whole bar hit-tests.
            p.fill_rect(Rect::new(0.0, 0.0, w, h), if has_backdrop || clear { Color::HIT } else { theme.menubar_tint.alpha(1.0) });

            for &(r, hit) in hits {
                if open == Some(hit) || pressed == Some(hit) {
                    // Tahoe highlights with a capsule.
                    let pill = r.inset(0.0, 2.5);
                    p.fill_round(pill, pill.h / 2.0, highlight);
                }
                match hit {
                    Hit::Logo => match (logo_kind.as_str(), logo) {
                        ("apple", _) => draw_apple(p, r, c),
                        ("windows", _) => draw_windows_logo(p, r, c),
                        (_, Some(bmp)) => p.bitmap(bmp, Rect::new(r.cx() - 8.0, r.cy() - 8.0, 16.0, 16.0), 1.0),
                        _ => draw_apple(p, r, c),
                    },
                    Hit::AppName => p.text(app_name, &bold, r, c),
                    Hit::Title(i) => p.text(MENU_TITLES[i], &regular, r, c),
                    Hit::Clock => p.text(clock, &regular, r, c),
                    Hit::ControlCenter => draw_control_center(p, r, c),
                    Hit::Tray => draw_tray(p, r, c),
                    Hit::Search => draw_search(p, r, c),
                    Hit::Wifi => draw_wifi(p, r, c),
                    Hit::Battery => {
                        if let Some(b) = battery {
                            draw_battery(p, r, b, c);
                            if show_pct {
                                p.text(&format!("{}%", b.percent), &trailing, Rect::new(r.x, r.y, r.w - 40.0, r.h), c);
                            }
                        }
                    }
                }
            }
        }));
        if let Err(e) = r {
            log!("menubar render failed: {e}");
        }
    }
}

impl MenuBar {
    /// Gives back reserved screen space early, before a replacement reserves its own.
    pub fn release_space(&mut self) {
        if std::mem::take(&mut self.reserved) {
            appbar::remove(self.surface.content);
        }
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        self.release_space();
    }
}

/// Whether the top strip of the desktop wallpaper is dark, from its average
/// luminance. `None` without a readable wallpaper image.
fn wallpaper_top_is_dark(gfx: &Gfx) -> Option<bool> {
    use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};
    let mut buf = [0u16; 520];
    unsafe {
        SystemParametersInfoW(SPI_GETDESKWALLPAPER, buf.len() as u32, Some(buf.as_mut_ptr() as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).ok()?;
    }
    let path = crate::win::from_wide(&buf);
    if path.is_empty() {
        return None;
    }
    let img = crate::gfx::icons::load_image_file(&gfx.wic, &path)?;
    let (w, h) = (img.width as usize, img.height as usize);
    let rows = (h / 16).max(1);
    let (mut sum, mut n) = (0.0f64, 0u32);
    for y in (0..rows).step_by((rows / 24).max(1)) {
        for x in (0..w).step_by((w / 96).max(1)) {
            let i = (y * w + x) * 4;
            let (b, g, r) = (img.data[i] as f64, img.data[i + 1] as f64, img.data[i + 2] as f64);
            sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
            n += 1;
        }
    }
    Some(n > 0 && sum / (n as f64) < 150.0)
}

// ---------------------------------------------------------------------------
// Vector glyphs
// ---------------------------------------------------------------------------

fn bez(sink: &ID2D1GeometrySink, c1: (f32, f32), c2: (f32, f32), end: (f32, f32)) {
    unsafe {
        sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: pt(c1.0, c1.1), point2: pt(c2.0, c2.1), point3: pt(end.0, end.1) });
    }
}

/// A generic apple silhouette with a bite and leaf, in a 17x20 unit box.
fn draw_apple(p: &Painter, r: Rect, c: Color) {
    let geom = p.gfx.path(|s| unsafe {
        s.BeginFigure(pt(8.5, 5.2), D2D1_FIGURE_BEGIN_FILLED);
        bez(s, (9.6, 5.2), (11.0, 4.2), (13.0, 4.3));
        bez(s, (14.6, 4.4), (15.8, 5.2), (16.5, 6.3));
        bez(s, (14.9, 7.3), (14.3, 8.8), (14.4, 10.2));
        bez(s, (14.5, 12.0), (15.6, 13.3), (17.0, 13.9));
        bez(s, (16.5, 15.4), (15.8, 16.8), (14.9, 18.0));
        bez(s, (14.0, 19.2), (13.0, 20.2), (11.8, 20.2));
        bez(s, (10.6, 20.2), (10.1, 19.5), (8.6, 19.5));
        bez(s, (7.1, 19.5), (6.6, 20.2), (5.4, 20.2));
        bez(s, (4.2, 20.2), (3.2, 19.1), (2.3, 17.8));
        bez(s, (0.4, 15.0), (0.0, 11.0), (1.0, 8.4));
        bez(s, (1.8, 6.2), (3.6, 4.4), (5.6, 4.4));
        bez(s, (6.8, 4.4), (7.6, 5.2), (8.5, 5.2));
        s.EndFigure(D2D1_FIGURE_END_CLOSED);

        s.BeginFigure(pt(12.6, 0.0), D2D1_FIGURE_BEGIN_FILLED);
        bez(s, (12.8, 1.4), (12.2, 2.6), (11.4, 3.4));
        bez(s, (10.6, 4.2), (9.4, 4.7), (8.5, 4.5));
        bez(s, (8.4, 3.2), (9.0, 2.0), (9.8, 1.2));
        bez(s, (10.6, 0.4), (11.8, -0.1), (12.6, 0.0));
        s.EndFigure(D2D1_FIGURE_END_CLOSED);
    });
    if let Some(g) = geom {
        let k = 14.5 / 20.2;
        p.transformed(r.cx() - 8.5 * k, r.cy() - 10.1 * k - 0.5, k, || p.fill_geometry(&g, c));
    }
}

fn draw_windows_logo(p: &Painter, r: Rect, c: Color) {
    let (s, gap) = (6.0, 1.0);
    let (x0, y0) = (r.cx() - s - gap / 2.0, r.cy() - s - gap / 2.0);
    for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
        p.fill_rect(Rect::new(x0 + dx * (s + gap), y0 + dy * (s + gap), s, s), c);
    }
}

fn draw_wifi(p: &Painter, r: Rect, c: Color) {
    let (cx, cy) = (r.cx(), r.cy() + 5.5);
    let geom = p.gfx.path(|s| unsafe {
        for radius in [4.2_f32, 7.6, 11.0] {
            let d = radius * std::f32::consts::FRAC_1_SQRT_2;
            s.BeginFigure(pt(cx - d, cy - d), D2D1_FIGURE_BEGIN_HOLLOW);
            s.AddArc(&D2D1_ARC_SEGMENT {
                point: pt(cx + d, cy - d),
                size: D2D_SIZE_F { width: radius, height: radius },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_SMALL,
            });
            s.EndFigure(D2D1_FIGURE_END_OPEN);
        }
    });
    if let Some(g) = geom {
        p.stroke_geometry(&g, c, 1.9);
    }
    p.fill_circle(cx, cy - 0.6, 1.4, c);
}

fn draw_search(p: &Painter, r: Rect, c: Color) {
    let (cx, cy) = (r.cx() - 1.2, r.cy() - 1.2);
    p.stroke_circle(cx, cy, 4.6, c, 1.6);
    p.line(cx + 3.4, cy + 3.4, cx + 6.6, cy + 6.6, c, 1.8);
}

/// The Windows "show hidden icons" chevron.
fn draw_tray(p: &Painter, r: Rect, c: Color) {
    let (cx, cy) = (r.cx(), r.cy() + 1.5);
    p.line(cx - 5.0, cy, cx, cy - 5.0, c, 1.6);
    p.line(cx, cy - 5.0, cx + 5.0, cy, c, 1.6);
}

fn draw_control_center(p: &Painter, r: Rect, c: Color) {
    let (cx, cy) = (r.cx(), r.cy());
    let pill = |y: f32, dot_left: bool| {
        let body = Rect::new(cx - 8.0, y - 3.25, 16.0, 6.5);
        p.stroke_round(body, 3.25, c, 1.3);
        let dx = if dot_left { body.x + 3.25 } else { body.right() - 3.25 };
        p.fill_circle(dx, y, 2.0, c);
    };
    pill(cy - 4.0, false);
    pill(cy + 4.0, true);
}

fn draw_battery(p: &Painter, r: Rect, b: Battery, c: Color) {
    let (bw, bh) = (24.0, 11.5);
    let body = Rect::new(r.right() - 10.0 - bw, r.cy() - bh / 2.0, bw, bh);
    let dim = c.alpha(c.a * 0.4);
    p.stroke_round(body.inset(0.5, 0.5), 3.5, dim, 1.0);
    p.fill_round(Rect::new(body.right() + 1.0, r.cy() - 2.0, 1.6, 4.0), 0.8, dim);

    let inner = body.inset(2.0, 2.0);
    let level = inner.w * (b.percent as f32 / 100.0);
    let fill = if b.charging {
        Color::rgba(52, 199, 89, 1.0)
    } else if b.percent <= 20 {
        Color::rgba(255, 59, 48, 1.0)
    } else {
        c
    };
    p.fill_round(Rect::new(inner.x, inner.y, level.max(1.5), inner.h), 1.5, fill);

    if b.charging {
        let (x, y) = (body.cx(), body.cy());
        let bolt = p.gfx.path(|s| unsafe {
            s.BeginFigure(pt(x + 1.0, y - 5.0), D2D1_FIGURE_BEGIN_FILLED);
            s.AddLine(pt(x - 3.0, y + 0.8));
            s.AddLine(pt(x - 0.2, y + 0.8));
            s.AddLine(pt(x - 1.0, y + 5.0));
            s.AddLine(pt(x + 3.0, y - 0.8));
            s.AddLine(pt(x + 0.2, y - 0.8));
            s.EndFigure(D2D1_FIGURE_END_CLOSED);
        });
        if let Some(g) = bolt {
            p.fill_geometry(&g, Color::WHITE);
        }
    }
}
