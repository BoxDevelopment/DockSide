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
use windows::Win32::Graphics::DirectWrite::IDWriteTextFormat;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{feel, lerp, Glide, Motion, Scroll};
use crate::chrome::{self, WindowAnim, SHADOW};
use crate::config::{Appearance, Backdrop, Config, ConfigValue, IconStyle, IslandStyle, ShortcutStyle};
use crate::finder::text::TextField;
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconStore;
use crate::gfx::{Align, Canvas, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::spotlight::web;
use crate::theme::Theme;
use crate::win::{self, startup, Monitor};

/// Lets the shared window procedure let this window activate (all others don't).
pub static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

/// Dock path for this window: it shows in the Dock like an app, and opens it when pinned.
pub const SETTINGS_PATH: &str = "dockside:settings";

const W: f32 = 780.0;
const H: f32 = 560.0;
const SIDEBAR: f32 = 210.0;
const TITLE_H: f32 = 52.0;
pub(crate) const ROW_H: f32 = 40.0;
const CONTENT_PAD: f32 = 24.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Page {
    General,
    Appearance,
    MenuBar,
    Dock,
    Finder,
    Desktops,
    Island,
    Widgets,
}

const PAGES: [(Page, &str); 8] = [
    (Page::General, "General"),
    (Page::Appearance, "Appearance"),
    (Page::MenuBar, "Menu Bar"),
    (Page::Island, "Dynamic Island"),
    (Page::Dock, "Dock"),
    (Page::Widgets, "Widgets"),
    (Page::Finder, "Finder"),
    (Page::Desktops, "Desktops"),
];

/// macOS accent colors: blue, purple, pink, red, orange, yellow, green, graphite.
const ACCENTS: [&str; 8] = ["#007AFF", "#A550A7", "#F74F9E", "#FF5257", "#F7821B", "#FFC600", "#62BA46", "#8C8C8C"];

/// The `[island]` on/off setting called `name`.
fn island_flag(c: &crate::config::IslandCfg, name: &str) -> bool {
    match name {
        "enabled" => c.enabled,
        "always_show" => c.always_show,
        "expand_on_hover" => c.expand_on_hover,
        "now_playing" => c.now_playing,
        "timers" => c.timers,
        "downloads" => c.downloads,
        "calendar" => c.calendar,
        "claude_code" => c.claude_code,
        "shelf" => c.shelf,
        "volume" => c.volume,
        "brightness" => c.brightness,
        "charging" => c.charging,
        "low_battery" => c.low_battery,
        "caps_lock" => c.caps_lock,
        "do_not_disturb" => c.do_not_disturb,
        "notifications" => c.notifications,
        "bluetooth" => c.bluetooth,
        "privacy_indicators" => c.privacy_indicators,
        "screenshots" => c.screenshots,
        "download_alerts" => c.download_alerts,
        _ => false,
    }
}

const CLOCKS: [(&str, &str); 4] = [
    ("9:41 AM", "%-I:%M %p"),
    ("Fri 9:41 AM", "%a %-I:%M %p"),
    ("Fri Oct 17 9:41 AM", "%a %b %-d  %-I:%M %p"),
    ("24-hour", "%a %b %-d  %H:%M"),
];

const FONT_CHOICES: [&str; 4] = ["SF Pro Text", "SF Pro", "Inter", "Segoe UI Variable Text"];

/// The font choices that are installed, for the Font control.
pub(crate) fn installed_fonts(gfx: &Gfx) -> Vec<String> {
    FONT_CHOICES.iter().filter(|f| gfx.font_exists(f)).map(|f| f.to_string()).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Key {
    StartAtLogin,
    HideTaskbar,
    ShortcutStyle,
    Appearance,
    Font,
    GlassRefraction,
    FinderSidebar,
    FinderView,
    FinderHidden,
    DesktopsAlt,
    DesktopsArrows,
    DesktopsIsland,
    MbEnabled,
    MbBackdrop,
    MbHeight,
    MbLogo,
    MbClock,
    MbReserve,
    MbBattery,
    MbBatteryPct,
    MbWifi,
    MbSearch,
    MbControlCenter,
    MbTray,
    /// An on/off setting in `[island]`, by name.
    Island(&'static str),
    IslandIcs,
    IslandStyle,
    IslandOffset,
    DockEnabled,
    DockSize,
    DockMag,
    DockMagSize,
    DockOffset,
    DockSpacing,
    DockAutohide,
    DockShowOnDesktop,
    DockIconStyle,
    DockBackdrop,
    DockRunning,
    DockTrash,
    DockShadow,
    DockMacIconsKey,
    SpotlightEnabled,
    SpotlightHotkey,
    SearchEngine,
    CustomSearchUrl,
    WidgetsEnabled,
    WidgetsBackdrop,
    WidgetsLocation,
    WidgetsUnit,
    WidgetsClocks,
}

impl Key {
    fn path(self) -> (&'static str, &'static str) {
        match self {
            Key::StartAtLogin => ("", ""),
            Key::HideTaskbar => ("general", "hide_taskbar"),
            Key::ShortcutStyle => ("general", "shortcut_style"),
            Key::Appearance => ("general", "appearance"),
            Key::Font => ("general", "font"),
            Key::GlassRefraction => ("general", "glass_refraction"),
            Key::FinderSidebar => ("finder", "sidebar"),
            Key::FinderView => ("finder", "view"),
            Key::FinderHidden => ("finder", "show_hidden"),
            Key::DesktopsAlt => ("desktops", "alt_number"),
            Key::DesktopsArrows => ("desktops", "replace_windows_switcher"),
            Key::DesktopsIsland => ("desktops", "island"),
            Key::MbEnabled => ("menubar", "enabled"),
            Key::MbBackdrop => ("menubar", "backdrop"),
            Key::MbHeight => ("menubar", "height"),
            Key::MbLogo => ("menubar", "logo"),
            Key::MbClock => ("menubar", "clock_format"),
            Key::MbReserve => ("menubar", "reserve_space"),
            Key::MbBattery => ("menubar", "show_battery"),
            Key::MbBatteryPct => ("menubar", "show_battery_percent"),
            Key::MbWifi => ("menubar", "show_wifi"),
            Key::MbSearch => ("menubar", "show_search"),
            Key::MbControlCenter => ("menubar", "show_control_center"),
            Key::MbTray => ("menubar", "show_tray"),
            Key::Island(name) => ("island", name),
            Key::IslandIcs => ("island", "calendar_ics"),
            Key::IslandStyle => ("island", "style"),
            Key::IslandOffset => ("island", "vertical_offset"),
            Key::DockEnabled => ("dock", "enabled"),
            Key::DockSize => ("dock", "tile_size"),
            Key::DockMag => ("dock", "magnification"),
            Key::DockMagSize => ("dock", "magnified_size"),
            Key::DockOffset => ("dock", "bottom_margin"),
            Key::DockSpacing => ("dock", "item_spacing"),
            Key::DockAutohide => ("dock", "autohide"),
            Key::DockShowOnDesktop => ("dock", "show_on_desktop"),
            Key::DockIconStyle => ("dock", "icon_style"),
            Key::DockBackdrop => ("dock", "backdrop"),
            Key::DockRunning => ("dock", "show_running"),
            Key::DockTrash => ("dock", "show_trash"),
            Key::DockShadow => ("dock", "shadow"),
            Key::DockMacIconsKey => ("dock", "macosicons_key"),
            Key::SpotlightEnabled => ("spotlight", "enabled"),
            Key::SpotlightHotkey => ("spotlight", "hotkey"),
            Key::SearchEngine => ("spotlight", "search_engine"),
            Key::CustomSearchUrl => ("spotlight", "custom_search_url"),
            Key::WidgetsEnabled => ("widgets", "enabled"),
            Key::WidgetsBackdrop => ("widgets", "backdrop"),
            Key::WidgetsLocation => ("widgets", "location"),
            Key::WidgetsUnit => ("widgets", "temperature_unit"),
            Key::WidgetsClocks => ("widgets", "world_clocks"),
        }
    }

    fn set(self, value: ConfigValue) -> Action {
        let (table, key) = self.path();
        Action::SetConfig { table, key, value }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Hit {
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
    OpenUpdate,
    Field(Key),
    ConnectClaude(bool),
    AddWidget(crate::config::WidgetKind),
    ResetWidgets,
    Onboarding,
    RunAsAdmin,
}

/// Springs behind the controls: hover fades, switch knobs, segment pills.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Anim {
    Hover(Hit),
    Knob(Key),
    SegX(Key),
    SegW(Key),
    Swatch(usize),
    Lights,
    /// Setup Assistant: progress dots (current, hovered) and footer buttons.
    Dot(usize),
    DotHover(usize),
    Button(usize),
}

/// Switches flick across quickly, with a touch of give at the end.
const KNOB_FEEL: (f32, f32) = (0.2, 0.86);

pub(crate) enum Row {
    Toggle(&'static str, Key),
    Segmented(&'static str, Key),
    Slider(&'static str, Key),
    Swatches(&'static str),
    App(usize),
    Buttons(&'static str, Vec<(&'static str, Hit)>),
    Info(&'static str, String),
    /// Label, key, placeholder.
    Text(&'static str, Key, &'static str),
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

/// A text row being edited.
pub(crate) struct Editing {
    key: Key,
    field: TextField,
    /// Last commit was rejected (shown with a red outline).
    invalid: bool,
}

/// Slider knob radius; the knob travels inside the track so it never
/// overhangs either end.
const KNOB: f32 = 9.0;

#[derive(Clone, Copy)]
pub(crate) struct SliderDrag {
    key: Key,
    /// Knob-centre travel: starts at `x0`, spans `w`.
    x0: f32,
    w: f32,
    min: f32,
    max: f32,
    /// Continuous position (0..1) so the knob follows the cursor smoothly.
    t: f32,
    /// Cursor offset from the knob centre when it was grabbed.
    grab: f32,
}

impl SliderDrag {
    fn value(&self) -> f32 {
        (self.min + self.t * (self.max - self.min)).round()
    }

    /// Starts dragging the slider whose hit rect (spanning the track) is `track`,
    /// pressed at `x`. `pending` is a value released but not yet reloaded.
    pub(crate) fn begin(key: Key, track: Rect, x: f32, cfg: &Config, pending: Option<(Key, f32)>) -> Self {
        let (value, min, max) = SettingsWindow::slider_spec(key, cfg);
        let value = pending.filter(|p| p.0 == key).map_or(value, |p| p.1);
        // The hit rect spans the track horizontally; the knob centre travels inside it.
        let (x0, w) = (track.x + KNOB, track.w - 2.0 * KNOB);
        let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
        let kx = x0 + w * t;
        // Grabbing the knob itself keeps it under the cursor instead of jumping.
        let grab = if (x - kx).abs() <= KNOB + 2.0 { x - kx } else { 0.0 };
        let mut s = SliderDrag { key, x0, w, min, max, t, grab };
        s.drag(x);
        s
    }

    /// Follows the cursor; true when the knob moved.
    pub(crate) fn drag(&mut self, x: f32) -> bool {
        let t = ((x - self.grab - self.x0) / self.w).clamp(0.0, 1.0);
        let moved = t != self.t;
        self.t = t;
        moved
    }

    /// The released value to show until the reload catches up, and the action
    /// saving it; `None` when the value didn't change.
    pub(crate) fn finish(&self, cfg: &Config) -> Option<((Key, f32), Action)> {
        let value = self.value();
        if value == SettingsWindow::slider_spec(self.key, cfg).0.round() {
            return None;
        }
        Some(((self.key, value), self.key.set(ConfigValue::Int(value as i64))))
    }
}

/// A slider setting's current value.
pub(crate) fn slider_value(key: Key, cfg: &Config) -> f32 {
    SettingsWindow::slider_spec(key, cfg).0
}

/// Text formats and colors shared by every settings row.
pub(crate) struct Style {
    regular: IDWriteTextFormat,
    right: IDWriteTextFormat,
    center: IDWriteTextFormat,
    button: IDWriteTextFormat,
    pub card_bg: Color,
    pub card_border: Color,
    pub control_bg: Color,
    pub selected_seg: Color,
    switch_off: Color,
    field_bg: Color,
}

impl Style {
    pub(crate) fn new(gfx: &Gfx, theme: &Theme) -> Self {
        let dark = theme.dark;
        Style {
            regular: gfx.format(&theme.font, 13.0, Weight::Regular, Align::Leading),
            right: gfx.format(&theme.font, 13.0, Weight::Regular, Align::Trailing),
            center: gfx.format(&theme.font, 12.0, Weight::Regular, Align::Center),
            button: gfx.format(&theme.font, 13.0, Weight::Regular, Align::Center),
            card_bg: if dark { Color::rgba(44, 44, 46, 1.0) } else { Color::WHITE },
            card_border: if dark { Color::gray(255, 0.07) } else { Color::gray(0, 0.07) },
            control_bg: if dark { Color::gray(255, 0.1) } else { Color::gray(0, 0.06) },
            selected_seg: if dark { Color::gray(255, 0.22) } else { Color::WHITE },
            switch_off: if dark { Color::gray(255, 0.16) } else { Color::gray(0, 0.12) },
            field_bg: if dark { Color::rgba(30, 30, 30, 1.0) } else { Color::WHITE },
        }
    }
}

/// What a row needs to draw itself: the config and the window's control state.
pub(crate) struct RowCtx<'a> {
    pub theme: &'a Theme,
    pub cfg: &'a Config,
    pub style: &'a Style,
    pub hover: Option<Hit>,
    pub slider: Option<SliderDrag>,
    pub pending: Option<(Key, f32)>,
    pub login: bool,
    pub fonts: &'a [String],
    /// Pinned-app icons (Dock page only).
    pub bitmaps: Option<&'a HashMap<String, Option<ID2D1Bitmap>>>,
    pub editing: Option<&'a Editing>,
    /// The window's springs; without them controls jump straight to their state.
    pub motion: Option<&'a Motion<Anim>>,
}

impl RowCtx<'_> {
    fn anim(&self, key: Anim, target: f32, feel: (f32, f32)) -> f32 {
        self.motion.map_or(target, |m| m.get(key, target, feel))
    }

    /// 0..1 hover highlight for `hit`.
    fn hover_t(&self, hit: Hit) -> f32 {
        let on = self.hover == Some(hit);
        self.motion.map_or(if on { 1.0 } else { 0.0 }, |m| m.fade(Anim::Hover(hit), on))
    }
}

/// A card holding `rows`, `ROW_H` each, with hairlines between them.
pub(crate) fn draw_card(p: &Painter, card: Rect, rows: &[Row], ctx: &RowCtx, hits: &mut Vec<(Rect, Hit)>) {
    let s = ctx.style;
    p.fill_round(card, 10.0, s.card_bg);
    p.stroke_round(card.inset(0.25, 0.25), 10.0, s.card_border, 0.5);
    for (i, row) in rows.iter().enumerate() {
        let r = Rect::new(card.x, card.y + i as f32 * ROW_H, card.w, ROW_H);
        if i > 0 {
            p.line(r.x + 14.0, r.y, r.right() - 14.0, r.y, s.card_border, 1.0);
        }
        draw_row(p, row, r, ctx, hits);
    }
}

/// The action for clicking a toggle, segment or accent swatch (`None` for any
/// other hit, or a segment that's already selected). Start at login flips `login`.
pub(crate) fn control_action(hit: Hit, login: &mut bool, fonts: &[String], cfg: &Config) -> Option<Action> {
    Some(match hit {
        Hit::Toggle(Key::StartAtLogin) => {
            *login = !*login;
            Action::SetStartAtLogin(*login)
        }
        Hit::Toggle(key) => key.set(ConfigValue::Bool(!SettingsWindow::toggle_on(*login, key, cfg))),
        Hit::Segment(key, i) => {
            let (_, values, sel) = SettingsWindow::segments(fonts, key, cfg);
            if sel == Some(i) {
                return None;
            }
            key.set(ConfigValue::Str(values[i].clone()))
        }
        Hit::Swatch(i) => Action::SetConfig {
            table: "general",
            key: "accent",
            value: ConfigValue::Str(ACCENTS[i].to_string()),
        },
        _ => return None,
    })
}

pub struct SettingsWindow {
    pub hwnd: HWND,
    canvas: Canvas,
    scale: f32,
    page: Page,
    /// Smooth (eased) scrolling of the page content.
    scroll: Scroll,
    max_scroll: f32,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    slider: Option<SliderDrag>,
    /// Value just released, shown until the config reload catches up so the
    /// knob doesn't flick back to the old value.
    pending: Option<(Key, f32)>,
    editing: Option<Editing>,
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    login: bool,
    /// "Run as administrator" is on (its scheduled task exists).
    admin: bool,
    fonts: Vec<String>,
    shown: bool,
    /// Open / minimize zoom, the control springs, and the sidebar's hover
    /// highlight gliding between pages. Page switches themselves are instant.
    window: WindowAnim,
    motion: Motion<Anim>,
    side_glide: Glide,
    /// The selected page's highlight, gliding to a newly chosen page.
    sel_glide: Glide,
    /// Set when only this area is animating (the page scrolling, a sidebar
    /// highlight gliding): the next frame repaints just that.
    part: Option<Rect>,
    /// The region the last frame repainted (device pixels; `None`: all of it).
    drawn_part: Option<RECT>,
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
        let fonts = installed_fonts(gfx);
        Ok(Self {
            hwnd,
            canvas: Canvas::new(gfx, w, h, s)?,
            scale: s,
            page,
            scroll: Scroll::default(),
            max_scroll: 0.0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            slider: None,
            pending: None,
            editing: None,
            bitmaps: HashMap::new(),
            login: startup::is_enabled(),
            admin: win::elevation::task_enabled(),
            fonts,
            shown: false,
            window: WindowAnim::opening(),
            motion: Motion::default(),
            side_glide: Glide::default(),
            sel_glide: Glide::selection(),
            part: None,
            drawn_part: None,
            tracking: false,
            dirty: true,
        })
    }

    /// On `WM_SIZE`: redraw when restored from minimized, so it zooms back in.
    pub fn on_size(&mut self) {
        if self.window.restored(self.hwnd) {
            self.dirty = true;
        }
    }

    /// Advances animations; returns true while frames are still needed.
    pub fn tick(&mut self, dt: f32) -> bool {
        // The page scrolling or a sidebar highlight gliding repaints only its own
        // area; anything else (or a redraw already asked for) repaints it all.
        let was_dirty = self.dirty;
        let full = self.window.tick(dt, self.hwnd) | self.motion.tick(dt);
        let mut part: Option<Rect> = None;
        if self.scroll.tick(dt) {
            part = Some(Self::viewport());
        }
        if self.side_glide.tick(dt) | self.sel_glide.tick(dt) {
            let b = Self::body();
            let side = Rect::new(b.x, b.y, SIDEBAR, b.h);
            part = Some(part.map_or(side, |p| p.union(side)));
        }
        let moving = full || part.is_some();
        if moving {
            let pending_full = was_dirty && self.part.is_none();
            self.part = match (full || pending_full, part, self.part) {
                (true, ..) => None,
                (false, Some(p), Some(q)) => Some(p.union(q)),
                (false, p, q) => p.or(q),
            };
            self.dirty = true;
        }
        moving
    }

    /// The scrolling page area.
    fn viewport() -> Rect {
        let b = Self::body();
        Rect::new(b.x + SIDEBAR, b.y + TITLE_H, b.w - SIDEBAR, b.h - TITLE_H)
    }

    pub fn animating(&self) -> bool {
        self.window.moving() || self.motion.moving() || self.scroll.moving() || self.side_glide.moving() || self.sel_glide.moving()
    }

    fn go_to(&mut self, page: Page) {
        if page != self.page {
            self.page = page;
            self.scroll.jump(0.0);
        }
        self.dirty = true;
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn set_admin(&mut self, on: bool) {
        self.admin = on;
        self.dirty = true;
    }

    pub fn show_page(&mut self, page: Option<Page>) {
        if let Some(p) = page {
            self.go_to(p);
        }
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
        // Called while the App is busy, so the restore's WM_SIZE never reaches us.
        self.on_size();
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
                        Row::Toggle("Start DockSide at login", Key::StartAtLogin),
                        Row::Toggle("Hide the Windows taskbar", Key::HideTaskbar),
                        Row::Segmented("Shortcut hints in menus", Key::ShortcutStyle),
                    ],
                },
                Group {
                    title: Some("Administrator Apps"),
                    rows: vec![Row::Buttons(
                        "Run as administrator (works with admin apps)",
                        vec![(if self.admin { "Turn Off" } else { "Turn On…" }, Hit::RunAsAdmin)],
                    )],
                },
                Group {
                    title: Some("Spotlight"),
                    rows: {
                        let mut rows = vec![
                            Row::Toggle("Spotlight search", Key::SpotlightEnabled),
                            Row::Segmented("Keyboard shortcut", Key::SpotlightHotkey),
                            Row::Segmented("Web search", Key::SearchEngine),
                        ];
                        if cfg.spotlight.search_engine.eq_ignore_ascii_case("custom") {
                            rows.push(Row::Text("Search URL", Key::CustomSearchUrl, "https://example.com/search?q=%s"));
                        }
                        rows
                    },
                },
                Group {
                    title: Some("Setup Assistant"),
                    rows: vec![Row::Buttons("Walk through the basics again", vec![("Open Setup Assistant…", Hit::Onboarding)])],
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
                    rows: {
                        let mut rows = vec![Row::Info("Version", crate::update::current())];
                        if crate::update::available().is_some() {
                            rows.push(Row::Buttons("A newer version is available", vec![("Download", Hit::OpenUpdate)]));
                        }
                        rows.push(Row::Info("Settings folder", crate::config::dir().display().to_string()));
                        rows
                    },
                },
            ],
            Page::Appearance => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Segmented("Appearance", Key::Appearance),
                        Row::Swatches("Accent color"),
                        Row::Segmented("Font", Key::Font),
                    ],
                },
                Group {
                    title: Some("Liquid Glass"),
                    rows: vec![
                        // Refraction hides glass surfaces from screenshots and screen sharing.
                        Row::Toggle("Refraction (hidden from screenshots)", Key::GlassRefraction),
                    ],
                },
            ],
            Page::MenuBar => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Toggle("Show menu bar", Key::MbEnabled),
                        Row::Segmented("Background", Key::MbBackdrop),
                        Row::Slider("Size", Key::MbHeight),
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
                        Row::Toggle("Tray icons", Key::MbTray),
                    ],
                },
            ],
            Page::Island => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Toggle("Show Dynamic Island", Key::Island("enabled")),
                        Row::Toggle("Always show the island", Key::Island("always_show")),
                        Row::Toggle("Expand when hovered", Key::Island("expand_on_hover")),
                        Row::Segmented("Style", Key::IslandStyle),
                        Row::Slider("Vertical offset", Key::IslandOffset),
                    ],
                },
                Group {
                    title: Some("Activities"),
                    rows: vec![
                        Row::Toggle("Now Playing", Key::Island("now_playing")),
                        Row::Toggle("Timers and stopwatch", Key::Island("timers")),
                        Row::Toggle("Downloads in progress", Key::Island("downloads")),
                        Row::Toggle("Upcoming meetings", Key::Island("calendar")),
                        Row::Text("Calendar link", Key::IslandIcs, "https://…/basic.ics or webcal://…"),
                        Row::Toggle("Claude Code", Key::Island("claude_code")),
                        Row::Toggle("File shelf", Key::Island("shelf")),
                    ],
                },
                Group {
                    title: Some("Alerts"),
                    rows: vec![
                        Row::Toggle("Volume", Key::Island("volume")),
                        Row::Toggle("Brightness", Key::Island("brightness")),
                        Row::Toggle("Charging", Key::Island("charging")),
                        Row::Toggle("Low battery", Key::Island("low_battery")),
                        Row::Toggle("Caps Lock", Key::Island("caps_lock")),
                        Row::Toggle("Do Not Disturb", Key::Island("do_not_disturb")),
                        Row::Toggle("Notifications", Key::Island("notifications")),
                        Row::Toggle("Bluetooth devices", Key::Island("bluetooth")),
                        Row::Toggle("Camera and microphone", Key::Island("privacy_indicators")),
                        Row::Toggle("Screenshots", Key::Island("screenshots")),
                        Row::Toggle("Finished downloads", Key::Island("download_alerts")),
                        Row::Toggle("Desktop switches", Key::DesktopsIsland),
                    ],
                },
                Group {
                    title: Some("Claude Code"),
                    rows: if crate::island::claude::connected() {
                        vec![
                            Row::Info("Status", "Connected: sessions show in the island".to_string()),
                            Row::Buttons("", vec![("Disconnect", Hit::ConnectClaude(false)), ("Reconnect", Hit::ConnectClaude(true))]),
                        ]
                    } else {
                        vec![
                            Row::Info("Status", "Not connected".to_string()),
                            Row::Buttons("", vec![("Connect Claude Code", Hit::ConnectClaude(true))]),
                        ]
                    },
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
                        Row::Slider("Item spacing", Key::DockSpacing),
                        Row::Slider("Vertical offset", Key::DockOffset),
                        Row::Toggle("Automatically hide and show the Dock", Key::DockAutohide),
                    ]
                    .into_iter()
                    // A refinement of auto-hide, so only offered while it's on.
                    .chain(cfg.dock.autohide.then_some(Row::Toggle("Always show on the desktop", Key::DockShowOnDesktop)))
                    .chain([
                        Row::Segmented("Icon style", Key::DockIconStyle),
                        Row::Segmented("Background", Key::DockBackdrop),
                        Row::Toggle("Shadow", Key::DockShadow),
                        Row::Toggle("Show running apps", Key::DockRunning),
                        Row::Toggle("Show Trash", Key::DockTrash),
                    ])
                    .collect(),
                },
                Group {
                    title: Some("Custom icons"),
                    rows: vec![
                        Row::Info("Change an app's icon", "Right-click it in the Dock, then Change Icon\u{2026}".into()),
                        Row::Text("macOSicons.com API key", Key::DockMacIconsKey, "Optional: adds Mac-style icons"),
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
            Page::Finder => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Segmented("Sidebar", Key::FinderSidebar),
                        Row::Segmented("New windows open in", Key::FinderView),
                        Row::Toggle("Show hidden files", Key::FinderHidden),
                    ],
                },
            ],
            Page::Widgets => {
                use crate::widgets::{title, KINDS};
                let add = |kinds: &[crate::config::WidgetKind]| {
                    Row::Buttons("", kinds.iter().map(|&k| (title(k), Hit::AddWidget(k))).collect())
                };
                let count = cfg.widgets.items.len();
                vec![
                    Group {
                        title: None,
                        rows: vec![
                            Row::Toggle("Show widgets on the desktop", Key::WidgetsEnabled),
                            Row::Segmented("Background", Key::WidgetsBackdrop),
                            Row::Info(
                                "On the desktop",
                                match count {
                                    0 => "No widgets yet".to_string(),
                                    1 => "1 widget: drag to move, right-click to resize".to_string(),
                                    n => format!("{n} widgets: drag to move, right-click to resize"),
                                },
                            ),
                        ],
                    },
                    Group {
                        title: Some("Add a widget"),
                        rows: vec![
                            add(&KINDS[..3]),
                            add(&KINDS[3..6]),
                            add(&KINDS[6..]),
                            Row::Buttons("Start over with the default widgets", vec![("Reset Widgets…", Hit::ResetWidgets)]),
                        ],
                    },
                    Group {
                        title: Some("Weather"),
                        rows: vec![
                            Row::Text("Location", Key::WidgetsLocation, "Automatic, or a place like \u{201C}Paris, France\u{201D}"),
                            Row::Segmented("Temperature", Key::WidgetsUnit),
                        ],
                    },
                    Group {
                        title: Some("World Clock"),
                        rows: vec![Row::Text("Time zones", Key::WidgetsClocks, "America/New_York, Europe/London")],
                    },
                ]
            }
            Page::Desktops => vec![Group {
                title: None,
                rows: vec![
                    Row::Toggle("Alt+1 … Alt+9 switch desktops", Key::DesktopsAlt),
                    Row::Toggle("Win+Ctrl+← / → without the Windows popup", Key::DesktopsArrows),
                    Row::Toggle("Show switches in the Dynamic Island", Key::DesktopsIsland),
                ],
            }],
        }
    }

    fn toggle_on(login: bool, key: Key, cfg: &Config) -> bool {
        let (g, m, d) = (&cfg.general, &cfg.menubar, &cfg.dock);
        match key {
            Key::StartAtLogin => login,
            Key::HideTaskbar => g.hide_taskbar,
            Key::GlassRefraction => g.glass_refraction,
            Key::FinderHidden => cfg.finder.show_hidden,
            Key::DesktopsAlt => cfg.desktops.alt_number,
            Key::DesktopsArrows => cfg.desktops.replace_windows_switcher,
            Key::DesktopsIsland => cfg.desktops.island,
            Key::MbEnabled => m.enabled,
            Key::MbReserve => m.reserve_space,
            Key::MbBattery => m.show_battery,
            Key::MbBatteryPct => m.show_battery_percent,
            Key::MbWifi => m.show_wifi,
            Key::MbSearch => m.show_search,
            Key::MbControlCenter => m.show_control_center,
            Key::MbTray => m.show_tray,
            Key::Island(name) => island_flag(&cfg.island, name),
            Key::DockEnabled => d.enabled,
            Key::DockMag => d.magnification,
            Key::DockAutohide => d.autohide,
            Key::DockShowOnDesktop => d.show_on_desktop,
            Key::DockRunning => d.show_running,
            Key::DockTrash => d.show_trash,
            Key::DockShadow => d.shadow,
            Key::SpotlightEnabled => cfg.spotlight.enabled,
            Key::WidgetsEnabled => cfg.widgets.enabled,
            _ => false,
        }
    }

    /// (labels, values, selected index) for a segmented control.
    fn segments(fonts: &[String], key: Key, cfg: &Config) -> (Vec<String>, Vec<String>, Option<usize>) {
        let backdrop = |b: Backdrop| match b {
            Backdrop::Acrylic => 0,
            Backdrop::Blur => 1,
            Backdrop::Glass => 2,
            Backdrop::None => 3,
            Backdrop::Clear => 4,
        };
        let fixed = |pairs: &[(&str, &str)], sel: Option<usize>| {
            (pairs.iter().map(|p| p.0.to_string()).collect(), pairs.iter().map(|p| p.1.to_string()).collect(), sel)
        };
        const BACKDROPS: [(&str, &str); 4] =
            [("Acrylic", "acrylic"), ("Blur", "blur"), ("Glass", "glass"), ("None", "none")];
        const MB_BACKDROPS: [(&str, &str); 5] =
            [("Acrylic", "acrylic"), ("Blur", "blur"), ("Glass", "glass"), ("None", "none"), ("Clear", "clear")];
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
            Key::FinderSidebar => fixed(
                &[("Glass", "glass"), ("Acrylic", "acrylic"), ("Blur", "blur"), ("Solid", "none")],
                Some(match cfg.finder.sidebar {
                    Backdrop::Glass => 0,
                    Backdrop::Acrylic => 1,
                    Backdrop::Blur => 2,
                    Backdrop::None | Backdrop::Clear => 3,
                }),
            ),
            Key::FinderView => fixed(
                &[("Icons", "icons"), ("List", "list"), ("Columns", "columns")],
                Some(match cfg.finder.view {
                    crate::finder::View::Icons => 0,
                    crate::finder::View::List => 1,
                    crate::finder::View::Columns => 2,
                }),
            ),
            Key::MbBackdrop => fixed(&MB_BACKDROPS, Some(backdrop(cfg.menubar.backdrop))),
            Key::DockBackdrop => fixed(&BACKDROPS, Some(backdrop(cfg.dock.backdrop)).filter(|&i| i < BACKDROPS.len())),
            Key::WidgetsBackdrop => fixed(&BACKDROPS, Some(backdrop(cfg.widgets.backdrop)).filter(|&i| i < BACKDROPS.len())),
            Key::WidgetsUnit => {
                const UNITS: [(&str, &str); 3] = [("Automatic", "auto"), ("\u{B0}C", "c"), ("\u{B0}F", "f")];
                let current = cfg.widgets.temperature_unit.to_lowercase();
                fixed(&UNITS, Some(UNITS.iter().position(|u| u.1 == current).unwrap_or(0)))
            }
            Key::MbLogo => {
                let sel = match cfg.menubar.logo.to_lowercase().as_str() {
                    "apple" => Some(0),
                    "windows" => Some(1),
                    _ => None,
                };
                fixed(&[("Apple", "apple"), ("Windows", "windows")], sel)
            }
            Key::MbClock => fixed(&CLOCKS, CLOCKS.iter().position(|c| c.1 == cfg.menubar.clock_format)),
            Key::SpotlightHotkey => {
                const KEYS: [(&str, &str); 3] =
                    [("Alt+Space", "alt+space"), ("Ctrl+Space", "ctrl+space"), ("Ctrl+Alt+Space", "ctrl+alt+space")];
                let current = cfg.spotlight.hotkey.to_lowercase().replace(' ', "");
                fixed(&KEYS, KEYS.iter().position(|k| k.1 == current))
            }
            Key::SearchEngine => {
                let mut labels: Vec<String> = web::ENGINES.iter().map(|e| e.1.to_string()).collect();
                let mut values: Vec<String> = web::ENGINES.iter().map(|e| e.0.to_string()).collect();
                labels.push("Custom".into());
                values.push("custom".into());
                let sel = values.iter().position(|v| v.eq_ignore_ascii_case(&cfg.spotlight.search_engine));
                (labels, values, sel)
            }
            Key::IslandStyle => fixed(
                &[("Pill", "pill"), ("Notch", "notch")],
                Some((cfg.island.style == IslandStyle::Notch) as usize),
            ),
            Key::DockIconStyle => fixed(
                &[("Native", "native"), ("Squircle", "squircle")],
                Some((cfg.dock.icon_style == IconStyle::Squircle) as usize),
            ),
            _ => (vec![], vec![], None),
        }
    }

    fn text_value(key: Key, cfg: &Config) -> String {
        match key {
            Key::CustomSearchUrl => cfg.spotlight.custom_search_url.clone(),
            Key::DockMacIconsKey => cfg.dock.macosicons_key.clone(),
            Key::IslandIcs => cfg.island.calendar_ics.clone(),
            Key::WidgetsLocation => cfg.widgets.location.clone(),
            Key::WidgetsClocks => cfg.widgets.world_clocks.join(", "),
            _ => String::new(),
        }
    }

    /// Finishes editing a text row: an action when the value changed, nothing
    /// when unchanged, and keeps editing (marked invalid) when rejected.
    fn commit(&mut self, cfg: &Config) -> SettingsEvent {
        let Some(ed) = &mut self.editing else { return SettingsEvent::None };
        let text = ed.field.string().trim().to_string();
        let bad_link = ed.key == Key::IslandIcs && !crate::island::calendar_links_valid(&text);
        let zones: Vec<String> = text.split(',').map(|z| z.trim().to_string()).filter(|z| !z.is_empty()).collect();
        let bad_zone = ed.key == Key::WidgetsClocks && zones.iter().any(|z| crate::widgets::sources::parse_zone(z).is_none());
        if bad_link || bad_zone || ed.key == Key::CustomSearchUrl && !text.is_empty() && !web::valid_template(&text) {
            ed.invalid = true;
            self.dirty = true;
            return SettingsEvent::None;
        }
        let key = ed.key;
        self.editing = None;
        self.dirty = true;
        if text == Self::text_value(key, cfg) {
            return SettingsEvent::None;
        }
        if key == Key::WidgetsClocks {
            return SettingsEvent::Action(key.set(ConfigValue::List(zones)));
        }
        SettingsEvent::Action(key.set(ConfigValue::Str(text)))
    }

    /// Keyboard input while the window is focused.
    pub fn on_key(&mut self, vk: u16, ctrl: bool, shift: bool, cfg: &Config) -> SettingsEvent {
        const ENTER: u16 = 0x0D;
        const ESC: u16 = 0x1B;
        let Some(ed) = &mut self.editing else {
            return if vk == ESC { SettingsEvent::Close } else { SettingsEvent::None };
        };
        self.dirty = true;
        match vk {
            ENTER => self.commit(cfg),
            ESC => {
                self.editing = None;
                SettingsEvent::None
            }
            _ => {
                if ed.field.on_key(vk, ctrl, shift) == crate::finder::text::KeyResult::Edited {
                    ed.invalid = false;
                }
                SettingsEvent::None
            }
        }
    }

    pub fn on_char(&mut self, c: char) {
        if let Some(ed) = self.editing.as_mut().filter(|_| !c.is_control()) {
            ed.field.insert(&c.to_string());
            ed.invalid = false;
            self.dirty = true;
        }
    }

    fn slider_spec(key: Key, cfg: &Config) -> (f32, f32, f32) {
        match key {
            Key::DockSize => (cfg.dock.tile_size, 24.0, 96.0),
            Key::MbHeight => (cfg.menubar.height, 22.0, 40.0),
            Key::DockMagSize => (cfg.dock.magnified_size, 48.0, 160.0),
            Key::DockOffset => (cfg.dock.bottom_margin, 0.0, 120.0),
            Key::DockSpacing => (cfg.dock.item_spacing, 0.0, 50.0),
            Key::IslandOffset => (cfg.island.vertical_offset, -10.0, 40.0),
            _ => (0.0, 0.0, 1.0),
        }
    }

    // --- input -------------------------------------------------------------

    pub fn on_move(&mut self, x: f32, y: f32) {
        if let Some(s) = &mut self.slider {
            self.dirty |= s.drag(x);
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
        self.scroll.wheel(delta, 56.0, self.max_scroll);
        // Only the page moved: unless a whole redraw is already due, repaint just it.
        if !self.dirty || self.part.is_some() {
            let vp = Self::viewport();
            self.part = Some(self.part.map_or(vp, |p| p.union(vp)));
        }
        self.dirty = true;
    }

    pub fn on_down(&mut self, x: f32, y: f32, cfg: &Config) {
        self.pressed = self.hit_at(x, y);
        if let Some(Hit::Slider(key)) = self.pressed {
            if let Some((track, _)) = self.hits.iter().find(|(_, h)| *h == Hit::Slider(key)) {
                self.slider = Some(SliderDrag::begin(key, *track, x, cfg, self.pending));
            }
        }
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32, cfg: &Config) -> SettingsEvent {
        let pressed = self.pressed.take();
        self.dirty = true;
        if let Some(s) = self.slider.take() {
            let Some((pending, action)) = s.finish(cfg) else {
                self.pending = None;
                return SettingsEvent::None;
            };
            self.pending = Some(pending);
            return SettingsEvent::Action(action);
        }
        let hit = self.hit_at(x, y).filter(|h| Some(*h) == pressed);
        // Clicking anywhere outside the field being edited commits it.
        if let Some(ed) = &self.editing {
            if hit != Some(Hit::Field(ed.key)) {
                return self.commit(cfg);
            }
        }
        let Some(hit) = hit else { return SettingsEvent::None };
        let action = match hit {
            Hit::Field(key) => {
                if self.editing.is_none() {
                    let mut field = TextField::new(&Self::text_value(key, cfg));
                    field.select_all();
                    self.editing = Some(Editing { key, field, invalid: false });
                }
                return SettingsEvent::None;
            }
            Hit::Close => return SettingsEvent::Close,
            Hit::Minimize => {
                self.window.minimize();
                return SettingsEvent::None;
            }
            Hit::Page(p) => {
                self.go_to(p);
                return SettingsEvent::None;
            }
            Hit::Toggle(_) | Hit::Segment(..) | Hit::Swatch(_) => match control_action(hit, &mut self.login, &self.fonts, cfg) {
                Some(a) => a,
                None => return SettingsEvent::None,
            },
            Hit::Onboarding => Action::OpenOnboarding,
            Hit::RunAsAdmin => Action::SetRunAsAdmin(!self.admin),
            Hit::Remove(i) => {
                let mut apps = cfg.dock.apps.clone();
                if i < apps.len() {
                    apps.remove(i);
                }
                Action::SetDockApps(apps)
            }
            Hit::AddApp => Action::AddDockApp,
            Hit::OpenConfig => Action::OpenConfig,
            Hit::OpenUpdate => Action::Open(
                crate::update::available().map_or_else(|| crate::update::REPO_URL.to_string() + "/releases", |r| r.url),
            ),
            Hit::OpenLog => Action::Open(crate::config::dir().join("dockside.log").display().to_string()),
            Hit::ConnectClaude(on) => Action::ConnectClaude(on),
            Hit::AddWidget(kind) => Action::Widget(crate::widgets::WidgetCmd::Add(kind)),
            Hit::ResetWidgets => Action::Widget(crate::widgets::WidgetCmd::Reset),
            Hit::Slider(_) => return SettingsEvent::None,
        };
        SettingsEvent::Action(action)
    }

    // --- drawing -----------------------------------------------------------

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme, cfg: &Config, icons: &mut IconStore) {
        self.dirty = false;
        if !self.window.begin_render(self.hwnd) {
            return;
        }
        self.motion.sweep();
        let style = Style::new(gfx, theme);
        let (regular, control_bg) = (&style.regular, style.control_bg);
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
        let viewport = Self::viewport();

        // Measure content height for scrolling.
        let content_h: f32 = groups
            .iter()
            .map(|g| if g.title.is_some() { 26.0 } else { 0.0 } + g.rows.len() as f32 * ROW_H + 20.0)
            .sum::<f32>()
            + 8.0;
        self.max_scroll = (content_h - viewport.h).max(0.0);
        self.scroll.clamp(self.max_scroll);
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        let (zoom_x, zoom_y, zoom, window_alpha) = self.window.transform(body);

        if let Some((key, v)) = self.pending {
            if Self::slider_spec(key, cfg).0.round() == v {
                self.pending = None;
            }
        }

        let mut hits: Vec<(Rect, Hit)> = Vec::new();
        let motion = &self.motion;
        let scroll = self.scroll.offset_px(self.scale);
        let page_row = |k: usize| Rect::new(body.x + 10.0, body.y + TITLE_H + k as f32 * 32.0, SIDEBAR - 20.0, 30.0);
        let side_target = match self.hover {
            Some(Hit::Page(pg)) if pg != self.page => PAGES.iter().position(|(p, _)| *p == pg).map(page_row),
            _ => None,
        };
        let side_glide = self.side_glide.update(side_target);
        let sel_glide = self.sel_glide.update(PAGES.iter().position(|(p, _)| *p == self.page).map(page_row));
        let (hover, page, slider, pending) = (self.hover, self.page, self.slider, self.pending);
        let page_title = PAGES.iter().find(|p| p.0 == page).map(|p| p.1).unwrap_or("");
        let ctx = RowCtx {
            theme,
            cfg,
            style: &style,
            hover,
            slider,
            pending,
            login: self.login,
            fonts: &self.fonts,
            bitmaps: Some(&self.bitmaps),
            editing: self.editing.as_ref(),
            motion: Some(motion),
        };

        let r = {
            let draw = |p: &Painter| p.scaled_about(zoom_x, zoom_y, zoom, || {
                chrome::window(p, body, SIDEBAR, theme, &chrome::Palette::new(theme));
                let group_hover = motion.fade(Anim::Lights, matches!(hover, Some(Hit::Close | Hit::Minimize)));
                for (r, light) in chrome::traffic_lights(p, body, group_hover, false) {
                    hits.push((r, if light == chrome::Light::Close { Hit::Close } else { Hit::Minimize }));
                }

                // Sidebar: the hover highlight and the selection both glide from
                // page to page. Names under the selection are redrawn white, clipped
                // to it, so they turn exactly where the highlight is.
                if let Some((g, a)) = side_glide {
                    p.fill_round(g, 6.0, control_bg.alpha(control_bg.a * a));
                }
                let name_rect = |row: Rect| Rect::new(row.x + 34.0, row.y, row.w - 40.0, row.h);
                for (k, (pg, name)) in PAGES.into_iter().enumerate() {
                    let row = page_row(k);
                    draw_page_icon(p, pg, Rect::new(row.x + 6.0, row.cy() - 10.0, 20.0, 20.0), &glyph_font);
                    p.text(name, regular, name_rect(row), theme.text);
                    hits.push((row, Hit::Page(pg)));
                }
                if let Some((g, a)) = sel_glide {
                    p.faded(a, || {
                        p.fill_round(g, 6.0, theme.accent);
                        p.clipped(g, || {
                            for (k, (pg, name)) in PAGES.into_iter().enumerate() {
                                let row = page_row(k);
                                if row.bottom() > g.y && row.y < g.bottom() {
                                    draw_page_icon(p, pg, Rect::new(row.x + 6.0, row.cy() - 10.0, 20.0, 20.0), &glyph_font);
                                    p.text(name, regular, name_rect(row), Color::WHITE);
                                }
                            }
                        });
                    });
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
                        draw_card(p, card, &group.rows, &ctx, &mut hits);
                        y += card.h + 20.0;
                    }
                });
            });
            let part = self.part.take().filter(|_| zoom == 1.0 && window_alpha == 1.0).and_then(|r| self.canvas.part_px(r));
            self.drawn_part = part;
            self.canvas.draw_part(gfx, part, draw)
        };
        // Content hits outside the viewport (scrolled away) must not be clickable.
        hits.retain(|(r, h)| matches!(h, Hit::Close | Hit::Minimize | Hit::Page(_)) || (r.y >= viewport.y - 1.0 && r.bottom() <= viewport.bottom() + 1.0));
        self.hits = hits;

        let presented = r.and_then(|_| self.canvas.present_part(self.hwnd, rc.left, rc.top, window_alpha, self.drawn_part));
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
        // A window fading out may outlive the next one to open: only clear our own handle.
        let _ = SETTINGS_HWND.compare_exchange(self.hwnd.0 as isize, 0, Ordering::Relaxed, Ordering::Relaxed);
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// One row's label and control, drawn into `r`.
fn draw_row(p: &Painter, row: &Row, r: Rect, ctx: &RowCtx, hits: &mut Vec<(Rect, Hit)>) {
    let (theme, cfg, st) = (ctx.theme, ctx.cfg, ctx.style);
    let (regular, right, center, button_font) = (&st.regular, &st.right, &st.center, &st.button);
    let (card_border, control_bg, selected_seg, switch_off, field_bg) =
        (st.card_border, st.control_bg, st.selected_seg, st.switch_off, st.field_bg);
    let (slider, pending, login, fonts, editing) = (ctx.slider, ctx.pending, ctx.login, ctx.fonts, ctx.editing);
    let label_r = Rect::new(r.x + 14.0, r.y, r.w * 0.5, r.h);
    let right_edge = r.right() - 14.0;
    match row {
        Row::Toggle(label, key) => {
            p.text(label, &regular, Rect::new(label_r.x, r.y, r.w - 80.0, r.h), theme.text);
            let on = SettingsWindow::toggle_on(login, *key, cfg);
            let t = ctx.anim(Anim::Knob(*key), if on { 1.0 } else { 0.0 }, KNOB_FEEL);
            let track = Rect::new(right_edge - 34.0, r.cy() - 10.0, 34.0, 20.0);
            p.fill_round(track, 10.0, switch_off.mix(theme.accent, t));
            let kx = lerp(track.x + 10.0, track.right() - 10.0, t);
            p.fill_circle(kx, r.cy() + 0.5, 8.5, Color::gray(0, 0.12));
            p.fill_circle(kx, r.cy(), 8.5, Color::WHITE);
            hits.push((track.inset(-4.0, -6.0), Hit::Toggle(*key)));
        }
        Row::Segmented(label, key) => {
            p.text(label, &regular, label_r, theme.text);
            let (labels, _, sel) = SettingsWindow::segments(fonts, *key, cfg);
            let widths: Vec<f32> = labels.iter().map(|l| p.gfx.measure(l, &center) + 22.0).collect();
            let total: f32 = widths.iter().sum::<f32>() + 4.0;
            let outer = Rect::new(right_edge - total, r.cy() - 12.0, total, 24.0);
            p.fill_round(outer, 7.0, control_bg);
            let seg_x = |i: usize| outer.x + 2.0 + widths[..i].iter().sum::<f32>();
            for (i, w) in widths.iter().enumerate() {
                let t = if sel == Some(i) { 0.0 } else { ctx.hover_t(Hit::Segment(*key, i)) };
                if t > 0.0 {
                    p.fill_round(Rect::new(seg_x(i), outer.y + 2.0, *w, 20.0), 5.0, control_bg.alpha(control_bg.a * t));
                }
            }
            // The selected pill slides (and resizes) to the new choice.
            if let Some(s) = sel {
                let x = ctx.anim(Anim::SegX(*key), seg_x(s), feel::SLIDE);
                let w = ctx.anim(Anim::SegW(*key), widths[s], feel::SLIDE);
                let pill = Rect::new(x, outer.y + 2.0, w, 20.0);
                p.fill_round(pill.offset(0.0, 0.5), 5.0, Color::gray(0, 0.08));
                p.fill_round(pill, 5.0, selected_seg);
            }
            let mut sx = outer.x + 2.0;
            for (i, (l, w)) in labels.iter().zip(&widths).enumerate() {
                let seg = Rect::new(sx, outer.y + 2.0, *w, 20.0);
                p.text(l, &center, seg, theme.text);
                hits.push((seg, Hit::Segment(*key, i)));
                sx += w;
            }
        }
        Row::Slider(label, key) => {
            p.text(label, &regular, label_r, theme.text);
            let (mut value, min, max) = SettingsWindow::slider_spec(*key, cfg);
            if let Some((_, v)) = pending.filter(|p| p.0 == *key) {
                value = v;
            }
            let mut t = ((value - min) / (max - min)).clamp(0.0, 1.0);
            if let Some(s) = slider.filter(|s| s.key == *key) {
                (value, t) = (s.value(), s.t);
            }
            let track = Rect::new(right_edge - 240.0, r.cy() - 2.0, 240.0, 4.0);
            let kx = track.x + KNOB + (track.w - 2.0 * KNOB) * t;
            p.fill_round(track, 2.0, switch_off);
            // The fill always ends under the knob, so nothing pokes out at the minimum.
            p.fill_round(Rect::new(track.x, track.y, kx - track.x, track.h), 2.0, theme.accent);
            p.fill_circle(kx, r.cy() + 0.5, KNOB, Color::gray(0, 0.15));
            p.fill_circle(kx, r.cy(), KNOB, Color::WHITE);
            let value_r = Rect::new(track.x - 62.0, r.y, 50.0, r.h);
            p.text(&format!("{}", value as i32), &right, value_r, theme.text_secondary);
            hits.push((Rect::new(track.x, r.y, track.w, r.h), Hit::Slider(*key)));
        }
        Row::Swatches(label) => {
            p.text(label, &regular, label_r, theme.text);
            for (i, hex) in ACCENTS.iter().enumerate() {
                let c = Color::parse(hex).unwrap_or(theme.accent);
                let cx = right_edge - 9.0 - (ACCENTS.len() - 1 - i) as f32 * 26.0;
                let selected = cfg.general.accent.eq_ignore_ascii_case(hex);
                // The ring and dot grow onto the chosen colour and shrink off the old one.
                let t = ctx.anim(Anim::Swatch(i), if selected { 1.0 } else { 0.0 }, feel::SLIDE).max(0.0);
                if t > 0.01 {
                    p.stroke_circle(cx, r.cy(), lerp(8.5, 11.5, t), c.alpha(c.a * t.min(1.0)), 2.0);
                }
                p.fill_circle(cx, r.cy(), 8.5, c);
                if t > 0.01 {
                    p.fill_circle(cx, r.cy(), 3.0 * t, Color::WHITE);
                }
                hits.push((Rect::new(cx - 12.0, r.cy() - 12.0, 24.0, 24.0), Hit::Swatch(i)));
            }
        }
        Row::App(i) => {
            if let Some(app) = cfg.dock.apps.get(*i) {
                let key = app.icon.clone().unwrap_or_else(|| app.path.clone());
                let icon = Rect::new(r.x + 12.0, r.cy() - 13.0, 26.0, 26.0);
                if let Some(Some(bmp)) = ctx.bitmaps.and_then(|b| b.get(&key)) {
                    p.bitmap(bmp, icon, 1.0);
                }
                let name = app.name.clone().unwrap_or_else(|| app.path.clone());
                p.text(&name, &regular, Rect::new(icon.right() + 10.0, r.y, r.w * 0.45, r.h), theme.text);
                let path_r = Rect::new(r.x + r.w * 0.45, r.y, r.w * 0.55 - 52.0, r.h);
                p.text(&app.path, &right, path_r, theme.text_disabled);
                let (cx, cy) = (right_edge - 9.0, r.cy());
                let hot = ctx.hover_t(Hit::Remove(*i));
                p.fill_circle(cx, cy, 9.0, control_bg.mix(Color::rgba(255, 82, 87, 1.0), hot));
                p.line(cx - 4.0, cy, cx + 4.0, cy, theme.text_secondary.mix(Color::WHITE, hot), 1.6);
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
                let bg = control_bg.mix(selected_seg, ctx.hover_t(*hit));
                p.fill_round(b, 6.0, bg);
                p.stroke_round(b.inset(0.25, 0.25), 6.0, card_border, 0.5);
                p.text(text, &button_font, b, theme.text);
                hits.push((b, *hit));
                bx -= 8.0;
            }
        }
        Row::Text(label, key, placeholder) => {
            p.text(label, &regular, label_r, theme.text);
            let f = Rect::new(right_edge - 320.0, r.cy() - 12.0, 320.0, 24.0);
            p.fill_round(f, 6.0, field_bg);
            let ed = editing.filter(|e| e.key == *key);
            let ring = match ed {
                Some(e) if e.invalid => Color::rgba(255, 69, 58, 1.0),
                Some(_) => theme.accent,
                None => card_border,
            };
            p.stroke_round(f.inset(0.5, 0.5), 6.0, ring, if ed.is_some() { 2.0 } else { 1.0 });
            let tr = Rect::new(f.x + 8.0, f.y, f.w - 16.0, f.h);
            match ed {
                Some(e) => {
                    let prefix = |n: usize| p.gfx.measure(&e.field.text[..n].iter().collect::<String>(), &regular);
                    let (a, b) = e.field.range();
                    if a != b {
                        let (x0, x1) = (prefix(a), prefix(b));
                        p.fill_rect(Rect::new(tr.x + x0, f.y + 4.0, x1 - x0, f.h - 8.0), theme.accent.alpha(0.35));
                    } else {
                        let cx = tr.x + prefix(a) + 0.5;
                        p.line(cx, f.y + 5.0, cx, f.bottom() - 5.0, theme.text, 1.0);
                    }
                    p.text(&e.field.string(), &regular, tr, theme.text);
                }
                None => {
                    let mut value = SettingsWindow::text_value(*key, cfg);
                    // A calendar link is a secret: show where it points, not the link.
                    if *key == Key::IslandIcs && !value.is_empty() {
                        value = crate::island::calendar_links_summary(&value);
                    }
                    // So is an API key: show only its last few characters.
                    if *key == Key::DockMacIconsKey && !value.is_empty() {
                        let tail: String = value.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
                        value = format!("\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022} {tail}");
                    }
                    if value.is_empty() {
                        p.text(placeholder, &regular, tr, theme.text_disabled);
                    } else {
                        p.text(&value, &regular, tr, theme.text);
                    }
                }
            }
            hits.push((f, Hit::Field(*key)));
        }
        Row::Info(label, value) => {
            p.text(label, &regular, label_r, theme.text);
            let vr = Rect::new(r.x + r.w * 0.3, r.y, r.w * 0.7 - 14.0, r.h);
            p.text(value, &right, vr, theme.text_secondary);
        }
    }
}

