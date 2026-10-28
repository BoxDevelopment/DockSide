//! The Setup Assistant: a welcome screen, one page per group of basic settings
//! (with the same controls as System Settings, applied as they change) and a
//! finish screen. It opens on launch until `[general] onboarding_done` is set,
//! which finishing or skipping does; System Settings and Spotlight reopen it.

use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{feel, lerp, Motion, Spring};
use crate::chrome::{self, WindowAnim, SHADOW};
use crate::config::Config;
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Canvas, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::settings::{self, Anim, Hit, Key, Page, Recording, Row, RowCtx, SliderDrag, Style, ROW_H};
use crate::theme::Theme;
use crate::win::{self, startup, Monitor};

/// Dock path for this window, which shows in the Dock like an app.
pub const ONBOARDING_PATH: &str = "dockside:setup-assistant";

const W: f32 = 620.0;
const H: f32 = 580.0;
const HERO: f32 = 72.0;
const CARD_Y: f32 = 276.0;
const CARD_PAD: f32 = 48.0;
const FOOTER_H: f32 = 64.0;

/// The picture at the top of a page.
#[derive(Clone, Copy)]
enum Hero {
    Welcome,
    /// The page's icon from the System Settings sidebar.
    Page(Page),
    /// A Segoe Fluent Icons glyph on a colored tile.
    Glyph(Color, char),
    Glass,
    Done,
}

struct Step {
    hero: Hero,
    title: &'static str,
    text: &'static str,
}

const STEPS: [Step; 11] = [
    Step {
        hero: Hero::Welcome,
        title: "Welcome to DockSide",
        text: "DockSide brings a Mac-style Dock, menu bar, Dynamic Island, Spotlight and widgets to Windows. \
               Let\u{2019}s take a minute to set it up the way you like. Everything you change here works right away, \
               and you can change it again any time in System Settings.",
    },
    Step {
        hero: Hero::Page(Page::Appearance),
        title: "Make it yours",
        text: "Pick a light or dark look, or let DockSide follow your Windows theme automatically. \
               The accent color tints switches, selections and highlights.",
    },
    Step {
        hero: Hero::Page(Page::Dock),
        title: "Your Dock",
        text: "The Dock holds your favorite apps and shows the ones that are running. Make it as big as you like, \
               let icons grow as you point at them, or tuck it away until you move the pointer to the bottom of the screen.",
    },
    Step {
        hero: Hero::Page(Page::MenuBar),
        title: "The menu bar",
        text: "The bar across the top shows the app you\u{2019}re using, the clock, battery, Wi-Fi and Control Center. \
               \u{201C}Clear\u{201D} drops the bar entirely so the text sits right on your wallpaper, like macOS Tahoe.",
    },
    Step {
        hero: Hero::Page(Page::Island),
        title: "The Dynamic Island",
        text: "A small island at the top of the screen comes alive with music, timers, downloads and meetings. \
               It can also stand in for the Windows volume and Caps Lock pop-ups with tidier ones of its own.",
    },
    Step {
        hero: Hero::Glass,
        title: "Liquid Glass",
        text: "Glass backgrounds bend and magnify what\u{2019}s behind them, like Apple\u{2019}s Liquid Glass. \
               To do that, glass is left out of screenshots and screen sharing; turn refraction off \
               for flat glass that shows up in captures.",
    },
    Step {
        hero: Hero::Page(Page::Desktops),
        title: "Switch desktops in a flash",
        text: "Jump straight to a virtual desktop with Alt+1 through Alt+9, or slide between them with \
               Win+Ctrl+\u{2190} and \u{2192}, without the Windows pop-up getting in the way.",
    },
    Step {
        hero: Hero::Page(Page::Widgets),
        title: "Widgets on your desktop",
        text: "Glanceable widgets for the time, weather, calendar, batteries and notes sit on your wallpaper, \
               below your windows. Drag one to move it; right-click it to resize or remove it.",
    },
    Step {
        hero: Hero::Glyph(Color::rgba(88, 86, 214, 1.0), '\u{E721}'),
        title: "Spotlight",
        text: "Press a shortcut from anywhere to find apps, files and settings, do quick math, \
               or run commands like \u{201C}dark mode\u{201D} and \u{201C}lock\u{201D}. Pick the shortcut that feels natural.",
    },
    Step {
        hero: Hero::Page(Page::General),
        title: "Ready when you are",
        text: "Start DockSide when you sign in, so your Dock and menu bar are always there. DockSide can also hide \
               the Windows taskbar while it runs; the taskbar comes back as soon as DockSide quits.",
    },
    Step {
        hero: Hero::Done,
        title: "You\u{2019}re all set",
        text: "Enjoy DockSide! Everything else, like pinned apps, fonts and island alerts, is in DockSide\u{2019}s \
               System Settings. Here\u{2019}s how to get there whenever you need it:",
    },
];

