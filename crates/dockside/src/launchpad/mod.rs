//! Launchpad, as in macOS Sequoia: every app on full-screen pages of 7 × 5
//! over the blurred, darkened wallpaper, with page dots, a search field,
//! folders, and drag-to-arrange. (Tahoe's Apps panel lives in Spotlight.)
//!
//! It's one full-screen window whose content is a Windows.UI.Composition
//! visual tree (see `comp`): each page is drawn once on the GPU, and paging,
//! zooming, folders opening and the selection gliding are the compositor
//! moving, scaling and fading those layers, so they stay smooth at full screen.

pub(crate) mod comp;
pub mod categories;
pub mod layout;

use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::AtomicIsize;
use std::time::Instant;

use windows::core::{Interface, Result};
use windows::Foundation::Numerics::{Vector2, Vector3};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::DirectWrite::IDWriteTextFormat;
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{ContainerVisual, SpriteVisual};

use self::comp::{Gpu, Layer};
use self::layout::{Slot, State, COLS, PER_PAGE, ROWS};
use crate::anim::{lerp, Motion, Spring};
use crate::finder::loader::IconLoader;
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconPixels;
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::spotlight::index::{self, Index, Kind};
use crate::theme::Theme;
use crate::win::Monitor;

/// Lets go of what only an open Launchpad needs (see `comp::release`).
pub fn release() {
    comp::release();
}

/// The Launchpad window, so the window procedure can close it when it loses focus.
pub static LAUNCHPAD_HWND: AtomicIsize = AtomicIsize::new(0);

/// Every app Launchpad shows: (name, target). The Start menu's apps
/// (clutter like uninstallers and readmes left out) plus any added in
/// Settings, less any hidden.
pub fn apps(index: &Index) -> Vec<(String, String)> {
    let state = State::load();
    let hidden: std::collections::HashSet<String> = state.hidden.iter().map(|h| h.target.to_lowercase()).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(String, String)> = index
        .apps
        .iter()
        .filter(|a| a.kind == Kind::App && !categories::is_clutter(&a.name, &a.target))
        .map(|a| (a.name.clone(), a.target.clone()))
        .chain(state.added.iter().map(|a| (a.name.clone(), a.target.clone())))
        .filter(|(_, t)| !hidden.contains(&t.to_lowercase()) && seen.insert(t.to_lowercase()))
        .collect();
    out.sort_by_key(|(n, _)| n.to_lowercase());
    out
}

/// Hides an app from Launchpad (saved at once), and from an open one.
pub fn hide_app(name: &str, target: &str, open: Option<&mut Launchpad>) {
    match open {
        Some(l) => l.hide(name, target),
        None => {
            let mut s = State::load();
            s.hide(name, target);
            s.save();
        }
    }
}

/// Adds an app that isn't in the Start menu.
pub fn add_app(name: &str, target: &str) {
    let mut s = State::load();
    s.hidden.retain(|h| !h.target.eq_ignore_ascii_case(target));
    if !s.added.iter().any(|a| a.target.eq_ignore_ascii_case(target)) {
        s.added.push(layout::Named { name: name.to_string(), target: target.to_string() });
    }
    s.save();
}

/// Brings back a hidden app.
pub fn unhide_app(target: &str) {
    let mut s = State::load();
    s.hidden.retain(|h| h.target != target);
    s.save();
}

/// Takes out an app that was added.
pub fn remove_added(target: &str) {
    let mut s = State::load();
    s.added.retain(|a| a.target != target);
    s.save();
}

/// Icon pixels to ask the loader for, for an icon `px` device pixels across.
pub fn request_size(px: f32) -> i32 {
    if px <= 48.0 {
        48
    } else if px <= 128.0 {
        128
    } else {
        256
    }
}

/// An app icon fitted to `size` points, from `cache` or the loader.
pub fn icon(p: &Painter, loader: &mut IconLoader, cache: &mut HashMap<(String, u32), Option<ID2D1Bitmap>>, target: &str, size: f32, scale: f32) -> Option<ID2D1Bitmap> {
    let px = (size * scale).round() as u32;
    let k = (target.to_string(), px);
    if let Some(b) = cache.get(&k) {
        return b.clone();
    }
    let req = request_size(px as f32);
    let key = format!("lp{req}:{target}");
    let Some(pixels) = loader.get(&key, Path::new(target), false, req) else {
        // No icon at all (not just not loaded yet): remember that.
        if !loader.is_pending(&key) {
            cache.insert(k, None);
        }
        return None;
    };
    let full = pixels.to_bitmap(p.rt)?;
    let b = p.fitted(&full, size, size);
    cache.insert(k, b.clone());
    b
}

pub enum LaunchpadEvent {
    None,
    Close,
    Launch(String),
    /// Pick an app (or shortcut) to add, then `add_app` it.
    AddApp,
    /// Open Settings at the Launchpad page (hidden apps).
    Settings,
}

/// The edit-mode toolbar's buttons, left to right: (label, width).
/// A page dot's size, in points.
const DOT: f32 = 8.0;

const TOOLS: [(&str, f32); 5] = [("Add App\u{2026}", 96.0), ("Hide", 120.0), ("Select Page", 100.0), ("Hidden Apps\u{2026}", 118.0), ("Done", 72.0)];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    /// An edit-mode toolbar button (index into `TOOLS`).
    Tool(usize),
    Slot(usize),
    FolderApp(usize),
    Dot(usize),
    Search,
    FolderTitle,
    FolderSheet,
    Result(usize),
}

struct Folder {
    /// Page and index of the folder's slot.
    page: usize,
    index: usize,
    open: Spring,
    /// The name, while it's being edited.
    editing: Option<Vec<char>>,
    closing: bool,
    layer: Layer,
}

struct Press {
    x: f32,
    y: f32,
    hit: Option<Hit>,
    at: Instant,
    /// Held long enough to start edit mode: letting go isn't a click.
    held: bool,
}

struct Drag {
    slot: Slot,
    /// Where the pointer is, and where in the icon it grabbed it.
    x: f32,
    y: f32,
    gx: f32,
    gy: f32,
    /// The slot index the drag rests on, and since when (resting makes a folder).
    over: Option<(usize, Instant)>,
    /// Index the dragged icon would drop at on the current page.
    at: usize,
    /// Pointer at a screen edge (-1 / 1), since when: turns the page.
    edge: Option<(i32, Instant)>,
    /// Pointer on a page dot, since when: goes to that page.
    dot: Option<(usize, Instant)>,
    lift: Spring,
    layer: Layer,
    /// Where it was taken from (page, index); `None` out of a folder.
    origin: Option<(usize, usize)>,
    /// Over the Dock, which shows it in its gap instead (the page leaves its own gap be).
    over_dock: bool,
    /// Its layer has been drawn (it only changes if its icon arrives late).
    drawn: bool,
}

/// Room around an icon's cell in its sprite, for its edit-mode badge, its
/// picked glow, and a label that runs long.
const PAD: f32 = 16.0;

/// The folder-to-be tile is drawn this much bigger than an icon, at its largest.
const TILE: f32 = 1.2;

/// A page: a sprite per icon, drawn once and moved by the compositor.
struct PageVis {
    visual: ContainerVisual,
    slots: HashMap<u64, SlotVis>,
    /// The (layout, live) generation laid out.
    shows: (u64, u64),
}

struct SlotVis {
    layer: Layer,
    /// What it shows (see `slot_key`); 0 to draw it again.
    key: u64,
    /// An icon on it is still loading.
    waiting: bool,
    /// Where it was last put: (x, y, scale, shown).
    at: Option<(f32, f32, f32, bool)>,
}

/// Everything an icon's sprite shows, hashed (never 0): its app or folder,
/// pressed, and edit mode's badge.
fn slot_key(slot: &Slot, pressed: bool, picked: Option<&std::collections::HashSet<String>>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match slot {
        Slot::App { target } => {
            target.hash(&mut h);
            picked.map(|p| p.contains(target)).hash(&mut h);
        }
        Slot::Folder { name, apps } => {
            name.hash(&mut h);
            apps.iter().take(9).for_each(|a| a.hash(&mut h));
            picked.map(|p| !apps.is_empty() && apps.iter().all(|t| p.contains(t))).hash(&mut h);
        }
    }
    pressed.hash(&mut h);
    h.finish() | 1
}

/// What an open Launchpad cost, logged when it closes.
struct Stats {
    opened: Instant,
    frames: u32,
    renders: u32,
    /// Icon sprites drawn (an icon and its label each).
    icons: u32,
    /// Other layers redrawn (search, toolbar, dots, results, folder, dragged icon).
    others: u32,
    render_us: u64,
    max_us: u64,
}

struct Swipe {
    x0: f32,
    offset0: f32,
    last: (f32, Instant),
    velocity: f32,
}

/// Geometry of a page, in points.
#[derive(Clone, Copy)]
struct Metrics {
    icon: f32,
    pitch_x: f32,
    pitch_y: f32,
    left: f32,
    top: f32,
    label: f32,
}

impl Metrics {
    fn cell(&self, i: usize) -> Rect {
        let (c, r) = (i % COLS, i / COLS);
        Rect::new(self.left + c as f32 * self.pitch_x, self.top + r as f32 * self.pitch_y, self.pitch_x, self.pitch_y)
    }

    fn icon_of(&self, cell: Rect) -> Rect {
        Rect::new(cell.cx() - self.icon / 2.0, cell.y, self.icon, self.icon)
    }

    /// What clicking picks: the icon and its label.
    fn target_of(&self, cell: Rect) -> Rect {
        let ir = self.icon_of(cell);
        Rect::new(ir.x - 12.0, ir.y - 4.0, ir.w + 24.0, cell.h - 10.0)
    }

    fn grid_h(&self) -> f32 {
        self.top + ROWS as f32 * self.pitch_y + 4.0
    }
}

