//! Spotlight, as in macOS Tahoe: a Liquid Glass capsule (Alt+Space by
//! default). Move the pointer and four glass bubbles spring out of its end:
//! Apps, Files, Actions and Clipboard (Ctrl+1–4). Type and the capsule grows
//! down into a panel of results: apps, files, folders, quick arithmetic,
//! Windows commands and Settings pages, LaTeX symbols, and the web.
//!
//! The capsule and each bubble are separate glass surfaces (a surface has one
//! glass shape); the bubbles sit behind the capsule, so they appear out of it.

pub mod calc;
pub mod clipboard;
pub mod commands;
pub mod index;
pub mod latex;
pub mod web;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicIsize;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use windows::core::Result;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use self::index::{Index, Item, Kind};
use crate::anim::{lerp, Glide, Spring};
use crate::chrome::SHADOW;
use crate::config::{Backdrop, Config, LaunchStyle};
use crate::finder::loader::IconLoader;
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::launchpad::categories::{self, Category};
use crate::launchpad::layout::State as LaunchState;
use crate::menu::Action;
use crate::surface::{Fading, Surface};
use crate::theme::Theme;
use crate::win::Monitor;

/// The panel's content window, so the window procedure can close it when it loses focus.
pub static SPOTLIGHT_HWND: AtomicIsize = AtomicIsize::new(0);

/// The capsule (and the panel it grows into): width, height, corner radius.
const W: f32 = 580.0;
const PILL_H: f32 = 52.0;
const RADIUS: f32 = 26.0;
/// The bubbles: diameter, and the gap between them (and the capsule).
const BUBBLE: f32 = 50.0;
const GAP: f32 = 8.0;
/// The capsule's width with the bubbles out: the whole row stays `W` wide.
const PILL_SHORT: f32 = W - 4.0 * (BUBBLE + GAP);
/// How far the body's and a bubble's shadows reach past them.
const BODY_REACH: f32 = 34.0;
const BUBBLE_REACH: f32 = 24.0;
const ROW_H: f32 = 40.0;
const HEADER_H: f32 = 26.0;
const MAX_APPS: usize = 5;
const MAX_FILES: usize = 7;
/// Tallest the list can get (12 rows + 6 headers + padding).
const MAX_LIST: f32 = 12.0 * ROW_H + 6.0 * HEADER_H + 20.0;
/// The Apps panel's height below the field (it scrolls).
const APPS_H: f32 = 470.0;
const MAX_H: f32 = PILL_H + if MAX_LIST > APPS_H { MAX_LIST } else { APPS_H };
const MAX_COMMANDS: usize = 3;
const MAX_SETTINGS: usize = 3;
const MAX_LATEX: usize = 12;
/// Commands and Settings pages need a word-start match to show up.
const MIN_COMMAND_SCORE: i32 = 640;
const SETTINGS_APP: &str = "shell:AppsFolder\\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel";
const ICON_PX: i32 = 64;
/// Apps panel grid: columns, cell height, icon size.
const APP_COLS: usize = 5;
const APP_CELL_H: f32 = 94.0;
const APP_ICON: f32 = 54.0;

/// Spotlight's browse modes (macOS's ⌘1–⌘4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Search,
    Apps,
    Files,
    Actions,
    Clipboard,
}

impl Mode {
    const BUBBLES: [Mode; 4] = [Mode::Apps, Mode::Files, Mode::Actions, Mode::Clipboard];

    fn placeholder(self) -> &'static str {
        match self {
            Mode::Search => "Spotlight Search",
            Mode::Apps => "Applications",
            Mode::Files => "Files",
            Mode::Actions => "Actions",
            Mode::Clipboard => "Clipboard",
        }
    }
}

#[derive(Clone, Debug)]
enum RowAction {
    Launch(String),
    OpenFile(String),
    OpenFolder(String),
    Copy(String),
    Url(String),
    Run(Action),
    Command(commands::Command),
    /// Enter copies the code, Shift+Enter the symbol.
    Latex { code: &'static str, symbol: &'static str },
    /// Puts a clipboard history entry back, and pastes it.
    Paste(usize),
    ClearClipboard,
}

/// What to draw in place of a file icon.
#[derive(Clone, Debug)]
enum Badge {
    Calculator,
    Web,
    /// A Segoe Fluent Icons glyph on a coloured tile.
    Glyph(char, Color),
    /// A LaTeX symbol preview.
    Symbol(&'static str),
    /// A text clipping.
    Text,
}

#[derive(Clone, Debug)]
struct Row {
    section: &'static str,
    title: String,
    subtitle: String,
    action: RowAction,
    /// Icon loader key and source path.
    icon: Option<(String, String)>,
    badge: Option<Badge>,
}

pub enum SpotlightEvent {
    None,
    Close,
    Run(Action),
    /// Close, then run these in order (paste needs the app focused again first).
    RunAll(Vec<Action>),
}

/// What the panel last showed, kept to draw while it closes: by then the
/// rows and mode are already the new ones.
struct Shown {
    mode: Mode,
    rows: Vec<Row>,
    selected: usize,
}

/// One glass bubble beside the capsule.
struct Bubble {
    surface: Surface,
    mode: Mode,
    /// 0 tucked in the capsule, 1 out in its place.
    p: Spring,
    hover: bool,
    /// What was last drawn (its rect, hover and glyph fade), to skip redrawing a
    /// settled bubble and to repaint only where it was and is.
    drawn: Option<(Rect, bool, f32)>,
}

/// The Tahoe Apps panel's content.
#[derive(Default)]
struct AppsView {
    /// Every app: (name, target, category).
    all: Vec<(String, String, Category)>,
    suggestions: Vec<String>,
    /// Sections showing all their apps ("Show More").
    expanded: Vec<usize>,
    /// Each toggled section opening (1) or closing (0), so its rows unfold.
    reveal: HashMap<usize, Spring>,
    /// A section unfolding: the panel scrolls along so its new rows come into view.
    follow: Option<usize>,
    list: bool,
    scroll: f32,
    scroll_to: f32,
    max_scroll: f32,
    /// The apps on show, in order, for the keyboard.
    shown: Vec<String>,
    selected: usize,
}

pub struct Spotlight {
    pub surface: Surface,
    bubbles: Vec<Bubble>,
    kind: Backdrop,
    text: Vec<char>,
    caret: usize,
    anchor: usize,
    rows: Vec<Row>,
    /// What the last file search matched, so typing on narrows it.
    narrow: index::Narrow,
    selected: usize,
    hits: Vec<(Rect, usize)>,
    bubble_hits: Vec<(Rect, usize)>,
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    app_icons: HashMap<(String, u32), Option<ID2D1Bitmap>>,
    /// Web search engine settings, captured when the panel opens.
    web: crate::config::SpotlightCfg,
    /// What the Apps button opens.
    apps_style: LaunchStyle,
    /// Whether low power mode is on, read in the background when the panel
    /// opens: asking Windows takes ~90ms, far too slow to do per keystroke.
    low_power: Arc<OnceLock<bool>>,
    /// Where the cursor was when Spotlight opened, and last; moving it shows the bubbles.
    opened_at: POINT,
    cursor: POINT,
    /// The selection, gliding from row to row as the pointer or arrow keys move it.
    glide: Glide,
    pub mode: Mode,
    /// Bubbles out (1) or tucked away (0), and how long since they were asked out.
    reveal: Spring,
    reveal_clock: f32,
    revealed: bool,
    /// The panel's height below the capsule.
    grow: Spring,
    /// The panel's content fading in as it opens (the glass snaps to size) and
    /// out as it closes (the glass shrinking back to the capsule under it).
    fade: Spring,
    /// See `Shown`; `rows_changed` says it's out of date.
    shown: Option<Shown>,
    rows_changed: bool,
    /// The Apps panel's selection, gliding like the list's.
    app_glide: Glide,
    apps: AppsView,
    app_hits: Vec<(Rect, usize)>,
    more_hits: Vec<(Rect, usize)>,
    view_hit: Option<Rect>,
    /// The body as last drawn: a frame repaints only where it was and is, not
    /// the whole window (which is sized for the tallest panel).
    painted: Option<Rect>,
    clips: Vec<clipboard::Entry>,
    pub tracking: bool,
    pub dirty: bool,
}

/// Spotlight opens instantly (you're already typing); once closed it fades out.
impl Fading for Spotlight {
    fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }
}

