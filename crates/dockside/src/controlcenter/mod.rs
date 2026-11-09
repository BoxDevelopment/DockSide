//! Control Center: macOS Tahoe's panel under the menu bar's switches icon,
//! holding everything in Windows 11's Quick Settings.
//!
//! The grid follows macOS (Wi-Fi, Bluetooth and AirDrop — here Nearby
//! Sharing — Now Playing, Focus, Screen Mirroring, Display, Sound and the round
//! controls), with Windows' own toggles in two more rows below. Wi-Fi,
//! Bluetooth, Focus, Display, Sound and Accessibility open a page of details
//! that slides in over the grid.

pub mod system;

use std::collections::HashMap;

use windows::core::Result;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;

use crate::anim::{feel, Motion, Scroll, Spring};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconPixels;
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::island::media::{Cmd, Media, NowPlaying};
use crate::menu::Action;
use crate::surface::{Fading, Surface};
use crate::theme::Theme;
use crate::win::system::Shortcut;
use crate::win::Monitor;
use system::{Req, Status, Toggle, Worker};

/// Panel width, in points (four 64pt cells, three gaps and the margins).
pub const W: f32 = 320.0;
const PAD: f32 = 14.0;
const GAP: f32 = 12.0;
const CELL: f32 = 64.0;
const RADIUS: f32 = 26.0;
/// Room for the drop shadow around the panel.
const SHADOW: f32 = 30.0;
/// Grid height (see `layout_main`).
const MAIN_H: f32 = 668.0;
/// Page list rows.
const ROW_H: f32 = 36.0;
const LINK_H: f32 = 30.0;

/// The detail pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Page {
    Main,
    Wifi,
    Bluetooth,
    Focus,
    Display,
    Sound,
    Accessibility,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
enum Slider {
    Brightness,
    Volume,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
enum MediaBtn {
    Prev,
    Play,
    Next,
    Open,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
enum Btn {
    Cast,
    Project,
    Calculator,
    Timer,
    Screenshot,
    Keyboard,
    Vpn,
    Settings,
    Edit,
    Mute,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
enum Hit {
    Toggle(Toggle),
    Open(Page),
    Slider(Slider),
    Media(MediaBtn),
    Run(Btn),
    Back,
    Network(usize),
    Device(usize),
    Output(usize),
    Link(&'static str),
}

/// What the App does after an input.
pub enum ControlEvent {
    None,
    Close,
    /// Close, then run this.
    Run(Action),
    /// Bring the Now Playing app forward (and close).
    OpenMedia(String),
}

pub struct ControlCenter {
    pub surface: Surface,
    ui: Ui,
}

impl std::ops::Deref for ControlCenter {
    type Target = Ui;
    fn deref(&self) -> &Ui {
        &self.ui
    }
}

impl std::ops::DerefMut for ControlCenter {
    fn deref_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }
}

/// Everything but the surface (so drawing can read it while the surface renders).
pub struct Ui {
    worker: Worker,
    scale: f32,
    media: Media,
    status: Status,
    /// Changes made here that the worker hasn't confirmed yet: (value, request number).
    local: HashMap<Toggle, (bool, u64)>,
    local_brightness: Option<(u8, u64)>,
    local_output: Option<(String, u64)>,
    gen: u64,
    volume: Option<(f32, bool)>,
    now: Option<NowPlaying>,
    art: Option<(u64, ID2D1Bitmap)>,
    battery: Option<crate::win::system::Battery>,
    page: Page,
    /// The page sliding in or out (stays set while the grid slides back).
    shown: Page,
    /// 0 on the grid, 1 on a page.
    slide: Spring,
    zoom: f32,
    body: Rect,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    drag: Option<(Slider, Rect)>,
    motion: Motion<(Hit, u8)>,
    scroll: Scroll,
    scroll_max: f32,
    pub tracking: bool,
    pub dirty: bool,
}

impl Fading for ControlCenter {
    fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }
}

/// Colours for the current appearance.
struct Pal {
    text: Color,
    sub: Color,
    module: Color,
    module_hover: Color,
    rim: Color,
    on_bg: Color,
    on_fg: Color,
    off_bg: Color,
    off_fg: Color,
    track: Color,
    fill: Color,
    sep: Color,
}

fn pal(theme: &Theme) -> Pal {
    if theme.dark {
        Pal {
            text: Color::gray(255, 0.95),
            sub: Color::gray(255, 0.58),
            module: Color::gray(255, 0.10),
            module_hover: Color::gray(255, 0.16),
            rim: Color::gray(255, 0.12),
            on_bg: Color::gray(255, 0.96),
            on_fg: theme.accent,
            off_bg: Color::gray(255, 0.14),
            off_fg: Color::gray(255, 0.95),
            track: Color::gray(255, 0.16),
            fill: Color::gray(255, 0.95),
            sep: Color::gray(255, 0.12),
        }
    } else {
        Pal {
            text: Color::gray(0, 0.86),
            sub: Color::gray(0, 0.5),
            module: Color::gray(255, 0.55),
            module_hover: Color::gray(255, 0.75),
            rim: Color::gray(255, 0.7),
            on_bg: theme.accent,
            on_fg: Color::WHITE,
            off_bg: Color::gray(0, 0.07),
            off_fg: Color::gray(0, 0.8),
            track: Color::gray(0, 0.12),
            fill: Color::WHITE,
            sep: Color::gray(0, 0.1),
        }
    }
}

const ICONS: &str = "Segoe Fluent Icons";

impl ControlCenter {
    /// Opens under the menu bar icon at `anchor` (its screen rect, in points),
    /// on `page`. `hwnd` receives worker and media updates.
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &crate::config::Config, monitor: &Monitor, anchor: Rect, page: Page, hwnd: isize) -> Result<Self> {
        let s = monitor.scale;
        let (mon_l, mon_r, mon_b) = (monitor.rect.left as f32 / s, monitor.rect.right as f32 / s, monitor.rect.bottom as f32 / s);
        let top = anchor.bottom() + 5.0;
        // Too tall for a small screen: the whole panel scales down to fit.
        let zoom = ((mon_b - top - 8.0) / MAIN_H).min(1.0).max(0.5);
        let (w, h) = (W * zoom, MAIN_H * zoom);
        let x = (anchor.cx() - w / 2.0).clamp(mon_l + 8.0, (mon_r - w - 8.0).max(mon_l));
        let pad = SHADOW * zoom;
        let rect = RECT {
            left: ((x - pad) * s).round() as i32,
            top: ((top - pad) * s).round() as i32,
            right: ((x + w + pad) * s).round() as i32,
            bottom: ((top + h + pad) * s).round() as i32,
        };
        let mut surface = Surface::new(gfx, rect, s, cfg.menubar.backdrop.for_panels(), theme.menu_tint)?;
        let body = Rect::new(SHADOW, SHADOW, W, MAIN_H);
        surface.set_backdrop_shape(Rect::new(pad, pad, w, h), RADIUS * zoom);

        let worker = Worker::start(hwnd, crate::win::WM_APP_CONTROL);
        let media = Media::shared(hwnd);
        media.send(Cmd::Refresh);
        let ui = Ui {
            worker,
            scale: s,
            now: media.snapshot(),
            media,
            status: Status::default(),
            local: HashMap::new(),
            local_brightness: None,
            local_output: None,
            gen: 0,
            volume: crate::island::sources::volume(),
            art: None,
            battery: crate::win::system::battery(),
            page: Page::Main,
            shown: Page::Main,
            slide: Spring::new(0.0, 0.3, 1.0),
            zoom,
            body,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            drag: None,
            motion: Motion::default(),
            scroll: Scroll::default(),
            scroll_max: 0.0,
            tracking: false,
            dirty: true,
        };
        let mut c = Self { surface, ui };
        if page != Page::Main {
            c.open_page(page);
            c.slide.snap(1.0);
        }
        Ok(c)
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        let Self { surface, ui } = self;
        ui.dirty = false;
        ui.motion.sweep();
        // Album art, made once per track.
        let art_id = ui.now.as_ref().and_then(|n| n.art.as_ref()).map(|a| a.id);
        if art_id != ui.art.as_ref().map(|(id, _)| *id) {
            let rt = surface.canvas.target().clone();
            ui.art = ui.now.as_ref().and_then(|n| n.art.as_ref()).and_then(|a| {
                IconPixels { width: a.size, height: a.size, data: a.data.clone() }.to_bitmap(&rt).map(|b| (a.id, b))
            });
        }
        let pal = pal(theme);
        let t = ui.slide.value.clamp(0.0, 1.0);
        let body = ui.body;
        let has_backdrop = surface.backdrop.is_some();
        let zoom = ui.zoom;
        let mut hits = Vec::new();
        let this = &*ui;
        let r = surface.render(gfx, |p| {
            p.transformed(0.0, 0.0, zoom, || {
                p.soft_shadow(body, RADIUS, 12, 2.0, 0.03, 6.0, theme.shadow);
                p.clear_rect(body.inset(3.0, 3.0));
                p.fill_round(body, RADIUS, if has_backdrop { theme.menu_tint.alpha(0.30) } else { theme.menu_tint.alpha(1.0) });
                p.stroke_round(body.inset(0.5, 0.5), RADIUS, pal.rim, 1.0);
                p.clipped(body, || {
                    if t < 0.999 {
                        let mut main_hits = Vec::new();
                        p.faded(1.0 - t, || {
                            p.scaled_about(body.cx(), body.y, 1.0 - 0.04 * t, || this.draw_main(p, theme, &pal, &mut main_hits));
                        });
                        if this.page == Page::Main {
                            hits = main_hits;
                        }
                    }
                    if t > 0.001 {
                        let mut page_hits = Vec::new();
                        p.faded(t, || {
                            p.transformed(24.0 * (1.0 - t), 0.0, 1.0, || this.draw_page(p, theme, &pal, &mut page_hits));
                        });
                        if this.page != Page::Main {
                            hits = page_hits;
                        }
                    }
                });
            });
        });
        if let Err(e) = r {
            log!("control center render failed: {e}");
        }
        ui.hits = hits;
        ui.sync_scroll();
    }
}

impl Ui {
    pub fn page(&self) -> Page {
        self.page
    }