/// Colored rounded-square page icons, like macOS System Settings.
pub(crate) fn draw_page_icon(p: &Painter, page: Page, r: Rect, glyph_font: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat) {
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
        Page::Island => {
            p.fill_round(r, 5.0, Color::rgba(28, 28, 30, 1.0));
            let pill = Rect::new(r.cx() - r.w * 0.3, r.y + r.h * 0.3, r.w * 0.6, r.h * 0.22);
            p.fill_round(pill, pill.h / 2.0, Color::rgba(0, 0, 0, 1.0));
            p.stroke_round(pill, pill.h / 2.0, white.alpha(0.55), 1.0);
            p.fill_circle(pill.right() - pill.h * 0.7, pill.cy(), pill.h * 0.22, Color::rgba(48, 209, 88, 1.0));
        }
        Page::Dock => {
            p.fill_round(r, 5.0, Color::rgba(28, 28, 30, 1.0));
            let screen = r.inset(4.0, 5.0);
            p.stroke_round(screen, 1.5, white, 1.2);
            p.fill_round(Rect::new(screen.cx() - 4.0, screen.bottom() - 3.5, 8.0, 2.0), 1.0, white);
        }
        Page::Desktops => {
            p.fill_round(r, 5.0, Color::rgba(88, 86, 214, 1.0));
            let (w, h) = (r.w * 0.5, r.h * 0.36);
            p.stroke_round(Rect::new(r.cx() - w / 2.0 + 2.0, r.cy() - h / 2.0 - 2.0, w, h), 1.2, white.alpha(0.6), 1.1);
            p.fill_round(Rect::new(r.cx() - w / 2.0 - 1.5, r.cy() - h / 2.0 + 1.5, w, h), 1.2, white);
        }
        Page::Widgets => {
            p.fill_round(r, 5.0, Color::rgba(255, 149, 0, 1.0));
            let k = r.w / 20.0;
            let tile = |x: f32, y: f32, w: f32, h: f32| p.fill_round(Rect::new(r.x + x * k, r.y + y * k, w * k, h * k), 1.6 * k, white);
            tile(4.0, 4.0, 5.5, 5.5);
            tile(10.5, 4.0, 5.5, 5.5);
            tile(4.0, 10.5, 12.0, 5.5);
        }
        Page::Finder => {
            // The two-tone Finder face.
            p.fill_round(r, 5.0, Color::rgba(30, 150, 255, 1.0));
            p.clipped(Rect::new(r.x, r.y, r.w * 0.5, r.h), || p.fill_round(r, 5.0, Color::rgba(120, 205, 255, 1.0)));
            let ink = Color::rgba(20, 40, 70, 1.0);
            p.line(r.x + r.w * 0.36, r.y + r.h * 0.3, r.x + r.w * 0.36, r.y + r.h * 0.45, ink, 1.3);
            p.line(r.x + r.w * 0.68, r.y + r.h * 0.3, r.x + r.w * 0.68, r.y + r.h * 0.45, ink, 1.3);
            p.line(r.x + r.w * 0.3, r.y + r.h * 0.66, r.x + r.w * 0.5, r.y + r.h * 0.72, ink, 1.2);
            p.line(r.x + r.w * 0.5, r.y + r.h * 0.72, r.x + r.w * 0.72, r.y + r.h * 0.64, ink, 1.2);
        }
    }
}