fn cursor_pos() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}

fn icon_for(item: &Item) -> (String, String) {
    let t = &item.target;
    let key = match item.kind {
        Kind::App => format!("app:{t}"),
        Kind::Folder => format!("dir:{t}"),
        Kind::File => {
            let ext = Path::new(t).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            if matches!(ext.as_str(), "exe" | "lnk" | "ico" | "url") {
                format!("file:{t}")
            } else {
                format!("ext:{ext}")
            }
        }
    };
    (key, t.clone())
}

/// "10 minute", "1 hour 30 minute", "45 second".
fn describe(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h} hour"));
    }
    if m > 0 {
        parts.push(format!("{m} minute"));
    }
    if s > 0 {
        parts.push(format!("{s} second"));
    }
    parts.join(" ")
}

/// Recently opened files and folders (Windows' Recent Items), newest first.
fn recent_files(n: usize) -> Vec<(String, PathBuf)> {
    let Some(dir) = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Microsoft\\Windows\\Recent")) else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<(SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter()
        .take(n)
        .map(|(_, p)| (p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), p))
        .collect()
}

impl Spotlight {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: &Monitor) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((MAX_H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + ((mon.bottom - mon.top) as f32 * 0.17) as i32 - (SHADOW * s) as i32;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let kind = cfg.spotlight.backdrop.for_panels();
        let surface = Surface::new(gfx, rect, s, kind, theme.menu_tint)?;
        SPOTLIGHT_HWND.store(surface.content.0 as isize, std::sync::atomic::Ordering::Relaxed);
        let low_power = Arc::new(OnceLock::new());
        let slot = low_power.clone();
        std::thread::spawn(move || {
            let _ = slot.set(crate::win::controls::low_power_mode());
        });
        let now = cursor_pos();
        Ok(Self {
            surface,
            bubbles: Vec::new(),
            kind,
            text: Vec::new(),
            caret: 0,
            anchor: 0,
            rows: Vec::new(),
            narrow: index::Narrow::default(),
            selected: 0,
            hits: Vec::new(),
            bubble_hits: Vec::new(),
            bitmaps: HashMap::new(),
            app_icons: HashMap::new(),
            web: cfg.spotlight.clone(),
            apps_style: cfg.launchpad.spotlight,
            low_power,
            opened_at: now,
            cursor: now,
            glide: Glide::default(),
            mode: Mode::Search,
            reveal: Spring::new(0.0, 0.42, 0.74),
            reveal_clock: 0.0,
            revealed: false,
            grow: Spring::new(0.0, 0.3, 0.9),
            fade: Spring::new(1.0, 0.12, 1.0),
            shown: None,
            rows_changed: true,
            app_glide: Glide::default(),
            apps: AppsView::default(),
            app_hits: Vec::new(),
            more_hits: Vec::new(),
            view_hit: None,
            painted: None,
            clips: Vec::new(),
            tracking: false,
            dirty: true,
        })
    }

    /// Gives the panel keyboard focus. A hotkey or click allows that; a tapped
    /// modifier (seen only by our hook) may not, so fall back to borrowing focus.
    pub fn focus(&self) {
        crate::win::apps::activate(self.surface.content);
    }

    /// Whether `hwnd` is one of the windows behind Spotlight's glass (they're
    /// bigger than what shows: a click on one is a click outside).
    pub fn owns_backdrop(&self, hwnd: HWND) -> bool {
        self.surface.backdrop == Some(hwnd) || self.bubbles.iter().any(|b| b.surface.backdrop == Some(hwnd))
    }

    /// The bubbles' windows, for routing their mouse messages.
    pub fn bubble_index(&self, hwnd: HWND) -> Option<usize> {
        self.bubbles.iter().position(|b| b.surface.content == hwnd)
    }

    fn query(&self) -> String {
        self.text.iter().collect()
    }

    fn range(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    fn insert(&mut self, s: &str) {
        let (a, b) = self.range();
        let s: Vec<char> = s.chars().filter(|c| !c.is_control()).collect();
        let n = s.len();
        self.text.splice(a..b, s);
        self.caret = a + n;
        self.anchor = self.caret;
    }

    // --- The bubbles ---------------------------------------------------------

    /// Whether the bubbles belong out: plain Search, nothing typed.
    fn wants_bubbles(&self) -> bool {
        self.mode == Mode::Search && self.text.is_empty()
    }

    /// Polled while Spotlight is open: the pointer moving brings the bubbles out.
    /// Returns true once they're out (stop polling).
    pub fn poll_cursor(&mut self) -> bool {
        let p = cursor_pos();
        if self.revealed {
            return true;
        }
        if (p.x - self.opened_at.x).abs() + (p.y - self.opened_at.y).abs() > 3 {
            self.show_bubbles(true);
        }
        self.revealed
    }

    fn show_bubbles(&mut self, on: bool) {
        let on = on && self.wants_bubbles();
        if on == self.revealed {
            return;
        }
        self.revealed = on;
        self.reveal.set(if on { 1.0 } else { 0.0 });
        self.reveal_clock = 0.0;
        if !on {
            for b in &mut self.bubbles {
                b.p.set(0.0);
                b.hover = false;
            }
        }
        self.dirty = true;
    }

    /// Hides the bubbles at once (Spotlight is closing: only the capsule fades).
    pub fn hide_bubbles(&mut self) {
        for b in &mut self.bubbles {
            b.surface.set_hidden(true);
        }
    }

    /// Where bubble `i` is at progress `p`: budding off the capsule's end, out to its place.
    fn bubble_rect(body: Rect, i: usize, p: f32) -> Rect {
        let start = body.x + PILL_SHORT + GAP * 0.5 + BUBBLE * 0.25;
        let end = body.x + PILL_SHORT + GAP + BUBBLE / 2.0 + i as f32 * (BUBBLE + GAP);
        let cx = lerp(start, end, p);
        let k = p.clamp(0.0, 1.0);
        // A liquid stretch on the way out, settling round. The height and corner
        // radius stay put: a new radius is a Composition change, which can land late.
        let stretch = (k * std::f32::consts::PI).sin();
        let w = BUBBLE * (1.0 + 0.28 * stretch);
        Rect::new(cx - w / 2.0, body.y + PILL_H / 2.0 - BUBBLE / 2.0, w, BUBBLE)
    }

    /// Makes the bubbles (hidden) ahead of their first showing: four windows
    /// and their glass take long enough to make that the animation would
    /// otherwise open on a dropped frame or two.
    pub fn prepare_bubbles(&mut self, gfx: &Gfx, theme: &Theme) {
        self.ensure_bubbles(gfx, theme);
    }

    fn ensure_bubbles(&mut self, gfx: &Gfx, theme: &Theme) {
        if !self.bubbles.is_empty() {
            return;
        }
        let s = self.surface.scale;
        let r = self.surface.rect;
        let rect = RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + ((PILL_H + 2.0 * SHADOW) * s).round() as i32 };
        for mode in Mode::BUBBLES {
            match Surface::new(gfx, rect, s, self.kind, theme.menu_tint) {
                Ok(mut surface) => {
                    // The glass is there in full from a bubble's first frame to
                    // its last; what's drawn on it fades.
                    surface.keep_backdrop_solid();
                    surface.set_opacity(0.0);
                    self.bubbles.push(Bubble { surface, mode, p: Spring::new(0.0, 0.42, 0.72), hover: false, drawn: None });
                }
                Err(e) => log!("spotlight bubble failed: {e}"),
            }
        }
        // Above the capsule, so they always take their own clicks.
        for b in &self.bubbles {
            b.surface.raise();
        }
    }

    pub fn on_bubble_move(&mut self, i: usize) {
        for (j, b) in self.bubbles.iter_mut().enumerate() {
            let on = j == i;
            if b.hover != on {
                b.hover = on;
                self.dirty = true;
            }
        }
    }

    pub fn on_bubble_leave(&mut self, i: usize) {
        if let Some(b) = self.bubbles.get_mut(i) {
            if b.hover {
                b.hover = false;
                self.dirty = true;
            }
        }
    }

    pub fn on_bubble_click(&mut self, i: usize, index: &Index) -> SpotlightEvent {
        match self.bubbles.get(i) {
            Some(b) if b.p.value > 0.5 => {
                let mode = b.mode;
                self.set_mode(mode, index)
            }
            _ => SpotlightEvent::None,
        }
    }

    // --- Modes -----------------------------------------------------------------

    /// Switches to a browse mode. Apps opens Sequoia's Launchpad instead when set to.
    pub fn set_mode(&mut self, mode: Mode, index: &Index) -> SpotlightEvent {
        if mode == Mode::Apps && self.apps_style == LaunchStyle::Sequoia {
            return SpotlightEvent::Run(Action::OpenLaunchpad(LaunchStyle::Sequoia));
        }
        self.mode = if self.mode == mode && mode != Mode::Search { Mode::Search } else { mode };
        self.text.clear();
        self.caret = 0;
        self.anchor = 0;
        if self.mode == Mode::Apps {
            self.load_apps(index);
        }
        if self.mode == Mode::Clipboard {
            self.clips = clipboard::entries();
        }
        self.show_bubbles(false);
        self.search(index);
        SpotlightEvent::None
    }

    /// Opens the Apps panel whatever the Apps button is set to (the Dock or a shortcut asked for Tahoe's).
    pub fn open_apps(&mut self, index: &Index) {
        self.apps_style = LaunchStyle::Tahoe;
        if self.mode != Mode::Apps {
            self.set_mode(Mode::Apps, index);
        }
    }

    fn load_apps(&mut self, index: &Index) {
        let all: Vec<(String, String, Category)> =
            crate::launchpad::apps(index).into_iter().map(|(n, t)| { let c = categories::of(&n, &t); (n, t, c) }).collect();
        let known: std::collections::HashSet<&str> = all.iter().map(|a| a.1.as_str()).collect();
        let mut suggestions: Vec<String> = LaunchState::load().suggestions(10).into_iter().filter(|t| known.contains(t.as_str())).collect();
        suggestions.truncate(APP_COLS);
        let list = self.apps.list;
        self.apps = AppsView { all, suggestions, list, ..Default::default() };
    }

    /// Recomputes results for the current text and mode.
    pub fn search(&mut self, index: &Index) {
        self.rows_changed = true;
        self.dirty = true;
        // New results: the selection appears on its row rather than sliding there.
        self.glide.reset();
        self.selected = 0;
        self.rows.clear();
        let raw = self.query();
        let q = raw.trim().to_lowercase();
        match self.mode {
            Mode::Apps => {
                self.apps.selected = 0;
                self.apps.scroll_to = 0.0;
                return;
            }
            Mode::Files => return self.search_files(index, &q),
            Mode::Actions => return self.search_actions(&raw, &q),
            Mode::Clipboard => return self.search_clipboard(&q),
            Mode::Search => {}
        }
        if q.is_empty() {
            return;
        }

        let top_n = |items: &[Item], n: usize| -> Vec<(i32, Item)> {
            let mut scored: Vec<(i32, &Item)> = items.iter().filter_map(|it| index::score(it, &q).map(|s| (s, it))).collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.len().cmp(&b.1.name.len())));
            scored.into_iter().take(n).map(|(s, it)| (s, it.clone())).collect()
        };
        let mut apps = top_n(&index.apps, MAX_APPS + 1);
        let mut files = index.files.top(&q, MAX_FILES + 1, &mut self.narrow);

        if let Some(lq) = latex::query(&raw) {
            for (code, symbol, _) in latex::search(lq, MAX_LATEX) {
                self.rows.push(Row {
                    section: "LaTeX",
                    title: code.to_string(),
                    subtitle: "Enter copies LaTeX, Shift+Enter the symbol".into(),
                    action: RowAction::Latex { code, symbol },
                    icon: None,
                    badge: Some(Badge::Symbol(symbol)),
                });
            }
            return;
        }

        if let Some(cmd) = crate::island::timer::parse_command(&raw) {
            use crate::island::{timer::Command, IslandCmd};
            let (title, cmd) = match cmd {
                Command::Timer(secs) => (format!("Start a {} timer", describe(secs)), IslandCmd::Timer(secs)),
                Command::Stopwatch => ("Start a stopwatch".to_string(), IslandCmd::Stopwatch),
            };
            self.rows.push(Row {
                section: "Dynamic Island",
                title,
                subtitle: "Runs in the Dynamic Island".into(),
                action: RowAction::Run(Action::Island(cmd)),
                icon: None,
                badge: Some(Badge::Glyph('\u{E916}', Color::rgba(255, 149, 0, 1.0))),
            });
        }
        if let Some(v) = calc::eval(&raw) {
            let result = calc::format(v);
            self.rows.push(Row {
                section: "Calculator",
                title: format!("= {result}"),
                subtitle: "Press Enter to copy".into(),
                action: RowAction::Copy(result),
                icon: None,
                badge: Some(Badge::Calculator),
            });
        }
        let low_power = self.low_power.get().copied().unwrap_or(false);
        for c in commands::search(&raw, low_power, crate::win::system::is_dark_mode())
            .into_iter()
            .filter(|c| c.score >= MIN_COMMAND_SCORE)
            .take(MAX_COMMANDS)
        {
            self.rows.push(command_row(c));
        }
        let settings: Vec<_> =
            commands::settings_pages(&raw).into_iter().filter(|s| s.0 >= MIN_COMMAND_SCORE).take(MAX_SETTINGS).collect();

        // Top Hit: a strong app match, else a near-exact file match.
        let best_app = apps.first().map(|a| a.0).unwrap_or(i32::MIN);
        let best_file = files.first().map(|f| f.0).unwrap_or(i32::MIN);
        if best_app >= 700 && best_app >= best_file - 100 {
            let (_, it) = apps.remove(0);
            self.rows.push(item_row("Top Hit", &it));
        } else if best_file >= 900 {
            let (_, it) = files.remove(0);
            self.rows.push(item_row("Top Hit", &it));
        }
        for (_, it) in apps.iter().take(MAX_APPS) {
            self.rows.push(item_row("Applications", it));
        }
        for (_, title, uri) in settings {
            self.rows.push(settings_row(title, uri));
        }
        for (_, it) in files.iter().take(MAX_FILES) {
            self.rows.push(item_row("Folders & Documents", it));
        }
        let (engine, url) = web::search(&self.web, &raw);
        self.rows.push(Row {
            section: "Web",
            title: format!("Search {engine} for \u{201C}{}\u{201D}", raw.trim()),
            subtitle: String::new(),
            action: RowAction::Url(url),
            icon: None,
            badge: Some(Badge::Web),
        });
    }

    fn search_files(&mut self, index: &Index, q: &str) {
        if q.is_empty() {
            for (name, path) in recent_files(14) {
                let target = path.display().to_string();
                self.rows.push(Row {
                    section: "Recent",
                    title: name,
                    subtitle: "Recent Items".into(),
                    action: RowAction::OpenFile(target.clone()),
                    icon: Some((format!("file:{target}"), target)),
                    badge: None,
                });
            }
            return;
        }
        for (_, it) in index.files.top(q, 14, &mut self.narrow) {
            self.rows.push(item_row("Folders & Documents", &it));
        }
    }

    fn search_actions(&mut self, raw: &str, q: &str) {
        let low_power = self.low_power.get().copied().unwrap_or(false);
        let found = if q.is_empty() { commands::all(low_power, crate::win::system::is_dark_mode()) } else { commands::search(raw, low_power, crate::win::system::is_dark_mode()) };
        for c in found.into_iter().take(14) {
            self.rows.push(command_row(c));
        }
        if !q.is_empty() {
            for (_, title, uri) in commands::settings_pages(raw).into_iter().take(MAX_SETTINGS) {
                self.rows.push(settings_row(title, uri));
            }
        }
    }

    fn search_clipboard(&mut self, q: &str) {
        for (i, e) in self.clips.iter().enumerate() {
            let title = clipboard::title(&e.clip);
            let hay = match &e.clip {
                clipboard::Clip::Text(t) => t.to_lowercase(),
                clipboard::Clip::Files(f) => f.iter().map(|p| p.display().to_string().to_lowercase()).collect::<Vec<_>>().join(" "),
            };
            if !q.is_empty() && !hay.contains(q) {
                continue;
            }
            let (icon, badge) = match &e.clip {
                clipboard::Clip::Files(f) => {
                    let t = f[0].display().to_string();
                    let key = if f[0].is_dir() { format!("dir:{t}") } else { format!("file:{t}") };
                    (Some((key, t)), None)
                }
                clipboard::Clip::Text(_) => (None, Some(Badge::Text)),
            };
            self.rows.push(Row { section: "Clipboard", title, subtitle: clipboard::ago(e.at), action: RowAction::Paste(i), icon, badge });
            if self.rows.len() >= 40 {
                break;
            }
        }
        if !self.clips.is_empty() && q.is_empty() {
            self.rows.push(Row {
                section: "Options",
                title: "Clear Clipboard History".into(),
                subtitle: String::new(),
                action: RowAction::ClearClipboard,
                icon: None,
                badge: Some(Badge::Glyph('\u{E74D}', Color::rgba(99, 99, 102, 1.0))),
            });
        }
    }

    fn run(&self, i: usize, reveal: bool, shift: bool) -> SpotlightEvent {
        let Some(row) = self.rows.get(i) else { return SpotlightEvent::None };
        let action = match &row.action {
            RowAction::OpenFile(p) | RowAction::OpenFolder(p) if reveal => {
                Action::OpenFinder(Path::new(p).parent().map(Path::to_path_buf))
            }
            RowAction::Launch(t) => {
                LaunchState::record_launch(t);
                Action::Launch { path: t.clone(), args: None }
            }
            RowAction::OpenFile(p) => Action::Open(p.clone()),
            RowAction::OpenFolder(p) => Action::OpenFinder(Some(PathBuf::from(p))),
            RowAction::Url(u) => Action::Open(u.clone()),
            RowAction::Run(a) => a.clone(),
            RowAction::Copy(text) => {
                crate::finder::fs::copy_text(text);
                return SpotlightEvent::Close;
            }
            RowAction::Latex { code, symbol } => {
                crate::finder::fs::copy_text(if shift { symbol } else { code });
                return SpotlightEvent::Close;
            }
            RowAction::Command(c) => match command_action(*c) {
                Some(a) => a,
                None => return SpotlightEvent::Close,
            },
            RowAction::ClearClipboard => {
                clipboard::clear();
                return SpotlightEvent::Close;
            }
            RowAction::Paste(k) => {
                let Some(e) = self.clips.get(*k) else { return SpotlightEvent::None };
                clipboard::put(&e.clip);
                // Shift+Enter only copies; Enter pastes into the app you were in.
                if shift {
                    return SpotlightEvent::Close;
                }
                return SpotlightEvent::RunAll(vec![Action::Keys(crate::win::system::Shortcut::new("c", b'V' as u16, "V"))]);
            }
        };
        SpotlightEvent::Run(action)
    }

    fn launch_app(&self, target: &str) -> SpotlightEvent {
        LaunchState::record_launch(target);
        SpotlightEvent::Run(Action::Launch { path: target.to_string(), args: None })
    }

    pub fn on_char(&mut self, c: char, index: &Index) {
        if c.is_control() {
            return;
        }
        self.insert(&c.to_string());
        self.show_bubbles(false);
        self.search(index);
    }

    pub fn on_key(&mut self, vk: u16, ctrl: bool, shift: bool, index: &Index) -> SpotlightEvent {
        const BACK: u16 = 0x08;
        const ENTER: u16 = 0x0D;
        const ESC: u16 = 0x1B;
        const END: u16 = 0x23;
        const HOME: u16 = 0x24;
        const LEFT: u16 = 0x25;
        const UP: u16 = 0x26;
        const RIGHT: u16 = 0x27;
        const DOWN: u16 = 0x28;
        const DEL: u16 = 0x2E;
        let len = self.text.len();
        self.dirty = true;
        // Ctrl+1–4: the browse modes (⌘1–⌘4 on a Mac).
        if ctrl && (b'1' as u16..=b'4' as u16).contains(&vk) {
            return self.set_mode(Mode::BUBBLES[(vk - b'1' as u16) as usize], index);
        }
        if self.mode == Mode::Apps {
            if let Some(e) = self.on_apps_key(vk) {
                return e;
            }
        }
        match vk {
            ESC if !self.text.is_empty() => {
                self.text.clear();
                self.caret = 0;
                self.anchor = 0;
                self.search(index);
            }
            // Esc in a browse mode goes back to plain Search first, like macOS.
            ESC if self.mode != Mode::Search => {
                self.mode = Mode::Search;
                self.search(index);
            }
            ESC => return SpotlightEvent::Close,
            ENTER => return self.run(self.selected, ctrl, shift),
            UP => self.selected = self.selected.saturating_sub(1),
            DOWN => self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1)),
            BACK | DEL => {
                let (a, b) = self.range();
                if a != b {
                    self.insert("");
                } else if vk == BACK && a > 0 {
                    // Ctrl+Backspace deletes the previous word.
                    let mut start = a - 1;
                    if ctrl {
                        while start > 0 && self.text[start - 1].is_whitespace() {
                            start -= 1;
                        }
                        while start > 0 && !self.text[start - 1].is_whitespace() {
                            start -= 1;
                        }
                    }
                    self.text.drain(start..a);
                    self.caret = start;
                    self.anchor = start;
                } else if vk == DEL && a < len {
                    self.text.remove(a);
                }
                self.search(index);
            }
            LEFT | RIGHT | HOME | END => {
                let (a, b) = self.range();
                self.caret = match vk {
                    LEFT if a != b && !shift => a,
                    RIGHT if a != b && !shift => b,
                    LEFT => self.caret.saturating_sub(1),
                    RIGHT => (self.caret + 1).min(len),
                    HOME => 0,
                    _ => len,
                };
                if !shift {
                    self.anchor = self.caret;
                }
            }
            k if ctrl && k == b'A' as u16 => {
                self.anchor = 0;
                self.caret = len;
            }
            k if ctrl && k == b'V' as u16 => {
                if let Some(t) = crate::finder::fs::paste_text() {
                    self.insert(t.lines().next().unwrap_or(""));
                    self.show_bubbles(false);
                    self.search(index);
                }
            }
            _ => {}
        }
        SpotlightEvent::None
    }

    /// Arrow keys and Enter in the Apps panel; `None` for keys it doesn't use.
    fn on_apps_key(&mut self, vk: u16) -> Option<SpotlightEvent> {
        let n = self.apps.shown.len();
        if n == 0 {
            return None;
        }
        let cols = if self.apps.list { 1 } else { APP_COLS };
        let i = self.apps.selected as i32;
        let next = match vk {
            0x25 if !self.apps.list && self.text.is_empty() => i - 1,
            0x27 if !self.apps.list && self.text.is_empty() => i + 1,
            0x26 => i - cols as i32,
            0x28 => i + cols as i32,
            0x0D => {
                let t = self.apps.shown[self.apps.selected.min(n - 1)].clone();
                return Some(self.launch_app(&t));
            }
            _ => return None,
        };
        self.apps.selected = next.clamp(0, n as i32 - 1) as usize;
        self.dirty = true;
        Some(SpotlightEvent::None)
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        let pos = cursor_pos();
        if pos == self.cursor {
            return;
        }
        self.cursor = pos;
        if !self.revealed {
            self.show_bubbles(true);
        }
        if let Some(&(_, i)) = self.hits.iter().find(|(r, _)| r.contains(x, y)) {
            if i != self.selected {
                self.selected = i;
                self.dirty = true;
            }
        }
        if let Some(&(_, i)) = self.app_hits.iter().find(|(r, _)| r.contains(x, y)) {
            if i != self.apps.selected {
                self.apps.selected = i;
                self.dirty = true;
            }
        }
    }

    pub fn on_click(&mut self, x: f32, y: f32, ctrl: bool) -> SpotlightEvent {
        if self.view_hit.is_some_and(|r| r.contains(x, y)) {
            self.apps.list = !self.apps.list;
            self.apps.scroll_to = 0.0;
            self.dirty = true;
            return SpotlightEvent::None;
        }
        if let Some(&(_, s)) = self.more_hits.iter().find(|(r, _)| r.contains(x, y)) {
            let open = match self.apps.expanded.iter().position(|&e| e == s) {
                Some(k) => {
                    self.apps.expanded.remove(k);
                    false
                }
                None => {
                    self.apps.expanded.push(s);
                    true
                }
            };
            let spring = self.apps.reveal.entry(s).or_insert_with(|| Spring::new(if open { 0.0 } else { 1.0 }, 0.42, 0.92));
            spring.set(if open { 1.0 } else { 0.0 });
            self.apps.follow = open.then_some(s);
            self.dirty = true;
            return SpotlightEvent::None;
        }
        if let Some(&(_, i)) = self.app_hits.iter().find(|(r, _)| r.contains(x, y)) {
            if let Some(t) = self.apps.shown.get(i).cloned() {
                return self.launch_app(&t);
            }
        }
        match self.hits.iter().find(|(r, _)| r.contains(x, y)) {
            Some(&(_, i)) => self.run(i, ctrl, false),
            None => SpotlightEvent::None,
        }
    }

    pub fn on_wheel(&mut self, delta: i32) {
        if self.mode == Mode::Apps {
            self.apps.scroll_to = (self.apps.scroll_to - delta as f32 * 0.6).clamp(0.0, self.apps.max_scroll);
            self.dirty = true;
        }
    }

    pub fn icons_arrived(&mut self) {
        self.dirty = true;
    }

    // --- Animation -----------------------------------------------------------

    /// Advances the bubbles, the panel and the selection; returns true while anything moves.
    pub fn tick(&mut self, dt: f32) -> bool {
        let mut moving = self.glide.tick(dt);
        moving |= self.reveal.step(dt);
        moving |= self.grow.step(dt);
        moving |= self.fade.step(dt);
        moving |= self.app_glide.tick(dt);
        if self.revealed {
            self.reveal_clock += dt;
            // Out one after another, just apart.
            for (i, b) in self.bubbles.iter_mut().enumerate() {
                if b.p.target < 1.0 && self.reveal_clock >= 0.045 * i as f32 {
                    b.p.set(1.0);
                }
            }
            moving |= self.bubbles.len() < Mode::BUBBLES.len();
        }
        for b in &mut self.bubbles {
            moving |= b.p.step(dt);
        }
        let s = &mut self.apps;
        for r in s.reveal.values_mut() {
            moving |= r.step(dt);
        }
        if (s.scroll - s.scroll_to).abs() > 0.3 {
            s.scroll = crate::anim::approach(s.scroll, s.scroll_to, dt, 18.0);
            moving = true;
        } else {
            s.scroll = s.scroll_to;
        }
        self.dirty |= moving;
        moving
    }

    pub fn animating(&self) -> bool {
        self.glide.moving()
            || !self.reveal.settled()
            || !self.grow.settled()
            || !self.fade.settled()
            || self.app_glide.moving()
            || self.bubbles.iter().any(|b| !b.p.settled())
            || (self.revealed && self.bubbles.iter().any(|b| b.p.target < 1.0))
            || (self.revealed && self.bubbles.len() < Mode::BUBBLES.len())
            || self.apps.scroll != self.apps.scroll_to
            || self.apps.reveal.values().any(|r| !r.settled())
    }

    // --- Drawing -------------------------------------------------------------

    /// The list's height for the current rows.
    fn list_height(&self) -> f32 {
        if self.rows.is_empty() {
            return 0.0;
        }
        let mut headers = 0;
        let mut last = "";
        for r in &self.rows {
            if r.section != last {
                headers += 1;
                last = r.section;
            }
        }
        (10.0 + self.rows.len() as f32 * ROW_H + headers as f32 * HEADER_H + 10.0).min(MAX_LIST)
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme, loader: &mut IconLoader) {
        self.dirty = false;
        if self.revealed {
            self.ensure_bubbles(gfx, theme);
        }
        let below = if self.mode == Mode::Apps { APPS_H } else { self.list_height() };
        if self.grow.target != below {
            if below <= 0.0 && self.grow.value > 0.0 && self.shown.is_some() {
                // Going back to the capsule: the glass draws in while what it held fades.
                self.grow.set(0.0);
                self.fade.set(0.0);
            } else {
                // The glass takes its new size at once; what's in it fades in.
                if self.grow.value <= 0.0 && below > 0.0 {
                    self.fade.snap(0.0);
                }
                self.fade.set(1.0);
                self.grow.snap(below);
            }
        }
        // Closing: what was there is drawn, not what's there now (nothing).
        let closing = below <= 0.0 && self.grow.value > 0.0;
        if !closing {
            if below <= 0.0 {
                self.shown = None;
            } else if std::mem::take(&mut self.rows_changed) || self.shown.is_none() {
                self.shown = Some(Shown { mode: self.mode, rows: self.rows.clone(), selected: self.selected });
            } else if let Some(s) = &mut self.shown {
                s.selected = self.selected;
            }
        }
        let ghost = if closing { self.shown.take() } else { None };
        let (panel, rows, selected) = match &ghost {
            Some(g) => (g.mode, &g.rows[..], g.selected),
            None => (self.mode, &self.rows[..], self.selected),
        };
        let r = self.reveal.value;
        let width = lerp(W, PILL_SHORT, r);
        let height = PILL_H + self.grow.value.max(0.0);
        let body = Rect::new(SHADOW, SHADOW, width, height);
        let radius = RADIUS;
        self.surface.set_backdrop_shape(body, radius);

        let big = gfx.format(&theme.font, 20.0, Weight::Regular, Align::Leading);
        let title_font = gfx.format(&theme.font, 14.0, Weight::Regular, Align::Leading);
        let sub_font = gfx.format(&theme.font, 12.0, Weight::Regular, Align::Trailing);
        let header_font = gfx.format(&theme.font, 11.0, Weight::SemiBold, Align::Leading);
        let more_font = gfx.format(&theme.font, 11.0, Weight::Regular, Align::Trailing);
        let glyph_font = gfx.format("Segoe Fluent Icons", 14.0, Weight::Regular, Align::Center);
        let mode_font = gfx.format("Segoe Fluent Icons", 17.0, Weight::Regular, Align::Center);
        let symbol_font = gfx.format("Cambria Math", 15.0, Weight::Regular, Align::Center);
        let symbol_small = gfx.format("Cambria Math", 10.0, Weight::Regular, Align::Center);
        let app_label = gfx.format_block(&theme.font, 11.0, Weight::Regular, Align::Center);
        let has_backdrop = self.surface.backdrop.is_some();

        let query = self.query();
        let prefix_w = |n: usize| gfx.measure(&self.text[..n].iter().collect::<String>(), &big);
        let (a, b) = self.range();
        let (sel_x0, sel_x1, caret_x) = (prefix_w(a), prefix_w(b), prefix_w(self.caret));

        // Where each row (and any section header above it) sits.
        let mut layout: Vec<(usize, Rect, bool)> = Vec::new();
        if panel != Mode::Apps {
            let mut y = body.y + PILL_H + 10.0;
            let mut last = "";
            for (i, row) in rows.iter().enumerate() {
                let header = row.section != last;
                if y + ROW_H + if header { HEADER_H } else { 0.0 } > SHADOW + PILL_H + MAX_LIST {
                    break;
                }
                if header {
                    last = row.section;
                    y += HEADER_H;
                }
                layout.push((i, Rect::new(body.x + 8.0, y, body.w - 16.0, ROW_H), header));
                y += ROW_H;
            }
        }
        let target = layout.iter().find(|(i, ..)| *i == selected).map(|(_, r, _)| *r);
        let glide = self.glide.update(target);

        // The Apps panel: sections of icons (or a list), scrolled.
        let mut app_cells: Vec<(Rect, usize, f32)> = Vec::new();
        let mut headers: Vec<(Rect, &'static str, Option<(usize, bool)>)> = Vec::new();
        let mut shown: Vec<String> = Vec::new();
        if panel == Mode::Apps {
            self.layout_apps(body, &mut app_cells, &mut headers, &mut shown);
            let top = body.y + PILL_H + 8.0 - self.apps.scroll;
            self.apps.max_scroll = (Self::apps_extent(&app_cells, top) + 16.0 - APPS_H).max(0.0);
            // Scroll along with an unfolding section (keeping its heading in view).
            if let Some(si) = self.apps.follow {
                let at = headers.iter().position(|h| h.2.is_some_and(|m| m.0 == si));
                if let Some(k) = at {
                    let header_y = headers[k].0.y;
                    let end = headers.get(k + 1).map_or_else(|| Self::apps_extent(&app_cells, top) + top, |h| h.0.y - 8.0);
                    let view_top = body.y + PILL_H + 8.0;
                    let view_bottom = body.y + PILL_H + APPS_H - 8.0;
                    let need = (end - view_bottom).min(header_y - view_top).max(0.0);
                    if need > 0.5 {
                        self.apps.scroll_to = (self.apps.scroll + need).min(self.apps.max_scroll).max(self.apps.scroll_to);
                    }
                }
                if self.apps.reveal.get(&si).is_none_or(|r| r.settled()) {
                    self.apps.follow = None;
                }
            }
        }
        self.apps.shown = shown;
        self.apps.selected = self.apps.selected.min(self.apps.shown.len().saturating_sub(1));
        let app_target = app_cells.iter().find(|(_, i, _)| *i == self.apps.selected).map(|(c, _, _)| {
            if self.apps.list { *c } else { Rect::new(c.cx() - APP_ICON / 2.0, c.y + 6.0, APP_ICON, APP_ICON).inset(-7.0, -7.0) }
        });
        let app_glide = if panel == Mode::Apps { self.app_glide.update(app_target) } else { None };
        let fade = self.fade.value.clamp(0.0, 1.0);

        let mode = self.mode;
        let bitmaps = &mut self.bitmaps;
        let app_icons = &mut self.app_icons;
        let apps = &self.apps;
        let scale = self.surface.scale;
        let mut hits = Vec::new();
        let mut app_hits = Vec::new();
        let mut more_hits = Vec::new();
        let mut view_hit = None;
        let names: HashMap<&str, &str> = if panel == Mode::Apps { apps.all.iter().map(|a| (a.1.as_str(), a.0.as_str())).collect() } else { HashMap::new() };

        let part = self.painted.replace(body).map(|was| was.union(body).inset(-BODY_REACH, -BODY_REACH));
        let r = self.surface.render_part(gfx, part, |p: &Painter| {
            // Shadow, then the tinted body over the glass.
            p.soft_shadow(body, radius, 12, 2.0, 0.035, 8.0, theme.shadow);
            p.clear_round(body.inset(3.0, 3.0), radius - 3.0);
            p.fill_round(body, radius, if has_backdrop { theme.menu_tint.alpha(0.28) } else { theme.menu_tint.alpha(1.0) });
            p.stroke_round(body.inset(0.25, 0.25), radius, theme.menu_border, 0.5);

            // The field: a magnifier (or the mode's glyph), the text, the caret.
            let gy = body.y + PILL_H / 2.0;
            let gx = body.x + 24.0;
            match mode {
                Mode::Search => {
                    p.stroke_circle(gx - 1.0, gy - 1.5, 7.0, theme.text_secondary, 1.8);
                    p.line(gx + 4.2, gy + 3.7, gx + 9.0, gy + 8.5, theme.text_secondary, 2.2);
                }
                m => {
                    let r = Rect::new(gx - 12.0, gy - 12.0, 24.0, 24.0);
                    draw_mode_glyph(p, m, r, theme.text_secondary, &mode_font);
                }
            }
            let field = Rect::new(body.x + 46.0, body.y, body.w - 64.0, PILL_H);
            p.clipped(field, || {
                if query.is_empty() {
                    p.text(mode.placeholder(), &big, field, theme.text_disabled);
                } else if a != b {
                    p.fill_rect(Rect::new(field.x + sel_x0, field.cy() - 13.0, sel_x1 - sel_x0, 26.0), theme.accent.alpha(0.35));
                }
                p.text(&query, &big, field, theme.text);
                if a == b {
                    let cx = field.x + caret_x + 0.5;
                    p.line(cx, field.cy() - 12.0, cx, field.cy() + 12.0, theme.accent, 1.5);
                }
            });
            if mode == Mode::Apps && body.w > W - 1.0 {
                // ⋯ in a circle: Grid or List.
                let (cx, cy) = (body.right() - 26.0, gy);
                p.stroke_circle(cx, cy, 9.0, theme.text_secondary, 1.3);
                for dx in [-4.0, 0.0, 4.0] {
                    p.fill_circle(cx + dx, cy, 1.2, theme.text_secondary);
                }
                view_hit = Some(Rect::new(cx - 14.0, cy - 14.0, 28.0, 28.0));
            }

            if body.h <= PILL_H + 1.0 {
                return;
            }
            p.line(body.x + 14.0, body.y + PILL_H, body.right() - 14.0, body.y + PILL_H, theme.menu_separator, 1.0);
            let content = Rect::new(body.x, body.y + PILL_H + 1.0, body.w, body.h - PILL_H - 1.0);

            if panel == Mode::Apps {
                p.clipped(content.inset(0.0, 2.0), || p.faded(fade, || {
                    if let Some((g, a)) = app_glide {
                        p.faded(a, || {
                            if apps.list {
                                p.fill_round(g, 10.0, theme.accent);
                            } else {
                                p.fill_round(g, 16.0, theme.text.alpha(0.1));
                            }
                        });
                    }
                    for (hr, title, more) in &headers {
                        if hr.bottom() < content.y || hr.y > content.bottom() {
                            continue;
                        }
                        p.text(title, &header_font, Rect::new(hr.x, hr.y, hr.w * 0.6, hr.h), theme.text);
                        if let Some((sec, open)) = more {
                            let mr = Rect::new(hr.right() - 90.0, hr.y, 90.0, hr.h);
                            p.text(if *open { "Show Less" } else { "Show More" }, &more_font, mr, theme.text_secondary);
                            more_hits.push((mr, *sec));
                        }
                    }
                    for &(cell, i, alpha) in &app_cells {
                        if cell.bottom() < content.y || cell.y > content.bottom() || alpha <= 0.01 {
                            continue;
                        }
                        // Rows a section unfolds fade and slide down into place.
                        let cell = cell.offset(0.0, -10.0 * (1.0 - alpha));
                        p.faded(alpha, || {
                        let target = &apps.shown[i];
                        let sel = i == apps.selected;
                        let name = names.get(target.as_str()).copied().unwrap_or("");
                        if apps.list {
                            let ir = Rect::new(cell.x + 10.0, cell.cy() - 13.0, 26.0, 26.0);
                            if let Some(b) = crate::launchpad::icon(p, loader, app_icons, target, ir.w, scale) {
                                p.stamp(&b, ir.x, ir.y, scale);
                            }
                            p.text(name, &title_font, Rect::new(ir.right() + 12.0, cell.y, cell.w - 60.0, cell.h), if sel { Color::WHITE } else { theme.text });
                        } else {
                            let ir = Rect::new(cell.cx() - APP_ICON / 2.0, cell.y + 6.0, APP_ICON, APP_ICON);
                            if let Some(b) = crate::launchpad::icon(p, loader, app_icons, target, APP_ICON, scale) {
                                p.stamp(&b, ir.x, ir.y, scale);
                            }
                            let lr = Rect::new(cell.x + 4.0, ir.bottom() + 6.0, cell.w - 8.0, 28.0);
                            p.clipped(lr, || p.text(name, &app_label, lr, theme.text));
                        }
                        });
                        if alpha > 0.5 {
                            app_hits.push((cell, i));
                        }
                    }
                }));
                return;
            }

            // Results. Each row draws in its plain colours or (`sel`) for under the highlight.
            let draw_row = |p: &Painter, loader: &mut IconLoader, bitmaps: &mut HashMap<String, Option<ID2D1Bitmap>>, i: usize, r: Rect, sel: bool| {
                let row = &rows[i];
                let tc = if sel { Color::WHITE } else { theme.text };
                let sc = if sel { Color::gray(255, 0.8) } else { theme.text_secondary };
                let icon_r = Rect::new(r.x + 10.0, r.cy() - 12.0, 24.0, 24.0);
                match &row.icon {
                    Some((key, path)) => {
                        let bmp = match bitmaps.get(key) {
                            Some(b) => b.clone(),
                            None => {
                                let b = loader.get(key, Path::new(path), false, ICON_PX).and_then(|px| px.to_bitmap(p.rt));
                                if b.is_some() {
                                    bitmaps.insert(key.clone(), b.clone());
                                }
                                b
                            }
                        };
                        if let Some(b) = bmp {
                            p.bitmap(&b, icon_r, 1.0);
                        }
                    }
                    None => match &row.badge {
                        Some(Badge::Calculator) => {
                            p.fill_round(icon_r.inset(1.0, 1.0), 6.0, Color::rgba(255, 149, 0, 1.0));
                            p.line(icon_r.cx() - 5.0, icon_r.cy() - 2.5, icon_r.cx() + 5.0, icon_r.cy() - 2.5, Color::WHITE, 1.8);
                            p.line(icon_r.cx() - 5.0, icon_r.cy() + 2.5, icon_r.cx() + 5.0, icon_r.cy() + 2.5, Color::WHITE, 1.8);
                        }
                        Some(Badge::Glyph(g, bg)) => {
                            p.fill_round(icon_r.inset(1.0, 1.0), 6.0, *bg);
                            p.text(&g.to_string(), &glyph_font, icon_r, Color::WHITE);
                        }
                        Some(Badge::Text) => {
                            let tile = if sel { Color::gray(255, 0.92) } else if theme.dark { Color::gray(255, 0.14) } else { Color::WHITE };
                            let page = Rect::new(icon_r.cx() - 8.0, icon_r.y + 1.0, 16.0, 22.0);
                            p.fill_round(page, 3.0, tile);
                            if !sel && !theme.dark {
                                p.stroke_round(page.inset(0.25, 0.25), 3.0, Color::gray(0, 0.16), 0.5);
                            }
                            let ink = if sel || !theme.dark { Color::gray(0, 0.4) } else { Color::gray(255, 0.5) };
                            for k in 0..4 {
                                let ly = page.y + 5.0 + k as f32 * 4.0;
                                p.line(page.x + 3.5, ly, page.right() - if k == 3 { 7.0 } else { 3.5 }, ly, ink, 1.0);
                            }
                        }
                        Some(Badge::Symbol(sym)) => {
                            let tile = if sel { Color::gray(255, 0.92) } else if theme.dark { Color::gray(255, 0.12) } else { Color::WHITE };
                            p.fill_round(icon_r, 6.0, tile);
                            if !sel && !theme.dark {
                                p.stroke_round(icon_r.inset(0.25, 0.25), 6.0, Color::gray(0, 0.12), 0.5);
                            }
                            let ink = if sel || !theme.dark { Color::gray(0, 0.88) } else { Color::WHITE };
                            let font = if sym.chars().count() > 2 { &symbol_small } else { &symbol_font };
                            p.text(sym, font, icon_r.inset(-6.0, 0.0), ink);
                        }
                        Some(Badge::Web) | None => {
                            // Web: a globe.
                            let (cx, cy) = (icon_r.cx(), icon_r.cy());
                            let c = if sel { Color::WHITE } else { theme.accent };
                            p.stroke_circle(cx, cy, 10.0, c, 1.6);
                            p.line(cx - 10.0, cy, cx + 10.0, cy, c, 1.2);
                            p.line(cx, cy - 10.0, cx, cy + 10.0, c, 1.2);
                        }
                    },
                }
                let title_w = if row.subtitle.is_empty() { r.w - 60.0 } else { r.w * 0.58 };
                p.text(&row.title, &title_font, Rect::new(icon_r.right() + 12.0, r.y, title_w, r.h), tc);
                if !row.subtitle.is_empty() {
                    let sub = Rect::new(icon_r.right() + 12.0 + title_w + 8.0, r.y, r.right() - 12.0 - (icon_r.right() + 20.0 + title_w), r.h);
                    p.text(&row.subtitle, &sub_font, sub, sc);
                }
            };
            p.clipped(content, || p.faded(fade, || {
                for &(i, r, header) in &layout {
                    if header {
                        let hr = Rect::new(body.x + 22.0, r.y - HEADER_H, body.w - 44.0, HEADER_H);
                        p.text(rows[i].section, &header_font, hr.offset(0.0, 3.0), theme.text_secondary);
                    }
                    draw_row(p, loader, bitmaps, i, r, false);
                    hits.push((r, i));
                }
                // The selection glides between rows; what it covers is redrawn in its
                // colours, clipped to it, so text turns exactly where the highlight is.
                if let Some((g, a)) = glide {
                    p.faded(a, || {
                        p.fill_round(g, 10.0, theme.accent);
                        p.clipped(g, || {
                            for &(i, r, _) in &layout {
                                if r.bottom() > g.y && r.y < g.bottom() {
                                    draw_row(p, loader, bitmaps, i, r, true);
                                }
                            }
                        });
                    });
                }
            }));
        });
        // Nothing in a closing panel takes clicks.
        if closing {
            hits.clear();
            app_hits.clear();
            more_hits.clear();
        }
        self.hits = hits;
        self.app_hits = app_hits;
        self.more_hits = more_hits;
        self.view_hit = view_hit;
        if ghost.is_some() {
            self.shown = ghost;
        }
        if let Err(e) = r {
            log!("spotlight render failed: {e}");
        }
        self.render_bubbles(gfx, theme, body);
    }

    /// Lays out the Apps panel: section headers and app cells (indexes into `shown`).
    fn layout_apps(&self, body: Rect, cells: &mut Vec<(Rect, usize, f32)>, headers: &mut Vec<(Rect, &'static str, Option<(usize, bool)>)>, shown: &mut Vec<String>) {
        let q = self.query().trim().to_lowercase();
        let inner = Rect::new(body.x + 14.0, body.y + PILL_H + 8.0 - self.apps.scroll, W - 28.0, 0.0);
        let mut y = inner.y;
        let list = self.apps.list;
        let cell_w = inner.w / APP_COLS as f32;
        // Lays out `targets`, the first `fixed` rows always shown and the rest
        // unfolded `r` (0..1) of the way: the section grows by that much, and
        // each further row fades in as the growth reaches it.
        let put = |targets: &[String], fixed: usize, r: f32, y: &mut f32, shown: &mut Vec<String>, cells: &mut Vec<(Rect, usize, f32)>| {
            let (row_h, per_row) = if list { (ROW_H, 1) } else { (APP_CELL_H, APP_COLS) };
            let rows = targets.len().div_ceil(per_row);
            let extra = rows.saturating_sub(fixed) as f32 * r;
            for (k, t) in targets.iter().enumerate() {
                let (c, row) = (k % per_row, k / per_row);
                let alpha = if row < fixed { 1.0 } else { (extra - (row - fixed) as f32 - c as f32 * 0.04).clamp(0.0, 1.0) };
                if alpha <= 0.0 {
                    continue;
                }
                let rect = if list {
                    Rect::new(inner.x - 6.0, *y + row as f32 * ROW_H, inner.w + 12.0, ROW_H)
                } else {
                    Rect::new(inner.x + c as f32 * cell_w, *y + row as f32 * APP_CELL_H, cell_w, APP_CELL_H)
                };
                cells.push((rect, shown.len(), alpha));
                shown.push(t.clone());
            }
            *y += (rows.min(fixed) as f32 + extra) * row_h;
        };
        if !q.is_empty() {
            // Typing: one flat set of matches, best first.
            let mut scored: Vec<(i32, &String)> =
                self.apps.all.iter().filter_map(|(n, t, _)| index::score_text(&n.to_lowercase(), &q).map(|s| (s, t))).collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            let targets: Vec<String> = scored.into_iter().map(|(_, t)| t.clone()).collect();
            put(&targets, usize::MAX, 1.0, &mut y, shown, cells);
            return;
        }
        let mut sections: Vec<(&'static str, Vec<String>)> = Vec::new();
        if !self.apps.suggestions.is_empty() {
            sections.push(("Suggestions", self.apps.suggestions.clone()));
        }
        for cat in Category::ALL {
            let v: Vec<String> = self.apps.all.iter().filter(|a| a.2 == cat).map(|a| a.1.clone()).collect();
            if !v.is_empty() {
                sections.push((cat.name(), v));
            }
        }
        for (si, (title, targets)) in sections.into_iter().enumerate() {
            let open = self.apps.expanded.contains(&si);
            // Collapsed: one row of icons (four in the list).
            let fixed = if list { 4 } else { 1 };
            let limit = if list { 4 } else { APP_COLS };
            let more = (targets.len() > limit).then_some((si, open));
            headers.push((Rect::new(inner.x, y, inner.w, 26.0), title, more));
            y += 28.0;
            let r = self.apps.reveal.get(&si).map_or(if open { 1.0 } else { 0.0 }, |s| s.value.clamp(0.0, 1.0));
            put(&targets, fixed, r, &mut y, shown, cells);
            y += 8.0;
        }
        let _ = y;
    }

    /// How far the Apps panel can scroll, from its laid-out content.
    fn apps_extent(cells: &[(Rect, usize, f32)], top: f32) -> f32 {
        cells.iter().map(|(c, ..)| c.bottom()).fold(top, f32::max) - top
    }

    /// Draws and places the bubbles beside the capsule.
    fn render_bubbles(&mut self, gfx: &Gfx, theme: &Theme, body: Rect) {
        let glyph_font = gfx.format("Segoe Fluent Icons", 18.0, Weight::Regular, Align::Center);
        let anchor = Rect::new(SHADOW, SHADOW, W, PILL_H);
        let mut bubble_hits = Vec::new();
        for (i, b) in self.bubbles.iter_mut().enumerate() {
            let p = b.p.value;
            let alpha = (p * 1.7).clamp(0.0, 1.0);
            b.surface.set_opacity(alpha);
            // Tucked away: hidden, so it doesn't catch clicks meant for the capsule.
            // Its glass doesn't fade, so on the way in it goes as soon as what's
            // on it has (not once the spring has rung out, a ring left on the capsule).
            let away = if b.p.target <= 0.0 { p <= 0.02 } else { p <= 0.0 };
            b.surface.set_hidden(away);
            if away {
                continue;
            }
            let r = Self::bubble_rect(anchor, i, p);
            let glyph = ((p - 0.35) * 2.0).clamp(0.0, 1.0);
            if b.drawn == Some((r, b.hover, glyph)) {
                bubble_hits.push((r, i));
                continue;
            }
            // Only where the bubble (and its shadow) was and is: its window
            // spans the whole capsule.
            let part = b.drawn.map(|(was, ..)| was.union(r).inset(-BUBBLE_REACH, -BUBBLE_REACH));
            b.drawn = Some((r, b.hover, glyph));
            b.surface.set_backdrop_shape(r, BUBBLE / 2.0);
            let has_backdrop = b.surface.backdrop.is_some();
            let (mode, hover) = (b.mode, b.hover);
            let res = b.surface.render_part(gfx, part, |pt: &Painter| {
                pt.soft_shadow(r, r.h / 2.0, 8, 2.0, 0.03, 5.0, theme.shadow);
                pt.clear_round(r.inset(2.0, 2.0), r.h / 2.0 - 2.0);
                pt.fill_round(r, r.h / 2.0, if has_backdrop { theme.menu_tint.alpha(0.28) } else { theme.menu_tint.alpha(1.0) });
                if hover {
                    pt.fill_round(r, r.h / 2.0, theme.text.alpha(0.08));
                }
                pt.stroke_round(r.inset(0.25, 0.25), r.h / 2.0, theme.menu_border, 0.5);
                let g = Rect::new(r.cx() - 12.0, r.cy() - 12.0, 24.0, 24.0);
                pt.faded(((p - 0.35) * 2.0).clamp(0.0, 1.0), || draw_mode_glyph(pt, mode, g, theme.text, &glyph_font));
            });
            if let Err(e) = res {
                log!("spotlight bubble render failed: {e}");
            }
            bubble_hits.push((r, i));
        }
        let _ = body;
        self.bubble_hits = bubble_hits;
    }
}

/// The browse modes' symbols: the App Store's "A" for Apps, then a folder,
/// stacked layers (Shortcuts) and a clipboard.
fn draw_mode_glyph(p: &Painter, mode: Mode, r: Rect, c: Color, font: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat) {
    match mode {
        Mode::Apps => {
            let (cx, cy, s) = (r.cx(), r.cy(), r.w * 0.36);
            // Two strokes leaning together and a crossbar, like the App Store's.
            p.line(cx - s * 0.95, cy + s * 0.95, cx + s * 0.15, cy - s * 0.95, c, 1.8);
            p.line(cx + s * 0.95, cy + s * 0.95, cx - s * 0.15, cy - s * 0.95, c, 1.8);
            p.line(cx - s * 1.0, cy + s * 0.35, cx + s * 1.0, cy + s * 0.35, c, 1.8);
        }
        Mode::Files => p.text("\u{E8B7}", font, r, c),
        Mode::Actions => p.text("\u{E81E}", font, r, c),
        Mode::Clipboard => p.text("\u{E77F}", font, r, c),
        Mode::Search => p.text("\u{E721}", font, r, c),
    }
}

fn item_row(section: &'static str, it: &Item) -> Row {
    let (subtitle, action) = match it.kind {
        Kind::App => ("Application".to_string(), RowAction::Launch(it.target.clone())),
        Kind::Folder => (it.target.clone(), RowAction::OpenFolder(it.target.clone())),
        Kind::File => (
            Path::new(&it.target).parent().map(|p| p.display().to_string()).unwrap_or_default(),
            RowAction::OpenFile(it.target.clone()),
        ),
    };
    Row { section, title: it.name.clone(), subtitle, action, icon: Some(icon_for(it)), badge: None }
}

fn command_row(c: commands::Found) -> Row {
    Row {
        section: "Commands",
        title: c.title,
        subtitle: c.subtitle.into(),
        action: RowAction::Command(c.command),
        icon: None,
        badge: Some(Badge::Glyph(c.glyph, Color::rgba(99, 99, 102, 1.0))),
    }
}

fn settings_row(title: &str, uri: &str) -> Row {
    Row {
        section: "System Settings",
        title: title.to_string(),
        subtitle: "System Settings".into(),
        action: RowAction::Url(uri.to_string()),
        icon: Some((format!("app:{SETTINGS_APP}"), SETTINGS_APP.to_string())),
        badge: None,
    }
}

/// Runs a command: an app `Action` for the shell-level ones, the rest directly.
fn command_action(c: commands::Command) -> Option<Action> {
    use crate::win::controls;
    use crate::win::system::Shortcut;
    use commands::Command;
    const WIN_D: Shortcut = Shortcut::new("w", b'D' as u16, "D");
    const WIN_SHIFT_S: Shortcut = Shortcut::new("ws", b'S' as u16, "S");
    let volume = |level: f32| {
        let _ = crate::island::sources::set_volume(level);
    };
    Some(match c {
        Command::Lock => Action::Lock,
        Command::Sleep => Action::Sleep,
        Command::Restart => Action::Restart,
        Command::ShutDown => Action::ShutDown,
        Command::LogOut => Action::LogOut,
        Command::EmptyTrash => Action::EmptyTrash,
        Command::ShowDesktop => Action::Keys(WIN_D),
        Command::Screenshot => Action::Keys(WIN_SHIFT_S),
        Command::TaskManager => Action::Launch { path: "taskmgr.exe".into(), args: None },
        Command::ControlPanel => Action::Launch { path: "control.exe".into(), args: None },
        Command::DockSideSettings => Action::OpenSettings(None),
        Command::SetupAssistant => Action::OpenOnboarding,
        Command::QuitDockSide => Action::Quit,
        Command::LowPowerMode(on) => {
            controls::set_low_power_mode(on);
            return None;
        }
        Command::PowerMode(m) => {
            controls::set_power_mode(m);
            return None;
        }
        Command::DarkMode(dark) => Action::SetDarkMode(dark),
        Command::Mute(mute) => {
            use crate::island::sources::{press_volume, volume as current, VolumeKey};
            if current().is_some_and(|(_, muted)| muted != mute) {
                let _ = press_volume(VolumeKey::Mute);
            }
            return None;
        }
        Command::Volume(n) => {
            volume(n as f32 / 100.0);
            return None;
        }
        Command::VolumeStep(d) => {
            let now = crate::island::sources::volume().map(|v| v.0).unwrap_or(0.5);
            volume(now + d as f32 / 100.0);
            return None;
        }
        Command::Brightness(n) => {
            controls::brightness(Some(n), 0);
            return None;
        }
        Command::BrightnessStep(d) => {
            controls::brightness(None, d);
            return None;
        }
    })
}

impl Drop for Spotlight {
    fn drop(&mut self) {
        // A panel fading out may outlive the next one to open: only clear our own handle.
        let me = self.surface.content.0 as isize;
        let _ = SPOTLIGHT_HWND.compare_exchange(me, 0, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed);
    }
}