pub struct Launchpad {
    pub hwnd: HWND,
    gpu: Rc<Gpu>,
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    content: ContainerVisual,
    pages_visual: ContainerVisual,
    bg: Layer,
    /// Each page's layer and the (layout, live) generation it shows.
    /// The pages kept, by index.
    page_layers: HashMap<usize, PageVis>,
    /// The folder-to-be tile behind an icon a drag rests on.
    tile: Layer,
    search_layer: Layer,
    /// One page dot, drawn once; each dot is a sprite showing it, so paging
    /// only changes their opacity (no redrawing).
    dot: Layer,
    dots_visual: ContainerVisual,
    dot_sprites: Vec<SpriteVisual>,
    results_layer: Layer,
    sel_layer: Layer,
    dim: SpriteVisual,
    pub scale: f32,
    w: f32,
    h: f32,
    m: Metrics,
    state: State,
    names: HashMap<String, String>,
    icons: HashMap<(String, u32), Option<ID2D1Bitmap>>,
    page: usize,
    offset: Spring,
    appear: Spring,
    closing: bool,
    search: Vec<char>,
    results: Vec<String>,
    /// Pages (0) or search results (1), cross-fading.
    results_fade: Spring,
    selected: Option<usize>,
    sel: (Spring, Spring, Spring),
    folder: Option<Folder>,
    press: Option<Press>,
    drag: Option<Drag>,
    swipe: Option<Swipe>,
    /// Scrolling so far towards the next page turn, and when it last came.
    wheel: (f32, Instant),
    last_flip: Instant,
    motion: Motion<u64>,
    /// A dropped icon gliding from where it was let go (its id, cell origin).
    landing: Option<(u64, f32, f32)>,
    /// The tile growing behind an icon a drag rests on (a folder in the making).
    merge_grow: Spring,
    /// A folder just made or added to, settling with a bounce (its id).
    pop: Option<(u64, Spring)>,
    /// Bumped when what every page shows changes (layout, icons), and when
    /// only the current page does (a press, icons sliding aside for a drag).
    gen: u64,
    live: u64,
    /// The magnifier sliding from the middle of the field to its left as you type.
    search_slide: Spring,
    blink: Instant,
    caret_on: bool,
    /// Edit mode (like macOS's jiggling icons): click apps to select them,
    /// then hide them together; add apps; rearrange. Its toolbar takes the
    /// search field's place.
    editing: bool,
    picked: std::collections::HashSet<String>,
    tool_layer: Layer,
    edit_fade: Spring,
    /// What `apply` last sent to the compositor.
    applied: std::cell::Cell<Option<[f32; 12]>>,
    /// The search field, toolbar and dots need drawing.
    chrome_dirty: bool,
    /// Just the search field (typing, its caret, its magnifier sliding).
    search_dirty: bool,
    results_dirty: bool,
    /// The results, or the open folder, show an icon that's still loading.
    results_waiting: bool,
    folder_waiting: bool,
    folder_dirty: bool,
    /// The layout changed and wants saving when Launchpad closes.
    changed: bool,
    pub dirty: bool,
    pub tracking: bool,
    shown: bool,
    stats: Stats,
}

thread_local! {
    /// The blurred wallpaper per monitor rect: (rect, width, height, pixels).
    static BLURRED: std::cell::RefCell<Option<(RECT, u32, u32, Vec<u8>)>> = const { std::cell::RefCell::new(None) };
}

/// The wallpaper changed: the blur is redone next time.
pub fn wallpaper_changed() {
    BLURRED.with(|b| *b.borrow_mut() = None);
}

/// Shrinks `px` (w × h BGRA) by `k` with a box filter.
fn downsample(px: &[u8], w: usize, h: usize, k: usize) -> (Vec<f32>, usize, usize) {
    let (sw, sh) = ((w / k).max(1), (h / k).max(1));
    let mut out = vec![0f32; sw * sh * 3];
    for y in 0..sh {
        for x in 0..sw {
            let mut acc = [0f32; 3];
            let mut n = 0f32;
            for yy in (y * k)..((y + 1) * k).min(h) {
                let row = yy * w;
                for xx in (x * k)..((x + 1) * k).min(w) {
                    let i = (row + xx) * 4;
                    acc[0] += px[i] as f32;
                    acc[1] += px[i + 1] as f32;
                    acc[2] += px[i + 2] as f32;
                    n += 1.0;
                }
            }
            let o = (y * sw + x) * 3;
            for c in 0..3 {
                out[o + c] = acc[c] / n.max(1.0);
            }
        }
    }
    (out, sw, sh)
}

/// One horizontal then vertical box pass of radius `r` over a 3-channel image.
fn box_blur(img: &mut [f32], w: usize, h: usize, r: usize) {
    let mut tmp = vec![0f32; img.len()];
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 3];
            let (a, b) = (x.saturating_sub(r), (x + r).min(w - 1));
            for xx in a..=b {
                let i = (y * w + xx) * 3;
                acc[0] += img[i];
                acc[1] += img[i + 1];
                acc[2] += img[i + 2];
            }
            let n = (b - a + 1) as f32;
            let o = (y * w + x) * 3;
            tmp[o..o + 3].copy_from_slice(&[acc[0] / n, acc[1] / n, acc[2] / n]);
        }
    }
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 3];
            let (a, b) = (y.saturating_sub(r), (y + r).min(h - 1));
            for yy in a..=b {
                let i = (yy * w + x) * 3;
                acc[0] += tmp[i];
                acc[1] += tmp[i + 1];
                acc[2] += tmp[i + 2];
            }
            let n = (b - a + 1) as f32;
            let o = (y * w + x) * 3;
            img[o..o + 3].copy_from_slice(&[acc[0] / n, acc[1] / n, acc[2] / n]);
        }
    }
}

/// The wallpaper behind `rect`, heavily blurred and darkened like Launchpad's
/// backdrop, as a small image to stretch over the screen.
fn blurred_wallpaper(rect: RECT) -> Option<(u32, u32, Vec<u8>)> {
    if let Some(hit) = BLURRED.with(|b| b.borrow().as_ref().filter(|c| c.0 == rect).map(|c| (c.1, c.2, c.3.clone()))) {
        return Some(hit);
    }
    let (w, h) = ((rect.right - rect.left) as usize, (rect.bottom - rect.top) as usize);
    let px = crate::win::wallpaper::render(rect).map_err(|e| log!("launchpad wallpaper: {e}")).ok()?;
    let k = (w.max(h) / 320).max(1);
    let (mut img, sw, sh) = downsample(&px, w, h, k);
    for _ in 0..3 {
        box_blur(&mut img, sw, sh, 4);
    }
    // Darker and a little more saturated, like Launchpad's.
    let mut out = vec![0u8; sw * sh * 4];
    for i in 0..sw * sh {
        let (b, g, r) = (img[i * 3], img[i * 3 + 1], img[i * 3 + 2]);
        let l = 0.114 * b + 0.587 * g + 0.299 * r;
        let sat = |c: f32| ((l + (c - l) * 1.2) * 0.62).clamp(0.0, 255.0) as u8;
        out[i * 4] = sat(b);
        out[i * 4 + 1] = sat(g);
        out[i * 4 + 2] = sat(r);
        out[i * 4 + 3] = 255;
    }
    BLURRED.with(|b| *b.borrow_mut() = Some((rect, sw as u32, sh as u32, out.clone())));
    Some((sw as u32, sh as u32, out))
}

/// Text cut short with an ellipsis to fit `max_w`.
fn fit_text(p: &Painter, text: &str, font: &IDWriteTextFormat, max_w: f32) -> String {
    if p.gfx.measure(text, font) <= max_w {
        return text.to_string();
    }
    let mut s: String = text.to_string();
    while !s.is_empty() && p.gfx.measure(&format!("{s}\u{2026}"), font) > max_w {
        s.pop();
    }
    format!("{}\u{2026}", s.trim_end())
}

/// What pages and the folder sheet need to draw their icons.
struct Ink<'a> {
    m: Metrics,
    scale: f32,
    font: IDWriteTextFormat,
    names: &'a HashMap<String, String>,
    /// Edit mode's picked apps (`None` outside edit mode).
    picked: Option<&'a std::collections::HashSet<String>>,
    accent: Color,
    /// An icon drawn since this was last cleared is still loading: that
    /// layer wants drawing again when icons arrive.
    waiting: std::cell::Cell<bool>,
}

impl Ink<'_> {
    /// An app icon `size` points across, noting one that's still loading.
    fn icon(&self, p: &Painter, icons: &mut HashMap<(String, u32), Option<ID2D1Bitmap>>, loader: &mut IconLoader, target: &str, size: f32) -> Option<ID2D1Bitmap> {
        let b = icon(p, loader, icons, target, size, self.scale);
        if b.is_none() && !icons.contains_key(&(target.to_string(), (size * self.scale).round() as u32)) {
            self.waiting.set(true);
        }
        b
    }

    /// A folder: a frosted tile with its first nine apps in a 3 × 3 grid.
    fn folder(&self, p: &Painter, icons: &mut HashMap<(String, u32), Option<ID2D1Bitmap>>, loader: &mut IconLoader, apps: &[String], r: Rect) {
        let r = r.inset(r.w * 0.04, r.w * 0.04);
        p.fill_round(r, r.w * 0.24, Color::gray(255, 0.22));
        p.stroke_round(r.inset(0.25, 0.25), r.w * 0.24, Color::gray(255, 0.28), 0.5);
        let pad = r.w * 0.13;
        let gap = r.w * 0.055;
        let mini = (r.w - 2.0 * pad - 2.0 * gap) / 3.0;
        for (i, t) in apps.iter().take(9).enumerate() {
            let (c, row) = (i % 3, i / 3);
            let mr = Rect::new(r.x + pad + c as f32 * (mini + gap), r.y + pad + row as f32 * (mini + gap), mini, mini);
            if let Some(b) = self.icon(p, icons, loader, t, mini) {
                p.stamp(&b, mr.x, mr.y, self.scale);
            }
        }
    }

    /// Runs `f` and says whether anything it drew is still loading.
    fn waits(&self, f: impl FnOnce()) -> bool {
        self.waiting.set(false);
        f();
        self.waiting.get()
    }

    /// An app or folder with its icon at the top of `cell`; `pressed` darkens it.
    #[allow(clippy::too_many_arguments)]
    fn slot(&self, p: &Painter, icons: &mut HashMap<(String, u32), Option<ID2D1Bitmap>>, loader: &mut IconLoader, slot: &Slot, cell: Rect, label: bool, pressed: bool) {
        let m = self.m;
        let ir = m.icon_of(cell);
        let name = match slot {
            Slot::App { target } => {
                match self.icon(p, icons, loader, target, m.icon) {
                    Some(b) => p.stamp(&b, ir.x, ir.y, self.scale),
                    // Still loading: a faint tile where it'll be.
                    None => p.fill_round(ir.inset(m.icon * 0.06, m.icon * 0.06), m.icon * 0.2, Color::gray(255, 0.1)),
                }
                self.names.get(target).cloned().unwrap_or_default()
            }
            Slot::Folder { name, apps } => {
                self.folder(p, icons, loader, apps, ir);
                name.clone()
            }
        };
        if pressed {
            p.fill_round(ir.inset(m.icon * 0.05, m.icon * 0.05), m.icon * 0.21, Color::gray(0, 0.32));
        }
        // Edit mode: a badge on every app and folder, filled with a tick when picked.
        if let Some(picked) = self.picked {
            let on = match slot {
                Slot::App { target } => picked.contains(target),
                Slot::Folder { apps, .. } => !apps.is_empty() && apps.iter().all(|t| picked.contains(t)),
            };
            if on {
                p.fill_round(ir.inset(-4.0, -4.0), m.icon * 0.26, self.accent.alpha(0.28));
            }
            let br = (m.icon * 0.13).max(9.0);
            let (cx, cy) = (ir.x + br * 0.55, ir.y + br * 0.55);
            p.fill_circle(cx, cy + 1.0, br + 1.0, Color::gray(0, 0.25));
            if on {
                p.fill_circle(cx, cy, br, self.accent);
                p.line(cx - br * 0.45, cy, cx - br * 0.1, cy + br * 0.38, Color::WHITE, 2.0);
                p.line(cx - br * 0.1, cy + br * 0.38, cx + br * 0.5, cy - br * 0.35, Color::WHITE, 2.0);
            } else {
                p.fill_circle(cx, cy, br, Color::rgba(60, 60, 64, 0.92));
                p.stroke_circle(cx, cy, br - 0.5, Color::gray(255, 0.85), 1.4);
            }
        }
        if label {
            let max_w = cell.w - 10.0;
            let s = fit_text(p, &name, &self.font, max_w);
            let r = Rect::new(cell.x + 5.0, ir.bottom() + m.icon * 0.09, max_w, m.label * 1.6);
            p.text(&s, &self.font, r.offset(0.0, 1.0), Color::gray(0, 0.4));
            p.text(&s, &self.font, r, Color::WHITE);
        }
    }
}

