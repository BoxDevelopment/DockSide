//! A macOS System Settings-style window: sidebar of pages, grouped cards of
//! rows with toggles, segmented controls, sliders and accent swatches.
//!
//! Every control writes `config.toml` (via [`Action::SetConfig`]) and the
//! change is applied immediately, so the file stays the single source of truth.

use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Appearance, Backdrop, Config, ConfigValue, IconStyle, ShortcutStyle};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconStore;
use crate::gfx::{Align, Canvas, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::theme::Theme;
use crate::win::{self, startup, Monitor};

/// Lets the shared window procedure let this window activate (all others don't).
pub static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

const SHADOW: f32 = 30.0;
const W: f32 = 780.0;
const H: f32 = 560.0;
const SIDEBAR: f32 = 210.0;
const TITLE_H: f32 = 52.0;
const RADIUS: f32 = 12.0;
const ROW_H: f32 = 40.0;
const CONTENT_PAD: f32 = 24.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    General,
    Appearance,
    MenuBar,
    Dock,
}

const PAGES: [(Page, &str); 4] =
    [(Page::General, "General"), (Page::Appearance, "Appearance"), (Page::MenuBar, "Menu Bar"), (Page::Dock, "Dock")];

/// macOS accent colors: blue, purple, pink, red, orange, yellow, green, graphite.
const ACCENTS: [&str; 8] = ["#007AFF", "#A550A7", "#F74F9E", "#FF5257", "#F7821B", "#FFC600", "#62BA46", "#8C8C8C"];

const CLOCKS: [(&str, &str); 4] = [
    ("9:41 AM", "%-I:%M %p"),
    ("Fri 9:41 AM", "%a %-I:%M %p"),
    ("Fri Oct 17 9:41 AM", "%a %b %-d  %-I:%M %p"),
    ("24-hour", "%a %b %-d  %H:%M"),
];

const FONT_CHOICES: [&str; 4] = ["SF Pro Text", "SF Pro", "Inter", "Segoe UI Variable Text"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    StartAtLogin,
    HideTaskbar,
    ShortcutStyle,
    Appearance,
    Font,
    MbEnabled,
    MbBackdrop,
    MbLogo,
    MbClock,
    MbReserve,
    MbBattery,
    MbBatteryPct,
    MbWifi,
    MbSearch,
    MbControlCenter,
    DockEnabled,
    DockSize,
    DockMag,
    DockMagSize,
    DockAutohide,
    DockIconStyle,
    DockBackdrop,
    DockRunning,
    DockTrash,
}

impl Key {
    fn path(self) -> (&'static str, &'static str) {
        match self {
            Key::StartAtLogin => ("", ""),
            Key::HideTaskbar => ("general", "hide_taskbar"),
            Key::ShortcutStyle => ("general", "shortcut_style"),
            Key::Appearance => ("general", "appearance"),
            Key::Font => ("general", "font"),
            Key::MbEnabled => ("menubar", "enabled"),
            Key::MbBackdrop => ("menubar", "backdrop"),
            Key::MbLogo => ("menubar", "logo"),
            Key::MbClock => ("menubar", "clock_format"),
            Key::MbReserve => ("menubar", "reserve_space"),
            Key::MbBattery => ("menubar", "show_battery"),
            Key::MbBatteryPct => ("menubar", "show_battery_percent"),
            Key::MbWifi => ("menubar", "show_wifi"),
            Key::MbSearch => ("menubar", "show_search"),
            Key::MbControlCenter => ("menubar", "show_control_center"),
            Key::DockEnabled => ("dock", "enabled"),
            Key::DockSize => ("dock", "tile_size"),
            Key::DockMag => ("dock", "magnification"),
            Key::DockMagSize => ("dock", "magnified_size"),
            Key::DockAutohide => ("dock", "autohide"),
            Key::DockIconStyle => ("dock", "icon_style"),
            Key::DockBackdrop => ("dock", "backdrop"),
            Key::DockRunning => ("dock", "show_running"),
            Key::DockTrash => ("dock", "show_trash"),
        }
    }