const LAST: usize = STEPS.len() - 1;

/// The controls on a page (some only make sense while another is on).
fn rows(step: usize, cfg: &Config) -> Vec<Row> {
    match step {
        1 => vec![Row::Segmented("Appearance", Key::Appearance), Row::Swatches("Accent color")],
        2 => {
            let mut rows = vec![Row::Slider("Size", Key::DockSize), Row::Toggle("Magnification", Key::DockMag)];
            if cfg.dock.magnification {
                rows.push(Row::Slider("Magnified size", Key::DockMagSize));
            }
            rows.push(Row::Toggle("Automatically hide and show the Dock", Key::DockAutohide));
            rows
        }
        3 => {
            let mut rows = vec![Row::Toggle("Show menu bar", Key::MbEnabled)];
            if cfg.menubar.enabled {
                rows.extend([Row::Segmented("Background", Key::MbBackdrop), Row::Segmented("Clock", Key::MbClock)]);
            }
            rows
        }
        4 => {
            let mut rows = vec![Row::Toggle("Show Dynamic Island", Key::Island("enabled"))];
            if cfg.island.enabled {
                rows.extend([
                    Row::Segmented("Style", Key::IslandStyle),
                    Row::Toggle("Volume pop-up", Key::Island("volume")),
                    Row::Toggle("Caps Lock pop-up", Key::Island("caps_lock")),
                ]);
            }
            rows
        }
        5 => vec![
            Row::Toggle("Refraction (hidden from screenshots)", Key::GlassRefraction),
            Row::Segmented("Try it on the Dock", Key::DockBackdrop),
        ],
        6 => vec![
            Row::Toggle("Alt+1 \u{2026} Alt+9 switch desktops", Key::DesktopsAlt),
            Row::Toggle("Win+Ctrl+\u{2190} / \u{2192} without the Windows popup", Key::DesktopsArrows),
            Row::Toggle("Show switches in the Dynamic Island", Key::DesktopsIsland),
        ],
        7 => {
            let mut rows = vec![Row::Toggle("Show widgets on the desktop", Key::WidgetsEnabled)];
            if cfg.widgets.enabled {
                rows.push(Row::Segmented("Background", Key::WidgetsBackdrop));
            }
            rows
        }
        8 => {
            let mut rows = vec![Row::Toggle("Spotlight search", Key::SpotlightEnabled)];
            if cfg.spotlight.enabled {
                rows.push(Row::Keybind("Keyboard shortcut", Key::SpotlightHotkey));
            }
            rows
        }
        9 => vec![
            Row::Toggle("Start DockSide at login", Key::StartAtLogin),
            Row::Toggle("Hide the Windows taskbar", Key::HideTaskbar),
        ],
        _ => vec![],
    }
}

/// Ways to open System Settings, for the last page: (Segoe Fluent glyph, how).
/// Ones that depend on a surface are left out while it's off.
fn settings_places(cfg: &Config) -> Vec<(char, String)> {
    let mut places = Vec::new();
    if cfg.menubar.enabled {
        places.push(('\u{E700}', "Click the logo at the left of the menu bar, then DockSide Settings\u{2026}".to_string()));
    }
    let mut surfaces = Vec::new();
    if cfg.dock.enabled {
        surfaces.push("the Dock");
    }
    if cfg.island.enabled {
        surfaces.push("the Dynamic Island");
    }
    if cfg.widgets.enabled {
        surfaces.push("a widget");
    }
    if !surfaces.is_empty() {
        let list = match surfaces.as_slice() {
            [a] => a.to_string(),
            [a, b] => format!("{a} or {b}"),
            [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
            [] => unreachable!(),
        };
        places.push(('\u{E962}', format!("Right-click {list} to jump to its settings")));
    }
    if cfg.spotlight.enabled {
        places.push(('\u{E721}', "Search \u{201C}DockSide Settings\u{201D} in Spotlight".to_string()));
    }
    places.push(('\u{E7C4}', "Open DockSide again from the Start menu".to_string()));
    places
}

/// A friendly tip under a page's controls.
fn note(step: usize, cfg: &Config) -> String {
    match step {
        0 => "Use \u{2190} and \u{2192} (or Enter) to move between pages, and Esc to skip.".into(),
        2 => "Right-click the Dock for more options.".into(),
        LAST => "To see this tour again, open System Settings \u{203A} General \u{203A} Setup Assistant.".into(),
        4 => "Click the island for timers, and drop files on it to keep them on its shelf.".into(),
        5 => "Glass appears wherever a background is set to \u{201C}Glass\u{201D}: the Dock, menu bar, widgets and Finder.".into(),
        6 => "These are Windows\u{2019} own virtual desktops: press Win+Ctrl+D to make a new one.".into(),
        7 => "Add more widgets in System Settings \u{203A} Widgets.".into(),
        8 if cfg.spotlight.enabled => {
            // Spelled out: the keys as printed on the keyboard read better in a sentence.
            match crate::keybind::Hotkey::parse(&cfg.spotlight.hotkey) {
                Some(h @ crate::keybind::Hotkey::Tap(m)) if m == crate::keybind::WIN => {
                    format!("Try it now: tap {}. Double-tap it for the Start menu.", h.label(false))
                }
                Some(h @ crate::keybind::Hotkey::Tap(_)) => format!("Try it now: tap {}.", h.label(false)),
                Some(h) => format!("Try it now: press {}.", h.label(false)),
                None => "Click the shortcut to choose one.".into(),
            }
        }
        _ => String::new(),
    }
}

/// Clickable parts of the window.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Target {
    Close,
    Minimize,
    Back,
    Next,
    Skip,
    /// "Open System Settings" on the last page.
    OpenSettings,
    Dot(usize),
    /// A settings control.
    Control(Hit),
}

