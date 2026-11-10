//! Control Center: macOS Tahoe's panel under the menu bar's switches icon,
//! holding everything in Windows 11's Quick Settings.
//!
//! Like Tahoe's, it has no panel of its own: each module is a piece of glass
//! floating over the desktop (Wi-Fi, Bluetooth, Nearby Sharing in AirDrop's
//! spot, Now Playing, Focus, Display, Sound and the round controls), packed
//! into a four-column grid in the order of [`MODULES`]. Which ones show is set
//! in Settings > Control Center.
//!
//! It's built the way Launchpad is: a Windows.UI.Composition tree in a window
//! of its own. Each module is a clipped glass container with its content drawn
//! once into a GPU layer; hovering, pressing, switching, dragging a slider,
//! scrolling a list and moving between pages only change visual properties.
//! A layer is drawn again only when what it shows changes (see `Tile::key`).

pub mod system;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Instant;

use windows::core::{Interface, Result};
use windows::Foundation::Numerics::{Vector2, Vector3};
use windows::Win32::Foundation::{BOOL, HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_HOSTBACKDROPBRUSH};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionBrush, CompositionColorBrush, CompositionRoundedRectangleGeometry, CompositionSpriteShape, Compositor, ContainerVisual,
    ShapeVisual, SpriteVisual,
};

use crate::anim::{lerp, Scroll, Spring};
use crate::config::{Backdrop, Config};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconPixels;
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::island::media::{Cmd, Media, NowPlaying};
use crate::launchpad::comp::{self, Gpu, Layer};
use crate::menu::Action;
use crate::theme::Theme;
use crate::win::system::Shortcut;
use crate::win::Monitor;
use system::{Req, Status, Toggle, Worker};

/// Grid cell and gap, in points (Tahoe's proportions).
const CELL: f32 = 64.0;
const GAP: f32 = 12.0;
const COLS: usize = 4;
/// Panel width: four cells and three gaps.
pub const W: f32 = CELL * 4.0 + GAP * 3.0;
/// Rounded-rect modules (Now Playing, Display, Sound).
const RADIUS: f32 = 26.0;
/// The Edit Controls pill under the grid.
const EDIT_H: f32 = 26.0;
/// A detail page is at least this tall.
const PAGE_MIN_H: f32 = 400.0;
const HEADER_H: f32 = 52.0;
const ROW_H: f32 = 38.0;
const LINK_H: f32 = 32.0;
const ICONS: &str = "Segoe Fluent Icons";

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

/// Everything Control Center can show.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Module {
    Wifi,
    Media,
    Bluetooth,
    Nearby,
    Focus,
    Project,
    Cast,
    Display,
    Sound,
    Dark,
    Calculator,
    Timer,
    Screenshot,
    Airplane,
    EnergySaver,
    NightLight,
    Hotspot,
    LiveCaptions,
    Accessibility,
    Keyboard,
    Vpn,
    ScreenshotMode,
    /// The Edit Controls pill (always there).
    Edit,
}

/// Where a module is listed in Settings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Modules,
    Controls,
    Windows,
}

pub struct ModuleInfo {
    pub module: Module,
    /// Its name in `[control_center]`.
    pub key: &'static str,
    pub label: &'static str,
    pub section: Section,
    /// Size in grid cells.
    cols: usize,
    rows: usize,
}

const fn info(module: Module, key: &'static str, label: &'static str, section: Section, cols: usize, rows: usize) -> ModuleInfo {
    ModuleInfo { module, key, label, section, cols, rows }
}

/// Every optional module, in grid order: each takes the first spot it fits,
/// which lays the defaults out exactly like macOS.
pub const MODULES: [ModuleInfo; 22] = [
    info(Module::Wifi, "wifi", "Wi-Fi", Section::Modules, 2, 1),
    info(Module::Media, "now_playing", "Now Playing", Section::Modules, 2, 2),
    info(Module::Bluetooth, "bluetooth", "Bluetooth", Section::Modules, 1, 1),
    info(Module::Nearby, "nearby_sharing", "Nearby Sharing", Section::Modules, 1, 1),
    info(Module::Focus, "focus", "Focus", Section::Modules, 2, 1),
    info(Module::Project, "project", "Project (Win+P)", Section::Modules, 1, 1),
    info(Module::Cast, "cast", "Screen Mirroring (Cast)", Section::Modules, 1, 1),
    info(Module::Display, "display", "Display", Section::Modules, 4, 1),
    info(Module::Sound, "sound", "Sound", Section::Modules, 4, 1),
    info(Module::Dark, "dark_mode", "Dark Mode", Section::Controls, 1, 1),
    info(Module::Calculator, "calculator", "Calculator", Section::Controls, 1, 1),
    info(Module::Timer, "timer", "Timer", Section::Controls, 1, 1),
    info(Module::Screenshot, "screenshot", "Screenshot", Section::Controls, 1, 1),
    info(Module::ScreenshotMode, "screenshot_mode", "Screenshot Mode", Section::Controls, 1, 1),
    info(Module::Airplane, "airplane_mode", "Airplane Mode", Section::Windows, 1, 1),
    info(Module::EnergySaver, "energy_saver", "Energy Saver", Section::Windows, 1, 1),
    info(Module::NightLight, "night_light", "Night Light", Section::Windows, 1, 1),
    info(Module::Hotspot, "hotspot", "Mobile Hotspot", Section::Windows, 1, 1),
    info(Module::LiveCaptions, "live_captions", "Live Captions", Section::Windows, 1, 1),
    info(Module::Accessibility, "accessibility", "Accessibility", Section::Windows, 1, 1),
    info(Module::Keyboard, "keyboard", "Keyboard Layout", Section::Windows, 1, 1),
    info(Module::Vpn, "vpn", "VPN", Section::Windows, 1, 1),
];

/// Whether Control Center shows the module called `key`.
pub fn shows(cfg: &Config, key: &str) -> bool {
    cfg.control_center.get(key).copied().unwrap_or(!matches!(key, "keyboard" | "vpn"))
}

fn label_of(m: Module) -> &'static str {
    MODULES.iter().find(|i| i.module == m).map_or("Edit Controls", |i| i.label)
}

/// The switch a module turns on and off, if any.
fn toggle_of(m: Module) -> Option<Toggle> {
    Some(match m {
        Module::Wifi => Toggle::Wifi,
        Module::Bluetooth => Toggle::Bluetooth,
        Module::Nearby => Toggle::Nearby,
        Module::Focus => Toggle::Focus,
        Module::Dark => Toggle::Dark,
        Module::ScreenshotMode => Toggle::ScreenshotMode,
        Module::Airplane => Toggle::Airplane,
        Module::EnergySaver => Toggle::EnergySaver,
        Module::NightLight => Toggle::NightLight,
        Module::Hotspot => Toggle::Hotspot,
        Module::LiveCaptions => Toggle::LiveCaptions,
        _ => return None,
    })
}