impl Launchpad {
    pub fn new(gfx: &Gfx, monitor: &Monitor, index: &Index) -> Result<Self> {
        let gpu = comp::gpu(gfx)?;
        let rect = monitor.rect;
        let s = monitor.scale;
        let ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP;
        let hwnd = crate::win::create_window(ex, WS_POPUP, rect, HWND::default())?;
        crate::win::accent::prepare(hwnd);
        let interop: ICompositorDesktopInterop = gpu.compositor.cast()?;
        let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, true)? };
        let root = gpu.compositor.CreateContainerVisual()?;
        root.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        target.SetRoot(&root)?;

        let (w, h) = ((rect.right - rect.left) as f32 / s, (rect.bottom - rect.top) as f32 / s);
        let m = Self::metrics(w, h);
        let bg = Layer::new(&gpu, 8.0, 8.0, s)?;
        let content = gpu.compositor.CreateContainerVisual()?;
        content.SetSize(Vector2 { X: w * s, Y: h * s })?;
        content.SetCenterPoint(Vector3 { X: w * s / 2.0, Y: h * s / 2.0, Z: 0.0 })?;
        let pages_visual = gpu.compositor.CreateContainerVisual()?;
        let results_layer = Layer::new(&gpu, w, m.grid_h(), s)?;
        let search_layer = Layer::new(&gpu, 320.0, 44.0, s)?;
        let tool_layer = Layer::new(&gpu, 640.0, 48.0, s)?;
        tool_layer.set_opacity(0.0);
        let dot = Layer::new(&gpu, DOT, DOT, s)?;
        let tile = Layer::new(&gpu, m.icon * TILE, m.icon * TILE, s)?;
        tile.set_opacity(0.0);
        let dots_visual = gpu.compositor.CreateContainerVisual()?;
        let hl = m.icon + 30.0;
        let sel_layer = Layer::new(&gpu, hl, hl, s)?;
        sel_layer.set_opacity(0.0);
        let dim = gpu.compositor.CreateSpriteVisual()?;
        dim.SetBrush(&gpu.compositor.CreateColorBrushWithColor(windows::UI::Color { A: 255, R: 0, G: 0, B: 0 })?)?;
        dim.SetSize(Vector2 { X: w * s, Y: h * s })?;
        dim.SetOpacity(0.0)?;
        let kids = root.Children()?;
        kids.InsertAtTop(&bg.visual)?;
        kids.InsertAtTop(&content)?;
        kids.InsertAtTop(&dim)?;
        let c = content.Children()?;
        c.InsertAtTop(&pages_visual)?;
        // Behind every page's icons.
        pages_visual.Children()?.InsertAtBottom(&tile.visual)?;
        c.InsertAtTop(&results_layer.visual)?;
        c.InsertAtTop(&sel_layer.visual)?;
        c.InsertAtTop(&search_layer.visual)?;
        c.InsertAtTop(&tool_layer.visual)?;
        c.InsertAtTop(&dots_visual)?;
        root.SetOpacity(0.0)?;
        LAUNCHPAD_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::Relaxed);

        let apps = apps(index);
        let mut state = State::load();
        let before = state.pages.clone();
        state.reconcile(&apps);
        let changed = state.pages != before;
        let names = apps.into_iter().map(|(n, t)| (t, n)).collect();
        let glide = |v: f32| Spring::new(v, 0.16, 1.0);
        let mut lp = Self {
            hwnd,
            gpu,
            _target: target,
            root,
            content,
            pages_visual,
            bg,
            page_layers: HashMap::new(),
            tile,
            search_layer,
            dot,
            dots_visual,
            dot_sprites: Vec::new(),
            results_layer,
            sel_layer,
            dim,
            scale: s,
            w,
            h,
            m,
            state,
            names,
            icons: HashMap::new(),
            page: 0,
            offset: Spring::new(0.0, 0.42, 1.0),
            appear: Spring::new(0.0, 0.34, 1.0),
            closing: false,
            search: Vec::new(),
            results: Vec::new(),
            results_fade: Spring::new(0.0, 0.22, 1.0),
            selected: None,
            sel: (glide(0.0), glide(0.0), Spring::new(0.0, 0.12, 1.0)),
            folder: None,
            press: None,
            drag: None,
            swipe: None,
            wheel: (0.0, Instant::now()),
            last_flip: Instant::now() - std::time::Duration::from_secs(1),
            motion: Motion::default(),
            landing: None,
            merge_grow: Spring::new(0.0, 0.26, 0.8),
            pop: None,
            gen: 1,
            live: 0,
            search_slide: Spring::new(0.0, 0.3, 0.86),
            blink: Instant::now(),
            caret_on: true,
            applied: std::cell::Cell::new(None),
            editing: false,
            picked: std::collections::HashSet::new(),
            tool_layer,
            edit_fade: Spring::new(0.0, 0.22, 1.0),
            chrome_dirty: true,
            search_dirty: true,
            results_dirty: true,
            results_waiting: false,
            folder_waiting: false,
            folder_dirty: false,
            changed,
            dirty: true,
            tracking: false,
            shown: false,
            stats: Stats { opened: Instant::now(), frames: 0, renders: 0, icons: 0, others: 0, render_us: 0, max_us: 0 },
        };
        lp.draw_bg(gfx, rect);
        lp.dot.draw(gfx, |p| p.fill_circle(DOT / 2.0, DOT / 2.0, DOT / 2.0, Color::WHITE));
        let ts = m.icon * TILE;
        lp.tile.draw(gfx, |p| p.fill_round(Rect::new(0.0, 0.0, ts, ts), ts * 0.24, Color::gray(255, 0.3)));
        lp.sel_layer.draw(gfx, |p| p.fill_round(Rect::new(0.0, 0.0, hl, hl), 22.0, Color::gray(255, 0.17)));
        lp.appear.set(1.0);
        lp.apply();
        Ok(lp)
    }

    fn metrics(w: f32, h: f32) -> Metrics {
        let icon = (w * 0.058).min(h * 0.093).clamp(48.0, 128.0);
        let pitch_x = (w * 0.115).min(w * 0.88 / COLS as f32);
        let pitch_y = h * 0.151;
        let left = (w - pitch_x * COLS as f32) / 2.0;
        Metrics { icon, pitch_x, pitch_y, left, top: h * 0.10, label: (icon * 0.15).clamp(11.0, 14.0) }
    }

    fn draw_bg(&mut self, gfx: &Gfx, rect: RECT) {
        let (w, h) = (self.w, self.h);
        match blurred_wallpaper(rect) {
            Some((iw, ih, px)) => {
                let (lw, lh) = (iw as f32 / self.scale, ih as f32 / self.scale);
                self.bg.resize(lw, lh);
                self.bg.draw(gfx, |p| {
                    if let Some(b) = (IconPixels { width: iw, height: ih, data: px }).to_bitmap(p.rt) {
                        p.bitmap(&b, Rect::new(0.0, 0.0, lw, lh), 1.0);
                    }
                });
            }
            // A live or missing wallpaper: Launchpad's dark fallback.
            None => {
                self.bg.resize(64.0, 64.0);
                self.bg.draw(gfx, |p| p.fill_round_gradient(Rect::new(0.0, 0.0, 64.0, 64.0), 0.0, Color::rgba(46, 52, 70, 1.0), Color::rgba(22, 24, 34, 1.0)));
            }
        }
        // Stretched a touch past the edges, where the blur fades.
        self.bg.set_size(w + 24.0, h + 24.0);
        self.bg.set_offset(-12.0, -12.0);
    }

    pub fn focus(&self) {
        crate::win::apps::activate(self.hwnd);
    }

    pub fn raise(&self) {
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    pub fn closed(&self) -> bool {
        self.closing && self.appear.value <= 0.002
    }

    /// Starts zooming away; returns false if already closing.
    pub fn close(&mut self) -> bool {
        if self.closing {
            return false;
        }
        self.commit_folder_name();
        let st = &self.stats;
        log!(
            "launchpad: open {:.1}s, {} frames, {} renders ({} icons, {} other layers), render {:.1} ms total, {:.2} ms max",
            st.opened.elapsed().as_secs_f32(),
            st.frames,
            st.renders,
            st.icons,
            st.others,
            st.render_us as f32 / 1000.0,
            st.max_us as f32 / 1000.0
        );
        self.closing = true;
        self.appear.set(0.0);
        if self.changed {
            self.state.save();
            self.changed = false;
        }
        LAUNCHPAD_HWND.store(0, std::sync::atomic::Ordering::Relaxed);
        crate::win::apps::pass_focus_from(self.hwnd);
        true
    }

    pub fn animating(&self) -> bool {
        !self.appear.settled()
            || !self.offset.settled()
            || !self.results_fade.settled()
            || self.motion.moving()
            || !self.sel.0.settled()
            || !self.sel.1.settled()
            || !self.sel.2.settled()
            || self.folder.as_ref().is_some_and(|f| !f.open.settled())
            || self.drag.is_some()
            || !self.edit_fade.settled()
            || !self.merge_grow.settled()
            || self.pop.as_ref().is_some_and(|(_, s)| !s.settled())
            // A press held still becomes edit mode (watched for in `tick`).
            || self.press.as_ref().is_some_and(|p| !p.held && !self.editing)
    }

    /// The search caret's blink, on a slow timer (a running frame loop would
    /// keep the compositor busy for one blinking line). True while it blinks.
    pub fn blink(&mut self) -> bool {
        if !self.searching() {
            return false;
        }
        let on = (self.blink.elapsed().as_millis() / 530) % 2 == 0;
        if on != self.caret_on {
            self.caret_on = on;
            self.search_dirty = true;
            self.dirty = true;
        }
        true
    }

    pub fn tick(&mut self, dt: f32) -> bool {
        self.stats.frames += 1;
        let mut moving = self.appear.step(dt);
        moving |= self.offset.step(dt);
        moving |= self.results_fade.step(dt);
        moving |= self.edit_fade.step(dt);
        // The folder-to-be's tile grows in (and away) rather than popping.
        let merging = self.merging().is_some();
        if (self.merge_grow.target > 0.5) != merging {
            self.merge_grow.set(if merging { 1.0 } else { 0.0 });
        }
        if self.merge_grow.step(dt) {
            moving = true;
            self.live += 1;
        }
        if let Some((_, s)) = &mut self.pop {
            if s.step(dt) {
                moving = true;
                self.live += 1;
            } else {
                self.pop = None;
                self.live += 1;
            }
        }
        // Holding an icon still, like macOS, starts edit mode.
        if !self.editing && self.drag.is_none() && self.swipe.is_none() {
            if let Some(p) = &mut self.press {
                if matches!(p.hit, Some(Hit::Slot(_))) && !p.held && p.at.elapsed().as_secs_f32() > 0.7 {
                    // Still pressed: it can carry on into a drag.
                    p.held = true;
                    self.set_editing(true);
                }
            }
        }
        // The search field's magnifier slides aside as you type.
        if self.search_slide.step(dt) {
            moving = true;
            self.search_dirty = true;
        }
        moving |= self.sel.0.step(dt) | self.sel.1.step(dt) | self.sel.2.step(dt);
        if self.motion.tick(dt) {
            moving = true;
            // Icons sliding aside for a drag: the page redraws.
            self.live += 1;
        }
        if let Some(f) = &mut self.folder {
            moving |= f.open.step(dt);
            if f.closing && f.open.value <= 0.01 {
                let _ = self.root.Children().map(|c| c.Remove(&f.layer.visual));
                self.folder = None;
            }
        }
        // A drag resting at a screen edge turns the page; resting on an icon makes a folder.
        if let Some(d) = &mut self.drag {
            d.lift.step(dt);
            if let Some((dir, since)) = d.edge {
                if since.elapsed().as_secs_f32() > 0.45 {
                    d.edge = Some((dir, Instant::now()));
                    let to = self.page as i32 + dir;
                    if to >= 0 && to <= self.state.pages.len() as i32 {
                        self.go_page(to as usize);
                        self.live += 1;
                    }
                }
            }
            // Resting on a page dot goes to that page.
            if let Some((i, since)) = self.drag.as_ref().and_then(|d| d.dot) {
                if since.elapsed().as_secs_f32() > 0.35 && i != self.page {
                    self.go_page(i);
                    self.live += 1;
                }
            }
            // (Resting on an icon redraws only as its folder tile grows: see `merge_grow`.)
            moving = true;
        }
        self.apply();
        if moving {
            self.dirty = true;
        }
        moving
    }

    /// Pushes the animated values to the visual tree (cheap: no redrawing).
    fn apply(&self) {
        // Nothing moved: send nothing (every property set makes DWM compose again).
        let d = self.drag.as_ref();
        let key = [
            self.appear.value,
            self.offset.value,
            self.results_fade.value,
            self.sel.0.value,
            self.sel.1.value,
            self.sel.2.value,
            self.folder.as_ref().map_or(-1.0, |f| f.open.value),
            d.map_or(-1.0, |d| d.x),
            d.map_or(-1.0, |d| d.y),
            d.map_or(-1.0, |d| d.lift.value + 10.0 * d.over_dock as u8 as f32),
            self.page as f32,
            self.searching() as u8 as f32 + 2.0 * self.edit_fade.value,
        ];
        if self.applied.get() == Some(key) {
            return;
        }
        self.applied.set(Some(key));
        let s = self.scale;
        let a = self.appear.value.clamp(0.0, 1.0);
        let _ = self.root.SetOpacity(a);
        // Zooms in as it opens; zooms on past as it closes, like macOS.
        // An open folder: the page draws back behind its sheet, like macOS.
        let f = self.folder.as_ref().map_or(0.0, |f| f.open.value.clamp(0.0, 1.0));
        let zoom = lerp(1.1, 1.0, a) * lerp(1.0, 0.94, f);
        let _ = self.content.SetScale(Vector3 { X: zoom, Y: zoom, Z: 1.0 });
        let _ = self.content.SetOpacity(1.0 - 0.45 * f);
        let _ = self.pages_visual.SetOffset(Vector3 { X: (-self.offset.value * self.w * s).round(), Y: 0.0, Z: 0.0 });
        let r = self.results_fade.value.clamp(0.0, 1.0);
        let _ = self.pages_visual.SetOpacity(1.0 - r);
        self.results_layer.set_opacity(r);
        // Results settle in from a touch smaller; the pages draw back as they go.
        self.results_layer.set_scale(lerp(0.96, 1.0, r), self.w / 2.0, self.m.grid_h() / 2.0);
        let _ = self.pages_visual.SetScale(Vector3 { X: 1.0, Y: 1.0, Z: 1.0 });
        let _ = self.dots_visual.SetOpacity(1.0 - r);
        // A dot brightens as its page slides in.
        for (i, d) in self.dot_sprites.iter().enumerate() {
            let near = (1.0 - (self.offset.value - i as f32).abs()).clamp(0.0, 1.0);
            let _ = d.SetOpacity(0.3 + 0.65 * near);
        }
        // Edit mode: the toolbar fades in where the search field was.
        let e = self.edit_fade.value.clamp(0.0, 1.0);
        self.search_layer.set_opacity(1.0 - e);
        self.tool_layer.set_opacity(e);
        // The selection glides; on a page it slides with the page.
        let hl = self.m.icon + 30.0;
        let shift = if self.searching() { 0.0 } else { (self.page as f32 - self.offset.value) * self.w };
        self.sel_layer.set_offset(shift + self.sel.0.value - hl / 2.0, self.sel.1.value - 15.0);
        self.sel_layer.set_opacity(self.sel.2.value);
        // An open folder: everything else dims, the sheet zooms out of its icon.
        match (&self.folder, self.folder_sheet()) {
            (Some(f), Some((sr, ..))) => {
                let t = f.open.value.max(0.0);
                let _ = self.dim.SetOpacity(0.22 * t.min(1.0));
                let src = self.m.icon_of(self.m.cell(f.index));
                f.layer.set_offset(lerp(src.x, sr.x, t), lerp(src.y, sr.y, t));
                f.layer.set_scale(lerp(src.w / sr.w, 1.0, t), 0.0, 0.0);
                f.layer.set_opacity(t.min(1.0));
            }
            _ => {
                let _ = self.dim.SetOpacity(0.0);
            }
        }
        if let Some(d) = &self.drag {
            d.layer.set_offset(d.x - d.gx, d.y - d.gy);
            d.layer.set_scale(d.lift.value, d.gx, d.gy);
            d.layer.set_opacity(if d.over_dock { 0.0 } else { 1.0 });
        }
    }

    fn query(&self) -> String {
        self.search.iter().collect()
    }

    fn searching(&self) -> bool {
        !self.search.is_empty()
    }

    fn name(&self, target: &str) -> String {
        self.names.get(target).cloned().unwrap_or_else(|| Path::new(target).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
    }

    fn refilter(&mut self) {
        let q = self.query().trim().to_lowercase();
        self.results.clear();
        self.results_dirty = true;
        self.search_dirty = true;
        self.dirty = true;
        self.results_fade.set(if q.is_empty() { 0.0 } else { 1.0 });
        self.search_slide.set(if self.search.is_empty() { 0.0 } else { 1.0 });
        // The caret shows solid while typing, then blinks.
        self.blink = Instant::now();
        self.caret_on = true;
        if q.is_empty() {
            self.select(None);
            return;
        }
        let mut scored: Vec<(i32, String)> =
            self.names.iter().filter_map(|(t, n)| index::score_text(&n.to_lowercase(), &q).map(|s| (s, t.clone()))).collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
        self.results = scored.into_iter().take(PER_PAGE).map(|(_, t)| t).collect();
        self.select(if self.results.is_empty() { None } else { Some(0) });
    }

    /// Moves the keyboard selection: it glides there, or appears where it starts.
    fn select(&mut self, i: Option<usize>) {
        let was = self.selected;
        self.selected = i;
        match i {
            Some(i) => {
                let ir = self.m.icon_of(self.m.cell(i));
                if was.is_none() || self.sel.2.value < 0.05 {
                    self.sel.0.snap(ir.cx());
                    self.sel.1.snap(ir.y);
                } else {
                    self.sel.0.set(ir.cx());
                    self.sel.1.set(ir.y);
                }
                self.sel.2.set(1.0);
            }
            None => self.sel.2.set(0.0),
        }
        self.dirty = true;
    }

    fn go_page(&mut self, page: usize) {
        let last = self.state.pages.len().saturating_sub(1) + self.drag.is_some() as usize;
        let page = page.min(last);
        if page != self.page {
            self.last_flip = Instant::now();
            if self.selected.is_some() && !self.searching() {
                self.select(None);
            }
        }
        self.page = page;
        self.offset.set(page as f32);
        self.dirty = true;
    }

    // --- Geometry ----------------------------------------------------------

    fn search_rect(&self) -> Rect {
        Rect::new((self.w - 240.0) / 2.0, (self.h * 0.042).max(26.0), 240.0, 28.0)
    }

    fn dots_y(&self) -> f32 {
        self.h * 0.875
    }

    fn dot_rects(&self) -> Vec<Rect> {
        let n = self.state.pages.len() + self.drag.is_some() as usize;
        let (d, gap) = (DOT, 10.0);
        let total = n as f32 * d + (n.saturating_sub(1)) as f32 * gap;
        let x0 = (self.w - total) / 2.0;
        (0..n).map(|i| Rect::new(x0 + i as f32 * (d + gap), self.dots_y(), d, d)).collect()
    }

    /// The open folder's sheet: (rect, columns, grid origin inside it).
    fn folder_sheet(&self) -> Option<(Rect, usize, f32, f32)> {
        let f = self.folder.as_ref()?;
        let Some(Slot::Folder { apps, .. }) = self.state.pages.get(f.page)?.get(f.index) else { return None };
        let cols = apps.len().clamp(4, COLS);
        let rows = apps.len().div_ceil(cols).clamp(1, 4);
        let pad_x = self.m.pitch_x * 0.3;
        let title_h = 84.0;
        let w = cols as f32 * self.m.pitch_x + 2.0 * pad_x;
        let h = title_h + rows as f32 * self.m.pitch_y + 16.0;
        let r = Rect::new((self.w - w) / 2.0, (self.h * 0.45 - h / 2.0).max(40.0), w, h);
        Some((r, cols, pad_x, title_h))
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        if let (Some(f), Some((sr, cols, gx, gy))) = (&self.folder, self.folder_sheet()) {
            if f.closing || !sr.contains(x, y) {
                return None;
            }
            if Rect::new(sr.x, sr.y + 16.0, sr.w, 52.0).contains(x, y) {
                return Some(Hit::FolderTitle);
            }
            if let Some(Slot::Folder { apps, .. }) = self.state.pages.get(f.page).and_then(|p| p.get(f.index)) {
                for i in 0..apps.len().min(cols * 4) {
                    let cell = Rect::new(sr.x + gx + (i % cols) as f32 * self.m.pitch_x, sr.y + gy + (i / cols) as f32 * self.m.pitch_y, self.m.pitch_x, self.m.pitch_y);
                    if self.m.target_of(cell).contains(x, y) {
                        return Some(Hit::FolderApp(i));
                    }
                }
            }
            return Some(Hit::FolderSheet);
        }
        if self.editing {
            if let Some(i) = self.tool_rects().iter().position(|r| r.contains(x, y)) {
                return Some(Hit::Tool(i));
            }
        } else if self.search_rect().inset(-6.0, -6.0).contains(x, y) {
            return Some(Hit::Search);
        }
        if self.searching() {
            return (0..self.results.len()).find(|&i| self.m.target_of(self.m.cell(i)).contains(x, y)).map(Hit::Result);
        }
        if let Some(i) = self.dot_rects().iter().position(|d| d.inset(-6.0, -6.0).contains(x, y)) {
            return Some(Hit::Dot(i));
        }
        let n = self.state.pages.get(self.page).map_or(0, Vec::len);
        (0..n).find(|&i| self.m.target_of(self.m.cell(i)).contains(x, y)).map(Hit::Slot)
    }

    // --- Input -------------------------------------------------------------

    pub fn on_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        if let Some(name) = self.folder.as_mut().and_then(|f| f.editing.as_mut()) {
            name.push(c);
            self.folder_dirty = true;
            self.dirty = true;
            return;
        }
        if self.folder.is_some() {
            self.close_folder();
        }
        if self.editing {
            self.set_editing(false);
        }
        self.search.push(c);
        self.refilter();
    }

    pub fn on_key(&mut self, vk: u16, ctrl: bool) -> LaunchpadEvent {
        const BACK: u16 = 0x08;
        const ENTER: u16 = 0x0D;
        const ESC: u16 = 0x1B;
        const LEFT: u16 = 0x25;
        const UP: u16 = 0x26;
        const RIGHT: u16 = 0x27;
        const DOWN: u16 = 0x28;
        self.dirty = true;
        if let Some(f) = &mut self.folder {
            if let Some(name) = &mut f.editing {
                match vk {
                    BACK => {
                        name.pop();
                        self.folder_dirty = true;
                    }
                    ENTER | ESC => self.commit_folder_name(),
                    _ => {}
                }
                return LaunchpadEvent::None;
            }
            if vk == ESC {
                self.close_folder();
                return LaunchpadEvent::None;
            }
        }
        match vk {
            // Option (Alt) toggles edit mode, like holding Option on a Mac.
            0x12 if !self.searching() => self.set_editing(!self.editing),
            ESC if self.editing => self.set_editing(false),
            ESC if self.searching() => {
                self.search.clear();
                self.refilter();
            }
            ESC => return LaunchpadEvent::Close,
            BACK => {
                if ctrl {
                    self.search.clear();
                } else {
                    self.search.pop();
                }
                self.refilter();
            }
            ENTER => {
                if self.searching() {
                    if let Some(t) = self.selected.and_then(|i| self.results.get(i)) {
                        return LaunchpadEvent::Launch(t.clone());
                    }
                } else if let Some(i) = self.selected {
                    return self.activate(i);
                }
            }
            LEFT | RIGHT | UP | DOWN => {
                let count = if self.searching() { self.results.len() } else { self.state.pages.get(self.page).map_or(0, Vec::len) };
                if count == 0 {
                    return LaunchpadEvent::None;
                }
                let Some(i) = self.selected else {
                    self.select(Some(0));
                    return LaunchpadEvent::None;
                };
                let next = match vk {
                    LEFT => i as i32 - 1,
                    RIGHT => i as i32 + 1,
                    UP => i as i32 - COLS as i32,
                    _ => i as i32 + COLS as i32,
                };
                if !self.searching() && (next < 0 || next >= count as i32) && matches!(vk, LEFT | RIGHT) {
                    // Off the end of the page: onto the next one.
                    let to = self.page as i32 + if vk == LEFT { -1 } else { 1 };
                    if to >= 0 && (to as usize) < self.state.pages.len() {
                        self.go_page(to as usize);
                        let n = self.state.pages[self.page].len();
                        self.select(Some(if vk == LEFT { n - 1 } else { 0 }));
                    }
                } else {
                    self.select(Some(next.clamp(0, count as i32 - 1) as usize));
                }
            }
            _ => {}
        }
        LaunchpadEvent::None
    }

    /// Clicked (or Enter on) slot `i` of the current page.
    fn activate(&mut self, i: usize) -> LaunchpadEvent {
        match self.state.pages.get(self.page).and_then(|p| p.get(i)) {
            Some(Slot::App { target }) => LaunchpadEvent::Launch(target.clone()),
            Some(Slot::Folder { .. }) => {
                self.open_folder(i);
                LaunchpadEvent::None
            }
            None => LaunchpadEvent::None,
        }
    }

    fn open_folder(&mut self, i: usize) {
        let mut open = Spring::new(0.0, 0.34, 0.8);
        open.set(1.0);
        let Ok(layer) = Layer::new(&self.gpu, 8.0, 8.0, self.scale) else { return };
        layer.set_opacity(0.0);
        let _ = self.root.Children().map(|c| c.InsertAtTop(&layer.visual));
        self.folder = Some(Folder { page: self.page, index: i, open, editing: None, closing: false, layer });
        self.select(None);
        self.folder_dirty = true;
        self.dirty = true;
    }

    fn close_folder(&mut self) {
        self.commit_folder_name();
        if let Some(f) = &mut self.folder {
            f.closing = true;
            f.open.set(0.0);
        }
        self.dirty = true;
    }

    fn commit_folder_name(&mut self) {
        let Some(f) = &mut self.folder else { return };
        let Some(name) = f.editing.take() else { return };
        let name: String = name.into_iter().collect();
        if let Some(Slot::Folder { name: n, .. }) = self.state.pages.get_mut(f.page).and_then(|p| p.get_mut(f.index)) {
            let name = name.trim();
            if !name.is_empty() && name != n {
                *n = name.to_string();
                self.changed = true;
            }
        }
        self.gen += 1;
        self.folder_dirty = true;
        self.dirty = true;
    }

    pub fn on_down(&mut self, x: f32, y: f32) {
        let hit = self.hit_at(x, y);
        self.press = Some(Press { x, y, hit, at: Instant::now(), held: false });
        unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(self.hwnd);
        }
        // A pressed icon darkens.
        match hit {
            Some(Hit::Slot(_)) => self.live += 1,
            Some(Hit::Result(_)) => self.results_dirty = true,
            Some(Hit::FolderApp(_)) => self.folder_dirty = true,
            _ => {}
        }
        self.dirty = true;
    }

    /// Lets go of a press: whatever it darkened lightens again.
    fn release_press(&mut self) {
        if let Some(p) = self.press.take() {
            match p.hit {
                Some(Hit::Slot(_)) => self.live += 1,
                Some(Hit::Result(_)) => self.results_dirty = true,
                Some(Hit::FolderApp(_)) => self.folder_dirty = true,
                _ => {}
            }
            self.dirty = true;
        }
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        if let Some(d) = &mut self.drag {
            d.x = x;
            d.y = y;
            // Over the Dock, the page keeps its gap: nothing turns or merges.
            if !d.over_dock {
                self.update_drag();
            }
            self.apply();
            return;
        }
        if let Some(s) = &mut self.swipe {
            let now = Instant::now();
            let dt = (now - s.last.1).as_secs_f32().max(0.001);
            s.velocity = s.velocity * 0.5 + 0.5 * (x - s.last.0) / dt;
            s.last = (x, now);
            let max = self.state.pages.len().saturating_sub(1) as f32;
            let mut o = s.offset0 - (x - s.x0) / self.w;
            // Rubber band past the first and last pages.
            if o < 0.0 {
                o *= 0.3;
            } else if o > max {
                o = max + (o - max) * 0.3;
            }
            self.offset.snap(o);
            self.dirty = true;
            self.apply();
            return;
        }
        let Some(p) = &self.press else { return };
        let (dx, dy) = (x - p.x, y - p.y);
        if dx.hypot(dy) < 6.0 {
            return;
        }
        let can_swipe = !self.searching() && self.folder.is_none();
        let (px, py, hit) = (p.x, p.y, p.hit);
        match hit {
            // An icon moves with the pointer; empty space swipes the page.
            Some(Hit::Slot(i)) if !self.searching() => self.start_drag(i, false, px, py, x, y),
            Some(Hit::FolderApp(i)) => self.start_drag(i, true, px, py, x, y),
            Some(Hit::Search) | Some(Hit::FolderSheet) | Some(Hit::FolderTitle) | Some(Hit::Tool(_)) => {}
            _ if can_swipe => {
                self.release_press();
                self.swipe = Some(Swipe { x0: px, offset0: self.offset.value, last: (x, Instant::now()), velocity: 0.0 });
                self.on_move(x, y);
            }
            _ => {}
        }
    }

    fn start_drag(&mut self, i: usize, from_folder: bool, px: f32, py: f32, x: f32, y: f32) {
        let origin = (!from_folder).then_some((self.page, i));
        let slot = if from_folder {
            let Some(f) = &self.folder else { return };
            let (fp, fi) = (f.page, f.index);
            let Some(Slot::Folder { apps, .. }) = self.state.pages.get_mut(fp).and_then(|p| p.get_mut(fi)) else { return };
            if i >= apps.len() {
                return;
            }
            let t = apps.remove(i);
            if apps.is_empty() {
                self.state.pages[fp].remove(fi);
            }
            // Out of the folder: it closes at once and the drag carries on over the page.
            if let Some(f) = self.folder.take() {
                let _ = self.root.Children().map(|c| c.Remove(&f.layer.visual));
            }
            Slot::App { target: t }
        } else {
            let Some(page) = self.state.pages.get_mut(self.page) else { return };
            if i >= page.len() {
                return;
            }
            page.remove(i)
        };
        let ir = self.m.icon_of(self.m.cell(i));
        let (gx, gy) = if from_folder { (self.m.icon / 2.0, self.m.icon / 2.0) } else { (px - ir.x, py - ir.y) };
        let Ok(layer) = Layer::new(&self.gpu, self.m.icon, self.m.icon, self.scale) else { return };
        let _ = self.root.Children().map(|c| c.InsertAtTop(&layer.visual));
        let mut lift = Spring::new(1.0, 0.22, 0.7);
        lift.set(1.12);
        self.drag = Some(Drag { slot, x, y, gx, gy, over: None, at: i, edge: None, dot: None, lift, layer, origin, over_dock: false, drawn: false });
        self.press = None;
        self.select(None);
        self.changed = true;
        self.gen += 1;
        self.update_drag();
        self.dirty = true;
    }

    /// Works out where the dragged icon would land, as the pointer moves.
    fn update_drag(&mut self) {
        let n = self.state.pages.get(self.page).map_or(0, Vec::len);
        let (w, m) = (self.w, self.m);
        let dots = self.dot_rects();
        let Some(d) = &mut self.drag else { return };
        let (x, y) = (d.x, d.y);
        // On a page dot: that page, after a moment.
        let on_dot = dots.iter().position(|r| r.inset(-12.0, -14.0).contains(x, y));
        d.dot = match (on_dot, d.dot) {
            (Some(i), Some((o, t))) if o == i => Some((o, t)),
            (Some(i), _) => Some((i, Instant::now())),
            (None, _) => None,
        };
        // Screen edges (a generous band) turn the page.
        let band = (w * 0.05).max(60.0);
        let dir = if x < band { -1 } else if x > w - band { 1 } else { 0 };
        d.edge = match (dir, d.edge) {
            (0, _) => None,
            (dir, Some((old, t))) if old == dir => Some((old, t)),
            (dir, _) => Some((dir, Instant::now())),
        };
        let col = ((x - m.left) / m.pitch_x).floor().clamp(0.0, COLS as f32 - 1.0) as usize;
        let row = ((y - m.top) / m.pitch_y).floor().clamp(0.0, ROWS as f32 - 1.0) as usize;
        let pos = row * COLS + col;
        // What's shown at `pos`, with the gap open at `d.at`.
        let item = if pos > d.at { Some(pos - 1) } else if pos == d.at { None } else { Some(pos) };
        let item = item.filter(|&k| k < n);
        // Over the middle of an icon: a folder in the making.
        let cell = m.cell(pos);
        let core = m.icon_of(cell).inset(m.icon * 0.18, m.icon * 0.18);
        let on_icon = item.filter(|_| core.contains(x, y) && matches!(d.slot, Slot::App { .. }));
        d.over = match (on_icon, d.over) {
            (Some(i), Some((o, t))) if o == i => Some((o, t)),
            (Some(i), _) => Some((i, Instant::now())),
            (None, _) => None,
        };
        if on_icon.is_none() {
            // Where it would go: before or after what's under the pointer.
            let shown = if x > cell.cx() { pos + 1 } else { pos };
            let at = if shown > d.at { shown - 1 } else { shown }.min(n);
            if at != d.at {
                d.at = at;
                self.live += 1;
            }
        }
        self.dirty = true;
    }

    /// The app being dragged, as (name, target); not a folder.
    pub fn dragged_app(&self) -> Option<(String, String)> {
        match &self.drag.as_ref()?.slot {
            Slot::App { target } => Some((self.name(target), target.clone())),
            Slot::Folder { .. } => None,
        }
    }

    /// The dragged app is over the Dock (or has left it): the Dock draws it
    /// there, so the lifted icon here hides.
    pub fn set_over_dock(&mut self, over: bool) {
        let Some(d) = &mut self.drag else { return };
        if d.over_dock == over {
            return;
        }
        d.over_dock = over;
        if over {
            (d.over, d.edge, d.dot) = (None, None, None);
        }
        self.dirty = true;
        self.apply();
    }

    /// The dragged app was let go over the Dock, which keeps it: here it goes
    /// back where it came from (a folder's app stays where the page's gap is).
    pub fn drop_on_dock(&mut self) {
        unsafe {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
        }
        let Some(d) = self.drag.take() else { return };
        let _ = self.root.Children().map(|c| c.Remove(&d.layer.visual));
        let (page, at) = d.origin.unwrap_or((self.page.min(self.state.pages.len()), d.at));
        self.state.insert(page, at, d.slot);
        self.state.tidy();
        self.go_page(self.page.min(self.state.pages.len() - 1));
        self.gen += 1;
        self.dirty = true;
    }

    /// Whether the drag has rested on an icon long enough to make a folder with it.
    fn merging(&self) -> Option<usize> {
        let d = self.drag.as_ref()?;
        d.over.filter(|(_, t)| t.elapsed().as_secs_f32() >= 0.35).map(|(i, _)| i)
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> LaunchpadEvent {
        unsafe {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
        }
        self.dirty = true;
        if self.drag.is_some() {
            self.drop_drag();
            return LaunchpadEvent::None;
        }
        if let Some(s) = self.swipe.take() {
            let o = self.offset.value;
            let mut to = o.round();
            if s.velocity.abs() > 350.0 {
                to = if s.velocity < 0.0 { o.ceil() } else { o.floor() };
            }
            let last = self.state.pages.len().saturating_sub(1) as f32;
            self.go_page(to.clamp(0.0, last) as usize);
            self.press = None;
            return LaunchpadEvent::None;
        }
        let pressed = self.press.as_ref().map(|p| (p.hit, p.held));
        self.release_press();
        let Some((pressed, held)) = pressed else { return LaunchpadEvent::None };
        if held {
            return LaunchpadEvent::None;
        }
        let hit = self.hit_at(x, y);
        if hit != pressed {
            return LaunchpadEvent::None;
        }
        if self.editing {
            match hit {
                Some(Hit::Tool(t)) => return self.tool(t),
                Some(Hit::Slot(i)) => {
                    match self.state.pages.get(self.page).and_then(|p| p.get(i)).cloned() {
                        Some(Slot::App { target }) => self.toggle_pick(&[target]),
                        // A folder selects (or lets go of) all its apps.
                        Some(Slot::Folder { apps, .. }) => self.toggle_pick(&apps),
                        None => {}
                    }
                    return LaunchpadEvent::None;
                }
                Some(Hit::FolderApp(i)) => {
                    let t = self.folder.as_ref().and_then(|f| match self.state.pages.get(f.page)?.get(f.index)? {
                        Slot::Folder { apps, .. } => apps.get(i).cloned(),
                        _ => None,
                    });
                    if let Some(t) = t {
                        self.toggle_pick(&[t]);
                    }
                    return LaunchpadEvent::None;
                }
                // Empty space ends edit mode, like macOS.
                None if self.folder.is_none() => {
                    self.set_editing(false);
                    return LaunchpadEvent::None;
                }
                _ => {}
            }
        }
        match hit {
            Some(Hit::Tool(_)) => LaunchpadEvent::None,
            Some(Hit::Slot(i)) => self.activate(i),
            Some(Hit::Result(i)) => self.results.get(i).cloned().map_or(LaunchpadEvent::None, LaunchpadEvent::Launch),
            Some(Hit::FolderApp(i)) => {
                let f = self.folder.as_ref();
                match f.and_then(|f| self.state.pages.get(f.page)?.get(f.index)) {
                    Some(Slot::Folder { apps, .. }) => apps.get(i).cloned().map_or(LaunchpadEvent::None, LaunchpadEvent::Launch),
                    _ => LaunchpadEvent::None,
                }
            }
            Some(Hit::FolderTitle) => {
                let name = match self.folder.as_ref().and_then(|f| self.state.pages.get(f.page)?.get(f.index)) {
                    Some(Slot::Folder { name, .. }) => name.chars().collect(),
                    _ => Vec::new(),
                };
                if let Some(f) = &mut self.folder {
                    f.editing = Some(name);
                }
                self.folder_dirty = true;
                LaunchpadEvent::None
            }
            Some(Hit::FolderSheet) | Some(Hit::Search) => LaunchpadEvent::None,
            Some(Hit::Dot(i)) => {
                self.go_page(i);
                LaunchpadEvent::None
            }
            None if self.folder.is_some() => {
                self.close_folder();
                LaunchpadEvent::None
            }
            // Empty space closes Launchpad, like macOS.
            None => LaunchpadEvent::Close,
        }
    }

    fn drop_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        let _ = self.root.Children().map(|c| c.Remove(&d.layer.visual));
        let merge = d.over.filter(|(_, t)| t.elapsed().as_secs_f32() >= 0.35).map(|(i, _)| i);
        let page = self.page.min(self.state.pages.len());
        let target = merge.and_then(|i| self.state.pages.get(page).and_then(|p| p.get(i)).cloned().map(|s| (i, s)));
        match (target, d.slot) {
            (Some((i, Slot::App { target: other })), Slot::App { target: mine }) => {
                let mut s = Spring::new(0.72, 0.36, 0.55);
                s.set(1.0);
                self.pop = Some((Slot::Folder { name: String::new(), apps: vec![other.clone()] }.id(), s));
                // Two apps make a folder, named for what they are.
                let cat = categories::of(&self.name(&other), &other);
                let name = if cat == categories::of(&self.name(&mine), &mine) && cat != categories::Category::Other {
                    cat.name().split(" & ").next().unwrap_or("Folder").to_string()
                } else {
                    "Untitled Folder".to_string()
                };
                self.state.pages[page][i] = Slot::Folder { name, apps: vec![other, mine] };
            }
            (Some((i, Slot::Folder { .. })), Slot::App { target: mine }) => {
                if let Some(Slot::Folder { apps, .. }) = self.state.pages[page].get_mut(i) {
                    apps.push(mine);
                    let mut s = Spring::new(1.14, 0.3, 0.6);
                    s.set(1.0);
                    self.pop = Some((Slot::Folder { name: String::new(), apps: vec![apps[0].clone()] }.id(), s));
                }
            }
            (_, slot) => {
                // It glides from where it was let go into its place.
                let (ix, iy) = (d.x - d.gx, d.y - d.gy);
                self.landing = Some((slot.id(), ix + self.m.icon / 2.0 - self.m.pitch_x / 2.0, iy));
                self.state.insert(page, d.at, slot);
            }
        }
        self.state.tidy();
        self.go_page(self.page.min(self.state.pages.len() - 1));
        self.changed = true;
        self.gen += 1;
        self.dirty = true;
    }

    /// The wheel or a touchpad turns pages, the same way: a page per wheel
    /// notch's worth of scrolling, no faster than a page can slide in.
    pub fn on_wheel(&mut self, delta: i32) {
        if self.searching() || self.folder.is_some() || self.swipe.is_some() || delta == 0 {
            return;
        }
        let now = Instant::now();
        let (acc, last) = self.wheel;
        // A pause, or turning back, starts counting afresh.
        let mut acc = if (now - last).as_secs_f32() > 0.3 || (acc > 0.0) != (delta > 0) { 0.0 } else { acc };
        acc += delta as f32;
        if acc.abs() >= 120.0 && self.last_flip.elapsed().as_secs_f32() > 0.28 {
            let to = self.page as i32 + if acc > 0.0 { -1 } else { 1 };
            // Dragging an icon can go one past the last page, to start a new one.
            let last = self.state.pages.len() + self.drag.is_some() as usize;
            if to >= 0 && (to as usize) < last {
                self.go_page(to as usize);
                if self.drag.is_some() {
                    self.update_drag();
                    self.live += 1;
                }
            }
            acc = 0.0;
        }
        self.wheel = (acc, now);
    }

    /// The menu for a right-click at (x, y): what's under the pointer, in
    /// screen points, or `None` over empty space.
    pub fn context_menu(&mut self, x: f32, y: f32) -> Option<(Vec<crate::menu::Entry>, f32, f32)> {
        use crate::menu::{Action, Entry};
        let s = self.scale;
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        let at = (rc.left as f32 / s + x, rc.top as f32 / s + y);
        let Some(hit) = self.hit_at(x, y) else {
            // Empty space: edit mode and adding apps.
            let v = vec![
                Entry::item(if self.editing { "Done Editing" } else { "Edit Apps" }, Action::LaunchpadEdit(!self.editing)),
                Entry::item("Add App\u{2026}", Action::LaunchpadAddHere),
            ];
            return Some((v, at.0, at.1));
        };
        let app = |name: String, target: String| {
            let mut v = vec![Entry::item("Open", Action::Launch { path: target.clone(), args: None })];
            // A file on disk can be shown in Explorer; a Store app's id can't.
            if !target.to_lowercase().starts_with("shell:") {
                v.push(Entry::item("Show in File Explorer", Action::Reveal(target.clone())));
            }
            v.push(Entry::Separator);
            v.push(Entry::item("Hide from Launchpad", Action::LaunchpadHide { name, target: target.clone() }));
            v.push(Entry::item("Select Apps to Hide\u{2026}", Action::LaunchpadSelect(target)));
            v
        };
        let entries = match hit {
            Hit::Slot(i) => match self.state.pages.get(self.page)?.get(i)? {
                Slot::App { target } => app(self.name(target), target.clone()),
                Slot::Folder { .. } => {
                    self.select(None);
                    vec![Entry::item("Rename Folder", Action::LaunchpadRenameFolder(i))]
                }
            },
            Hit::Result(i) => {
                let t = self.results.get(i)?.clone();
                app(self.name(&t), t)
            }
            Hit::FolderApp(i) => {
                let f = self.folder.as_ref()?;
                let Some(Slot::Folder { apps, .. }) = self.state.pages.get(f.page)?.get(f.index) else { return None };
                let t = apps.get(i)?.clone();
                app(self.name(&t), t)
            }
            _ => return None,
        };
        Some((entries, at.0, at.1))
    }

    /// Turns edit mode on or off (selections go with it).
    pub fn set_editing(&mut self, on: bool) {
        if on == self.editing {
            return;
        }
        self.editing = on;
        self.picked.clear();
        self.edit_fade.set(if on { 1.0 } else { 0.0 });
        if on {
            if self.searching() {
                self.search.clear();
                self.refilter();
            }
            self.select(None);
        }
        self.gen += 1;
        self.folder_dirty = true;
        self.chrome_dirty = true;
        self.dirty = true;
    }

    /// Edit mode with `target` already picked (from its right-click menu).
    pub fn select_for_hiding(&mut self, target: &str) {
        self.set_editing(true);
        self.picked.insert(target.to_string());
        self.gen += 1;
        self.chrome_dirty = true;
        self.dirty = true;
    }

    /// Picks these apps, or lets go of them if they're all picked already.
    fn toggle_pick(&mut self, targets: &[String]) {
        if targets.iter().all(|t| self.picked.contains(t)) {
            for t in targets {
                self.picked.remove(t);
            }
        } else {
            self.picked.extend(targets.iter().cloned());
        }
        // Only this page (or its open folder) shows what was picked.
        self.live += 1;
        self.folder_dirty = true;
        self.chrome_dirty = true;
        self.dirty = true;
    }

    /// The toolbar buttons' rects (window points), left to right.
    fn tool_rects(&self) -> Vec<Rect> {
        let gap = 8.0;
        let total: f32 = TOOLS.iter().map(|t| t.1).sum::<f32>() + gap * (TOOLS.len() - 1) as f32;
        let r = self.search_rect();
        let mut x = (self.w - total) / 2.0;
        TOOLS
            .iter()
            .map(|t| {
                let b = Rect::new(x, r.y - 2.0, t.1, 32.0);
                x += t.1 + gap;
                b
            })
            .collect()
    }

    fn tool(&mut self, i: usize) -> LaunchpadEvent {
        match i {
            0 => LaunchpadEvent::AddApp,
            1 => {
                self.hide_picked();
                LaunchpadEvent::None
            }
            2 => {
                // Everything on this page (folders' apps included).
                let mut all = Vec::new();
                for slot in self.state.pages.get(self.page).cloned().unwrap_or_default() {
                    match slot {
                        Slot::App { target } => all.push(target),
                        Slot::Folder { apps, .. } => all.extend(apps),
                    }
                }
                self.toggle_pick(&all);
                LaunchpadEvent::None
            }
            3 => LaunchpadEvent::Settings,
            _ => {
                self.set_editing(false);
                LaunchpadEvent::None
            }
        }
    }

    /// Hides every picked app at once.
    fn hide_picked(&mut self) {
        let picked: Vec<String> = self.picked.drain().collect();
        if picked.is_empty() {
            return;
        }
        for t in &picked {
            let name = self.name(t);
            self.state.hide(&name, t);
            self.names.remove(t);
        }
        self.state.save();
        self.changed = false;
        self.after_removal();
    }

    /// Adds an app to the open Launchpad (on its last page, which comes into view).
    pub fn add_app(&mut self, name: &str, target: &str) {
        self.state.hidden.retain(|h| !h.target.eq_ignore_ascii_case(target));
        if !self.state.added.iter().any(|a| a.target.eq_ignore_ascii_case(target)) {
            self.state.added.push(layout::Named { name: name.to_string(), target: target.to_string() });
        }
        if !self.names.contains_key(target) {
            self.names.insert(target.to_string(), name.to_string());
            self.state.push(Slot::app(target));
        }
        self.state.save();
        self.changed = false;
        self.go_page(self.state.pages.len().saturating_sub(1));
        self.gen += 1;
        self.chrome_dirty = true;
        self.dirty = true;
    }

    /// After apps leave: pages, the open folder and results catch up.
    fn after_removal(&mut self) {
        let names = &self.names;
        self.results.retain(|t| names.contains_key(t));
        self.page = self.page.min(self.state.pages.len().saturating_sub(1));
        self.offset.set(self.page as f32);
        if self.folder.as_ref().is_some_and(|f| !matches!(self.state.pages.get(f.page).and_then(|p| p.get(f.index)), Some(Slot::Folder { .. }))) {
            if let Some(f) = self.folder.take() {
                let _ = self.root.Children().map(|c| c.Remove(&f.layer.visual));
            }
        }
        self.gen += 1;
        self.results_dirty = true;
        self.folder_dirty = true;
        self.chrome_dirty = true;
        self.dirty = true;
    }

    /// Takes an app out of the open Launchpad (and saves that).
    pub fn hide(&mut self, name: &str, target: &str) {
        self.state.hide(name, target);
        self.state.save();
        self.changed = false;
        self.names.remove(target);
        self.picked.remove(target);
        self.after_removal();
    }

    /// Opens folder `i` of the current page with its name ready to edit.
    pub fn rename_folder(&mut self, i: usize) {
        if !matches!(self.state.pages.get(self.page).and_then(|p| p.get(i)), Some(Slot::Folder { .. })) {
            return;
        }
        if self.folder.is_none() {
            self.open_folder(i);
        }
        let name = match self.state.pages.get(self.page).and_then(|p| p.get(i)) {
            Some(Slot::Folder { name, .. }) => name.chars().collect(),
            _ => Vec::new(),
        };
        if let Some(f) = &mut self.folder {
            f.editing = Some(name);
        }
        self.folder_dirty = true;
        self.dirty = true;
    }

    /// Icons finished loading: only layers still showing a placeholder redraw
    /// (icons for Finder and Spotlight arrive here too).
    pub fn icons_arrived(&mut self) {
        let mut any = false;
        for pv in self.page_layers.values_mut() {
            for sv in pv.slots.values_mut().filter(|sv| sv.waiting) {
                sv.key = 0;
                pv.shows = (u64::MAX, u64::MAX);
                any = true;
            }
        }
        if self.results_waiting {
            self.results_dirty = true;
            any = true;
        }
        if self.folder_waiting {
            self.folder_dirty = true;
            any = true;
        }
        any |= self.drag.as_ref().is_some_and(|d| !d.drawn);
        self.dirty |= any;
    }

    // --- Drawing -----------------------------------------------------------

    /// Redraws the layers whose content changed. Movement needs no drawing (see `apply`).
    pub fn render(&mut self, gfx: &Gfx, theme: &Theme, loader: &mut IconLoader) {
        let t0 = Instant::now();
        self.render_layers(gfx, theme, loader);
        let us = t0.elapsed().as_micros() as u64;
        self.stats.renders += 1;
        self.stats.render_us += us;
        self.stats.max_us = self.stats.max_us.max(us);
    }

    fn render_layers(&mut self, gfx: &Gfx, theme: &Theme, loader: &mut IconLoader) {
        self.dirty = false;
        self.place_dots();
        let (m, s, w) = (self.m, self.scale, self.w);
        let ink = Ink {
            m,
            scale: s,
            font: gfx.format(&theme.font, m.label, Weight::SemiBold, Align::Center),
            names: &self.names,
            picked: self.editing.then_some(&self.picked),
            accent: theme.accent,
            waiting: std::cell::Cell::new(false),
        };
        let pressed = self.press.as_ref().and_then(|p| p.hit);
        let merging = self.merging();
        let drag_over = self.drag.as_ref().and_then(|d| d.over.map(|o| o.0));
        let drag_at = self.drag.as_ref().map(|d| d.at);

        // Pages next to the one in view. Each icon (with its label) is a sprite
        // drawn once: sliding aside for a drag, landing and popping are the
        // compositor moving and scaling it, and a press redraws just that icon.
        let last = self.state.pages.len().saturating_sub(1) + self.drag.is_some() as usize;
        // Only the pages either side of this one are kept (each holds a
        // screen's worth of GPU surfaces).
        let (lo, hi) = (self.page.saturating_sub(1), (self.page + 1).min(last));
        let far: Vec<usize> = self.page_layers.keys().copied().filter(|&p| p < lo || p > hi).collect();
        for p in far {
            if let Some(pv) = self.page_layers.remove(&p) {
                let _ = self.pages_visual.Children().map(|c| c.Remove(&pv.visual));
            }
        }
        let mut tile: Option<(f32, f32)> = None;
        let mut tile_seen = false;
        for pi in lo..=hi {
            let current = pi == self.page;
            let want = (self.gen, if current { self.live } else { 0 });
            if self.page_layers.get(&pi).is_some_and(|pv| pv.shows == want) {
                continue;
            }
            if !self.page_layers.contains_key(&pi) {
                let Ok(visual) = self.gpu.compositor.CreateContainerVisual() else { continue };
                let _ = visual.SetOffset(Vector3 { X: (pi as f32 * w * s).round(), Y: 0.0, Z: 0.0 });
                let _ = self.pages_visual.Children().map(|c| c.InsertAtTop(&visual));
                self.page_layers.insert(pi, PageVis { visual, slots: HashMap::new(), shows: (0, 0) });
            }
            let slots: Vec<Slot> = self.state.pages.get(pi).cloned().unwrap_or_default();
            // Springs are only forgotten when the page they move is laid out:
            // other redraws (the search field, the dots) don't touch them, and
            // forgetting them there made icons snap instead of sliding.
            if current {
                self.motion.sweep();
            }
            let cells: Vec<Rect> = slots
                .iter()
                .enumerate()
                .map(|(i, slot)| {
                    let pos = match drag_at {
                        Some(at) if current && i >= at => i + 1,
                        _ => i,
                    };
                    let c = m.cell(pos.min(PER_PAGE - 1));
                    if !current {
                        return c;
                    }
                    let id = slot.id();
                    // A dropped icon starts where it was let go.
                    if let Some((_, lx, ly)) = self.landing.filter(|l| l.0 == id) {
                        // Made there (or put there), then sent on to its cell below.
                        self.motion.get(id, lx, crate::anim::feel::SLIDE);
                        self.motion.get(id ^ 1, ly, crate::anim::feel::SLIDE);
                        self.motion.snap(id, lx);
                        self.motion.snap(id ^ 1, ly);
                    }
                    let x = self.motion.get(id, c.x, crate::anim::feel::SLIDE);
                    let y = self.motion.get(id ^ 1, c.y, crate::anim::feel::SLIDE);
                    Rect::new(x, y, c.w, c.h)
                })
                .collect();
            if current {
                self.landing = None;
                tile_seen = true;
            }
            let grow = self.merge_grow.value.clamp(0.0, 1.2);
            let pop = self.pop.as_ref().map(|(id, s)| (*id, s.value));
            let icons = &mut self.icons;
            let pv = self.page_layers.get_mut(&pi).unwrap();
            pv.shows = want;
            // A full page with a gap opened: its last icon moves on to the next page.
            let shown = |i: usize| !(current && drag_at.is_some_and(|at| i >= at && i + 1 >= PER_PAGE));
            let mut ids = Vec::with_capacity(slots.len());
            for (i, (slot, cell)) in slots.iter().zip(&cells).enumerate() {
                let id = slot.id();
                ids.push(id);
                // The folder-to-be: a tile behind the icon being rested on.
                if current && (merging == Some(i) || (merging.is_none() && grow > 0.01 && drag_over == Some(i))) {
                    let ir = m.icon_of(*cell);
                    tile = Some((ir.cx(), ir.cy()));
                }
                let down = current && pressed == Some(Hit::Slot(i));
                let key = slot_key(slot, down, ink.picked);
                let sv = match pv.slots.entry(id) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let Ok(layer) = Layer::new(&self.gpu, m.pitch_x + 2.0 * PAD, m.pitch_y + 2.0 * PAD, s) else { continue };
                        let _ = pv.visual.Children().map(|c| c.InsertAtTop(&layer.visual));
                        e.insert(SlotVis { layer, key: 0, waiting: false, at: None })
                    }
                };
                if sv.key != key {
                    sv.key = key;
                    self.stats.icons += 1;
                    let local = Rect::new(PAD, PAD, m.pitch_x, m.pitch_y);
                    sv.waiting = ink.waits(|| sv.layer.draw(gfx, |p| ink.slot(p, icons, loader, slot, local, true, down)));
                }
                let k = pop.filter(|(pid, _)| current && *pid == id).map_or(1.0, |(_, k)| k);
                let place = (cell.x, cell.y, k, shown(i));
                if sv.at != Some(place) {
                    sv.at = Some(place);
                    sv.layer.set_offset(cell.x - PAD, cell.y - PAD);
                    // Pops about the icon's middle.
                    sv.layer.set_scale(k, PAD + m.pitch_x / 2.0, PAD + m.icon / 2.0);
                    let _ = sv.layer.visual.SetIsVisible(place.3);
                }
            }
            // Gone from this page (moved, dragged away, hidden).
            let kids = pv.visual.Children();
            pv.slots.retain(|id, sv| {
                let keep = ids.contains(id);
                if !keep {
                    let _ = kids.as_ref().map(|c| c.Remove(&sv.layer.visual));
                }
                keep
            });
        }
        // The folder tile grows in and out by scale and opacity alone.
        if tile_seen {
            match tile {
                Some((cx, cy)) => {
                    let grow = self.merge_grow.value.clamp(0.0, 1.2);
                    let size = m.icon * TILE;
                    self.tile.set_offset(self.page as f32 * w + cx - size / 2.0, cy - size / 2.0);
                    self.tile.set_scale(lerp(0.8, 1.2, grow) / TILE, size / 2.0, size / 2.0);
                    self.tile.set_opacity(grow.min(1.0));
                }
                None => self.tile.set_opacity(0.0),
            }
        }

        if self.results_dirty {
            self.results_dirty = false;
            self.stats.others += 1;
            let results = self.results.clone();
            let icons = &mut self.icons;
            self.results_waiting = ink.waits(|| {
                self.results_layer.draw(gfx, |p| {
                    for (i, t) in results.iter().enumerate() {
                        let down = pressed == Some(Hit::Result(i));
                        ink.slot(p, icons, loader, &Slot::App { target: t.clone() }, m.cell(i), true, down);
                    }
                })
            });
        }

        if self.chrome_dirty {
            self.chrome_dirty = false;
            self.stats.others += 3;
            self.draw_search(gfx, theme);
            self.draw_tools(gfx, theme);
        } else if self.search_dirty {
            self.stats.others += 1;
            self.draw_search(gfx, theme);
        }
        self.search_dirty = false;

        // The open folder's sheet.
        let sheet = self.folder_sheet();
        let folder = self.folder.as_ref().map(|f| (f.page, f.index, f.editing.as_ref().map(|e| e.iter().collect::<String>())));
        if let (true, Some((sr, cols, gx, gy)), Some((fp, fi, editing))) = (self.folder_dirty, sheet, folder) {
            self.folder_dirty = false;
            self.stats.others += 1;
            if let Some(Slot::Folder { name, apps }) = self.state.pages.get(fp).and_then(|p| p.get(fi)).cloned() {
                let title_font = gfx.format(&theme.font, 30.0, Weight::Regular, Align::Center);
                let icons = &mut self.icons;
                if let Some(f) = self.folder.as_mut() {
                    f.layer.resize(sr.w, sr.h);
                    ink.waiting.set(false);
                    f.layer.draw(gfx, |p| {
                        let r = Rect::new(0.0, 0.0, sr.w, sr.h);
                        p.fill_round(r, 36.0, Color::rgba(72, 76, 88, 0.64));
                        p.stroke_round(r.inset(0.25, 0.25), 36.0, Color::gray(255, 0.22), 0.5);
                        let tr = Rect::new(0.0, 18.0, sr.w, 48.0);
                        match &editing {
                            Some(e) => {
                                let tw = p.gfx.measure(e, &title_font);
                                let bw = tw.max(60.0) + 32.0;
                                p.fill_round(Rect::new(tr.cx() - bw / 2.0, tr.y + 4.0, bw, tr.h - 8.0), 10.0, Color::gray(255, 0.16));
                                p.text(e, &title_font, tr, Color::WHITE);
                                let cx = tr.cx() + tw / 2.0 + 2.0;
                                p.line(cx, tr.y + 10.0, cx, tr.bottom() - 10.0, Color::WHITE, 1.5);
                            }
                            None => p.text(&name, &title_font, tr, Color::WHITE),
                        }
                        for (i, t) in apps.iter().enumerate().take(cols * 4) {
                            let cell = Rect::new(gx + (i % cols) as f32 * m.pitch_x, gy + (i / cols) as f32 * m.pitch_y, m.pitch_x, m.pitch_y);
                            let down = pressed == Some(Hit::FolderApp(i));
                            ink.slot(p, icons, loader, &Slot::App { target: t.clone() }, cell, true, down);
                        }
                    });
                    self.folder_waiting = ink.waiting.get();
                }
            }
        }

        // The dragged icon, lifted under the pointer (one icon: cheap to redraw).
        if let Some(slot) = self.drag.as_ref().filter(|d| !d.drawn).map(|d| d.slot.clone()) {
            self.stats.others += 1;
            let icons = &mut self.icons;
            if let Some(d) = self.drag.as_mut() {
                let waiting = ink.waits(|| {
                    d.layer.draw(gfx, |p| {
                        let cell = Rect::new(m.icon / 2.0 - m.pitch_x / 2.0, 0.0, m.pitch_x, m.pitch_y);
                        ink.slot(p, icons, loader, &slot, cell, false, false);
                    })
                });
                d.drawn = !waiting;
            }
        }

        self.apply();
        if !self.shown {
            self.shown = true;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
    }

    /// A sprite per page dot (none for a single page), when their number changes.
    fn place_dots(&mut self) {
        let dots = self.dot_rects();
        let n = if dots.len() > 1 { dots.len() } else { 0 };
        if n == self.dot_sprites.len() {
            return;
        }
        let Ok(kids) = self.dots_visual.Children() else { return };
        let _ = kids.RemoveAll();
        self.dot_sprites.clear();
        let brush = self.dot.visual.Brush();
        let s = self.scale;
        for d in dots.iter().take(n) {
            let Ok(sprite) = self.gpu.compositor.CreateSpriteVisual() else { return };
            if let Ok(b) = &brush {
                let _ = sprite.SetBrush(b);
            }
            let _ = sprite.SetSize(Vector2 { X: (DOT * s).ceil(), Y: (DOT * s).ceil() });
            let _ = sprite.SetOffset(Vector3 { X: (d.x * s).round(), Y: (d.y * s).round(), Z: 0.0 });
            let _ = kids.InsertAtTop(&sprite);
            self.dot_sprites.push(sprite);
        }
        // Their opacities are set with everything else that moves.
        self.applied.set(None);
    }

    fn draw_tools(&self, gfx: &Gfx, theme: &Theme) {
        let rects = self.tool_rects();
        let (Some(first), Some(last)) = (rects.first().copied(), rects.last().copied()) else { return };
        let bar = Rect::new(first.x - 6.0, first.y - 6.0, last.right() - first.x + 12.0, first.h + 12.0);
        let lw = self.tool_layer.w;
        let ox = (lw - bar.w) / 2.0;
        self.tool_layer.set_offset(bar.x - ox, bar.y - 2.0);
        let font = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Center);
        let bold = gfx.format(&theme.font, 13.0, Weight::SemiBold, Align::Center);
        let n = self.picked.len();
        self.tool_layer.draw(gfx, |p| {
            let shift = |r: Rect| Rect::new(r.x - bar.x + ox, r.y - bar.y + 2.0, r.w, r.h);
            p.fill_round(shift(bar), 14.0, Color::rgba(40, 40, 46, 0.55));
            p.stroke_round(shift(bar).inset(0.25, 0.25), 14.0, Color::gray(255, 0.18), 0.5);
            for (i, (r, (label, _))) in rects.iter().zip(TOOLS).enumerate() {
                let r = shift(*r);
                let (text, fill, ink, f) = match i {
                    1 if n > 0 => (format!("Hide {n} App{}", if n == 1 { "" } else { "s" }), Color::rgba(255, 69, 58, 0.9), Color::WHITE, &bold),
                    1 => ("Hide".to_string(), Color::gray(255, 0.08), Color::gray(255, 0.4), &font),
                    4 => (label.to_string(), Color::WHITE, Color::gray(0, 0.85), &bold),
                    _ => (label.to_string(), Color::gray(255, 0.14), Color::WHITE, &font),
                };
                p.fill_round(r, 9.0, fill);
                p.text(&text, f, r, ink);
            }
        });
    }

    fn draw_search(&self, gfx: &Gfx, theme: &Theme) {
        let r = self.search_rect();
        let lr = Rect::new(0.0, 0.0, self.search_layer.w, self.search_layer.h);
        let field = Rect::new((lr.w - r.w) / 2.0, (lr.h - r.h) / 2.0, r.w, r.h);
        self.search_layer.set_offset(r.x - field.x, r.y - field.y);
        let query = self.query();
        let t = self.search_slide.value;
        let caret = self.caret_on && !query.is_empty();
        let font = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Leading);
        let glyph = gfx.format("Segoe Fluent Icons", 11.0, Weight::Regular, Align::Center);
        self.search_layer.draw(gfx, |p| {
            // A touch brighter while there's something typed.
            let k = t.clamp(0.0, 1.0);
            p.fill_round(field, 8.0, Color::gray(255, 0.13 + 0.05 * k));
            p.stroke_round(field.inset(0.25, 0.25), 8.0, Color::gray(255, 0.2 + 0.1 * k), 0.5);
            let dim = Color::gray(255, 0.55);
            // The magnifier slides from beside "Search", in the middle, to the left edge.
            let tw = p.gfx.measure("Search", &font);
            let centre_x = field.cx() - (tw + 18.0) / 2.0;
            let gx = lerp(centre_x, field.x + 9.0, t);
            p.text("\u{E721}", &glyph, Rect::new(gx, field.y, 12.0, field.h), dim);
            if query.is_empty() {
                p.faded((1.0 - k * 1.6).clamp(0.0, 1.0), || {
                    p.text("Search", &font, Rect::new(gx + 18.0, field.y, tw + 4.0, field.h), dim);
                });
            } else {
                let tr = Rect::new(gx + 18.0, field.y, field.right() - 9.0 - (gx + 18.0), field.h);
                p.faded(k, || {
                    p.clipped(tr, || p.text(&query, &font, tr, Color::WHITE));
                });
                if caret {
                    let w = p.gfx.measure(&query, &font).min(tr.w);
                    p.line(tr.x + w + 1.5, field.y + 7.0, tr.x + w + 1.5, field.bottom() - 7.0, theme.accent, 1.5);
                }
            }
        });
    }
}

impl Drop for Launchpad {
    fn drop(&mut self) {
        let me = self.hwnd.0 as isize;
        let _ = LAUNCHPAD_HWND.compare_exchange(me, 0, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