#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore]
    fn bench_settings() {
        use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let _ = crate::win::register_class(Some(proc_));
        let gfx = Gfx::new().unwrap();
        let cfg = crate::config::defaults();
        let theme = Theme::new(&cfg, &gfx);
        let mut icons = IconStore::default();
        let mut s = SettingsWindow::new(&gfx, &Monitor::primary(), Page::Appearance).unwrap();
        s.shown = true;
        s.render(&gfx, &theme, &cfg, &mut icons);
        unsafe {
            let _ = ShowWindow(s.hwnd, SW_SHOWNOACTIVATE);
        }
        let t = std::time::Instant::now();
        for k in 0..120 {
            s.on_wheel(if k % 20 < 10 { -120 } else { 120 });
            s.tick(1.0 / 165.0);
            s.render(&gfx, &theme, &cfg, &mut icons);
            unsafe {
                let _ = windows::Win32::Graphics::Dwm::DwmFlush();
            }
        }
        println!("on screen: {:.0} fps", 120.0 / t.elapsed().as_secs_f64());
        // Partial frames (scrolling, then a sidebar glide) must match a full repaint.
        s.on_wheel(-240);
        s.hover = Some(Hit::Page(Page::Dock));
        s.dirty = true;
        s.render(&gfx, &theme, &cfg, &mut icons);
        s.hover = Some(Hit::Page(Page::General));
        let mut partial = 0;
        for _ in 0..60 {
            if s.tick(1.0 / 165.0) {
                partial += s.part.is_some() as u32;
                s.render(&gfx, &theme, &cfg, &mut icons);
            }
        }
        let after = s.canvas.pixels();
        s.dirty = true;
        s.render(&gfx, &theme, &cfg, &mut icons);
        let full = s.canvas.pixels();
        let differing = after.chunks(4).zip(full.chunks(4)).filter(|(a, b)| a != b).count();
        println!("{partial} partial frames; pixels differing from a full repaint: {differing}");
        // Antialiasing on curves may shift a level or two; nothing may be stale.
        let worst = after.iter().zip(&full).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
        println!("largest difference: {worst} / 255");
        assert!(partial > 0);
        assert!(worst <= 2);
        let t = std::time::Instant::now();
        for _ in 0..60 {
            s.render(&gfx, &theme, &cfg, &mut icons);
        }
        println!("settings render {:.2} ms, device context: {}", t.elapsed().as_secs_f64() * 1000.0 / 60.0, windows::core::Interface::cast::<windows::Win32::Graphics::Direct2D::ID2D1DeviceContext>(s.canvas.target()).is_ok());
    }
}