fn glyph_of(m: Module) -> char {
    match m {
        Module::Wifi => '\u{E701}',
        Module::Bluetooth => '\u{E702}',
        Module::Nearby => '\u{E72D}',
        Module::Focus => '\u{E708}',
        Module::Project => '\u{EBC6}',
        Module::Cast => '\u{EC15}',
        Module::Dark => '\u{E793}',
        Module::Calculator => '\u{E8EF}',
        Module::Timer => '\u{E916}',
        Module::Screenshot => '\u{E722}',
        Module::Airplane => '\u{E709}',
        Module::EnergySaver => '\u{E8BE}',
        Module::NightLight => '\u{F08C}',
        Module::Hotspot => '\u{E704}',
        Module::LiveCaptions => '\u{E7F0}',
        Module::Accessibility => '\u{E776}',
        Module::Keyboard => '\u{E765}',
        Module::Vpn => '\u{E705}',
        // Hide (an eye, struck through).
        Module::ScreenshotMode => '\u{ED1A}',
        _ => ' ',
    }
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
enum Hit {
    /// A module's own action (its switch, or what the button does).
    Tile(Module),
    Toggle(Toggle),
    Open(Page),
    Slider(Slider),
    Media(MediaBtn),
    Mute,
    Back,
    Network(usize),
    Device(usize),
    Output(usize),
    Link(usize),
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

/// Colours for the current appearance.
struct Pal {
    text: Color,
    sub: Color,
    tint: Color,
    lit: Color,
    on_bg: Color,
    on_fg: Color,
    chip: Color,
    track: Color,
    fill: Color,
    sep: Color,
    tip_bg: Color,
}

fn pal(theme: &Theme, solid: bool) -> Pal {
    if theme.dark {
        Pal {
            text: Color::gray(255, 0.96),
            sub: Color::gray(255, 0.62),
            tint: if solid { Color::rgba(44, 44, 48, 0.97) } else { Color::rgba(28, 28, 34, 0.26) },
            lit: Color::gray(255, 0.08),
            on_bg: Color::gray(255, 0.97),
            on_fg: theme.accent,
            chip: Color::gray(255, 0.16),
            track: Color::gray(255, 0.18),
            fill: Color::gray(255, 0.96),
            sep: Color::gray(255, 0.14),
            tip_bg: Color::rgba(40, 40, 44, 0.94),
        }
    } else {
        Pal {
            text: Color::gray(0, 0.86),
            sub: Color::gray(0, 0.52),
            tint: if solid { Color::rgba(246, 246, 248, 0.97) } else { Color::rgba(255, 255, 255, 0.38) },
            lit: Color::gray(255, 0.3),
            on_bg: theme.accent,
            on_fg: Color::WHITE,
            chip: Color::gray(0, 0.07),
            track: Color::gray(0, 0.12),
            fill: Color::WHITE,
            sep: Color::gray(0, 0.1),
            tip_bg: Color::rgba(250, 250, 252, 0.96),
        }
    }
}

fn ui_color(c: Color) -> windows::UI::Color {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    windows::UI::Color { A: b(c.a), R: b(c.r), G: b(c.g), B: b(c.b) }
}

fn v2(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

fn v3(x: f32, y: f32) -> Vector3 {
    Vector3 { X: x, Y: y, Z: 0.0 }
}

/// The shared brushes every piece of glass is made of.
struct Brushes {
    /// DWM's blur of what's behind the window (`None` for a solid look).
    backdrop: Option<CompositionBrush>,
    tint: CompositionColorBrush,
    sheen: CompositionBrush,
    rim: CompositionBrush,
    lit: CompositionColorBrush,
    fill: CompositionColorBrush,
}

/// One piece of glass: a clipped container with the glass layers inside.
struct Glass {
    vis: ContainerVisual,
    clip: CompositionRoundedRectangleGeometry,
    rim: CompositionRoundedRectangleGeometry,
    /// The hover highlight.
    lit: SpriteVisual,
}

impl Glass {
    fn new(c: &Compositor, b: &Brushes, w: f32, h: f32, r: f32) -> Result<Self> {
        let vis = c.CreateContainerVisual()?;
        let clip = c.CreateRoundedRectangleGeometry()?;
        vis.SetClip(&c.CreateGeometricClipWithGeometry(&clip)?)?;
        let kids = vis.Children()?;
        let sprite = |brush: &CompositionBrush| -> Result<SpriteVisual> {
            let s = c.CreateSpriteVisual()?;
            s.SetBrush(brush)?;
            s.SetRelativeSizeAdjustment(v2(1.0, 1.0))?;
            kids.InsertAtTop(&s)?;
            Ok(s)
        };
        if let Some(bd) = &b.backdrop {
            sprite(bd)?;
        }
        sprite(&b.tint.cast()?)?;
        sprite(&b.sheen)?;
        let lit = sprite(&b.lit.cast()?)?;
        lit.SetOpacity(0.0)?;
        let rim = c.CreateRoundedRectangleGeometry()?;
        let stroke = c.CreateSpriteShapeWithGeometry(&rim)?;
        stroke.SetStrokeThickness(1.0)?;
        stroke.SetStrokeBrush(&b.rim)?;
        let rim_layer = c.CreateShapeVisual()?;
        rim_layer.SetRelativeSizeAdjustment(v2(1.0, 1.0))?;
        rim_layer.Shapes()?.Append(&stroke)?;
        kids.InsertAtTop(&rim_layer)?;
        let g = Self { vis, clip, rim, lit };
        g.set_size(w, h, r);
        Ok(g)
    }

    /// Size and corner radius, in pixels.
    fn set_size(&self, w: f32, h: f32, r: f32) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let _ = self.vis.SetSize(v2(w, h));
        let _ = self.vis.SetCenterPoint(v3(w / 2.0, h / 2.0));
        let _ = self.clip.SetSize(v2(w, h));
        let _ = self.clip.SetCornerRadius(v2(r, r));
        let _ = self.rim.SetOffset(v2(0.5, 0.5));
        let _ = self.rim.SetSize(v2((w - 1.0).max(0.0), (h - 1.0).max(0.0)));
        let _ = self.rim.SetCornerRadius(v2((r - 0.5).max(0.0), (r - 0.5).max(0.0)));
    }
}

/// A rounded bar the compositor sizes (a slider's fill, a row highlight).
struct Bar {
    vis: ShapeVisual,
    geo: CompositionRoundedRectangleGeometry,
}

impl Bar {
    fn new(c: &Compositor, brush: &CompositionColorBrush) -> Result<Self> {
        let geo = c.CreateRoundedRectangleGeometry()?;
        let shape: CompositionSpriteShape = c.CreateSpriteShapeWithGeometry(&geo)?;
        shape.SetFillBrush(brush)?;
        let vis = c.CreateShapeVisual()?;
        vis.SetRelativeSizeAdjustment(v2(1.0, 1.0))?;
        vis.Shapes()?.Append(&shape)?;
        Ok(Self { vis, geo })
    }

    /// In pixels.
    fn set(&self, r: Rect, radius: f32) {
        let _ = self.geo.SetOffset(v2(r.x, r.y));
        let _ = self.geo.SetSize(v2(r.w.max(0.0), r.h.max(0.0)));
        let _ = self.geo.SetCornerRadius(v2(radius, radius));
    }
}

struct Tile {
    m: Module,
    /// Panel points.
    rect: Rect,
    radius: f32,
    /// Grid row (for the opening stagger).
    row: usize,
    glass: Glass,
    base: Layer,
    /// The switch's "on" look, faded in over `base`, and where it sits in the tile.
    knob: Option<(Layer, Rect)>,
    /// A slider's fill and track (tile points).
    fill: Option<(Bar, Rect, Slider)>,
    key: u64,
    lit: Spring,
    press: Spring,
    on: Spring,
    level: Spring,
}

/// The detail page: one large piece of glass.
struct PageVis {
    glass: Glass,
    header: Layer,
    /// Clips the list; `scroller` inside it moves.
    view: ContainerVisual,
    scroller: ContainerVisual,
    list: Layer,
    hl: Bar,
    footer: Layer,
    fhl: Bar,
    keys: (u64, u64, u64),
    /// List rows and footer links: (content rect, hit).
    rows: Vec<(Rect, Hit)>,
    links: Vec<(Rect, Hit)>,
    content_h: f32,
    /// Where it grows from (a tile's rect) and how far (0 grid, 1 page).
    from: Rect,
    t: Spring,
    hl_y: Spring,
    hl_h: f32,
    hl_a: Spring,
    fhl_y: Spring,
    fhl_a: Spring,
}

/// What an open Control Center cost, logged when it closes.
#[derive(Default)]
struct Stats {
    frames: u32,
    draws: u32,
    draw_us: u64,
}

pub struct ControlCenter {
    pub hwnd: HWND,
    /// Physical pixels per point (the panel shrinks on small screens).
    pub scale: f32,
    /// Keeps the GPU device its layers draw with alive.
    _gpu: Rc<Gpu>,
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    grid: ContainerVisual,
    _brushes: Brushes,
    tiles: Vec<Tile>,
    page_vis: PageVis,
    tip: Layer,
    tip_label: Option<&'static str>,
    tip_a: Spring,
    tip_at: Rect,
    pal: Pal,
    font: String,
    worker: Worker,
    media: Media,
    status: Status,
    /// Changes made here that the worker hasn't confirmed yet: (value, request number).
    local: std::collections::HashMap<Toggle, (bool, u64)>,
    local_brightness: Option<(u8, u64)>,
    local_output: Option<(String, u64)>,
    /// The level being dragged (volume follows the pointer, not the worker).
    local_volume: Option<(f32, bool)>,
    gen: u64,
    now: Option<NowPlaying>,
    page: Page,
    shown: Page,
    /// Panel size in points: the grid's, and the window's.
    grid_h: f32,
    h: f32,
    /// Where the panel grows from (under the menu bar icon), in points.
    origin: (f32, f32),
    hits: Vec<(Rect, Hit, Option<usize>)>,
    hover: Option<Hit>,
    hover_tile: Option<usize>,
    hover_since: Instant,
    pressed: Option<(Hit, Option<usize>)>,
    drag: Option<(Slider, Rect)>,
    scroll: Scroll,
    opened: Instant,
    closing: Option<Instant>,
    stats: Stats,
    pub tracking: bool,
    pub dirty: bool,
}

/// How long opening takes (each row a little after the one above).
const OPEN_S: f32 = 0.34;
const STAGGER_S: f32 = 0.022;
const CLOSE_S: f32 = 0.16;

impl ControlCenter {
    /// Opens under the menu bar icon at `anchor` (its screen rect, in points),
    /// on `page`. `msg_hwnd` receives worker and media updates.
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: &Monitor, anchor: Rect, page: Page, msg_hwnd: isize) -> Result<Self> {
        let started = Instant::now();
        let gpu = comp::gpu(gfx)?;
        let c = gpu.compositor.clone();
        let s = monitor.scale;
        let (mon_l, mon_r, mon_b) = (monitor.rect.left as f32 / s, monitor.rect.right as f32 / s, monitor.rect.bottom as f32 / s);

        let (placed, rows) = layout(MODULES.iter().filter(|i| shows(cfg, i.key)));
        let cells_h = if rows == 0 { 0.0 } else { rows as f32 * CELL + (rows - 1) as f32 * GAP };
        let edit_y = if rows == 0 { 0.0 } else { cells_h + GAP + 4.0 };
        let grid_h = edit_y + EDIT_H;
        let h = grid_h.max(PAGE_MIN_H);

        // Under the icon, at the screen's right like macOS; scaled down to fit a short screen.
        let top = anchor.bottom() + 6.0;
        let zoom = ((mon_b - top - 8.0) / h).clamp(0.5, 1.0);
        let px = s * zoom;
        let (w_pt, h_pt) = (W * zoom, h * zoom);
        let x = (mon_r - 10.0 - w_pt).min(anchor.x).max(mon_l + 10.0);
        let rect = RECT {
            left: (x * s).round() as i32,
            top: (top * s).round() as i32,
            right: ((x + w_pt) * s).round() as i32,
            bottom: ((top + h_pt) * s).round() as i32,
        };
        let ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP;
        let hwnd = crate::win::create_window(ex, WS_POPUP, rect, HWND::default())?;
        crate::win::accent::prepare(hwnd);
        let kind = cfg.menubar.backdrop.for_panels();
        let solid = kind == Backdrop::None;
        if !solid {
            unsafe {
                let on = BOOL(1);
                let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_HOSTBACKDROPBRUSH, &on as *const _ as *const _, std::mem::size_of::<BOOL>() as u32);
            }
        }
        let interop: ICompositorDesktopInterop = c.cast()?;
        let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, true)? };
        let root = c.CreateContainerVisual()?;
        root.SetRelativeSizeAdjustment(v2(1.0, 1.0))?;
        target.SetRoot(&root)?;
        let grid = c.CreateContainerVisual()?;
        grid.SetRelativeSizeAdjustment(v2(1.0, 1.0))?;
        root.Children()?.InsertAtTop(&grid)?;

        let pal = pal(theme, solid);
        let white = |a: f32| Color::rgba(255, 255, 255, a);
        let k = if theme.dark { 0.55 } else { 1.0 };
        let gradient = |from: Vector2, to: Vector2, stops: &[(f32, Color)]| -> Result<CompositionBrush> {
            let g = c.CreateLinearGradientBrush()?;
            g.SetStartPoint(from)?;
            g.SetEndPoint(to)?;
            let list = g.ColorStops()?;
            for &(o, col) in stops {
                list.Append(&c.CreateColorGradientStopWithOffsetAndColor(o, ui_color(col))?)?;
            }
            g.cast()
        };
        let tint = match kind {
            Backdrop::Acrylic => pal.tint.alpha((pal.tint.a * 2.2).min(0.85)),
            Backdrop::Blur => pal.tint.alpha(pal.tint.a * 1.4),
            _ => pal.tint,
        };
        let brushes = Brushes {
            backdrop: if solid { None } else { Some(c.CreateHostBackdropBrush()?.cast()?) },
            tint: c.CreateColorBrushWithColor(ui_color(tint))?,
            sheen: gradient(v2(0.5, 0.0), v2(0.5, 1.0), &[(0.0, white(0.16 * k)), (0.5, white(0.02 * k)), (1.0, white(0.05 * k))])?,
            rim: gradient(
                v2(0.0, 0.0),
                v2(1.0, 1.0),
                &[(0.0, white(0.6 * k)), (0.35, white(0.16 * k)), (0.65, white(0.08 * k)), (1.0, white(0.38 * k))],
            )?,
            lit: c.CreateColorBrushWithColor(ui_color(pal.lit.alpha(1.0)))?,
            fill: c.CreateColorBrushWithColor(ui_color(pal.fill))?,
        };

        let mut tiles = Vec::new();
        let mut add = |m: Module, r: Rect, radius: f32, row: usize| -> Result<()> {
            let glass = Glass::new(&c, &brushes, r.w * px, r.h * px, radius * px)?;
            let _ = glass.vis.SetOffset(v3((r.x * px).round(), (r.y * px).round()));
            let base = Layer::new(&gpu, r.w, r.h, px)?;
            glass.vis.Children()?.InsertAtTop(&base.visual)?;
            let fill = match m {
                Module::Display | Module::Sound => {
                    let bar = Bar::new(&c, &brushes.fill)?;
                    glass.vis.Children()?.InsertAtTop(&bar.vis)?;
                    let s = if m == Module::Display { Slider::Brightness } else { Slider::Volume };
                    Some((bar, slider_track(s, r.w), s))
                }
                _ => None,
            };
            let knob = match toggle_of(m) {
                Some(_) => {
                    let kr = knob_rect(m, r);
                    let l = Layer::new(&gpu, kr.w, kr.h, px)?;
                    l.set_offset(kr.x, kr.y);
                    l.set_opacity(0.0);
                    glass.vis.Children()?.InsertAtTop(&l.visual)?;
                    Some((l, kr))
                }
                None => None,
            };
            grid.Children()?.InsertAtTop(&glass.vis)?;
            tiles.push(Tile {
                m,
                rect: r,
                radius,
                row,
                glass,
                base,
                knob,
                fill,
                key: 0,
                lit: Spring::new(0.0, 0.18, 1.0),
                press: Spring::new(1.0, 0.22, 0.62),
                on: Spring::new(0.0, 0.22, 0.9),
                level: Spring::new(0.0, 0.16, 1.0),
            });
            Ok(())
        };
        for &(m, col, row, cols, rows) in &placed {
            let r = Rect::new(
                col as f32 * (CELL + GAP),
                row as f32 * (CELL + GAP),
                cols as f32 * CELL + (cols - 1) as f32 * GAP,
                rows as f32 * CELL + (rows - 1) as f32 * GAP,
            );
            let radius = match (cols, rows) {
                (1, 1) | (2, 1) => r.h / 2.0,
                _ => RADIUS,
            };
            add(m, r, radius, row)?;
        }
        let ew = gfx.measure("Edit Controls", &gfx.format(&theme.font, 12.0, Weight::Regular, Align::Center)) + 28.0;
        add(Module::Edit, Rect::new((W - ew) / 2.0, edit_y, ew, EDIT_H), EDIT_H / 2.0, rows)?;

        // The detail page, grown out of a tile when opened.
        let full = Rect::new(0.0, 0.0, W, h);
        let pg = Glass::new(&c, &brushes, W * px, h * px, RADIUS * px)?;
        pg.vis.SetOpacity(0.0)?;
        let header = Layer::new(&gpu, W, HEADER_H, px)?;
        let view = c.CreateContainerVisual()?;
        view.SetClip(&c.CreateInsetClip()?)?;
        let scroller = c.CreateContainerVisual()?;
        view.Children()?.InsertAtTop(&scroller)?;
        let hl_brush = c.CreateColorBrushWithColor(ui_color(pal.chip))?;
        let hl = Bar::new(&c, &hl_brush)?;
        scroller.Children()?.InsertAtTop(&hl.vis)?;
        let list = Layer::new(&gpu, W, 100.0, px)?;
        scroller.Children()?.InsertAtTop(&list.visual)?;
        let fhl = Bar::new(&c, &hl_brush)?;
        let footer = Layer::new(&gpu, W, 100.0, px)?;
        let pk = pg.vis.Children()?;
        pk.InsertAtTop(&header.visual)?;
        pk.InsertAtTop(&view)?;
        pk.InsertAtTop(&fhl.vis)?;
        pk.InsertAtTop(&footer.visual)?;
        root.Children()?.InsertAtTop(&pg.vis)?;
        let page_vis = PageVis {
            glass: pg,
            header,
            view,
            scroller,
            list,
            hl,
            footer,
            fhl,
            keys: (0, 0, 0),
            rows: Vec::new(),
            links: Vec::new(),
            content_h: 0.0,
            from: full,
            t: Spring::new(0.0, 0.34, 0.86),
            hl_y: Spring::new(0.0, 0.14, 1.0),
            hl_h: ROW_H,
            hl_a: Spring::new(0.0, 0.14, 1.0),
            fhl_y: Spring::new(0.0, 0.14, 1.0),
            fhl_a: Spring::new(0.0, 0.14, 1.0),
        };
        let tip = Layer::new(&gpu, 160.0, 24.0, px)?;
        tip.set_opacity(0.0);
        root.Children()?.InsertAtTop(&tip.visual)?;
        root.SetOpacity(0.0)?;

        let worker = Worker::start(msg_hwnd, crate::win::WM_APP_CONTROL);
        let media = Media::shared(msg_hwnd);
        media.send(Cmd::Refresh);
        let origin = ((anchor.cx() - x) / zoom, 0.0);
        let mut cc = Self {
            hwnd,
            scale: px,
            _gpu: gpu,
            _target: target,
            root,
            grid,
            _brushes: brushes,
            tiles,
            page_vis,
            tip,
            tip_label: None,
            tip_a: Spring::new(0.0, 0.14, 1.0),
            tip_at: Rect::new(0.0, 0.0, 0.0, 0.0),
            pal,
            font: theme.font.clone(),
            worker,
            now: media.snapshot(),
            media,
            status: Status::default(),
            local: std::collections::HashMap::new(),
            local_brightness: None,
            local_output: None,
            local_volume: None,
            gen: 0,
            page: Page::Main,
            shown: Page::Main,
            grid_h,
            h,
            origin,
            hits: Vec::new(),
            hover: None,
            hover_tile: None,
            hover_since: Instant::now(),
            pressed: None,
            drag: None,
            scroll: Scroll::default(),
            opened: Instant::now(),
            closing: None,
            stats: Stats::default(),
            tracking: false,
            dirty: true,
        };
        // Volume is known at once (the worker's first reading takes a moment).
        cc.status.volume = crate::island::sources::volume();
        if page != Page::Main {
            cc.open_page(page, None);
            cc.page_vis.t.snap(1.0);
        }
        cc.build_hits();
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        log!("control center: made in {:.1} ms", started.elapsed().as_secs_f32() * 1000.0);
        Ok(cc)
    }

    pub fn page(&self) -> Page {
        self.page
    }

    /// Keeps it above a newly maximized window (see `App::raise_surfaces`).
    pub fn raise(&self) {
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// Starts closing: it fades away, click-through, and is dropped once `closed`.
    pub fn close(&mut self) {
        if self.closing.is_some() {
            return;
        }
        log!(
            "control center: open {:.1}s, {} frames, {} layer draws ({:.1} ms)",
            self.opened.elapsed().as_secs_f32(),
            self.stats.frames,
            self.stats.draws,
            self.stats.draw_us as f32 / 1000.0
        );
        self.closing = Some(Instant::now());
        unsafe {
            let ex = GetWindowLongW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_TRANSPARENT.0 as i32 | WS_EX_LAYERED.0 as i32);
        }
    }

    pub fn closed(&self) -> bool {
        self.closing.is_some_and(|t| t.elapsed().as_secs_f32() >= CLOSE_S || !crate::anim::motion_enabled())
    }

    // --- state ---------------------------------------------------------------

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
        self.dirty = true;
    }

    pub fn on_media(&mut self) {
        self.now = self.media.snapshot();
        self.dirty = true;
    }

    fn on(&self, t: Toggle) -> Option<bool> {
        if t == Toggle::ScreenshotMode {
            return Some(crate::screenshot_mode::on());
        }
        self.local.get(&t).map(|(v, _)| *v).or_else(|| self.status.toggles.get(&t).copied())
    }

    fn brightness(&self) -> Option<u8> {
        self.local_brightness.map(|(v, _)| v).or(self.status.brightness)
    }

    fn volume(&self) -> Option<(f32, bool)> {
        self.local_volume.or(self.status.volume)
    }

    fn level(&self, s: Slider) -> Option<f32> {
        match s {
            Slider::Brightness => self.brightness().map(|b| b as f32 / 100.0),
            Slider::Volume => self.volume().map(|(v, m)| if m { 0.0 } else { v }),
        }
    }

    fn open_page(&mut self, page: Page, from: Option<usize>) {
        self.page = page;
        if page != Page::Main {
            self.shown = page;
            self.scroll.jump(0.0);
            let full = Rect::new(0.0, 0.0, W, self.h);
            self.page_vis.from = from.map(|i| self.tiles[i].rect).unwrap_or(full);
            self.page_vis.hl_a.snap(0.0);
            self.page_vis.fhl_a.snap(0.0);
            self.page_vis.keys = (0, 0, 0);
        }
        self.page_vis.t.set(if page == Page::Main { 0.0 } else { 1.0 });
        match page {
            Page::Wifi => self.worker.send(Req::ScanWifi),
            Page::Bluetooth => self.worker.send(Req::ListDevices),
            Page::Sound => self.worker.send(Req::ListOutputs),
            _ => {}
        }
        self.set_hover(None, None);
        self.tip_label = None;
        self.tip_a.set(0.0);
        self.dirty = true;
    }

    // --- frames -----------------------------------------------------------------

    pub fn tick(&mut self, dt: f32) -> bool {
        self.stats.frames += 1;
        let mut moving = false;
        for t in &mut self.tiles {
            moving |= t.lit.step(dt) | t.press.step(dt) | t.on.step(dt) | t.level.step(dt);
        }
        let pv = &mut self.page_vis;
        moving |= pv.t.step(dt) | pv.hl_y.step(dt) | pv.hl_a.step(dt) | pv.fhl_y.step(dt) | pv.fhl_a.step(dt);
        moving |= self.scroll.tick(dt);
        moving |= self.tip_a.step(dt);
        // A tooltip comes up once the pointer rests on a round control.
        if self.tip_label.is_some() && self.tip_a.target == 0.0 && self.hover_since.elapsed().as_secs_f32() > 0.5 {
            self.tip_a.set(1.0);
            moving = true;
        }
        moving |= self.tip_label.is_some() && self.tip_a.target == 0.0;
        moving |= self.appearing();
        self.apply();
        moving
    }

    fn appearing(&self) -> bool {
        let opening = self.opened.elapsed().as_secs_f32() < OPEN_S + STAGGER_S * 8.0 && crate::anim::motion_enabled();
        opening || (self.closing.is_some() && !self.closed())
    }

    pub fn animating(&self) -> bool {
        self.appearing()
            || self.tiles.iter().any(|t| !t.lit.settled() || !t.press.settled() || !t.on.settled() || !t.level.settled())
            || !self.page_vis.t.settled()
            || !self.page_vis.hl_y.settled()
            || !self.page_vis.hl_a.settled()
            || !self.page_vis.fhl_a.settled()
            || !self.page_vis.fhl_y.settled()
            || self.scroll.moving()
            || !self.tip_a.settled()
            || (self.tip_label.is_some() && self.tip_a.target == 0.0)
    }

    /// Pushes every animated property to the compositor (nothing is drawn).
    fn apply(&self) {
        let px = self.scale;
        let motion = crate::anim::motion_enabled();
        // Opening: the whole panel fades in, and each row drops into place a moment after the one above.
        let since = self.opened.elapsed().as_secs_f32();
        let (cx, cy) = (self.origin.0 * px, self.origin.1 * px);
        let _ = self.root.SetCenterPoint(v3(cx, cy));
        match self.closing {
            Some(t0) => {
                let t = (t0.elapsed().as_secs_f32() / CLOSE_S).clamp(0.0, 1.0);
                let e = t * t;
                let _ = self.root.SetOpacity(1.0 - e);
                let k = lerp(1.0, 0.96, e);
                let _ = self.root.SetScale(Vector3 { X: k, Y: k, Z: 1.0 });
            }
            None => {
                let t = if motion { (since / 0.16).clamp(0.0, 1.0) } else { 1.0 };
                let _ = self.root.SetOpacity(t * (2.0 - t));
                let g = if motion { ease_out_back((since / OPEN_S).clamp(0.0, 1.0), 1.1) } else { 1.0 };
                let k = lerp(0.9, 1.0, g);
                let _ = self.root.SetScale(Vector3 { X: k, Y: k, Z: 1.0 });
            }
        }

        // Moving to a page: the grid recedes and the page grows out of the tile.
        let pt = self.page_vis.t.value;
        let pe = pt.clamp(0.0, 1.0);
        let _ = self.grid.SetOpacity((1.0 - pe * 1.6).clamp(0.0, 1.0));
        let from = self.page_vis.from;
        let gk = 1.0 - 0.05 * pe;
        let _ = self.grid.SetCenterPoint(v3(from.cx() * px, from.cy() * px));
        let _ = self.grid.SetScale(Vector3 { X: gk, Y: gk, Z: 1.0 });

        for t in &self.tiles {
            let open = if motion && self.closing.is_none() {
                ease_out_back(((since - t.row as f32 * STAGGER_S) / OPEN_S).clamp(0.0, 1.0), 1.4)
            } else {
                1.0
            };
            let k = t.press.value * lerp(0.92, 1.0, open);
            let _ = t.glass.vis.SetScale(Vector3 { X: k, Y: k, Z: 1.0 });
            let _ = t.glass.vis.SetOffset(v3((t.rect.x * px).round(), ((t.rect.y + lerp(-10.0, 0.0, open)) * px).round()));
            let _ = t.glass.lit.SetOpacity(t.lit.value.clamp(0.0, 1.0) * self.pal.lit.a);
            if let Some((l, r)) = &t.knob {
                let on = t.on.value.clamp(0.0, 1.0);
                l.set_opacity(on);
                let _ = l.visual.SetCenterPoint(v3(r.w * px / 2.0, r.h * px / 2.0));
                let ks = lerp(0.8, 1.0, t.on.value.max(0.0));
                let _ = l.visual.SetScale(Vector3 { X: ks, Y: ks, Z: 1.0 });
            }
            if let Some((bar, track, s)) = &t.fill {
                let v = t.level.value.clamp(0.0, 1.0);
                let shown = self.level(*s).is_some();
                let w = if shown { (track.w * v).max(track.h) } else { 0.0 };
                bar.set(Rect::new(track.x * px, track.y * px, w * px, track.h * px), track.h * px / 2.0);
            }
        }

        // The page itself.
        let pv = &self.page_vis;
        let full = Rect::new(0.0, 0.0, W, self.h);
        let e = smooth(pe);
        let r = Rect::new(lerp(from.x, full.x, e), lerp(from.y, full.y, e), lerp(from.w, full.w, e), lerp(from.h, full.h, e));
        let from_radius = self.tiles.iter().find(|t| t.rect == from).map_or(RADIUS, |t| t.radius);
        pv.glass.set_size(r.w * px, r.h * px, lerp(from_radius, RADIUS, e) * px);
        let _ = pv.glass.vis.SetOffset(v3((r.x * px).round(), (r.y * px).round()));
        let _ = pv.glass.vis.SetOpacity((pt * 3.0).clamp(0.0, 1.0));
        let _ = pv.glass.vis.SetScale(Vector3 { X: 1.0, Y: 1.0, Z: 1.0 });
        let content = ((pt - 0.4) / 0.5).clamp(0.0, 1.0);
        // The content stays put at full size while the glass reveals it.
        let (ox, oy) = (-r.x * px, -r.y * px);
        pv.header.set_opacity(content);
        let _ = pv.header.visual.SetOffset(v3(ox.round(), oy.round()));
        let (view, foot_y) = self.page_frame();
        let _ = pv.view.SetOffset(v3((view.x * px + ox).round(), (view.y * px + oy).round()));
        let _ = pv.view.SetSize(v2(view.w * px, view.h * px));
        let _ = pv.view.SetOpacity(content);
        let off = self.scroll.offset_px(px);
        let _ = pv.scroller.SetOffset(v3(0.0, (-off * px).round()));
        pv.hl.set(Rect::new(10.0 * px, pv.hl_y.value * px, (W - 20.0) * px, pv.hl_h * px), 9.0 * px);
        let _ = pv.hl.vis.SetOpacity(pv.hl_a.value.clamp(0.0, 1.0));
        let _ = pv.footer.visual.SetOffset(v3(ox.round(), (foot_y * px + oy).round()));
        pv.footer.set_opacity(content);
        pv.fhl.set(Rect::new(10.0 * px + ox, (foot_y + pv.fhl_y.value) * px + oy, (W - 20.0) * px, LINK_H * px), 9.0 * px);
        let _ = pv.fhl.vis.SetOpacity(pv.fhl_a.value.clamp(0.0, 1.0) * content);

        // The tooltip.
        let a = self.tip_a.value.clamp(0.0, 1.0) * (1.0 - pe);
        self.tip.set_opacity(a);
        self.tip.set_offset(self.tip_at.x, self.tip_at.y + 4.0 * (1.0 - a));
    }

    /// The page's list area and the footer's top, in panel points.
    fn page_frame(&self) -> (Rect, f32) {
        let links = page_links(self.shown).len() as f32;
        let foot_h = links * LINK_H + 16.0;
        let foot_y = self.h - foot_h;
        (Rect::new(0.0, HEADER_H, W, foot_y - HEADER_H), foot_y)
    }

    // --- drawing ----------------------------------------------------------------

    /// Draws again whatever layer shows something that changed, then applies.
    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        let t0 = Instant::now();
        let mut draws = 0;
        // Targets the springs move to.
        for i in 0..self.tiles.len() {
            let m = self.tiles[i].m;
            let on = toggle_of(m).and_then(|t| self.on(t)) == Some(true);
            // The first frame shows things as they are; later changes animate.
            let first = self.tiles[i].key == 0;
            if first {
                self.tiles[i].on.snap(if on { 1.0 } else { 0.0 });
            } else {
                self.tiles[i].on.set(if on { 1.0 } else { 0.0 });
            }
            if let Some((_, _, s)) = self.tiles[i].fill {
                let v = self.level(s).unwrap_or(0.0);
                if first || self.drag.is_some_and(|(d, _)| d == s) {
                    self.tiles[i].level.snap(v);
                } else {
                    self.tiles[i].level.set(v);
                }
            }
            let key = self.tile_key(m);
            if key != self.tiles[i].key {
                self.tiles[i].key = key;
                let t = &self.tiles[i];
                let size = Rect::new(0.0, 0.0, t.rect.w, t.rect.h);
                t.base.draw(gfx, |p| self.draw_tile(p, m, size));
                if let Some((l, kr)) = &t.knob {
                    let local = Rect::new(0.0, 0.0, kr.w, kr.h);
                    l.draw(gfx, |p| self.draw_knob(p, m, local));
                }
                draws += 1;
            }
        }
        if self.page != Page::Main || !self.page_vis.t.settled() {
            draws += self.render_page(gfx, theme);
        }
        if draws > 0 {
            self.stats.draws += draws;
            self.stats.draw_us += t0.elapsed().as_micros() as u64;
        }
        self.apply();
    }

    /// What a tile shows, hashed: it's drawn again only when this changes.
    fn tile_key(&self, m: Module) -> u64 {
        let mut h = DefaultHasher::new();
        m.hash(&mut h);
        match m {
            Module::Wifi => {
                self.on(Toggle::Wifi).hash(&mut h);
                self.status.ssid.hash(&mut h);
                crate::screenshot_mode::on().hash(&mut h);
            }
            Module::Focus => self.on(Toggle::Focus).hash(&mut h),
            Module::Media => {
                crate::screenshot_mode::on().hash(&mut h);
                if let Some(n) = &self.now {
                    n.title.hash(&mut h);
                    n.artist.hash(&mut h);
                    n.playing.hash(&mut h);
                    n.art.as_ref().map(|a| a.id).hash(&mut h);
                }
            }
            Module::Display => self.brightness().is_some().hash(&mut h),
            Module::Sound => self.volume().map(|(_, m)| m).hash(&mut h),
            _ => {
                if let Some(t) = toggle_of(m) {
                    self.on(t).is_some().hash(&mut h);
                    if t == Toggle::ScreenshotMode {
                        self.on(t).hash(&mut h);
                    }
                }
            }
        }
        h.finish() | 1
    }

    /// A module's content (over its glass), in tile points.
    fn draw_tile(&self, p: &Painter, m: Module, r: Rect) {
        let pal = &self.pal;
        let title = p.gfx.format(&self.font, 13.0, Weight::SemiBold, Align::Leading);
        let sub = p.gfx.format(&self.font, 12.0, Weight::Regular, Align::Leading);
        let icon = |size: f32| p.gfx.format(ICONS, size, Weight::Regular, Align::Center);
        let avail = |t: Toggle| self.on(t).is_some();
        match m {
            Module::Wifi | Module::Focus => {
                let t = toggle_of(m).unwrap();
                let c = knob_rect(m, r);
                p.fill_circle(c.cx(), c.cy(), c.w / 2.0, pal.chip);
                let fg = if avail(t) { pal.text } else { pal.sub };
                p.text(&glyph_of(m).to_string(), &icon(17.0), c, fg);
                let tx = c.right() + 9.0;
                let tw = r.right() - tx - 12.0;
                let detail = match (m, self.on(t)) {
                    (Module::Wifi, None) => Some("Unavailable".to_string()),
                    (Module::Wifi, Some(false)) => Some("Off".to_string()),
                    (Module::Wifi, Some(true)) => Some(match &self.status.ssid {
                        Some(_) if crate::screenshot_mode::on() => crate::screenshot_mode::SSID.to_string(),
                        Some(s) => s.clone(),
                        None => "Not Connected".into(),
                    }),
                    (_, Some(true)) => Some("Do Not Disturb".to_string()),
                    _ => None,
                };
                match detail {
                    Some(d) => {
                        p.text(label_of(m), &title, Rect::new(tx, r.cy() - 17.0, tw, 17.0), pal.text);
                        p.text(&d, &sub, Rect::new(tx, r.cy() + 0.5, tw, 16.0), pal.sub);
                    }
                    None => p.text(label_of(m), &title, Rect::new(tx, r.y, tw, r.h), pal.text),
                }
            }
            Module::Media => self.draw_media(p, r),
            Module::Display | Module::Sound => {
                let s = if m == Module::Display { Slider::Brightness } else { Slider::Volume };
                p.text(label_of(m), &title, Rect::new(16.0, 9.0, r.w - 32.0, 20.0), pal.text);
                let track = slider_track(s, r.w);
                let cy = track.cy();
                p.fill_round(track, track.h / 2.0, pal.track);
                match s {
                    Slider::Brightness => {
                        p.text("\u{E706}", &icon(11.0), Rect::new(track.x - 22.0, cy - 9.0, 16.0, 18.0), pal.text);
                        p.text("\u{E706}", &icon(16.0), Rect::new(track.right() + 5.0, cy - 10.0, 20.0, 20.0), pal.text);
                        if self.brightness().is_none() {
                            let note = p.gfx.format(&self.font, 10.0, Weight::Regular, Align::Trailing);
                            p.text("Not available for this display", &note, Rect::new(16.0, 11.0, r.w - 32.0, 16.0), pal.sub);
                        }
                    }
                    Slider::Volume => {
                        let muted = self.volume().is_some_and(|(_, m)| m);
                        p.text(if muted { "\u{E74F}" } else { "\u{E993}" }, &icon(12.0), Rect::new(track.x - 23.0, cy - 9.0, 18.0, 18.0), pal.text);
                        p.text("\u{E995}", &icon(15.0), Rect::new(track.right() + 4.0, cy - 10.0, 22.0, 20.0), pal.text);
                        // The output picker, in AirPlay's spot.
                        let out = sound_output_rect(r.w);
                        p.fill_circle(out.cx(), out.cy(), out.w / 2.0, pal.chip);
                        p.text("\u{E7F5}", &icon(11.0), out, pal.text);
                    }
                }
            }
            Module::Edit => {
                let f = p.gfx.format(&self.font, 12.0, Weight::Regular, Align::Center);
                p.text("Edit Controls", &f, r, pal.text);
            }
            _ => {
                let fg = match toggle_of(m) {
                    Some(t) if !avail(t) => pal.sub.alpha(pal.sub.a * 0.6),
                    _ => pal.text,
                };
                p.text(&glyph_of(m).to_string(), &icon(21.0), r, fg);
            }
        }
    }

    /// A switch's "on" look: a filled disc with the glyph in the accent.
    fn draw_knob(&self, p: &Painter, m: Module, r: Rect) {
        p.fill_circle(r.cx(), r.cy(), r.w / 2.0, self.pal.on_bg);
        let size = if matches!(m, Module::Wifi | Module::Focus) { 17.0 } else { 21.0 };
        let f = p.gfx.format(ICONS, size, Weight::Regular, Align::Center);
        p.text(&glyph_of(m).to_string(), &f, r, self.pal.on_fg);
    }

    fn draw_media(&self, p: &Painter, r: Rect) {
        let pal = &self.pal;
        let title = p.gfx.format(&self.font, 13.0, Weight::SemiBold, Align::Leading);
        let sub = p.gfx.format(&self.font, 12.0, Weight::Regular, Align::Leading);
        let art = media_art_rect();
        let demo = self.now.as_ref().filter(|_| crate::screenshot_mode::on()).map(crate::screenshot_mode::now_playing);
        let now = demo.as_ref().or(self.now.as_ref()).filter(|n| !n.title.is_empty());
        let bmp = now.and_then(|n| n.art.as_ref()).and_then(|a| IconPixels { width: a.size, height: a.size, data: a.data.clone() }.to_bitmap(p.rt));
        match bmp {
            Some(b) => p.bitmap_cover(&b, art, 10.0, 1.0),
            None => {
                p.fill_round(art, 10.0, pal.chip);
                let f = p.gfx.format(ICONS, 20.0, Weight::Regular, Align::Center);
                p.text("\u{E8D6}", &f, art, pal.sub);
            }
        }
        let tw = r.w - 28.0;
        match now {
            Some(n) => {
                p.text(&n.title, &title, Rect::new(14.0, art.bottom() + 10.0, tw, 18.0), pal.text);
                p.text(&n.artist, &sub, Rect::new(14.0, art.bottom() + 28.0, tw, 16.0), pal.sub);
            }
            None => p.text("Not Playing", &title, Rect::new(14.0, art.bottom() + 14.0, tw, 18.0), pal.sub),
        }
        let playing = now.is_some_and(|n| n.playing);
        let c = if now.is_some() { pal.text } else { pal.sub.alpha(pal.sub.a * 0.6) };
        for (b, br) in media_buttons(r.w, r.h) {
            let (glyph, size) = match b {
                MediaBtn::Prev => ('\u{F8AC}', 17.0),
                MediaBtn::Next => ('\u{F8AD}', 17.0),
                _ => (if playing { '\u{F8AE}' } else { '\u{F5B0}' }, 22.0),
            };
            let f = p.gfx.format(ICONS, size, Weight::Regular, Align::Center);
            p.text(&glyph.to_string(), &f, br, c);
        }
    }

    // --- pages --------------------------------------------------------------------

    fn render_page(&mut self, gfx: &Gfx, _theme: &Theme) -> u32 {
        let page = self.shown;
        let items = self.page_items(page);
        let mut draws = 0;

        // Header: back, title and the page's switch.
        let switch = page_switch(page);
        let mut hk = DefaultHasher::new();
        (page, switch.map(|t| self.on(t))).hash(&mut hk);
        let hk = hk.finish() | 1;
        if hk != self.page_vis.keys.0 {
            self.page_vis.keys.0 = hk;
            self.page_vis.header.draw(gfx, |p| self.draw_header(p, page));
            draws += 1;
        }

        // The list, drawn whole: scrolling just moves it.
        let mut lk = DefaultHasher::new();
        page.hash(&mut lk);
        for it in &items {
            self.item_key(it, &mut lk);
        }
        let lk = lk.finish() | 1;
        if lk != self.page_vis.keys.1 {
            self.page_vis.keys.1 = lk;
            let mut rows = Vec::new();
            let mut y = 6.0;
            for it in &items {
                let h = it.height();
                if let Some(hit) = self.item_hit(it) {
                    rows.push((Rect::new(10.0, y, W - 20.0, h), hit));
                }
                y += h;
            }
            let content_h = y + 6.0;
            let pv = &mut self.page_vis;
            pv.list.resize(W, content_h);
            pv.rows = rows;
            pv.content_h = content_h;
            let pv = &self.page_vis;
            pv.list.draw(gfx, |p| {
                let mut y = 6.0;
                for it in &items {
                    let h = it.height();
                    self.draw_item(p, it, Rect::new(20.0, y, W - 40.0, h));
                    y += h;
                }
            });
            draws += 1;
            let (view, _) = self.page_frame();
            self.scroll.clamp((self.page_vis.content_h - view.h).max(0.0));
        }

        // Footer links.
        let fk = (page as u64 + 1) * 7919;
        if fk != self.page_vis.keys.2 {
            self.page_vis.keys.2 = fk;
            let links = page_links(page);
            let fh = links.len() as f32 * LINK_H + 16.0;
            self.page_vis.footer.resize(W, fh);
            self.page_vis.links = links.iter().enumerate().map(|(i, _)| (Rect::new(10.0, 8.0 + i as f32 * LINK_H, W - 20.0, LINK_H), Hit::Link(i))).collect();
            let pal = &self.pal;
            let font = self.font.clone();
            self.page_vis.footer.draw(gfx, |p| {
                p.line(20.0, 0.5, W - 20.0, 0.5, pal.sep, 1.0);
                let f = p.gfx.format(&font, 13.0, Weight::Regular, Align::Leading);
                for (i, (label, _)) in links.iter().enumerate() {
                    p.text(label, &f, Rect::new(22.0, 8.0 + i as f32 * LINK_H, W - 44.0, LINK_H), pal.text);
                }
            });
            draws += 1;
        }
        self.build_hits();
        draws
    }

    fn draw_header(&self, p: &Painter, page: Page) {
        let pal = &self.pal;
        let back = Rect::new(12.0, 10.0, 32.0, 32.0);
        p.fill_circle(back.cx(), back.cy(), 15.0, pal.chip);
        let chevron = p.gfx.format(ICONS, 12.0, Weight::Regular, Align::Center);
        p.text("\u{E76B}", &chevron, back, pal.text);
        let title = p.gfx.format(&self.font, 15.0, Weight::SemiBold, Align::Leading);
        p.text(page_title(page), &title, Rect::new(back.right() + 10.0, 10.0, W - 120.0, 32.0), pal.text);
        if let Some(t) = page_switch(page) {
            draw_switch(p, pal, self.on(t), header_switch_rect(), self.pal.on_bg);
        }
        p.line(20.0, HEADER_H - 0.5, W - 20.0, HEADER_H - 0.5, pal.sep, 1.0);
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
                            v.push(Item::Note(if crate::screenshot_mode::on() { crate::screenshot_mode::SSID.into() } else { s.clone() }));
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
                v.push(Item::Switch(Toggle::Dark, "Dark Mode", '\u{E793}'));
                v.push(Item::Switch(Toggle::NightLight, "Night Light", '\u{F08C}'));
            }
            Page::Sound => {
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

    fn item_key(&self, it: &Item, h: &mut DefaultHasher) {
        crate::screenshot_mode::on().hash(h);
        match it {
            Item::Section(s) => s.hash(h),
            Item::Note(s) => s.hash(h),
            Item::Network(i) => self.status.networks.as_ref().and_then(|l| l.get(*i)).map(|n| (&n.ssid, n.bars, n.secure, n.connected)).hash(h),
            Item::Device(i) => self.status.devices.as_ref().and_then(|l| l.get(*i)).map(|d| (&d.name, d.connected)).hash(h),
            Item::Output(i) => {
                let o = self.status.outputs.as_ref().and_then(|l| l.get(*i));
                o.map(|o| (&o.name, self.output_default(o))).hash(h)
            }
            Item::Switch(t, name, _) => (name, self.on(*t)).hash(h),
        }
    }

    fn output_default(&self, o: &system::Output) -> bool {
        match &self.local_output {
            Some((id, _)) => *id == o.id,
            None => o.default,
        }
    }

    fn item_hit(&self, it: &Item) -> Option<Hit> {
        match it {
            Item::Network(i) => Some(Hit::Network(*i)),
            Item::Device(i) => Some(Hit::Device(*i)),
            Item::Output(i) => Some(Hit::Output(*i)),
            Item::Switch(t, ..) => Some(Hit::Toggle(*t)),
            _ => None,
        }
    }

    fn draw_item(&self, p: &Painter, item: &Item, r: Rect) {
        let pal = &self.pal;
        let name_f = p.gfx.format(&self.font, 13.0, Weight::Regular, Align::Leading);
        let small = p.gfx.format(&self.font, 11.0, Weight::Regular, Align::Trailing);
        // Rows: an icon disc, the name, and a trailing detail.
        let row = |glyph: char, on: bool, name: &str, trailing: Option<&str>, check: bool| {
            let disc = Rect::new(r.x, r.cy() - 13.0, 26.0, 26.0);
            p.fill_circle(disc.cx(), disc.cy(), 13.0, if on { pal.on_bg } else { pal.chip });
            let g = p.gfx.format(ICONS, 12.0, Weight::Regular, Align::Center);
            p.text(&glyph.to_string(), &g, disc, if on { pal.on_fg } else { pal.text });
            let mut right = r.right();
            if check {
                let f = p.gfx.format(ICONS, 12.0, Weight::Regular, Align::Center);
                p.text("\u{E73E}", &f, Rect::new(right - 16.0, r.y, 16.0, r.h), pal.text);
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
                let f = p.gfx.format(&self.font, 11.0, Weight::SemiBold, Align::Leading);
                p.text(s, &f, Rect::new(r.x, r.y + 6.0, r.w, r.h - 6.0), pal.sub);
            }
            Item::Note(s) => {
                let f = p.gfx.format(&self.font, 12.0, Weight::Regular, Align::Leading);
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
                let name = if crate::screenshot_mode::on() { crate::screenshot_mode::network_name(*i, n.connected) } else { n.ssid.clone() };
                row(glyph, n.connected, &name, n.secure.then_some("Secured"), false);
            }
            Item::Device(i) => {
                let Some(d) = self.status.devices.as_ref().and_then(|l| l.get(*i)) else { return };
                let name = if crate::screenshot_mode::on() { crate::screenshot_mode::device_name(*i) } else { d.name.clone() };
                row(device_glyph(&name), d.connected, &name, d.connected.then_some("Connected"), false);
            }
            Item::Output(i) => {
                let Some(o) = self.status.outputs.as_ref().and_then(|l| l.get(*i)) else { return };
                let default = self.output_default(o);
                row(output_glyph(&o.name), default, &o.name, None, default);
            }
            Item::Switch(t, name, glyph) => {
                let on = self.on(*t);
                // The switch sits at the right; the name doesn't run under it.
                row(*glyph, on == Some(true), name, None, false);
                draw_switch(p, pal, on, Rect::new(r.right() - 38.0, r.cy() - 11.0, 38.0, 22.0), pal.on_bg);
            }
        }
    }

    // --- input ----------------------------------------------------------------------

    /// The hit regions for the current page, in panel points.
    fn build_hits(&mut self) {
        let mut hits = Vec::new();
        if self.page == Page::Main {
            for (i, t) in self.tiles.iter().enumerate() {
                let r = t.rect;
                let at = |x: f32, y: f32, w: f32, h: f32| Rect::new(r.x + x, r.y + y, w, h);
                match t.m {
                    Module::Wifi | Module::Focus => {
                        let page = if t.m == Module::Wifi { Page::Wifi } else { Page::Focus };
                        hits.push((r, Hit::Open(page), Some(i)));
                        let k = knob_rect(t.m, Rect::new(0.0, 0.0, r.w, r.h));
                        hits.push((at(k.x, k.y, k.w, k.h), Hit::Tile(t.m), Some(i)));
                    }
                    Module::Media => {
                        hits.push((r, Hit::Media(MediaBtn::Open), None));
                        for (b, br) in media_buttons(r.w, r.h) {
                            hits.push((at(br.x, br.y, br.w, br.h), Hit::Media(b), None));
                        }
                    }
                    Module::Display | Module::Sound => {
                        let (s, page) = if t.m == Module::Display { (Slider::Brightness, Page::Display) } else { (Slider::Volume, Page::Sound) };
                        hits.push((r, Hit::Open(page), Some(i)));
                        let tr = slider_track(s, r.w);
                        hits.push((at(tr.x - 8.0, tr.y - 14.0, tr.w + 16.0, tr.h + 28.0), Hit::Slider(s), None));
                        if s == Slider::Volume {
                            hits.push((at(tr.x - 26.0, tr.y - 10.0, 22.0, tr.h + 20.0), Hit::Mute, None));
                            let o = sound_output_rect(r.w);
                            hits.push((at(o.x - 3.0, o.y - 3.0, o.w + 6.0, o.h + 6.0), Hit::Open(Page::Sound), Some(i)));
                        }
                    }
                    Module::Accessibility => hits.push((r, Hit::Open(Page::Accessibility), Some(i))),
                    m => hits.push((r, Hit::Tile(m), Some(i))),
                }
            }
        } else {
            hits.push((Rect::new(8.0, 6.0, 44.0, 40.0), Hit::Back, None));
            if page_switch(self.shown).is_some() {
                let s = header_switch_rect();
                hits.push((s.inset(-6.0, -6.0), Hit::Toggle(page_switch(self.shown).unwrap()), None));
            }
            let (view, foot_y) = self.page_frame();
            let off = self.scroll.target();
            for (r, h) in &self.page_vis.rows {
                let y = view.y + r.y - off;
                let top = y.max(view.y);
                let bot = (y + r.h).min(view.bottom());
                if bot > top {
                    hits.push((Rect::new(r.x, top, r.w, bot - top), *h, None));
                }
            }
            for (r, h) in &self.page_vis.links {
                hits.push((r.offset(0.0, foot_y), *h, None));
            }
        }
        self.hits = hits;
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<(Hit, Option<usize>)> {
        self.hits.iter().rev().find(|(r, _, _)| r.contains(x, y)).map(|(_, h, i)| (*h, *i))
    }

    /// Whether a point (panel points) is on the panel rather than the empty space below it.
    pub fn in_body(&self, x: f32, y: f32) -> bool {
        let h = if self.page == Page::Main { self.grid_h } else { self.h };
        x >= 0.0 && x < W && y >= 0.0 && y < h
    }

    fn set_hover(&mut self, h: Option<Hit>, tile: Option<usize>) {
        if tile != self.hover_tile {
            if let Some(t) = self.hover_tile.and_then(|i| self.tiles.get_mut(i)) {
                t.lit.set(0.0);
            }
            if let Some(t) = tile.and_then(|i| self.tiles.get_mut(i)) {
                t.lit.set(1.0);
            }
            self.hover_tile = tile;
        }
        if h != self.hover {
            self.hover = h;
            self.hover_since = Instant::now();
            // Row and link highlights glide to the row under the pointer.
            let (view, _) = self.page_frame();
            let row = self.page_vis.rows.iter().find(|(_, hh)| Some(*hh) == h).map(|(r, _)| *r);
            let link = self.page_vis.links.iter().find(|(_, hh)| Some(*hh) == h).map(|(r, _)| *r);
            let pv = &mut self.page_vis;
            match row {
                Some(r) => {
                    if pv.hl_a.value < 0.05 {
                        pv.hl_y.snap(r.y);
                    } else {
                        pv.hl_y.set(r.y);
                    }
                    pv.hl_h = r.h;
                    pv.hl_a.set(1.0);
                }
                None => pv.hl_a.set(0.0),
            }
            match link {
                Some(r) => {
                    if pv.fhl_a.value < 0.05 {
                        pv.fhl_y.snap(r.y);
                    } else {
                        pv.fhl_y.set(r.y);
                    }
                    pv.fhl_a.set(1.0);
                }
                None => pv.fhl_a.set(0.0),
            }
            let _ = view;
        }
    }

    /// The tooltip for round controls (they have no label of their own, like macOS).
    fn update_tip(&mut self, gfx: Option<&Gfx>) {
        let tile = self.hover_tile.and_then(|i| self.tiles.get(i)).filter(|t| t.rect.w <= CELL + 0.5 && t.m != Module::Edit);
        let label = tile.map(|t| label_of(t.m));
        if label == self.tip_label {
            return;
        }
        self.tip_label = label;
        self.tip_a.set(0.0);
        let (Some(t), Some(label), Some(gfx)) = (tile, label, gfx) else { return };
        let f = gfx.format(&self.font, 11.0, Weight::Regular, Align::Center);
        let w = (gfx.measure(label, &f) + 18.0).min(160.0);
        let x = (t.rect.cx() - w / 2.0).clamp(2.0, W - w - 2.0);
        // Under the control, or above it on the last row.
        let below = t.rect.bottom() + 6.0;
        let y = if below + 24.0 > self.grid_h { t.rect.y - 28.0 } else { below };
        self.tip_at = Rect::new(x, y, w, 22.0);
        let (bg, fg) = (self.pal.tip_bg, self.pal.text);
        self.tip.draw(gfx, |p| {
            p.fill_round(Rect::new(0.5, 0.5, w - 1.0, 21.0), 11.0, bg);
            p.text(label, &f, Rect::new(0.0, 0.0, w, 22.0), fg);
        });
        self.stats.draws += 1;
    }

    pub fn on_move(&mut self, gfx: &Gfx, x: f32, y: f32) {
        if let Some((s, r)) = self.drag {
            self.slide_to(s, r, x);
            return;
        }
        let h = self.hit_at(x, y);
        self.set_hover(h.map(|h| h.0), h.and_then(|h| h.1));
        if self.page == Page::Main {
            self.update_tip(Some(gfx));
        }
        // Pressed and moved off: the tile springs back up.
        if let Some((ph, Some(i))) = self.pressed {
            let over = h.map(|h| h.0) == Some(ph);
            self.tiles[i].press.set(if over { 0.94 } else { 1.0 });
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        self.set_hover(None, None);
        self.update_tip(None);
    }

    /// A button went down: sliders start here, everything else waits for the release.
    pub fn on_down(&mut self, x: f32, y: f32) -> bool {
        let h = self.hit_at(x, y);
        self.pressed = h;
        self.tip_a.set(0.0);
        if let Some((Hit::Slider(s), _)) = h {
            if s == Slider::Brightness && self.brightness().is_none() {
                return false;
            }
            let tile = self.tiles.iter().find(|t| t.fill.as_ref().is_some_and(|f| f.2 == s));
            if let Some(t) = tile {
                let tr = slider_track(s, t.rect.w).offset(t.rect.x, t.rect.y);
                self.drag = Some((s, tr));
                self.slide_to(s, tr, x);
                return true;
            }
        }
        if let Some((_, Some(i))) = h {
            self.tiles[i].press.set(0.94);
        }
        self.dirty = true;
        false
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> ControlEvent {
        let pressed = self.pressed.take();
        for t in &mut self.tiles {
            t.press.set(1.0);
        }
        self.dirty = true;
        if let Some((s, _)) = self.drag.take() {
            match s {
                // Let the worker's next reading confirm it.
                Slider::Brightness => {
                    if let Some((v, _)) = self.local_brightness {
                        self.gen += 1;
                        self.local_brightness = Some((v, self.gen));
                        self.worker.send(Req::Brightness(v, self.gen));
                    }
                }
                Slider::Volume => {
                    if let Some(v) = self.local_volume.take() {
                        self.status.volume = Some(v);
                    }
                }
            }
            return ControlEvent::None;
        }
        let h = self.hit_at(x, y);
        match (h, pressed) {
            (Some(a), Some(b)) if a.0 == b.0 => self.activate(a.0, a.1),
            _ => ControlEvent::None,
        }
    }

    /// Right-click: a control with details opens its page (like a long press on macOS).
    pub fn on_right(&mut self, x: f32, y: f32) -> ControlEvent {
        let hit = self.hit_at(x, y);
        let open = |uri: &str| ControlEvent::Run(Action::Open(uri.into()));
        let m = match hit {
            Some((Hit::Tile(m), _)) => Some(m),
            Some((_, Some(i))) => Some(self.tiles[i].m),
            _ => None,
        };
        let page = match (m, hit.map(|h| h.0)) {
            (Some(Module::Wifi), _) => Page::Wifi,
            (Some(Module::Bluetooth), _) => Page::Bluetooth,
            (Some(Module::Focus), _) => Page::Focus,
            (Some(Module::NightLight | Module::Dark | Module::Display), _) | (_, Some(Hit::Slider(Slider::Brightness))) => Page::Display,
            (Some(Module::Sound), _) | (_, Some(Hit::Slider(Slider::Volume) | Hit::Mute)) => Page::Sound,
            (Some(Module::LiveCaptions | Module::Accessibility), _) => Page::Accessibility,
            (Some(Module::Nearby), _) => return open("ms-settings:crossdevice"),
            (Some(Module::Hotspot), _) => return open("ms-settings:network-mobilehotspot"),
            (Some(Module::Airplane), _) => return open("ms-settings:network-airplanemode"),
            (Some(Module::EnergySaver), _) => return open("ms-settings:batterysaver"),
            (Some(Module::Vpn), _) => return open("ms-settings:network-vpn"),
            _ => return ControlEvent::None,
        };
        if self.page == Page::Main {
            let from = hit.and_then(|h| h.1);
            self.open_page(page, from);
        }
        ControlEvent::None
    }

    pub fn on_wheel(&mut self, delta: i32) {
        if self.page != Page::Main {
            let (view, _) = self.page_frame();
            self.scroll.wheel(delta, 60.0, (self.page_vis.content_h - view.h).max(0.0));
            self.page_vis.hl_a.set(0.0);
            self.hover = None;
            self.dirty = true;
        }
    }

    /// Esc steps back from a page, then closes.
    pub fn on_key(&mut self, vk: u16) -> ControlEvent {
        match vk {
            0x1B | 0x25 if self.page != Page::Main => {
                self.open_page(Page::Main, None);
                self.build_hits();
                ControlEvent::None
            }
            0x1B => ControlEvent::Close,
            _ => ControlEvent::None,
        }
    }

    fn slide_to(&mut self, s: Slider, track: Rect, x: f32) {
        let v = ((x - track.x) / track.w).clamp(0.0, 1.0);
        match s {
            Slider::Volume => {
                self.local_volume = crate::island::sources::set_volume(v).or(Some((v, false)));
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

    fn flip(&mut self, t: Toggle) -> ControlEvent {
        let open = |uri: &str| ControlEvent::Run(Action::Open(uri.into()));
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
        if t == Toggle::ScreenshotMode {
            // Saved to the config; the App repaints everything (this panel too).
            let value = crate::config::ConfigValue::Bool(!now);
            return ControlEvent::Run(Action::SetConfig { table: "general", key: "screenshot_mode", value });
        }
        if t == Toggle::Dark {
            // Every surface is rebuilt in the new appearance; the App reopens this panel.
            self.local.insert(t, (!now, u64::MAX));
            return ControlEvent::Run(Action::SetDarkMode(!now));
        }
        self.set_toggle(t, !now);
        self.dirty = true;
        ControlEvent::None
    }

    fn activate(&mut self, h: Hit, tile: Option<usize>) -> ControlEvent {
        let open = |uri: &str| ControlEvent::Run(Action::Open(uri.into()));
        let keys = |sc: Shortcut| ControlEvent::Run(Action::Keys(sc));
        match h {
            Hit::Tile(m) => {
                if let Some(t) = toggle_of(m) {
                    return self.flip(t);
                }
                match m {
                    Module::Cast => keys(Shortcut::new("w", b'K' as u16, "K")),
                    Module::Project => keys(Shortcut::new("w", b'P' as u16, "P")),
                    Module::Calculator => ControlEvent::Run(Action::Launch { path: "calc.exe".into(), args: None }),
                    Module::Timer => open("ms-clock:"),
                    Module::Screenshot => keys(Shortcut::new("ws", b'S' as u16, "S")),
                    Module::Keyboard => keys(Shortcut::new("w", 0x20, "Space")),
                    Module::Vpn => open("ms-settings:network-vpn"),
                    Module::Edit => ControlEvent::Run(Action::OpenSettings(Some(crate::settings::Page::ControlCenter))),
                    _ => ControlEvent::None,
                }
            }
            Hit::Toggle(t) => {
                let e = self.flip(t);
                self.dirty = true;
                e
            }
            Hit::Open(p) => {
                self.open_page(p, tile);
                self.dirty = true;
                ControlEvent::None
            }
            Hit::Back => {
                self.open_page(Page::Main, None);
                self.build_hits();
                ControlEvent::None
            }
            Hit::Slider(_) => ControlEvent::None,
            Hit::Mute => {
                self.status.volume = crate::island::sources::press_volume(crate::island::sources::VolumeKey::Mute).or(self.status.volume);
                self.dirty = true;
                ControlEvent::None
            }
            Hit::Media(b) => {
                let Some(n) = self.now.as_mut().filter(|n| !n.title.is_empty()) else { return ControlEvent::None };
                match b {
                    MediaBtn::Prev => self.media.send(Cmd::Prev),
                    MediaBtn::Play => {
                        n.playing = !n.playing;
                        self.media.send(Cmd::TogglePlay);
                    }
                    MediaBtn::Next => self.media.send(Cmd::Next),
                    MediaBtn::Open => return ControlEvent::OpenMedia(n.app_id.clone()),
                }
                self.dirty = true;
                ControlEvent::None
            }
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
            Hit::Link(i) => match page_links(self.shown).get(i) {
                Some((_, uri)) => open(uri),
                None => ControlEvent::None,
            },
        }
    }
}

impl Drop for ControlCenter {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

// --- layout helpers ----------------------------------------------------------------

/// Packs modules into the grid, each in the first spot it fits:
/// (module, column, row, columns, rows) and the number of rows.
fn layout<'a>(modules: impl Iterator<Item = &'a ModuleInfo>) -> (Vec<(Module, usize, usize, usize, usize)>, usize) {
    let mut placed = Vec::new();
    let mut used: Vec<[bool; COLS]> = Vec::new();
    for i in modules {
        let (mut row, mut col) = (0, 0);
        loop {
            while used.len() < row + i.rows {
                used.push([false; COLS]);
            }
            if col + i.cols <= COLS && (row..row + i.rows).all(|r| (col..col + i.cols).all(|c| !used[r][c])) {
                break;
            }
            col += 1;
            if col + i.cols > COLS {
                col = 0;
                row += 1;
            }
        }
        for r in row..row + i.rows {
            for c in col..col + i.cols {
                used[r][c] = true;
            }
        }
        placed.push((i.module, col, row, i.cols, i.rows));
    }
    let rows = used.len();
    (placed, rows)
}

/// The round switch in a module, in tile points (the whole tile for a round control).
fn knob_rect(m: Module, r: Rect) -> Rect {
    match m {
        Module::Wifi | Module::Focus => {
            let d = r.h - 22.0;
            Rect::new(11.0, 11.0, d, d)
        }
        _ => Rect::new(0.0, 0.0, r.w, r.h),
    }
}

/// A slider module's track, in tile points.
fn slider_track(s: Slider, w: f32) -> Rect {
    let right = match s {
        Slider::Brightness => w - 40.0,
        Slider::Volume => w - 70.0,
    };
    Rect::new(40.0, 39.0, right - 40.0, 8.0)
}

fn sound_output_rect(w: f32) -> Rect {
    Rect::new(w - 38.0, 31.0, 24.0, 24.0)
}

fn media_art_rect() -> Rect {
    Rect::new(14.0, 14.0, 44.0, 44.0)
}

fn media_buttons(w: f32, h: f32) -> [(MediaBtn, Rect); 3] {
    let cy = h - 26.0;
    let b = |dx: f32| Rect::new(w / 2.0 + dx - 17.0, cy - 17.0, 34.0, 34.0);
    [(MediaBtn::Prev, b(-40.0)), (MediaBtn::Play, b(0.0)), (MediaBtn::Next, b(40.0))]
}

fn header_switch_rect() -> Rect {
    Rect::new(W - 58.0, 15.0, 38.0, 22.0)
}

fn page_switch(page: Page) -> Option<Toggle> {
    match page {
        Page::Wifi => Some(Toggle::Wifi),
        Page::Bluetooth => Some(Toggle::Bluetooth),
        Page::Focus => Some(Toggle::Focus),
        _ => None,
    }
}

fn page_title(page: Page) -> &'static str {
    match page {
        Page::Wifi => "Wi-Fi",
        Page::Bluetooth => "Bluetooth",
        Page::Focus => "Focus",
        Page::Display => "Display",
        Page::Sound => "Sound",
        Page::Accessibility => "Accessibility",
        Page::Main => "",
    }
}

fn page_links(page: Page) -> &'static [(&'static str, &'static str)] {
    match page {
        Page::Wifi => &[("Other Networks…", "ms-availablenetworks:"), ("Wi-Fi Settings…", "ms-settings:network-wifi")],
        Page::Bluetooth => &[("Add Device…", "ms-settings-connectabledevices:devicediscovery"), ("Bluetooth Settings…", "ms-settings:bluetooth")],
        Page::Focus => &[("Notifications Settings…", "ms-settings:notifications"), ("Focus Settings…", "ms-settings:focus")],
        Page::Display => &[("Display Settings…", "ms-settings:display")],
        Page::Sound => &[("Volume Mixer…", "ms-settings:apps-volume"), ("Sound Settings…", "ms-settings:sound")],
        Page::Accessibility => &[("Accessibility Settings…", "ms-settings:easeofaccess")],
        Page::Main => &[],
    }
}

/// A macOS switch.
fn draw_switch(p: &Painter, pal: &Pal, on: Option<bool>, r: Rect, accent: Color) {
    let k = if on == Some(true) { 1.0 } else { 0.0 };
    let bg = pal.track.mix(accent, k);
    let bg = if on.is_none() { bg.alpha(bg.a * 0.4) } else { bg };
    p.fill_round(r, r.h / 2.0, bg);
    let rad = r.h / 2.0 - 2.0;
    let cx = r.x + 2.0 + rad + (r.w - 4.0 - 2.0 * rad) * k;
    p.fill_circle(cx, r.cy() + 0.5, rad, Color::gray(0, 0.12));
    p.fill_circle(cx, r.cy(), rad, Color::WHITE);
}

fn ease_out_back(t: f32, overshoot: f32) -> f32 {
    let c3 = overshoot + 1.0;
    1.0 + c3 * (t - 1.0).powi(3) + overshoot * (t - 1.0).powi(2)
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

enum Item {
    Section(&'static str),
    Note(String),
    Network(usize),
    Device(usize),
    Output(usize),
    Switch(Toggle, &'static str, char),
}

impl Item {
    fn height(&self) -> f32 {
        match self {
            Item::Section(_) => 26.0,
            Item::Note(_) => 30.0,
            _ => ROW_H,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_grid_matches_macos() {
        // Wi-Fi and Now Playing on top, Bluetooth and Nearby under Wi-Fi, then Focus and two buttons.
        let (placed, _) = layout(MODULES.iter().take(7));
        let at: Vec<_> = placed.iter().map(|p| (p.0, p.1, p.2)).collect();
        assert_eq!(
            at,
            vec![
                (Module::Wifi, 0, 0),
                (Module::Media, 2, 0),
                (Module::Bluetooth, 0, 1),
                (Module::Nearby, 1, 1),
                (Module::Focus, 0, 2),
                (Module::Project, 2, 2),
                (Module::Cast, 3, 2),
            ]
        );
        // Display and Sound take whole rows under them.
        let (placed, rows) = layout(MODULES.iter().take(9));
        assert_eq!((placed[7].2, placed[8].2, rows), (3, 4, 5));
    }
}