    /// The worker has news.
    pub fn on_status(&mut self) {
        self.status = self.worker.snapshot();
        let confirmed = self.status.gen;
        self.local.retain(|_, (_, g)| *g > confirmed);
        if self.local_brightness.is_some_and(|(_, g)| g <= confirmed) && self.drag.is_none() {
            self.local_brightness = None;
        }
        if self.local_output.as_ref().is_some_and(|(_, g)| *g <= confirmed) {
            self.local_output = None;
        }
        if self.drag.is_none() {
            self.volume = crate::island::sources::volume();
        }
        self.battery = crate::win::system::battery();
        self.dirty = true;
    }

    pub fn on_media(&mut self) {
        self.now = self.media.snapshot();
        self.dirty = true;
    }

    fn on(&self, t: Toggle) -> Option<bool> {
        self.local.get(&t).map(|(v, _)| *v).or_else(|| self.status.toggles.get(&t).copied())
    }

    fn brightness(&self) -> Option<u8> {
        self.local_brightness.map(|(v, _)| v).or(self.status.brightness)
    }

    fn open_page(&mut self, page: Page) {
        self.page = page;
        if page != Page::Main {
            self.shown = page;
            self.scroll.jump(0.0);
        }
        self.slide.set(if page == Page::Main { 0.0 } else { 1.0 });
        match page {
            Page::Wifi => self.worker.send(Req::ScanWifi),
            Page::Bluetooth => self.worker.send(Req::ListDevices),
            Page::Sound => self.worker.send(Req::ListOutputs),
            _ => {}
        }
        self.hover = None;
        self.dirty = true;
    }

    pub fn tick(&mut self, dt: f32) -> bool {
        let mut moving = self.slide.step(dt);
        moving |= self.motion.tick(dt);
        moving |= self.scroll.tick(dt);
        // The Now Playing tile follows nothing time-based, so nothing else animates.
        self.dirty |= moving;
        moving
    }

    pub fn animating(&self) -> bool {
        !self.slide.settled() || self.motion.moving() || self.scroll.moving()
    }