    fn set(self, value: ConfigValue) -> Action {
        let (table, key) = self.path();
        Action::SetConfig { table, key, value }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Close,
    Minimize,
    Page(Page),
    Toggle(Key),
    Segment(Key, usize),
    Slider(Key),
    Swatch(usize),
    Remove(usize),
    AddApp,
    OpenConfig,
    OpenLog,
}

enum Row {
    Toggle(&'static str, Key),
    Segmented(&'static str, Key),
    Slider(&'static str, Key),
    Swatches(&'static str),
    App(usize),
    Buttons(&'static str, Vec<(&'static str, Hit)>),
    Info(&'static str, String),
}

struct Group {
    title: Option<&'static str>,
    rows: Vec<Row>,
}

pub enum SettingsEvent {
    None,
    Action(Action),
    Close,
}

#[derive(Clone, Copy)]
struct SliderDrag {
    key: Key,
    track: Rect,
    min: f32,
    max: f32,
    value: f32,
}

pub struct SettingsWindow {
    pub hwnd: HWND,
    canvas: Canvas,
    scale: f32,
    page: Page,
    scroll: f32,
    max_scroll: f32,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    slider: Option<SliderDrag>,
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    login: bool,
    fonts: Vec<String>,
    shown: bool,
    pub tracking: bool,
    pub dirty: bool,
}

impl SettingsWindow {
    pub fn new(gfx: &Gfx, monitor: &Monitor, page: Page) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + (mon.bottom - mon.top - h) / 2;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let hwnd = win::create_window(WS_EX_LAYERED | WS_EX_APPWINDOW, WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX, rect, HWND::default())?;
        unsafe {
            let _ = SetWindowTextW(hwnd, w!("System Settings"));
        }
        SETTINGS_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        let fonts = FONT_CHOICES.iter().filter(|f| gfx.font_exists(f)).map(|f| f.to_string()).collect();
        Ok(Self {
            hwnd,
            canvas: Canvas::new(gfx, w, h, s)?,
            scale: s,
            page,
            scroll: 0.0,
            max_scroll: 0.0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            slider: None,
            bitmaps: HashMap::new(),
            login: startup::is_enabled(),
            fonts,
            shown: false,
            tracking: false,
            dirty: true,
        })
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn show_page(&mut self, page: Option<Page>) {
        if let Some(p) = page {
            if p != self.page {
                self.page = p;
                self.scroll = 0.0;
                self.dirty = true;
            }
        }
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    pub fn minimize(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
        }
    }

    fn body() -> Rect {
        Rect::new(SHADOW, SHADOW, W, H)
    }

    /// Title bar area (window-local points) that drags the window.
    pub fn is_caption(&self, x: f32, y: f32) -> bool {
        let b = Self::body();
        let title = Rect::new(b.x, b.y, b.w, TITLE_H);
        title.contains(x, y) && self.hit_at(x, y).is_none()
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    fn groups(&self, cfg: &Config) -> Vec<Group> {
        match self.page {
            Page::General => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Toggle("Start MacFinder at login", Key::StartAtLogin),
                        Row::Toggle("Hide the Windows taskbar", Key::HideTaskbar),
                        Row::Segmented("Shortcut hints in menus", Key::ShortcutStyle),
                    ],
                },
                Group {
                    title: Some("Configuration"),
                    rows: vec![Row::Buttons(
                        "Settings file",
                        vec![("Show Log", Hit::OpenLog), ("Open config.toml", Hit::OpenConfig)],
                    )],
                },
                Group {
                    title: Some("About"),
                    rows: vec![
                        Row::Info("Version", env!("CARGO_PKG_VERSION").to_string()),
                        Row::Info("Settings folder", crate::config::dir().display().to_string()),
                    ],
                },
            ],
            Page::Appearance => vec![Group {
                title: None,
                rows: vec![
                    Row::Segmented("Appearance", Key::Appearance),
                    Row::Swatches("Accent color"),
                    Row::Segmented("Font", Key::Font),
                ],
            }],
            Page::MenuBar => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Toggle("Show menu bar", Key::MbEnabled),
                        Row::Segmented("Background", Key::MbBackdrop),
                        Row::Segmented("Logo", Key::MbLogo),
                        Row::Segmented("Clock", Key::MbClock),
                        Row::Toggle("Keep windows below the menu bar", Key::MbReserve),
                    ],
                },
                Group {
                    title: Some("Status items"),
                    rows: vec![
                        Row::Toggle("Battery", Key::MbBattery),
                        Row::Toggle("Battery percentage", Key::MbBatteryPct),
                        Row::Toggle("Wi-Fi", Key::MbWifi),
                        Row::Toggle("Search", Key::MbSearch),
                        Row::Toggle("Control Center", Key::MbControlCenter),
                    ],
                },
            ],
            Page::Dock => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Toggle("Show Dock", Key::DockEnabled),
                        Row::Slider("Size", Key::DockSize),
                        Row::Toggle("Magnification", Key::DockMag),
                        Row::Slider("Magnified size", Key::DockMagSize),
                        Row::Toggle("Automatically hide and show the Dock", Key::DockAutohide),
                        Row::Segmented("Icon style", Key::DockIconStyle),
                        Row::Segmented("Background", Key::DockBackdrop),
                        Row::Toggle("Show running apps", Key::DockRunning),
                        Row::Toggle("Show Trash", Key::DockTrash),
                    ],
                },
                Group {
                    title: Some("Pinned apps"),
                    rows: (0..cfg.dock.apps.len())
                        .map(Row::App)
                        .chain(std::iter::once(Row::Buttons("", vec![("Add App…", Hit::AddApp)])))
                        .collect(),
                },
            ],
        }
    }

    fn toggle_on(login: bool, key: Key, cfg: &Config) -> bool {
        let (g, m, d) = (&cfg.general, &cfg.menubar, &cfg.dock);
        match key {
            Key::StartAtLogin => login,
            Key::HideTaskbar => g.hide_taskbar,
            Key::MbEnabled => m.enabled,
            Key::MbReserve => m.reserve_space,
            Key::MbBattery => m.show_battery,
            Key::MbBatteryPct => m.show_battery_percent,
            Key::MbWifi => m.show_wifi,
            Key::MbSearch => m.show_search,
            Key::MbControlCenter => m.show_control_center,
            Key::DockEnabled => d.enabled,
            Key::DockMag => d.magnification,
            Key::DockAutohide => d.autohide,
            Key::DockRunning => d.show_running,
            Key::DockTrash => d.show_trash,
            _ => false,
        }
    }

    /// (labels, values, selected index) for a segmented control.
    fn segments(fonts: &[String], key: Key, cfg: &Config) -> (Vec<String>, Vec<String>, Option<usize>) {
        let backdrop = |b: Backdrop| match b {
            Backdrop::Acrylic => 0,
            Backdrop::Blur => 1,
            Backdrop::None => 2,
        };
        let fixed = |pairs: &[(&str, &str)], sel: Option<usize>| {
            (pairs.iter().map(|p| p.0.to_string()).collect(), pairs.iter().map(|p| p.1.to_string()).collect(), sel)
        };
        const BACKDROPS: [(&str, &str); 3] = [("Acrylic", "acrylic"), ("Blur", "blur"), ("None", "none")];
        match key {
            Key::ShortcutStyle => fixed(
                &[("Mac  ⌘C", "mac"), ("Windows  Ctrl+C", "windows")],
                Some((cfg.general.shortcut_style == ShortcutStyle::Windows) as usize),
            ),
            Key::Appearance => fixed(
                &[("Auto", "auto"), ("Light", "light"), ("Dark", "dark")],
                Some(match cfg.general.appearance {
                    Appearance::Auto => 0,
                    Appearance::Light => 1,
                    Appearance::Dark => 2,
                }),
            ),
            Key::Font => {
                let mut values = fonts.to_vec();
                let mut labels = fonts.to_vec();
                // The configured font is listed even when missing (the UI falls back), but say so.
                if !values.contains(&cfg.general.font) {
                    values.push(cfg.general.font.clone());
                    labels.push(format!("{} (not installed)", cfg.general.font));
                }
                let sel = values.iter().position(|f| *f == cfg.general.font);
                (labels, values, sel)
            }
            Key::MbBackdrop => fixed(&BACKDROPS, Some(backdrop(cfg.menubar.backdrop))),
            Key::DockBackdrop => fixed(&BACKDROPS, Some(backdrop(cfg.dock.backdrop))),
            Key::MbLogo => {
                let sel = match cfg.menubar.logo.to_lowercase().as_str() {
                    "apple" => Some(0),
                    "windows" => Some(1),
                    _ => None,
                };
                fixed(&[("Apple", "apple"), ("Windows", "windows")], sel)
            }
            Key::MbClock => fixed(&CLOCKS, CLOCKS.iter().position(|c| c.1 == cfg.menubar.clock_format)),
            Key::DockIconStyle => fixed(
                &[("Native", "native"), ("Squircle", "squircle")],
                Some((cfg.dock.icon_style == IconStyle::Squircle) as usize),
            ),
            _ => (vec![], vec![], None),
        }
    }

    fn slider_spec(key: Key, cfg: &Config) -> (f32, f32, f32) {
        match key {
            Key::DockSize => (cfg.dock.tile_size, 24.0, 96.0),
            Key::DockMagSize => (cfg.dock.magnified_size, 48.0, 160.0),
            _ => (0.0, 0.0, 1.0),
        }
    }

    // --- input -------------------------------------------------------------

    pub fn on_move(&mut self, x: f32, y: f32) {
        if let Some(s) = &mut self.slider {
            let t = ((x - s.track.x) / s.track.w).clamp(0.0, 1.0);
            s.value = (s.min + t * (s.max - s.min)).round();
            self.dirty = true;
            return;
        }
        let h = self.hit_at(x, y);
        if h != self.hover {
            self.hover = h;
            self.dirty = true;
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if self.hover.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn on_wheel(&mut self, delta: i32) {
        let s = (self.scroll - delta as f32 / 120.0 * 48.0).clamp(0.0, self.max_scroll);
        if s != self.scroll {
            self.scroll = s;
            self.dirty = true;
        }
    }

    pub fn on_down(&mut self, x: f32, y: f32, cfg: &Config) {
        self.pressed = self.hit_at(x, y);
        if let Some(Hit::Slider(key)) = self.pressed {
            if let Some((track, _)) = self.hits.iter().find(|(_, h)| *h == Hit::Slider(key)) {
                let (value, min, max) = Self::slider_spec(key, cfg);
                // The hit rect is the track grown vertically; recover the track.
                let track = Rect::new(track.x + 9.0, track.y, track.w - 18.0, track.h);
                self.slider = Some(SliderDrag { key, track, min, max, value });
                self.on_move(x, y);
            }
        }
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32, cfg: &Config) -> SettingsEvent {
        let pressed = self.pressed.take();
        self.dirty = true;
        if let Some(s) = self.slider.take() {
            return SettingsEvent::Action(s.key.set(ConfigValue::Int(s.value as i64)));
        }
        let Some(hit) = self.hit_at(x, y).filter(|h| Some(*h) == pressed) else { return SettingsEvent::None };
        let action = match hit {
            Hit::Close => return SettingsEvent::Close,
            Hit::Minimize => {
                self.minimize();
                return SettingsEvent::None;
            }
            Hit::Page(p) => {
                self.page = p;
                self.scroll = 0.0;
                return SettingsEvent::None;
            }
            Hit::Toggle(Key::StartAtLogin) => {
                self.login = !self.login;
                Action::SetStartAtLogin(self.login)
            }
            Hit::Toggle(key) => key.set(ConfigValue::Bool(!Self::toggle_on(self.login, key, cfg))),
            Hit::Segment(key, i) => {
                let (_, values, sel) = Self::segments(&self.fonts, key, cfg);
                if sel == Some(i) {
                    return SettingsEvent::None;
                }
                key.set(ConfigValue::Str(values[i].clone()))
            }
            Hit::Swatch(i) => Action::SetConfig {
                table: "general",
                key: "accent",
                value: ConfigValue::Str(ACCENTS[i].to_string()),
            },
            Hit::Remove(i) => {
                let mut apps = cfg.dock.apps.clone();
                if i < apps.len() {
                    apps.remove(i);
                }
                Action::SetDockApps(apps)
            }
            Hit::AddApp => Action::AddDockApp,
            Hit::OpenConfig => Action::OpenConfig,
            Hit::OpenLog => Action::Open(crate::config::dir().join("macfinder.log").display().to_string()),
            Hit::Slider(_) => return SettingsEvent::None,
        };
        SettingsEvent::Action(action)
    }

    // --- drawing -----------------------------------------------------------

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme, cfg: &Config, icons: &mut IconStore) {
        self.dirty = false;
        let dark = theme.dark;
        let window_bg = if dark { Color::rgba(30, 30, 30, 1.0) } else { Color::rgba(246, 246, 246, 1.0) };
        let sidebar_bg = if dark { Color::rgba(40, 40, 40, 1.0) } else { Color::rgba(232, 230, 233, 1.0) };
        let card_bg = if dark { Color::rgba(44, 44, 46, 1.0) } else { Color::WHITE };
        let card_border = if dark { Color::gray(255, 0.07) } else { Color::gray(0, 0.07) };
        let control_bg = if dark { Color::gray(255, 0.1) } else { Color::gray(0, 0.06) };
        let selected_seg = if dark { Color::gray(255, 0.22) } else { Color::WHITE };
        let switch_off = if dark { Color::gray(255, 0.16) } else { Color::gray(0, 0.12) };

        let regular = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Leading);
        let right = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Trailing);
        let center = gfx.format(&theme.font, 12.0, Weight::Regular, Align::Center);
        let button_font = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Center);
        let title_font = gfx.format(&theme.font, 15.0, Weight::Bold, Align::Leading);
        let header_font = gfx.format(&theme.font, 12.0, Weight::Bold, Align::Leading);
        let glyph_font = gfx.format("Segoe Fluent Icons", 12.0, Weight::Regular, Align::Center);

        // Icons for the pinned-apps list.
        if self.page == Page::Dock {
            for app in &cfg.dock.apps {
                let key = app.icon.clone().unwrap_or_else(|| app.path.clone());
                if !self.bitmaps.contains_key(&key) {
                    let bmp = icons
                        .get(&gfx.wic, &app.path, app.icon.as_deref())
                        .and_then(|px| px.to_bitmap(self.canvas.target()));
                    self.bitmaps.insert(key, bmp);
                }
            }
        }

        let groups = self.groups(cfg);
        let body = Self::body();
        let content_x = body.x + SIDEBAR + CONTENT_PAD;
        let content_w = body.w - SIDEBAR - 2.0 * CONTENT_PAD;
        let viewport = Rect::new(body.x + SIDEBAR, body.y + TITLE_H, body.w - SIDEBAR, body.h - TITLE_H);

        // Measure content height for scrolling.
        let content_h: f32 = groups
            .iter()
            .map(|g| if g.title.is_some() { 26.0 } else { 0.0 } + g.rows.len() as f32 * ROW_H + 20.0)
            .sum::<f32>()
            + 8.0;
        self.max_scroll = (content_h - viewport.h).max(0.0);
        self.scroll = self.scroll.min(self.max_scroll);

        let mut hits: Vec<(Rect, Hit)> = Vec::new();
        let (hover, page, scroll, slider) = (self.hover, self.page, self.scroll, self.slider);
        let page_title = PAGES.iter().find(|p| p.0 == page).map(|p| p.1).unwrap_or("");
        let (login, fonts, bitmaps) = (self.login, &self.fonts, &self.bitmaps);

        let r = {
            let draw = |p: &Painter| {
                // Shadow + window body.
                for i in 0..14 {
                    let grow = i as f32 * 1.8;
                    let a = 0.035 * (14 - i) as f32 / 14.0;
                    p.fill_round(body.inset(-grow, -grow).offset(0.0, 8.0), RADIUS + grow, theme.shadow.alpha(a));
                }
                p.clear_rect(body.inset(4.0, 4.0));
                p.fill_round(body, RADIUS, sidebar_bg);
                let content_bg = Rect::new(body.x + SIDEBAR, body.y, body.w - SIDEBAR, body.h);
                p.fill_round(content_bg, RADIUS, window_bg);
                p.fill_rect(Rect::new(content_bg.x, body.y, RADIUS, body.h), window_bg);
                p.line(content_bg.x, body.y, content_bg.x, body.bottom(), card_border, 1.0);
                p.stroke_round(body.inset(0.25, 0.25), RADIUS, theme.menu_border, 0.5);

                // Traffic lights.
                let lights = [
                    (Hit::Close, Color::rgba(255, 95, 87, 1.0), "\u{E8BB}"),
                    (Hit::Minimize, Color::rgba(254, 188, 46, 1.0), "\u{E921}"),
                    (Hit::Minimize, Color::gray(128, 0.35), ""),
                ];
                let group_hover = matches!(hover, Some(Hit::Close | Hit::Minimize));
                for (i, (hit, color, glyph)) in lights.iter().enumerate() {
                    let (cx, cy) = (body.x + 20.0 + i as f32 * 20.0, body.y + 20.0);
                    p.fill_circle(cx, cy, 6.0, *color);
                    p.stroke_circle(cx, cy, 6.0, Color::gray(0, 0.12), 0.5);
                    if i < 2 {
                        hits.push((Rect::new(cx - 8.0, cy - 8.0, 16.0, 16.0), *hit));
                        if group_hover {
                            p.text(glyph, &glyph_font, Rect::new(cx - 6.0, cy - 6.5, 12.0, 12.0), Color::gray(0, 0.55));
                        }
                    }
                }

                // Sidebar.
                let mut y = body.y + TITLE_H;
                for (pg, name) in PAGES {
                    let row = Rect::new(body.x + 10.0, y, SIDEBAR - 20.0, 30.0);
                    let selected = pg == page;
                    if selected {
                        p.fill_round(row, 6.0, theme.accent);
                    } else if hover == Some(Hit::Page(pg)) {
                        p.fill_round(row, 6.0, control_bg);
                    }
                    let icon = Rect::new(row.x + 6.0, row.cy() - 10.0, 20.0, 20.0);
                    draw_page_icon(p, pg, icon, &glyph_font);
                    let text_c = if selected { Color::WHITE } else { theme.text };
                    p.text(name, &regular, Rect::new(icon.right() + 8.0, row.y, row.w - 40.0, row.h), text_c);
                    hits.push((row, Hit::Page(pg)));
                    y += 32.0;
                }

                // Toolbar title.
                p.text(page_title, &title_font, Rect::new(content_x, body.y, content_w, TITLE_H), theme.text);

                // Scrolling content.
                p.clipped(viewport, || {
                    let mut y = viewport.y + 8.0 - scroll;
                    for group in &groups {
                        if let Some(t) = group.title {
                            p.text(t, &header_font, Rect::new(content_x + 4.0, y, content_w, 26.0), theme.text_secondary);
                            y += 26.0;
                        }
                        let card = Rect::new(content_x, y, content_w, group.rows.len() as f32 * ROW_H);
                        p.fill_round(card, 10.0, card_bg);
                        p.stroke_round(card.inset(0.25, 0.25), 10.0, card_border, 0.5);
                        for (i, row) in group.rows.iter().enumerate() {
                            let r = Rect::new(card.x, y, card.w, ROW_H);
                            if i > 0 {
                                p.line(r.x + 14.0, r.y, r.right() - 14.0, r.y, card_border, 1.0);
                            }
                            let label_r = Rect::new(r.x + 14.0, r.y, r.w * 0.5, r.h);
                            let right_edge = r.right() - 14.0;
                            match row {
                                Row::Toggle(label, key) => {
                                    p.text(label, &regular, Rect::new(label_r.x, r.y, r.w - 80.0, r.h), theme.text);
                                    let on = Self::toggle_on(login, *key, cfg);
                                    let track = Rect::new(right_edge - 34.0, r.cy() - 10.0, 34.0, 20.0);
                                    p.fill_round(track, 10.0, if on { theme.accent } else { switch_off });
                                    let kx = if on { track.right() - 10.0 } else { track.x + 10.0 };
                                    p.fill_circle(kx, r.cy() + 0.5, 8.5, Color::gray(0, 0.12));
                                    p.fill_circle(kx, r.cy(), 8.5, Color::WHITE);
                                    hits.push((track.inset(-4.0, -6.0), Hit::Toggle(*key)));
                                }
                                Row::Segmented(label, key) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    let (labels, _, sel) = Self::segments(fonts, *key, cfg);
                                    let widths: Vec<f32> = labels.iter().map(|l| p.gfx.measure(l, &center) + 22.0).collect();
                                    let total: f32 = widths.iter().sum::<f32>() + 4.0;
                                    let outer = Rect::new(right_edge - total, r.cy() - 12.0, total, 24.0);
                                    p.fill_round(outer, 7.0, control_bg);
                                    let mut sx = outer.x + 2.0;
                                    for (i, (l, w)) in labels.iter().zip(&widths).enumerate() {
                                        let seg = Rect::new(sx, outer.y + 2.0, *w, 20.0);
                                        if sel == Some(i) {
                                            p.fill_round(seg.offset(0.0, 0.5), 5.0, Color::gray(0, 0.08));
                                            p.fill_round(seg, 5.0, selected_seg);
                                        } else if hover == Some(Hit::Segment(*key, i)) {
                                            p.fill_round(seg, 5.0, control_bg);
                                        }
                                        p.text(l, &center, seg, theme.text);
                                        hits.push((seg, Hit::Segment(*key, i)));
                                        sx += w;
                                    }
                                }
                                Row::Slider(label, key) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    let (mut value, min, max) = Self::slider_spec(*key, cfg);
                                    if let Some(s) = slider.filter(|s| s.key == *key) {
                                        value = s.value;
                                    }
                                    let track = Rect::new(right_edge - 240.0, r.cy() - 2.0, 240.0, 4.0);
                                    let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
                                    p.fill_round(track, 2.0, switch_off);
                                    p.fill_round(Rect::new(track.x, track.y, track.w * t, track.h), 2.0, theme.accent);
                                    let kx = track.x + track.w * t;
                                    p.fill_circle(kx, r.cy() + 0.5, 9.0, Color::gray(0, 0.15));
                                    p.fill_circle(kx, r.cy(), 9.0, Color::WHITE);
                                    let value_r = Rect::new(track.x - 60.0, r.y, 50.0, r.h);
                                    p.text(&format!("{}", value as i32), &right, value_r, theme.text_secondary);
                                    hits.push((Rect::new(track.x - 9.0, r.y, track.w + 18.0, r.h), Hit::Slider(*key)));
                                }
                                Row::Swatches(label) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    for (i, hex) in ACCENTS.iter().enumerate() {
                                        let c = Color::parse(hex).unwrap_or(theme.accent);
                                        let cx = right_edge - 9.0 - (ACCENTS.len() - 1 - i) as f32 * 26.0;
                                        let selected = cfg.general.accent.eq_ignore_ascii_case(hex);
                                        if selected {
                                            p.stroke_circle(cx, r.cy(), 11.5, c, 2.0);
                                        }
                                        p.fill_circle(cx, r.cy(), 8.5, c);
                                        if selected {
                                            p.fill_circle(cx, r.cy(), 3.0, Color::WHITE);
                                        }
                                        hits.push((Rect::new(cx - 12.0, r.cy() - 12.0, 24.0, 24.0), Hit::Swatch(i)));
                                    }
                                }
                                Row::App(i) => {
                                    if let Some(app) = cfg.dock.apps.get(*i) {
                                        let key = app.icon.clone().unwrap_or_else(|| app.path.clone());
                                        let icon = Rect::new(r.x + 12.0, r.cy() - 13.0, 26.0, 26.0);
                                        if let Some(Some(bmp)) = bitmaps.get(&key) {
                                            p.bitmap(bmp, icon, 1.0);
                                        }
                                        let name = app.name.clone().unwrap_or_else(|| app.path.clone());
                                        p.text(&name, &regular, Rect::new(icon.right() + 10.0, r.y, r.w * 0.45, r.h), theme.text);
                                        let path_r = Rect::new(r.x + r.w * 0.45, r.y, r.w * 0.55 - 52.0, r.h);
                                        p.text(&app.path, &right, path_r, theme.text_disabled);
                                        let (cx, cy) = (right_edge - 9.0, r.cy());
                                        let hot = hover == Some(Hit::Remove(*i));
                                        p.fill_circle(cx, cy, 9.0, if hot { Color::rgba(255, 82, 87, 1.0) } else { control_bg });
                                        p.line(cx - 4.0, cy, cx + 4.0, cy, if hot { Color::WHITE } else { theme.text_secondary }, 1.6);
                                        hits.push((Rect::new(cx - 11.0, cy - 11.0, 22.0, 22.0), Hit::Remove(*i)));
                                    }
                                }
                                Row::Buttons(label, buttons) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    let mut bx = right_edge;
                                    for (text, hit) in buttons.iter() {
                                        let bw = p.gfx.measure(text, &button_font) + 24.0;
                                        bx -= bw;
                                        let b = Rect::new(bx, r.cy() - 12.0, bw, 24.0);
                                        let bg = if hover == Some(*hit) { selected_seg } else { control_bg };
                                        p.fill_round(b, 6.0, bg);
                                        p.stroke_round(b.inset(0.25, 0.25), 6.0, card_border, 0.5);
                                        p.text(text, &button_font, b, theme.text);
                                        hits.push((b, *hit));
                                        bx -= 8.0;
                                    }
                                }
                                Row::Info(label, value) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    let vr = Rect::new(r.x + r.w * 0.3, r.y, r.w * 0.7 - 14.0, r.h);
                                    p.text(value, &right, vr, theme.text_secondary);
                                }
                            }
                            y += ROW_H;
                        }
                        y += 20.0;
                    }
                });
            };
            self.canvas.draw(gfx, draw)
        };
        // Content hits outside the viewport (scrolled away) must not be clickable.
        hits.retain(|(r, h)| matches!(h, Hit::Close | Hit::Minimize | Hit::Page(_)) || (r.y >= viewport.y - 1.0 && r.bottom() <= viewport.bottom() + 1.0));
        self.hits = hits;

        let presented = r.and_then(|_| {
            if self.shown {
                self.canvas.present_in_place(self.hwnd)
            } else {
                let mut rc = RECT::default();
                unsafe { GetWindowRect(self.hwnd, &mut rc)? };
                self.canvas.present(self.hwnd, rc.left, rc.top)
            }
        });
        if let Err(e) = presented {
            log!("settings render failed: {e}");
        }
        if !self.shown {
            self.shown = true;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOW);
                let _ = SetForegroundWindow(self.hwnd);
            }
        }
    }
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        SETTINGS_HWND.store(0, Ordering::Relaxed);
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Colored rounded-square page icons, like macOS System Settings.
fn draw_page_icon(p: &Painter, page: Page, r: Rect, glyph_font: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat) {
    let white = Color::WHITE;
    match page {
        Page::General => {
            p.fill_round(r, 5.0, Color::rgba(142, 142, 147, 1.0));
            p.text("\u{E713}", glyph_font, r, white);
        }
        Page::Appearance => {
            p.fill_round(r, 5.0, Color::rgba(28, 28, 30, 1.0));
            p.stroke_circle(r.cx(), r.cy(), 5.5, white, 1.4);
            p.clipped(Rect::new(r.cx(), r.y, r.w / 2.0, r.h), || p.fill_circle(r.cx(), r.cy(), 5.5, white));
        }
        Page::MenuBar => {
            p.fill_round(r, 5.0, Color::rgba(0, 122, 255, 1.0));
            let screen = r.inset(4.0, 5.0);
            p.stroke_round(screen, 1.5, white, 1.2);
            p.fill_rect(Rect::new(screen.x, screen.y, screen.w, 2.5), white);
        }
        Page::Dock => {
            p.fill_round(r, 5.0, Color::rgba(28, 28, 30, 1.0));
            let screen = r.inset(4.0, 5.0);
            p.stroke_round(screen, 1.5, white, 1.2);
            p.fill_round(Rect::new(screen.cx() - 4.0, screen.bottom() - 3.5, 8.0, 2.0), 1.0, white);
        }
    }
}