pub enum OnboardingEvent {
    None,
    Action(Action),
    /// Close the assistant and remember it's done, then run the action, if any.
    Finish(Option<Action>),
}

pub struct Onboarding {
    pub hwnd: HWND,
    canvas: Canvas,
    scale: f32,
    step: usize,
    hits: Vec<(Rect, Target)>,
    hover: Option<Target>,
    pressed: Option<Target>,
    slider: Option<SliderDrag>,
    /// Slider value just released, shown until the config reload catches up.
    pending: Option<(Key, f32)>,
    /// The Spotlight shortcut row capturing keys.
    recording: Option<Recording>,
    login: bool,
    fonts: Vec<String>,
    shown: bool,
    /// Open / minimize zoom, control springs, and the slide between pages.
    window: WindowAnim,
    motion: Motion<Anim>,
    page_t: Spring,
    /// -1 going back a page, 1 going forward.
    page_dir: f32,
    pub tracking: bool,
    pub dirty: bool,
}

impl Onboarding {
    pub fn new(gfx: &Gfx, monitor: &Monitor) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + (mon.bottom - mon.top - h) / 2;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let hwnd = win::create_window(WS_EX_LAYERED | WS_EX_APPWINDOW, WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX, rect, HWND::default())?;
        unsafe {
            let _ = SetWindowTextW(hwnd, w!("Setup Assistant"));
        }
        Ok(Self {
            hwnd,
            canvas: Canvas::new(gfx, w, h, s)?,
            scale: s,
            step: 0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            slider: None,
            pending: None,
            recording: None,
            login: startup::is_enabled(),
            fonts: settings::installed_fonts(gfx),
            shown: false,
            window: WindowAnim::opening(),
            motion: Motion::default(),
            page_t: Spring::new(1.0, 0.22, 1.0),
            page_dir: 1.0,
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
        let moving = self.window.tick(dt, self.hwnd) | self.motion.tick(dt) | self.page_t.step(dt);
        self.dirty |= moving;
        moving
    }

    pub fn animating(&self) -> bool {
        self.window.moving() || self.motion.moving() || !self.page_t.settled()
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Brings the window back to the front.
    pub fn show(&mut self) {
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

    fn hit_at(&self, x: f32, y: f32) -> Option<Target> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, t)| *t)
    }

    /// Empty space above the controls drags the window.
    pub fn is_caption(&self, x: f32, y: f32) -> bool {
        let b = Self::body();
        Rect::new(b.x, b.y, b.w, CARD_Y - 8.0).contains(x, y) && self.hit_at(x, y).is_none()
    }

    fn go(&mut self, step: usize) {
        let step = step.min(LAST);
        if step != self.step {
            // The new page slides in from the side it's on.
            self.page_dir = if step > self.step { 1.0 } else { -1.0 };
            // Already mostly there on the first frame: a nudge, not a wait.
            self.page_t.snap(0.3);
            self.page_t.set(1.0);
            self.step = step;
            self.slider = None;
            self.pending = None;
            self.hover = None;
            self.dirty = true;
        }
    }

    fn next(&mut self) -> OnboardingEvent {
        if self.step == LAST {
            return OnboardingEvent::Finish(None);
        }
        self.go(self.step + 1);
        OnboardingEvent::None
    }

    /// A key captured while the shortcut row is recording.
    pub fn on_record_key(&mut self, wp: usize, cfg: &Config) -> OnboardingEvent {
        let Some(r) = &mut self.recording else { return OnboardingEvent::None };
        self.dirty = true;
        match r.on_key(wp, cfg) {
            None => OnboardingEvent::None,
            Some(action) => {
                self.recording = None;
                action.map_or(OnboardingEvent::None, OnboardingEvent::Action)
            }
        }
    }