    // --- input ------------------------------------------------------------

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        let (x, y) = (x / self.zoom, y / self.zoom);
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    /// Whether a point (surface points) is on the panel rather than its shadow.
    pub fn in_body(&self, x: f32, y: f32) -> bool {
        self.body.contains(x / self.zoom, y / self.zoom)
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        if let Some((s, r)) = self.drag {
            self.slide_to(s, r, x / self.zoom);
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

    /// A button went down: sliders start here, everything else waits for the release.
    pub fn on_down(&mut self, x: f32, y: f32) -> bool {
        let h = self.hit_at(x, y);
        self.pressed = h;
        self.dirty = true;
        if let Some(Hit::Slider(s)) = h {
            if s == Slider::Brightness && self.brightness().is_none() {
                return false;
            }
            if let Some(r) = self.hits.iter().find(|(_, hh)| *hh == Hit::Slider(s)).map(|(r, _)| *r) {
                self.drag = Some((s, r));
                self.slide_to(s, r, x / self.zoom);
                return true;
            }
        }
        false
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> ControlEvent {
        let pressed = self.pressed.take();
        self.dirty = true;
        if let Some((s, _)) = self.drag.take() {
            if s == Slider::Brightness {
                // Let the worker's next reading confirm it.
                if let Some((v, _)) = self.local_brightness {
                    self.gen += 1;
                    self.local_brightness = Some((v, self.gen));
                    self.worker.send(Req::Brightness(v, self.gen));
                }
            }
            return ControlEvent::None;
        }
        let h = self.hit_at(x, y);
        if h.is_none() || h != pressed {
            return ControlEvent::None;
        }
        self.activate(h.unwrap())
    }

    /// Right-click: a control with details opens its page (like a long press on macOS).
    pub fn on_right(&mut self, x: f32, y: f32) -> ControlEvent {
        let page = match self.hit_at(x, y) {
            Some(Hit::Toggle(Toggle::Wifi)) => Page::Wifi,
            Some(Hit::Toggle(Toggle::Bluetooth)) => Page::Bluetooth,
            Some(Hit::Toggle(Toggle::Focus)) => Page::Focus,
            Some(Hit::Toggle(Toggle::NightLight | Toggle::Dark)) | Some(Hit::Slider(Slider::Brightness)) => Page::Display,
            Some(Hit::Slider(Slider::Volume)) | Some(Hit::Run(Btn::Mute)) => Page::Sound,
            Some(Hit::Toggle(Toggle::LiveCaptions)) => Page::Accessibility,
            Some(Hit::Toggle(Toggle::Nearby)) => return ControlEvent::Run(Action::Open("ms-settings:crossdevice".into())),
            Some(Hit::Toggle(Toggle::Hotspot)) => return ControlEvent::Run(Action::Open("ms-settings:network-mobilehotspot".into())),
            Some(Hit::Toggle(Toggle::Airplane)) => return ControlEvent::Run(Action::Open("ms-settings:network-airplanemode".into())),
            Some(Hit::Toggle(Toggle::EnergySaver)) => return ControlEvent::Run(Action::Open("ms-settings:batterysaver".into())),
            _ => return ControlEvent::None,
        };
        if self.page == Page::Main {
            self.open_page(page);
        }
        ControlEvent::None
    }

    pub fn on_wheel(&mut self, delta: i32) {
        if self.page != Page::Main {
            self.scroll.wheel(delta, 60.0, self.scroll_max);
            self.dirty = true;
        }
    }

    /// Esc steps back from a page, then closes.
    pub fn on_key(&mut self, vk: u16) -> ControlEvent {
        match vk {
            0x1B if self.page != Page::Main => {
                self.open_page(Page::Main);
                ControlEvent::None
            }
            0x1B => ControlEvent::Close,
            0x25 if self.page != Page::Main => {
                self.open_page(Page::Main);
                ControlEvent::None
            }
            _ => ControlEvent::None,
        }
    }

    fn slide_to(&mut self, s: Slider, track: Rect, x: f32) {
        let v = ((x - track.x) / track.w).clamp(0.0, 1.0);
        match s {
            Slider::Volume => {
                self.volume = crate::island::sources::set_volume(v).or(Some((v, false)));
            }
            Slider::Brightness => {
                let level = (v * 100.0).round() as u8;
                if self.brightness() != Some(level) {
                    self.gen += 1;
                    self.local_brightness = Some((level, self.gen));
                    self.worker.send(Req::Brightness(level, self.gen));
                }
            }
        }
        self.dirty = true;
    }

    fn set_toggle(&mut self, t: Toggle, on: bool) {
        self.gen += 1;
        self.local.insert(t, (on, self.gen));
        self.worker.send(Req::Set(t, on, self.gen));
        if t == Toggle::Wifi && on && self.page == Page::Wifi {
            self.worker.send(Req::ScanWifi);
        }
        if t == Toggle::Bluetooth && self.page == Page::Bluetooth {
            self.worker.send(Req::ListDevices);
        }
    }

    fn activate(&mut self, h: Hit) -> ControlEvent {
        let open = |uri: &str| ControlEvent::Run(Action::Open(uri.into()));
        let keys = |sc: Shortcut| ControlEvent::Run(Action::Keys(sc));
        match h {
            Hit::Toggle(t) => {
                let Some(now) = self.on(t) else {
                    return open(match t {
                        Toggle::Wifi => "ms-settings:network-wifi",
                        Toggle::Bluetooth => "ms-settings:bluetooth",
                        Toggle::Airplane => "ms-settings:network-airplanemode",
                        Toggle::Focus => "ms-settings:notifications",
                        Toggle::NightLight => "ms-settings:nightlight",
                        Toggle::Hotspot => "ms-settings:network-mobilehotspot",
                        Toggle::Nearby => "ms-settings:crossdevice",
                        _ => "ms-settings:",
                    });
                };
                if t == Toggle::Dark {
                    // Every surface is rebuilt in the new appearance; the App reopens this panel.
                    self.local.insert(t, (!now, u64::MAX));
                    return ControlEvent::Run(Action::SetDarkMode(!now));
                }
                self.set_toggle(t, !now);
                self.dirty = true;
                ControlEvent::None
            }
            Hit::Open(p) => {
                self.open_page(p);
                ControlEvent::None
            }
            Hit::Back => {
                self.open_page(Page::Main);
                ControlEvent::None
            }
            Hit::Slider(_) => ControlEvent::None,
            Hit::Media(b) => {
                match b {
                    MediaBtn::Prev => self.media.send(Cmd::Prev),
                    MediaBtn::Play => {
                        self.media.send(Cmd::TogglePlay);
                        if let Some(n) = &mut self.now {
                            n.playing = !n.playing;
                        }
                    }
                    MediaBtn::Next => self.media.send(Cmd::Next),
                    MediaBtn::Open => {
                        return match &self.now {
                            Some(n) => ControlEvent::OpenMedia(n.app_id.clone()),
                            None => ControlEvent::None,
                        }
                    }
                }
                self.dirty = true;
                ControlEvent::None
            }
            Hit::Run(b) => match b {
                Btn::Cast => keys(Shortcut::new("w", b'K' as u16, "K")),
                Btn::Project => keys(Shortcut::new("w", b'P' as u16, "P")),
                Btn::Calculator => ControlEvent::Run(Action::Launch { path: "calc.exe".into(), args: None }),
                Btn::Timer => open("ms-clock:"),
                Btn::Screenshot => keys(Shortcut::new("ws", b'S' as u16, "S")),
                Btn::Keyboard => keys(Shortcut::new("w", 0x20, "Space")),
                Btn::Vpn => open("ms-settings:network-vpn"),
                Btn::Settings => open("ms-settings:"),
                Btn::Edit => ControlEvent::Run(Action::OpenSettings(Some(crate::settings::Page::MenuBar))),
                Btn::Mute => {
                    self.volume = crate::island::sources::press_volume(crate::island::sources::VolumeKey::Mute);
                    self.dirty = true;
                    ControlEvent::None
                }
            },
            Hit::Network(i) => {
                if let Some(n) = self.status.networks.as_ref().and_then(|l| l.get(i)) {
                    if !n.connected {
                        self.worker.send(Req::Connect(n.ssid.clone()));
                    }
                }
                ControlEvent::None
            }
            Hit::Device(_) => open("ms-settings:bluetooth"),
            Hit::Output(i) => {
                if let Some(o) = self.status.outputs.as_ref().and_then(|l| l.get(i)) {
                    self.gen += 1;
                    self.local_output = Some((o.id.clone(), self.gen));
                    self.worker.send(Req::SetOutput(o.id.clone(), self.gen));
                    self.dirty = true;
                }
                ControlEvent::None
            }
            Hit::Link(uri) => open(uri),
        }
    }

    // --- drawing ----------------------------------------------------------

    /// 0..1 hover highlight for `h`.
    fn lit(&self, h: Hit) -> f32 {
        let pressed = self.pressed == Some(h) && self.hover == Some(h);
        let a = self.motion.fade((h, 0), self.hover == Some(h));
        if pressed {
            1.6
        } else {
            a
        }
    }

    /// A module background (rounded, glassy), brightened on hover.
    fn module(&self, p: &Painter, pal: &Pal, r: Rect, radius: f32, hover: f32) {
        p.fill_round(r, radius, pal.module.mix(pal.module_hover, hover.min(1.0)));
        p.stroke_round(r.inset(0.5, 0.5), radius - 0.5, pal.rim, 1.0);
    }

    /// A round control: filled when on, glassy when off.
    fn circle(&self, p: &Painter, pal: &Pal, r: Rect, glyph: char, on: Option<bool>, h: Hit, size: f32) {
        let lit = self.lit(h);
        let on_now = on.unwrap_or(false);
        // The fill eases between off and on.
        let k = self.motion.get((h, 1), if on_now { 1.0 } else { 0.0 }, feel::SMOOTH).clamp(0.0, 1.0);
        let off_bg = pal.off_bg.mix(pal.module_hover, lit.min(1.0) * 0.6);
        let bg = off_bg.mix(pal.on_bg, k);
        let fg = pal.off_fg.mix(pal.on_fg, k);
        let rad = r.w.min(r.h) / 2.0;
        let (cx, cy) = (r.cx(), r.cy());
        p.fill_circle(cx, cy, rad, bg);
        if k < 0.5 {
            p.stroke_circle(cx, cy, rad - 0.5, pal.rim, 1.0);
        }
        if lit > 1.0 {
            p.fill_circle(cx, cy, rad, Color::gray(0, 0.12));
        }
        let fg = if on.is_none() { fg.alpha(fg.a * 0.4) } else { fg };
        let f = p.gfx.format(ICONS, size, Weight::Regular, Align::Center);
        p.text(&glyph.to_string(), &f, r, fg);
    }

    fn draw_main(&self, p: &Painter, theme: &Theme, pal: &Pal, hits: &mut Vec<(Rect, Hit)>) {
        let font = &theme.font;
        let title = p.gfx.format(font, 13.0, Weight::SemiBold, Align::Leading);
        let sub = p.gfx.format(font, 11.0, Weight::Regular, Align::Leading);
        let label = p.gfx.format(font, 10.0, Weight::Regular, Align::Center);
        let x0 = self.body.x + PAD;
        let y0 = self.body.y + PAD;
        let wide = CELL * 2.0 + GAP;
        let full = CELL * 4.0 + GAP * 3.0;
        let col = |i: f32| x0 + i * (CELL + GAP);

        // Wi-Fi and Focus: a pill with a round switch and a name.
        let pill = |r: Rect, t: Toggle, glyph: char, name: &str, detail: Option<String>, page: Page, hits: &mut Vec<(Rect, Hit)>| {
            let open = Hit::Open(page);
            self.module(p, pal, r, r.h / 2.0, self.lit(open));
            let c = Rect::new(r.x + 9.0, r.y + 9.0, r.h - 18.0, r.h - 18.0);
            hits.push((r, open));
            hits.push((c, Hit::Toggle(t)));
            self.circle(p, pal, c, glyph, self.on(t), Hit::Toggle(t), 17.0);
            let tx = c.right() + 9.0;
            let tw = r.right() - tx - 10.0;
            match detail {
                Some(d) => {
                    p.text(name, &title, Rect::new(tx, r.cy() - 16.0, tw, 17.0), pal.text);
                    p.text(&d, &sub, Rect::new(tx, r.cy() + 0.5, tw, 15.0), pal.sub);
                }
                None => p.text(name, &title, Rect::new(tx, r.y, tw, r.h), pal.text),
            }
        };

        let wifi_detail = match self.on(Toggle::Wifi) {
            None => Some("Unavailable".to_string()),
            Some(false) => Some("Off".to_string()),
            Some(true) => Some(self.status.ssid.clone().unwrap_or_else(|| "Not Connected".into())),
        };
        pill(Rect::new(x0, y0, wide, CELL), Toggle::Wifi, '\u{E701}', "Wi-Fi", wifi_detail, Page::Wifi, hits);

        let bt = Rect::new(col(0.0), y0 + CELL + GAP, CELL, CELL);
        hits.push((bt, Hit::Toggle(Toggle::Bluetooth)));
        self.circle(p, pal, bt, '\u{E702}', self.on(Toggle::Bluetooth), Hit::Toggle(Toggle::Bluetooth), 22.0);
        let nearby = Rect::new(col(1.0), y0 + CELL + GAP, CELL, CELL);
        hits.push((nearby, Hit::Toggle(Toggle::Nearby)));
        self.circle(p, pal, nearby, '\u{E72D}', self.on(Toggle::Nearby), Hit::Toggle(Toggle::Nearby), 22.0);

        self.draw_media(p, theme, pal, Rect::new(col(2.0), y0, wide, wide), hits);

        let y1 = y0 + wide + GAP;
        let focus_detail = (self.on(Toggle::Focus) == Some(true)).then(|| "Do Not Disturb".to_string());
        pill(Rect::new(x0, y1, wide, CELL), Toggle::Focus, '\u{E708}', "Focus", focus_detail, Page::Focus, hits);
        for (i, glyph, b) in [(2.0, '\u{EBC6}', Btn::Project), (3.0, '\u{EC15}', Btn::Cast)] {
            let r = Rect::new(col(i), y1, CELL, CELL);
            hits.push((r, Hit::Run(b)));
            self.circle(p, pal, r, glyph, Some(false), Hit::Run(b), 21.0);
        }

        let y2 = y1 + CELL + GAP;
        let level = self.brightness().map(|b| b as f32 / 100.0);
        self.slider_module(p, theme, pal, Rect::new(x0, y2, full, 56.0), "Display", Slider::Brightness, level, Page::Display, hits);
        let y3 = y2 + 56.0 + GAP;
        let vol = self.volume.map(|(v, m)| if m { 0.0 } else { v });
        self.slider_module(p, theme, pal, Rect::new(x0, y3, full, 56.0), "Sound", Slider::Volume, vol, Page::Sound, hits);

        let y4 = y3 + 56.0 + GAP;
        let row4: [(char, Hit, Option<bool>); 4] = [
            ('\u{E793}', Hit::Toggle(Toggle::Dark), self.on(Toggle::Dark)),
            ('\u{E8EF}', Hit::Run(Btn::Calculator), Some(false)),
            ('\u{E916}', Hit::Run(Btn::Timer), Some(false)),
            ('\u{E722}', Hit::Run(Btn::Screenshot), Some(false)),
        ];
        for (i, (glyph, h, on)) in row4.into_iter().enumerate() {
            let r = Rect::new(col(i as f32), y4, CELL, CELL);
            hits.push((r, h));
            self.circle(p, pal, r, glyph, on, h, 21.0);
        }

        // Windows' own Quick Settings, labelled (they have no macOS counterpart to recognise).
        let y5 = y4 + CELL + GAP;
        let extras: [[(char, Hit, Option<bool>, &str); 4]; 2] = [
            [
                ('\u{E709}', Hit::Toggle(Toggle::Airplane), self.on(Toggle::Airplane), "Airplane Mode"),
                ('\u{E8BE}', Hit::Toggle(Toggle::EnergySaver), self.on(Toggle::EnergySaver), "Energy Saver"),
                ('\u{F08C}', Hit::Toggle(Toggle::NightLight), self.on(Toggle::NightLight), "Night Light"),
                ('\u{E704}', Hit::Toggle(Toggle::Hotspot), self.on(Toggle::Hotspot), "Hotspot"),
            ],
            [
                ('\u{E7F0}', Hit::Toggle(Toggle::LiveCaptions), self.on(Toggle::LiveCaptions), "Live Captions"),
                ('\u{E776}', Hit::Open(Page::Accessibility), Some(false), "Accessibility"),
                ('\u{E765}', Hit::Run(Btn::Keyboard), Some(false), "Keyboard"),
                ('\u{E705}', Hit::Run(Btn::Vpn), Some(false), "VPN"),
            ],
        ];
        for (row, items) in extras.into_iter().enumerate() {
            let y = y5 + row as f32 * (CELL + 24.0);
            for (i, (glyph, h, on, name)) in items.into_iter().enumerate() {
                let r = Rect::new(col(i as f32), y, CELL, CELL);
                hits.push((Rect::new(r.x, r.y, r.w, r.h + 16.0), h));
                self.circle(p, pal, r, glyph, on, h, 21.0);
                p.text(name, &label, Rect::new(r.x - GAP / 2.0, r.bottom() + 2.0, r.w + GAP, 14.0), pal.text.alpha(pal.text.a * 0.85));
            }
        }

        // Footer: battery, Edit Controls, Settings.
        let y7 = y5 + 2.0 * (CELL + 24.0) + 2.0;
        let fh = 26.0;
        if let Some(b) = self.battery {
            let glyph = p.gfx.format(ICONS, 14.0, Weight::Regular, Align::Leading);
            let g = if b.charging { '\u{EA93}' } else { battery_glyph(b.percent) };
            p.text(&g.to_string(), &glyph, Rect::new(x0 + 2.0, y7, 20.0, fh), pal.text);
            p.text(&format!("{}%", b.percent), &sub, Rect::new(x0 + 24.0, y7, 60.0, fh), pal.text);
        }
        let edit_f = p.gfx.format(font, 12.0, Weight::Regular, Align::Center);
        let ew = p.gfx.measure("Edit Controls", &edit_f) + 26.0;
        let edit = Rect::new(self.body.cx() - ew / 2.0, y7, ew, fh);
        hits.push((edit, Hit::Run(Btn::Edit)));
        self.module(p, pal, edit, fh / 2.0, self.lit(Hit::Run(Btn::Edit)));
        p.text("Edit Controls", &edit_f, edit, pal.text);
        let gear = Rect::new(x0 + full - fh, y7, fh, fh);
        hits.push((gear, Hit::Run(Btn::Settings)));
        self.circle(p, pal, gear, '\u{E713}', Some(false), Hit::Run(Btn::Settings), 13.0);
    }

    #[allow(clippy::too_many_arguments)]
    fn slider_module(
        &self,
        p: &Painter,
        theme: &Theme,
        pal: &Pal,
        r: Rect,
        name: &str,
        s: Slider,
        level: Option<f32>,
        page: Page,
        hits: &mut Vec<(Rect, Hit)>,
    ) {
        let open = Hit::Open(page);
        self.module(p, pal, r, 22.0, self.lit(open) * 0.6);
        hits.push((Rect::new(r.x, r.y, r.w, 26.0), open));
        let title = p.gfx.format(&theme.font, 12.0, Weight::SemiBold, Align::Leading);
        p.text(name, &title, Rect::new(r.x + 16.0, r.y + 6.0, r.w - 60.0, 18.0), pal.text);
        let chevron = p.gfx.format(ICONS, 9.0, Weight::Regular, Align::Center);
        p.text("\u{E76C}", &chevron, Rect::new(r.right() - 26.0, r.y + 6.0, 14.0, 18.0), pal.sub);
        let track_y = r.y + 34.0;
        let mut x = r.x + 16.0;
        let mut right = r.right() - 16.0;
        let small = p.gfx.format(ICONS, 11.0, Weight::Regular, Align::Center);
        let big = p.gfx.format(ICONS, 15.0, Weight::Regular, Align::Center);
        match s {
            Slider::Brightness => {
                p.text("\u{E706}", &small, Rect::new(x - 2.0, track_y - 8.0, 14.0, 16.0), pal.text);
                p.text("\u{E706}", &big, Rect::new(right - 16.0, track_y - 9.0, 18.0, 18.0), pal.text);
                x += 18.0;
                right -= 24.0;
            }
            Slider::Volume => {
                let mute = Rect::new(x - 4.0, track_y - 10.0, 20.0, 20.0);
                hits.push((mute, Hit::Run(Btn::Mute)));
                let muted = self.volume.is_some_and(|(_, m)| m);
                p.text(if muted { "\u{E74F}" } else { "\u{E992}" }, &small, mute, pal.text);
                p.text("\u{E995}", &big, Rect::new(right - 44.0, track_y - 9.0, 20.0, 18.0), pal.text);
                // The output picker (AirPlay's spot on macOS).
                let out = Rect::new(right - 20.0, track_y - 11.0, 22.0, 22.0);
                hits.push((out, open));
                self.circle(p, pal, out, '\u{E7F5}', Some(false), open, 10.0);
                x += 18.0;
                right -= 52.0;
            }
        }
        let track = Rect::new(x, track_y - 4.0, right - x, 8.0);
        hits.push((Rect::new(track.x - 6.0, track.y - 10.0, track.w + 12.0, track.h + 20.0), Hit::Slider(s)));
        // Hit testing maps the pointer onto this exact track.
        if let Some(last) = hits.last_mut() {
            last.0 = Rect::new(track.x, last.0.y, track.w, last.0.h);
        }
        p.fill_round(track, 4.0, pal.track);
        match level {
            Some(v) => {
                let v = self.motion.get((Hit::Slider(s), 2), v, if self.drag.is_some() { (0.04, 1.0) } else { feel::SMOOTH });
                let w = (track.w * v.clamp(0.0, 1.0)).max(track.h);
                p.fill_round(Rect::new(track.x, track.y, w, track.h), 4.0, pal.fill);
            }
            None => {
                let note = p.gfx.format(&theme.font, 10.0, Weight::Regular, Align::Center);
                p.fill_round(track, 4.0, pal.track);
                p.text("Not available for this display", &note, Rect::new(track.x, track.y - 14.0, track.w, 12.0), pal.sub);
            }
        }
    }

    fn draw_media(&self, p: &Painter, theme: &Theme, pal: &Pal, r: Rect, hits: &mut Vec<(Rect, Hit)>) {
        self.module(p, pal, r, 24.0, 0.0);
        let title = p.gfx.format(&theme.font, 13.0, Weight::SemiBold, Align::Leading);
        let sub = p.gfx.format(&theme.font, 11.0, Weight::Regular, Align::Leading);
        let art = Rect::new(r.x + 14.0, r.y + 14.0, 48.0, 48.0);
        let now = self.now.as_ref().filter(|n| !n.title.is_empty());
        match (&self.art, now) {
            (Some((_, b)), Some(_)) => p.bitmap_cover(b, art, 9.0, 1.0),
            _ => {
                p.fill_round(art, 9.0, pal.off_bg);
                let f = p.gfx.format(ICONS, 20.0, Weight::Regular, Align::Center);
                p.text("\u{E8D6}", &f, art, pal.sub);
            }
        }
        if now.is_some() {
            hits.push((art, Hit::Media(MediaBtn::Open)));
        }
        let tw = r.w - 28.0;
        match now {
            Some(n) => {
                p.text(&n.title, &title, Rect::new(r.x + 14.0, art.bottom() + 8.0, tw, 17.0), pal.text);
                p.text(&n.artist, &sub, Rect::new(r.x + 14.0, art.bottom() + 25.0, tw, 15.0), pal.sub);
            }
            None => p.text("Not Playing", &title, Rect::new(r.x + 14.0, art.bottom() + 12.0, tw, 17.0), pal.sub),
        }
        let playing = now.is_some_and(|n| n.playing);
        let cy = r.bottom() - 22.0;
        let btns = [
            (MediaBtn::Prev, '\u{E892}', -38.0, 15.0),
            (MediaBtn::Play, if playing { '\u{E769}' } else { '\u{E768}' }, 0.0, 20.0),
            (MediaBtn::Next, '\u{E893}', 38.0, 15.0),
        ];
        for (b, glyph, dx, size) in btns {
            let br = Rect::new(r.cx() + dx - 16.0, cy - 16.0, 32.0, 32.0);
            let enabled = now.is_some();
            if enabled {
                hits.push((br, Hit::Media(b)));
                let lit = self.lit(Hit::Media(b));
                if lit > 0.0 {
                    p.fill_circle(br.cx(), br.cy(), 16.0, pal.module_hover.alpha(pal.module_hover.a * lit.min(1.0)));
                }
            }
            let f = p.gfx.format(ICONS, size, Weight::Regular, Align::Center);
            let c = if enabled { pal.text } else { pal.sub.alpha(pal.sub.a * 0.6) };
            // Filled glyphs read like macOS's transport controls.
            p.text(&filled(glyph).to_string(), &f, br, c);
        }
    }

    // --- detail pages -----------------------------------------------------

    fn draw_page(&self, p: &Painter, theme: &Theme, pal: &Pal, hits: &mut Vec<(Rect, Hit)>) {
        let page = self.shown;
        let font = &theme.font;
        let x0 = self.body.x + PAD;
        let w = self.body.w - 2.0 * PAD;
        let mut y = self.body.y + PAD;

        // Header: back, title, and the page's switch.
        let back = Rect::new(x0 - 4.0, y, 30.0, 32.0);
        hits.push((back, Hit::Back));
        let lit = self.lit(Hit::Back);
        if lit > 0.0 {
            p.fill_round(back.inset(1.0, 2.0), 8.0, pal.module_hover.alpha(pal.module_hover.a * lit.min(1.0)));
        }
        let chevron = p.gfx.format(ICONS, 12.0, Weight::Regular, Align::Center);
        p.text("\u{E76B}", &chevron, back, pal.text);
        let (name, switch) = match page {
            Page::Wifi => ("Wi-Fi", Some(Toggle::Wifi)),
            Page::Bluetooth => ("Bluetooth", Some(Toggle::Bluetooth)),
            Page::Focus => ("Focus", None),
            Page::Display => ("Display", None),
            Page::Sound => ("Sound", None),
            Page::Accessibility => ("Accessibility", None),
            Page::Main => ("", None),
        };
        let title = p.gfx.format(font, 15.0, Weight::SemiBold, Align::Leading);
        p.text(name, &title, Rect::new(x0 + 30.0, y, w - 80.0, 32.0), pal.text);
        if let Some(t) = switch {
            let r = Rect::new(x0 + w - 40.0, y + 6.0, 40.0, 22.0);
            hits.push((r, Hit::Toggle(t)));
            self.switch(p, theme, pal, r, self.on(t), Hit::Toggle(t));
        }
        y += 40.0;
        p.line(x0, y, x0 + w, y, pal.sep, 1.0);
        y += 6.0;

        let links: &[(&str, &'static str)] = match page {
            Page::Wifi => &[("Other Networks…", "ms-availablenetworks:"), ("Wi-Fi Settings…", "ms-settings:network-wifi")],
            Page::Bluetooth => &[("Add Device…", "ms-settings-connectabledevices:devicediscovery"), ("Bluetooth Settings…", "ms-settings:bluetooth")],
            Page::Focus => &[("Notifications Settings…", "ms-settings:notifications"), ("Focus Settings…", "ms-settings:focus")],
            Page::Display => &[("Display Settings…", "ms-settings:display")],
            Page::Sound => &[("Volume Mixer…", "ms-settings:apps-volume"), ("Sound Settings…", "ms-settings:sound")],
            Page::Accessibility => &[("Accessibility Settings…", "ms-settings:easeofaccess")],
            Page::Main => &[],
        };
        let footer_h = links.len() as f32 * LINK_H + 12.0;
        let bottom = self.body.bottom() - PAD - footer_h;
        let view = Rect::new(self.body.x, y, self.body.w, bottom - y);

        // The scrolling list.
        let items = self.page_items(page);
        let content_h: f32 = items.iter().map(Item::height).sum();
        let max = (content_h - view.h).max(0.0);
        let off = self.scroll.offset_px(self.scale * self.zoom).min(max + 40.0);
        p.clipped(view, || {
            let mut iy = y - off;
            for item in &items {
                let h = item.height();
                let row = Rect::new(x0, iy, w, h);
                if iy + h > view.y && iy < view.bottom() {
                    self.draw_item(p, theme, pal, item, row, hits, view);
                }
                iy += h;
            }
        });
        // Remember the scroll range for the wheel (a render-time measurement).
        SCROLL_MAX.with(|c| c.set(max));

        // Footer links.
        let mut fy = bottom + 6.0;
        p.line(x0, bottom, x0 + w, bottom, pal.sep, 1.0);
        let link_f = p.gfx.format(font, 13.0, Weight::Regular, Align::Leading);
        for &(label, uri) in links {
            let r = Rect::new(x0 - 6.0, fy, w + 12.0, LINK_H);
            hits.push((r, Hit::Link(uri)));
            let lit = self.lit(Hit::Link(uri));
            if lit > 0.0 {
                p.fill_round(r, 8.0, pal.module_hover.alpha(pal.module_hover.a * lit.min(1.0)));
            }
            p.text(label, &link_f, Rect::new(r.x + 10.0, r.y, r.w - 20.0, r.h), pal.text);
            fy += LINK_H;
        }
    }

    fn page_items(&self, page: Page) -> Vec<Item> {
        let mut v = Vec::new();
        match page {
            Page::Wifi => match self.on(Toggle::Wifi) {
                Some(false) => v.push(Item::Note("Wi-Fi is off.".into())),
                None => v.push(Item::Note("No Wi-Fi adapter.".into())),
                Some(true) => match &self.status.networks {
                    None => v.push(Item::Note("Searching for networks…".into())),
                    Some(_) if self.status.scan_failed => {
                        v.push(Item::Note("Windows didn't let DockSide scan for networks.".into()));
                        if let Some(s) = &self.status.ssid {
                            v.push(Item::Section("Known Network"));
                            v.push(Item::Note(s.clone()));
                        }
                    }
                    Some(list) if list.is_empty() => v.push(Item::Note("No networks found.".into())),
                    Some(list) => {
                        let known = list.iter().filter(|n| n.connected).count();
                        if known > 0 {
                            v.push(Item::Section("Known Network"));
                        }
                        for i in 0..list.len() {
                            if i == known {
                                v.push(Item::Section("Other Networks"));
                            }
                            v.push(Item::Network(i));
                        }
                    }
                },
            },
            Page::Bluetooth => match self.on(Toggle::Bluetooth) {
                Some(false) => v.push(Item::Note("Bluetooth is off.".into())),
                None => v.push(Item::Note("No Bluetooth adapter.".into())),
                Some(true) => match &self.status.devices {
                    None => v.push(Item::Note("Looking for devices…".into())),
                    Some(list) if list.is_empty() => v.push(Item::Note("No paired devices.".into())),
                    Some(list) => {
                        v.push(Item::Section("Devices"));
                        v.extend((0..list.len()).map(Item::Device));
                    }
                },
            },
            Page::Focus => {
                v.push(Item::Switch(Toggle::Focus, "Do Not Disturb", '\u{E708}'));
                v.push(Item::Note("Only priority notifications come through.".into()));
            }
            Page::Display => {
                v.push(Item::Section("Display"));
                v.push(Item::Slider(Slider::Brightness));
                v.push(Item::Switch(Toggle::Dark, "Dark Mode", '\u{E793}'));
                v.push(Item::Switch(Toggle::NightLight, "Night Light", '\u{F08C}'));
            }
            Page::Sound => {
                v.push(Item::Section("Sound"));
                v.push(Item::Slider(Slider::Volume));
                v.push(Item::Section("Output"));
                match &self.status.outputs {
                    None => v.push(Item::Note("Loading…".into())),
                    Some(list) if list.is_empty() => v.push(Item::Note("No output devices.".into())),
                    Some(list) => v.extend((0..list.len()).map(Item::Output)),
                }
            }
            Page::Accessibility => {
                v.push(Item::Switch(Toggle::Magnifier, "Magnifier", '\u{E71E}'));
                v.push(Item::Switch(Toggle::ColorFilters, "Color Filters", '\u{E790}'));
                v.push(Item::Switch(Toggle::Narrator, "Narrator", '\u{E7F5}'));
                v.push(Item::Switch(Toggle::LiveCaptions, "Live Captions", '\u{E7F0}'));
                v.push(Item::Switch(Toggle::StickyKeys, "Sticky Keys", '\u{E765}'));
            }
            Page::Main => {}
        }
        v
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_item(&self, p: &Painter, theme: &Theme, pal: &Pal, item: &Item, r: Rect, hits: &mut Vec<(Rect, Hit)>, view: Rect) {
        let font = &theme.font;
        let name_f = p.gfx.format(font, 13.0, Weight::Regular, Align::Leading);
        let small = p.gfx.format(font, 11.0, Weight::Regular, Align::Trailing);
        // Rows: hover highlight, an icon disc, the name, and a trailing detail.
        let row = |h: Hit, glyph: char, on: bool, name: &str, trailing: Option<&str>, check: bool, hits: &mut Vec<(Rect, Hit)>| {
            let hr = Rect::new(r.x - 6.0, r.y + 1.0, r.w + 12.0, r.h - 2.0);
            // Only the part inside the list can be clicked.
            let top = hr.y.max(view.y);
            let bot = hr.bottom().min(view.bottom());
            if bot > top {
                hits.push((Rect::new(hr.x, top, hr.w, bot - top), h));
            }
            let lit = self.lit(h);
            if lit > 0.0 {
                p.fill_round(hr, 8.0, pal.module_hover.alpha(pal.module_hover.a * lit.min(1.0)));
            }
            let disc = Rect::new(r.x + 2.0, r.cy() - 13.0, 26.0, 26.0);
            p.fill_circle(disc.cx(), disc.cy(), 13.0, if on { theme.accent } else { pal.off_bg });
            let g = p.gfx.format(ICONS, 12.0, Weight::Regular, Align::Center);
            p.text(&glyph.to_string(), &g, disc, if on { Color::WHITE } else { pal.text });
            let mut right = r.right() - 4.0;
            if check {
                let f = p.gfx.format(ICONS, 12.0, Weight::Regular, Align::Center);
                p.text("\u{E73E}", &f, Rect::new(right - 16.0, r.y, 16.0, r.h), theme.accent);
                right -= 22.0;
            }
            if let Some(t) = trailing {
                let tw = p.gfx.measure(t, &small) + 4.0;
                p.text(t, &small, Rect::new(right - tw, r.y, tw, r.h), pal.sub);
                right -= tw + 6.0;
            }
            p.text(name, &name_f, Rect::new(disc.right() + 10.0, r.y, right - disc.right() - 10.0, r.h), pal.text);
        };
        match item {
            Item::Section(s) => {
                let f = p.gfx.format(font, 11.0, Weight::SemiBold, Align::Leading);
                p.text(s, &f, Rect::new(r.x, r.y + 4.0, r.w, r.h - 4.0), pal.sub);
            }
            Item::Note(s) => {
                let f = p.gfx.format(font, 12.0, Weight::Regular, Align::Leading);
                p.text(s, &f, r, pal.sub);
            }
            Item::Network(i) => {
                let Some(n) = self.status.networks.as_ref().and_then(|l| l.get(*i)) else { return };
                let glyph = match n.bars {
                    0 | 1 => '\u{E872}',
                    2 => '\u{E873}',
                    3 => '\u{E874}',
                    _ => '\u{E701}',
                };
                row(Hit::Network(*i), glyph, n.connected, &n.ssid, n.secure.then_some("Secured"), false, hits);
            }
            Item::Device(i) => {
                let Some(d) = self.status.devices.as_ref().and_then(|l| l.get(*i)) else { return };
                row(Hit::Device(*i), device_glyph(&d.name), d.connected, &d.name, d.connected.then_some("Connected"), false, hits);
            }
            Item::Output(i) => {
                let Some(o) = self.status.outputs.as_ref().and_then(|l| l.get(*i)) else { return };
                let default = match &self.local_output {
                    Some((id, _)) => *id == o.id,
                    None => o.default,
                };
                row(Hit::Output(*i), output_glyph(&o.name), default, &o.name, None, default, hits);
            }
            Item::Switch(t, name, glyph) => {
                let on = self.on(*t);
                let sw = Rect::new(r.right() - 40.0, r.cy() - 11.0, 40.0, 22.0);
                row(Hit::Toggle(*t), *glyph, on == Some(true), name, None, false, hits);
                self.switch(p, theme, pal, sw, on, Hit::Toggle(*t));
            }
            Item::Slider(s) => {
                let level = match s {
                    Slider::Brightness => self.brightness().map(|b| b as f32 / 100.0),
                    Slider::Volume => self.volume.map(|(v, m)| if m { 0.0 } else { v }),
                };
                let m = Rect::new(r.x, r.y + 2.0, r.w, 56.0);
                let page = if *s == Slider::Brightness { Page::Display } else { Page::Sound };
                let mut local = Vec::new();
                let name = if *s == Slider::Brightness { "Brightness" } else { "Volume" };
                self.slider_module(p, theme, pal, m, name, *s, level, page, &mut local);
                // On its own page, the module's title doesn't open anything.
                hits.extend(local.into_iter().filter(|(_, h)| !matches!(h, Hit::Open(_))));
            }
        }
    }

    /// A macOS switch.
    fn switch(&self, p: &Painter, theme: &Theme, pal: &Pal, r: Rect, on: Option<bool>, h: Hit) {
        let k = self.motion.get((h, 3), if on == Some(true) { 1.0 } else { 0.0 }, feel::SLIDE);
        let bg = pal.track.mix(theme.accent, k.clamp(0.0, 1.0));
        let bg = if on.is_none() { bg.alpha(bg.a * 0.4) } else { bg };
        p.fill_round(r, r.h / 2.0, bg);
        let rad = r.h / 2.0 - 2.0;
        let cx = r.x + 2.0 + rad + (r.w - 4.0 - 2.0 * rad) * k;
        p.fill_circle(cx, r.cy() + 0.5, rad, Color::gray(0, 0.12));
        p.fill_circle(cx, r.cy(), rad, Color::WHITE);
    }

    fn sync_scroll(&mut self) {
        self.scroll_max = SCROLL_MAX.with(|c| c.get());
        self.scroll.clamp(self.scroll_max);
    }
}

thread_local! {
    static SCROLL_MAX: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

enum Item {
    Section(&'static str),
    Note(String),
    Network(usize),
    Device(usize),
    Output(usize),
    Switch(Toggle, &'static str, char),
    Slider(Slider),
}

impl Item {
    fn height(&self) -> f32 {
        match self {
            Item::Section(_) => 24.0,
            Item::Note(_) => 30.0,
            Item::Slider(_) => 64.0,
            _ => ROW_H,
        }
    }
}

/// The filled version of a transport glyph (Segoe Fluent's "Solid" set).
fn filled(g: char) -> char {
    match g {
        '\u{E768}' => '\u{F5B0}',
        '\u{E769}' => '\u{F8AE}',
        '\u{E892}' => '\u{F8AC}',
        '\u{E893}' => '\u{F8AD}',
        other => other,
    }
}

fn battery_glyph(percent: u8) -> char {
    // Battery0 … Battery10 (EBA0 … EBAA).
    char::from_u32(0xEBA0 + (percent as u32 + 5) / 10).unwrap_or('\u{EBAA}')
}

fn device_glyph(name: &str) -> char {
    let n = name.to_lowercase();
    if ["buds", "pods", "head", "ear", "wh-", "wf-", "bose", "jabra"].iter().any(|k| n.contains(k)) {
        '\u{E7F6}'
    } else if n.contains("mouse") || n.contains("mx ") {
        '\u{E962}'
    } else if n.contains("keyboard") || n.contains("keys") {
        '\u{E765}'
    } else if n.contains("controller") || n.contains("xbox") || n.contains("gamepad") {
        '\u{E7FC}'
    } else if n.contains("phone") || n.contains("galaxy") || n.contains("pixel") {
        '\u{E8EA}'
    } else if n.contains("speaker") || n.contains("soundbar") {
        '\u{E7F5}'
    } else {
        '\u{E702}'
    }
}

fn output_glyph(name: &str) -> char {
    let n = name.to_lowercase();
    if ["headphone", "headset", "buds", "pods", "earphone"].iter().any(|k| n.contains(k)) {
        '\u{E7F6}'
    } else if n.contains("hdmi") || n.contains("display") || n.contains("monitor") || n.contains("tv") {
        '\u{E7F4}'
    } else {
        '\u{E7F5}'
    }
}