    /// The window lost focus: stop taking the keyboard.
    pub fn on_deactivate(&mut self) {
        if self.recording.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn on_key(&mut self, vk: u16) -> OnboardingEvent {
        const ENTER: u16 = 0x0D;
        const ESC: u16 = 0x1B;
        const LEFT: u16 = 0x25;
        const RIGHT: u16 = 0x27;
        match vk {
            ENTER => self.next(),
            RIGHT if self.step < LAST => self.next(),
            LEFT => {
                self.go(self.step.saturating_sub(1));
                OnboardingEvent::None
            }
            ESC => OnboardingEvent::Finish(None),
            _ => OnboardingEvent::None,
        }
    }

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

    pub fn on_down(&mut self, x: f32, y: f32, cfg: &Config) {
        self.pressed = self.hit_at(x, y);
        if let Some(Target::Control(Hit::Slider(key))) = self.pressed {
            if let Some((track, _)) = self.hits.iter().find(|(_, t)| *t == Target::Control(Hit::Slider(key))) {
                self.slider = Some(SliderDrag::begin(key, *track, x, cfg, self.pending));
            }
        }
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32, cfg: &Config) -> OnboardingEvent {
        let pressed = self.pressed.take();
        self.dirty = true;
        if let Some(s) = self.slider.take() {
            let Some((pending, action)) = s.finish(cfg) else {
                self.pending = None;
                return OnboardingEvent::None;
            };
            self.pending = Some(pending);
            return OnboardingEvent::Action(action);
        }
        // Any click stops recording; clicking the recording row again only stops it.
        let was_recording = self.recording.take().map(|r| r.key());
        let Some(target) = self.hit_at(x, y).filter(|t| Some(*t) == pressed) else { return OnboardingEvent::None };
        match target {
            Target::Control(Hit::Keybind(key)) => {
                if was_recording != Some(key) {
                    self.recording = Recording::start(self.hwnd, key);
                }
                OnboardingEvent::None
            }
            Target::Close | Target::Skip => OnboardingEvent::Finish(None),
            Target::Minimize => {
                self.window.minimize();
                OnboardingEvent::None
            }
            Target::Back => {
                self.go(self.step.saturating_sub(1));
                OnboardingEvent::None
            }
            Target::Next => self.next(),
            Target::OpenSettings => OnboardingEvent::Finish(Some(Action::OpenSettings(None))),
            Target::Dot(i) => {
                self.go(i);
                OnboardingEvent::None
            }
            Target::Control(hit) => match settings::control_action(hit, &mut self.login, &self.fonts, cfg) {
                Some(a) => OnboardingEvent::Action(a),
                None => OnboardingEvent::None,
            },
        }
    }

    // --- drawing -----------------------------------------------------------

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme, cfg: &Config) {
        self.dirty = false;
        if !self.window.begin_render(self.hwnd) {
            return;
        }
        self.motion.sweep();
        if let Some((key, v)) = self.pending {
            if settings::slider_value(key, cfg).round() == v {
                self.pending = None;
            }
        }
        let frame = Frame {
            step: self.step,
            hover: self.hover,
            pressed: self.pressed,
            slider: self.slider,
            pending: self.pending,
            recording: self.recording.as_ref(),
            login: self.login,
            fonts: &self.fonts,
            motion: Some(&self.motion),
            page_t: self.page_t.value,
            page_dir: self.page_dir,
        };
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        let (zx, zy, zoom, alpha) = self.window.transform(Self::body());
        let mut hits = Vec::new();
        let r = self.canvas.draw(gfx, |p| p.scaled_about(zx, zy, zoom, || hits = paint(p, gfx, theme, cfg, &frame)));
        self.hits = hits;

        let presented = r.and_then(|_| self.canvas.present_alpha(self.hwnd, rc.left, rc.top, alpha));
        if let Err(e) = presented {
            log!("setup assistant render failed: {e}");
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

impl Drop for Onboarding {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// The window's state for one paint.
struct Frame<'a> {
    step: usize,
    hover: Option<Target>,
    pressed: Option<Target>,
    slider: Option<SliderDrag>,
    pending: Option<(Key, f32)>,
    recording: Option<&'a Recording>,
    login: bool,
    fonts: &'a [String],
    motion: Option<&'a Motion<Anim>>,
    /// Page transition (0 -> 1) and the side the new page comes from.
    page_t: f32,
    page_dir: f32,
}

/// Paints the whole window (body at `Onboarding::body()`) and returns its hit rects.
fn paint(p: &Painter, gfx: &Gfx, theme: &Theme, cfg: &Config, f: &Frame) -> Vec<(Rect, Target)> {
    {
        let style = Style::new(gfx, theme);
        let font = theme.font.as_str();
        let caption_font = gfx.format(font, 11.0, Weight::Bold, Align::Center);
        let title_font = gfx.format(font, 24.0, Weight::Bold, Align::Center);
        let text_font = gfx.format_block(font, 13.5, Weight::Regular, Align::Center);
        let note_font = gfx.format_block(font, 12.0, Weight::Regular, Align::Center);
        let button_font = gfx.format(font, 13.0, Weight::Regular, Align::Center);
        let small_font = gfx.format(font, 11.0, Weight::Regular, Align::Center);
        let glyph_small = gfx.format("Segoe Fluent Icons", 12.0, Weight::Regular, Align::Center);
        let glyph_big = gfx.format("Segoe Fluent Icons", 34.0, Weight::Regular, Align::Center);

        let (step, hover, pressed) = (f.step, f.hover, f.pressed);
        let page = &STEPS[step];
        let rows = rows(step, cfg);
        let note = note(step, cfg);
        let body = Onboarding::body();
        let ctx = RowCtx {
            theme,
            cfg,
            style: &style,
            hover: match hover {
                Some(Target::Control(h)) => Some(h),
                _ => None,
            },
            slider: f.slider,
            pending: f.pending,
            login: f.login,
            fonts: f.fonts,
            bitmaps: None,
            editing: None,
            recording: f.recording,
            motion: f.motion,
        };
        let mut hits: Vec<(Rect, Target)> = Vec::new();
        let content = if theme.dark { Color::rgba(30, 30, 30, 1.0) } else { Color::rgba(250, 250, 250, 1.0) };

        {
            chrome::shadow(p, body, theme);
            p.clear_rect(body.inset(4.0, 4.0));
            p.fill_round(body, chrome::RADIUS, content);
            // A soft wash of the accent color behind the hero.
            p.clipped(body, || {
                let glow = Rect::new(body.x, body.y, body.w, 240.0);
                p.fill_round_gradient(glow, chrome::RADIUS, theme.accent.alpha(if theme.dark { 0.16 } else { 0.10 }), content.alpha(0.0));
            });
            p.stroke_round(body.inset(0.25, 0.25), chrome::RADIUS, theme.menu_border, 0.5);

            let fade = |key: Anim, on: bool| f.motion.map_or(if on { 1.0 } else { 0.0 }, |m| m.fade(key, on));
            let group_hover = fade(Anim::Lights, matches!(hover, Some(Target::Close | Target::Minimize)));
            for (r, light) in chrome::traffic_lights(p, body, group_hover, false) {
                hits.push((r, if light == chrome::Light::Close { Target::Close } else { Target::Minimize }));
            }

            // The page itself (hero to note) slides and fades in from the side it's on.
            let page_a = f.page_t.clamp(0.0, 1.0);
            let shift = (1.0 - f.page_t) * 24.0 * f.page_dir;
            p.faded(page_a, || p.transformed(shift, 0.0, 1.0, || {
            // Hero, caption, title and explanation.
            let hero = Rect::new(body.cx() - HERO / 2.0, body.y + 44.0, HERO, HERO);
            draw_hero(p, page.hero, hero, theme, &glyph_small, &glyph_big);
            if (1..LAST).contains(&step) {
                let caption = format!("STEP {step} OF {}", LAST - 1);
                p.text(&caption, &caption_font, Rect::new(body.x, body.y + 126.0, body.w, 18.0), theme.accent);
            }
            p.text(page.title, &title_font, Rect::new(body.x + 40.0, body.y + 146.0, body.w - 80.0, 34.0), theme.text);
            p.text(page.text, &text_font, Rect::new(body.x + 60.0, body.y + 188.0, body.w - 120.0, 84.0), theme.text_secondary);

            // The page's settings, in a System Settings card.
            let card = Rect::new(body.x + CARD_PAD, body.y + CARD_Y, body.w - 2.0 * CARD_PAD, rows.len() as f32 * ROW_H);
            let mut below = card.y;
            if !rows.is_empty() {
                let mut controls = Vec::new();
                settings::draw_card(p, card, &rows, &ctx, &mut controls);
                hits.extend(controls.into_iter().map(|(r, h)| (r, Target::Control(h))));
                below = card.bottom();
            } else if step == 0 {
                below = draw_features(p, Rect::new(body.x + CARD_PAD, card.y, card.w, 76.0), theme, &style, &glyph_small, &small_font);
            } else if step == LAST {
                below = draw_places(p, Rect::new(card.x, card.y, card.w, 0.0), &settings_places(cfg), theme, &style, &glyph_small);
            }
            if !note.is_empty() {
                p.text(&note, &note_font, Rect::new(body.x + 60.0, below + 14.0, body.w - 120.0, 36.0), theme.text_secondary);
            }
            }));

            // Progress dots: one per page, clickable.
            let footer_y = body.bottom() - FOOTER_H;
            let gap = 14.0;
            let dots_w = gap * (STEPS.len() - 1) as f32;
            let dy = footer_y - 20.0;
            for i in 0..STEPS.len() {
                let cx = body.cx() - dots_w / 2.0 + i as f32 * gap;
                // The current dot grows and takes the accent; the old one shrinks back.
                let on = f.motion.map_or(if i == step { 1.0 } else { 0.0 }, |m| m.get(Anim::Dot(i), if i == step { 1.0 } else { 0.0 }, feel::SLIDE));
                let hot = fade(Anim::DotHover(i), hover == Some(Target::Dot(i)));
                let c = theme.text_disabled.mix(theme.text_secondary, hot).mix(theme.accent, on);
                p.fill_circle(cx, dy, lerp(3.0, 4.0, on), c);
                hits.push((Rect::new(cx - 7.0, dy - 8.0, 14.0, 16.0), Target::Dot(i)));
            }

            // Footer buttons.
            p.line(body.x, footer_y, body.right(), footer_y, style.card_border, 1.0);
            let by = footer_y + (FOOTER_H - 30.0) / 2.0;
            let width = |label: &str| (p.gfx.measure(label, &button_font) + 32.0).max(92.0);
            let mut button = |label: &str, x: f32, kind: Kind, target: Target, slot: usize| {
                let r = Rect::new(x, by, width(label), 30.0);
                let hot = fade(Anim::Button(slot), hover == Some(target));
                draw_button(p, r, label, kind, hot, pressed == Some(target), theme, &style, &button_font);
                hits.push((r, target));
            };
            let left = body.x + 20.0;
            if step == LAST {
                button("Open System Settings", left, Kind::Link, Target::OpenSettings, 0);
            } else {
                button("Skip Setup", left, Kind::Link, Target::Skip, 0);
            }
            let next = match step {
                0 => "Get Started",
                LAST => "Done",
                _ => "Continue",
            };
            let next_x = body.right() - 20.0 - width(next);
            button(next, next_x, Kind::Primary, Target::Next, 1);
            if step > 0 {
                button("Back", next_x - 10.0 - width("Back"), Kind::Secondary, Target::Back, 2);
            }
        }
        hits
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Primary,
    Secondary,
    Link,
}

#[allow(clippy::too_many_arguments)]
fn draw_button(
    p: &Painter,
    r: Rect,
    label: &str,
    kind: Kind,
    hot: f32,
    down: bool,
    theme: &Theme,
    style: &Style,
    font: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
) {
    match kind {
        Kind::Primary => {
            p.fill_round(r.offset(0.0, 0.5), 8.0, Color::gray(0, 0.12));
            p.fill_round(r, 8.0, theme.accent);
            if down {
                p.fill_round(r, 8.0, Color::gray(0, 0.18));
            } else if hot > 0.0 {
                p.fill_round(r, 8.0, Color::gray(255, 0.12 * hot));
            }
            p.text(label, font, r, Color::WHITE);
        }
        Kind::Secondary => {
            p.fill_round(r, 8.0, style.control_bg.mix(style.selected_seg, hot));
            p.stroke_round(r.inset(0.25, 0.25), 8.0, style.card_border, 0.5);
            if down {
                p.fill_round(r, 8.0, Color::gray(0, 0.08));
            }
            p.text(label, font, r, theme.text);
        }
        Kind::Link => {
            if hot > 0.0 {
                p.fill_round(r, 8.0, style.control_bg.alpha(style.control_bg.a * hot));
            }
            p.text(label, font, r, if down { theme.text } else { theme.accent });
        }
    }
}

/// The last page's card of ways to open System Settings; returns its bottom.
fn draw_places(
    p: &Painter,
    top: Rect,
    places: &[(char, String)],
    theme: &Theme,
    style: &Style,
    glyph: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
) -> f32 {
    let card = Rect::new(top.x, top.y, top.w, places.len() as f32 * ROW_H);
    p.fill_round(card, 10.0, style.card_bg);
    p.stroke_round(card.inset(0.25, 0.25), 10.0, style.card_border, 0.5);
    let text = p.gfx.format(&theme.font, 13.0, Weight::Regular, Align::Leading);
    for (i, (ch, how)) in places.iter().enumerate() {
        let r = Rect::new(card.x, card.y + i as f32 * ROW_H, card.w, ROW_H);
        if i > 0 {
            p.line(r.x + 14.0, r.y, r.right() - 14.0, r.y, style.card_border, 1.0);
        }
        let icon = Rect::new(r.x + 12.0, r.cy() - 11.0, 22.0, 22.0);
        p.fill_round(icon, 6.0, theme.accent);
        p.text(&ch.to_string(), glyph, icon, Color::WHITE);
        p.text(how, &text, Rect::new(icon.right() + 12.0, r.y, r.right() - icon.right() - 26.0, r.h), theme.text);
    }
    card.bottom()
}

/// The welcome page's row of what DockSide adds.
fn draw_features(
    p: &Painter,
    r: Rect,
    theme: &Theme,
    style: &Style,
    glyph: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    label_font: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
) -> f32 {
    const FEATURES: [(Hero, &str); 5] = [
        (Hero::Page(Page::Dock), "Dock"),
        (Hero::Page(Page::MenuBar), "Menu Bar"),
        (Hero::Page(Page::Island), "Dynamic Island"),
        (Hero::Glyph(Color::rgba(88, 86, 214, 1.0), '\u{E721}'), "Spotlight"),
        (Hero::Page(Page::Widgets), "Widgets"),
    ];
    p.fill_round(r, 10.0, style.card_bg);
    p.stroke_round(r.inset(0.25, 0.25), 10.0, style.card_border, 0.5);
    let cell = r.w / FEATURES.len() as f32;
    for (i, (hero, label)) in FEATURES.iter().enumerate() {
        let cx = r.x + cell * (i as f32 + 0.5);
        let icon = Rect::new(cx - 15.0, r.y + 12.0, 30.0, 30.0);
        match hero {
            Hero::Page(page) => p.transformed(icon.x, icon.y, icon.w / 20.0, || {
                settings::draw_page_icon(p, *page, Rect::new(0.0, 0.0, 20.0, 20.0), glyph);
            }),
            Hero::Glyph(c, ch) => {
                p.fill_round(icon, 7.5, *c);
                let big = p.gfx.format("Segoe Fluent Icons", 15.0, Weight::Regular, Align::Center);
                p.text(&ch.to_string(), &big, icon, Color::WHITE);
            }
            _ => {}
        }
        p.text(label, label_font, Rect::new(cx - cell / 2.0, icon.bottom() + 6.0, cell, 18.0), theme.text);
    }
    r.bottom()
}

fn draw_hero(
    p: &Painter,
    hero: Hero,
    r: Rect,
    theme: &Theme,
    glyph_small: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    glyph_big: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
) {
    // The check is a circle: its shadow rounds all the way.
    let radius = if matches!(hero, Hero::Done) { r.w / 2.0 } else { r.w * 0.225 };
    // A soft shadow so the tile floats over the page.
    for i in 0..6 {
        let grow = i as f32 * 1.5;
        p.fill_round(r.inset(-grow, -grow).offset(0.0, 3.0), radius + grow, theme.shadow.alpha(0.03 * (6 - i) as f32 / 6.0));
    }
    match hero {
        Hero::Page(page) => p.transformed(r.x, r.y, r.w / 20.0, || {
            settings::draw_page_icon(p, page, Rect::new(0.0, 0.0, 20.0, 20.0), glyph_small);
        }),
        Hero::Glyph(c, ch) => {
            p.fill_round(r, radius, c);
            p.text(&ch.to_string(), glyph_big, r, Color::WHITE);
        }
        Hero::Welcome => {
            // A tiny desktop: wallpaper, menu bar, island and a Dock of four apps.
            p.fill_round_gradient(r, radius, Color::rgba(94, 92, 230, 1.0), Color::rgba(255, 55, 95, 1.0));
            let k = r.w / 72.0;
            p.fill_rect(Rect::new(r.x + 10.0 * k, r.y + 10.0 * k, 52.0 * k, 4.0 * k), Color::gray(255, 0.35));
            p.fill_round(Rect::new(r.cx() - 9.0 * k, r.y + 10.0 * k, 18.0 * k, 5.0 * k), 2.5 * k, Color::BLACK);
            let dock = Rect::new(r.x + 12.0 * k, r.bottom() - 22.0 * k, 48.0 * k, 12.0 * k);
            p.fill_round(dock, 4.0 * k, Color::gray(255, 0.45));
            p.stroke_round(dock, 4.0 * k, Color::gray(255, 0.7), 0.8);
            let apps = [Color::rgba(0, 122, 255, 1.0), Color::rgba(52, 199, 89, 1.0), Color::rgba(255, 149, 0, 1.0), Color::rgba(28, 28, 30, 1.0)];
            for (i, c) in apps.iter().enumerate() {
                let x = dock.x + 3.0 * k + i as f32 * 11.5 * k;
                p.fill_round(Rect::new(x, dock.y + 2.0 * k, 8.0 * k, 8.0 * k), 2.2 * k, *c);
            }
        }
        Hero::Glass => {
            // A glass lens over colored stripes, with a specular rim.
            p.clipped(r, || {
                p.fill_round_gradient(r, radius, Color::rgba(100, 210, 255, 1.0), Color::rgba(175, 82, 222, 1.0));
                for i in 0..4 {
                    let y = r.y + r.h * (0.18 + i as f32 * 0.2);
                    p.fill_rect(Rect::new(r.x, y, r.w, r.h * 0.07), Color::gray(255, 0.22));
                }
            });
            let lens = r.inset(r.w * 0.2, r.h * 0.2);
            p.fill_circle(lens.cx(), lens.cy(), lens.w / 2.0, Color::gray(255, 0.28));
            p.stroke_circle(lens.cx(), lens.cy(), lens.w / 2.0, Color::gray(255, 0.85), 1.5);
            p.fill_circle(lens.cx() - lens.w * 0.18, lens.cy() - lens.h * 0.2, lens.w * 0.1, Color::gray(255, 0.7));
        }
        Hero::Done => {
            p.fill_circle(r.cx(), r.cy(), r.w / 2.0, Color::rgba(52, 199, 89, 1.0));
            p.text("\u{E73E}", glyph_big, r, Color::WHITE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_fits() {
        let cfg = crate::config::defaults();
        for step in 0..STEPS.len() {
            let n = if step == LAST { settings_places(&cfg).len() } else { rows(step, &cfg).len() };
            // Card, note and dots must clear the footer.
            assert!(CARD_Y + n as f32 * ROW_H + 14.0 + 36.0 <= H - FOOTER_H - 26.0, "step {step} has {n} rows");
        }
        assert!(rows(0, &cfg).is_empty() && rows(LAST, &cfg).is_empty());
    }

    /// Paints every page, light and dark, into `$DOCKSIDE_GALLERY-<step>-<light|dark>.bmp`, to look at by eye.
    #[test]
    #[ignore]
    fn gallery() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let out = std::env::var("DOCKSIDE_GALLERY").unwrap_or_else(|_| "onboarding-gallery".into());
        let gfx = Gfx::new().unwrap();
        let fonts = settings::installed_fonts(&gfx);
        for dark in [false, true] {
            let mut cfg = crate::config::defaults();
            cfg.general.appearance = if dark { crate::config::Appearance::Dark } else { crate::config::Appearance::Light };
            let theme = Theme::new(&cfg, &gfx);
            let (cw, ch) = ((W + 2.0 * SHADOW) as i32, (H + 2.0 * SHADOW) as i32);
            for step in 0..STEPS.len() {
                let mut canvas = Canvas::new(&gfx, cw, ch, 1.0).unwrap();
                let frame = Frame {
                    step,
                    hover: Some(Target::Next),
                    pressed: None,
                    slider: None,
                    pending: None,
                    recording: None,
                    login: false,
                    fonts: &fonts,
                    motion: None,
                    page_t: 1.0,
                    page_dir: 1.0,
                };
                canvas.draw(&gfx, |p| drop(paint(p, &gfx, &theme, &cfg, &frame))).unwrap();
                let px = canvas.pixels();
                let (iw, ih) = (cw as usize, ch as usize);
                // 24-bit bottom-up BMP over a mid-gray wallpaper.
                let stride = (iw * 3 + 3) & !3;
                let size = 54 + stride * ih;
                let mut f = Vec::with_capacity(size);
                f.extend_from_slice(b"BM");
                f.extend_from_slice(&(size as u32).to_le_bytes());
                f.extend_from_slice(&[0; 4]);
                f.extend_from_slice(&54u32.to_le_bytes());
                f.extend_from_slice(&40u32.to_le_bytes());
                f.extend_from_slice(&(iw as i32).to_le_bytes());
                f.extend_from_slice(&(ih as i32).to_le_bytes());
                f.extend_from_slice(&1u16.to_le_bytes());
                f.extend_from_slice(&24u16.to_le_bytes());
                f.extend_from_slice(&[0; 24]);
                for row in (0..ih).rev() {
                    for x in 0..iw {
                        let s = (row * iw + x) * 4;
                        let a = px[s + 3] as f32 / 255.0;
                        for k in 0..3 {
                            f.push((px[s + k] as f32 + 128.0 * (1.0 - a)).min(255.0) as u8);
                        }
                    }
                    f.extend(std::iter::repeat(0).take(stride - iw * 3));
                }
                std::fs::write(format!("{out}-{step:02}-{}.bmp", if dark { "dark" } else { "light" }), f).unwrap();
            }
        }
    }

    #[test]
    fn settings_places_follow_what_is_on() {
        let mut cfg = crate::config::defaults();
        let all = settings_places(&cfg);
        assert_eq!(all.len(), 4);
        assert_eq!(all[1].1, "Right-click the Dock, the Dynamic Island, or a widget to jump to its settings");
        cfg.menubar.enabled = false;
        cfg.island.enabled = false;
        cfg.widgets.enabled = false;
        cfg.spotlight.enabled = false;
        let few = settings_places(&cfg);
        assert_eq!(few.len(), 2);
        assert_eq!(few[0].1, "Right-click the Dock to jump to its settings");
    }

    #[test]
    fn hotkey_note_reads_naturally() {
        let mut cfg = crate::config::defaults();
        cfg.spotlight.hotkey = "ctrl+alt+space".into();
        assert_eq!(note(8, &cfg), "Try it now: press Ctrl+Alt+Space.");
        cfg.spotlight.hotkey = "win".into();
        assert_eq!(note(8, &cfg), "Try it now: tap Win. Double-tap it for the Start menu.");
    }
}
