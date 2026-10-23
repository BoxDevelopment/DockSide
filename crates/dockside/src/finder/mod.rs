//! Finder: a macOS-style file manager window with a sidebar, back/forward,
//! tabs, and icon / list / column views.

pub mod dnd;
pub mod fs;
mod glyphs;
pub mod loader;
pub mod text;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use windows::core::{Result, HSTRING};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap, D2D1_DRAW_TEXT_OPTIONS_NONE};
use windows::Win32::Graphics::DirectWrite::{
    IDWriteTextFormat, DWRITE_HIT_TEST_METRICS, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, GetDoubleClickTime, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

use self::fs::{Entry as FileEntry, KindCache, Place, SortKey};
use self::loader::IconLoader;
use self::text::{KeyResult, TextField};
use crate::anim::{feel, lerp, Glide, Motion, Scroll, Spring};
use crate::chrome::{self, Light, SidebarBackdrop, WindowAnim, SHADOW};
use crate::config::FinderCfg;
use crate::geom::{Color, Rect};
use crate::gfx::{pt as gfx_pt, Align, Canvas, Gfx, Painter, Weight};
use crate::menu::{Action, Entry};
use crate::theme::Theme;
use crate::win::{self, Monitor};

/// Dock path that opens a Finder window instead of launching a program.
pub const FINDER_PATH: &str = "dockside:finder";
/// Same, as written in configs from before the rename to DockSide.
pub const LEGACY_FINDER_PATH: &str = "macfinder:finder";

const W: f32 = 940.0;
const H: f32 = 580.0;
const MIN_W: f32 = 560.0;
const MIN_H: f32 = 340.0;
const SIDEBAR: f32 = 190.0;
const TOOLBAR: f32 = 56.0;
/// Tab bar under the toolbar, shown while a window has more than one tab.
const TABBAR: f32 = 28.0;
const NEW_TAB_W: f32 = 30.0;
const STATUS: f32 = 24.0;
const PATHBAR: f32 = 24.0;
/// Toolbar controls: height, and their vertical centre (lined up with the traffic lights).
const CONTROL_H: f32 = 34.0;
const CONTROL_CY: f32 = 28.0;
const ROW: f32 = 24.0;
const HEADER: f32 = 26.0;
const CELL_W: f32 = 104.0;
const CELL_H: f32 = 108.0;
const ICON: f32 = 64.0;
const COL_W: f32 = 240.0;
const PREVIEW_W: f32 = 300.0;
const SEARCH_W: f32 = 180.0;
/// Inner padding of the icon grid.
const GRID_PAD: f32 = 12.0;
// Virtual-key codes.
const BACK: u16 = 0x08;
const TAB: u16 = 0x09;
const ENTER: u16 = 0x0D;
const ESC: u16 = 0x1B;
const SPACE: u16 = 0x20;
const END: u16 = 0x23;
const HOME: u16 = 0x24;
const LEFT: u16 = 0x25;
const UP: u16 = 0x26;
const RIGHT: u16 = 0x27;
const DOWN: u16 = 0x28;
const DEL: u16 = 0x2E;
const F2: u16 = 0x71;
const F5: u16 = 0x74;
const LBRACKET: u16 = 0xDB;
const RBRACKET: u16 = 0xDD;
const PERIOD: u16 = 0xBE;

/// Width of the invisible resize border inside the body edge.
const EDGE: f32 = 6.0;
/// How long the overlay scrollbar lingers after scrolling stops, in seconds.
const SCROLLBAR_LINGER: f32 = 0.9;

/// Everything in a Finder window that animates, keyed for [`Motion`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Anim {
    Hover(Hit),
    Scrollbar,
    ViewPill,
    SearchW,
    TabBar,
    Lights,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    Icons,
    List,
    Columns,
}

impl View {
    /// Name used in config.toml.
    pub fn name(self) -> &'static str {
        match self {
            View::Icons => "icons",
            View::List => "list",
            View::Columns => "columns",
        }
    }
}

#[derive(Clone, Debug)]
pub enum FinderCmd {
    Open(PathBuf),
    /// Open a folder in a new tab of this window.
    OpenTab(PathBuf),
    Reveal(PathBuf),
    /// Open a folder in File Explorer itself.
    Explorer(PathBuf),
    CopyPath(Vec<PathBuf>),
    Trash(Vec<PathBuf>),
    NewFolder,
    Rename,
    SetView(View),
    Sort(SortKey),
    Terminal(PathBuf),
    /// Drag the items out with OLE drag and drop (modal).
    Drag(Vec<PathBuf>),
    /// Copy or move dropped items into a folder (modal: the shell shows progress).
    Transfer { paths: Vec<PathBuf>, dest: PathBuf, copy: bool },
    /// Re-list and reset mouse state, e.g. after a modal drag.
    Reload,
}

impl FinderCmd {
    /// Commands that run a modal loop and so must run outside the App borrow (see [`run_modal`]).
    pub fn is_modal(&self) -> bool {
        matches!(self, FinderCmd::Drag(_) | FinderCmd::Transfer { .. })
    }
}

/// Runs a modal command for the Finder window `hwnd`; follow up with [`FinderCmd::Reload`].
pub fn run_modal(hwnd: HWND, cmd: FinderCmd) {
    match cmd {
        FinderCmd::Drag(paths) => {
            dnd::drag_out(hwnd, &paths);
        }
        FinderCmd::Transfer { paths, dest, copy } => {
            fs::transfer(&paths, &dest, copy, hwnd);
        }
        _ => {}
    }
}

pub enum FinderEvent {
    None,
    Close,
    NewWindow,
    /// Context menu entries and the screen point (points) to show them at.
    Menu(Vec<Entry>, f32, f32),
    /// A modal command to run once the current message is handled.
    Run(FinderCmd),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Hit {
    Light(Light),
    Back,
    Forward,
    ViewBtn(View),
    Place(usize),
    Item(usize),
    Column(usize, usize),
    ColumnBg(usize),
    Header(SortKey),
    Content,
    Search,
    SearchClear,
    /// Quick Look: dimmed area around the panel, the panel, and its buttons.
    QlBackdrop,
    QlPanel,
    QlClose,
    QlOpen,
    Tab(usize),
    TabClose(usize),
    NewTab,
    /// Path bar segment: the cwd ancestor at this depth (0 = root).
    Crumb(usize),
}

struct Column {
    dir: PathBuf,
    entries: Vec<FileEntry>,
    selected: Option<usize>,
    scroll: Scroll,
    error: Option<String>,
}

/// Inline rename field.
struct Rename {
    target: PathBuf,
    field: TextField,
}

/// Rubber-band selection in icon and list views.
struct Band {
    /// Start point in content coordinates (window y + scroll at the time).
    x0: f32,
    y0: f32,
    /// Current mouse point in window coordinates.
    x1: f32,
    y1: f32,
    /// Selection before the drag; Ctrl toggles against it, Shift adds to it.
    base: BTreeSet<usize>,
    toggle: bool,
}

/// Quick Look state: the previewed file's cached text and scroll.
#[derive(Default)]
struct QuickLook {
    path: PathBuf,
    text: Option<String>,
    scroll: Scroll,
    max_scroll: f32,
}

/// A background tab's folder state, swapped into the window's fields when it's selected.
struct Tab {
    cwd: PathBuf,
    history: Vec<PathBuf>,
    hist_pos: usize,
    listing: Vec<FileEntry>,
    entries: Vec<FileEntry>,
    error: Option<String>,
    free: Option<u64>,
    view: View,
    sort: SortKey,
    ascending: bool,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    cursor: Option<usize>,
    columns: Vec<Column>,
    active_col: usize,
    scroll: f32,
    search: TextField,
}

/// Per-render context the App lends to a Finder window.
pub struct Ctx<'a> {
    pub gfx: &'a Gfx,
    pub theme: &'a Theme,
    pub loader: &'a mut IconLoader,
    pub kinds: &'a mut KindCache,
}

pub struct FinderWindow {
    pub id: u32,
    pub hwnd: HWND,
    canvas: Canvas,
    scale: f32,
    cwd: PathBuf,
    history: Vec<PathBuf>,
    hist_pos: usize,
    /// Everything in the folder; `entries` is this filtered by the search field.
    listing: Vec<FileEntry>,
    entries: Vec<FileEntry>,
    error: Option<String>,
    free: Option<u64>,
    view: View,
    sort: SortKey,
    ascending: bool,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    /// Keyboard focus item (moves with arrows; `anchor` is where shift-selection started).
    cursor: Option<usize>,
    columns: Vec<Column>,
    active_col: usize,
    /// Smooth (eased) scrolling of the icon and list views.
    scroll: Scroll,
    max_scroll: f32,
    grid_cols: usize,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    last_click: Option<(Instant, Hit)>,
    rename: Option<Rename>,
    search: TextField,
    search_focus: bool,
    /// Caret x positions (window points) for each index of the search text, from the last render.
    search_xs: Vec<f32>,
    band: Option<Band>,
    /// Where an item press started, until it turns into a drag or a click.
    drag_from: Option<(f32, f32)>,
    /// Plain click on an already-selected item: reduce the selection to it on release
    /// (not on press, so the whole selection can be dragged).
    pending_select: Option<usize>,
    /// What a drag hovering over the window would drop onto.
    drop_hit: Option<Hit>,
    quicklook: Option<QuickLook>,
    ql_bitmap: Option<(String, Option<ID2D1Bitmap>)>,
    typed: String,
    typed_at: Instant,
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    favorites: Vec<Place>,
    drives: Vec<Place>,
    watcher: Option<notify::RecommendedWatcher>,
    zoom_restore: Option<RECT>,
    show_hidden: bool,
    /// One slot per tab, in order; the active tab's state lives in the fields above (its slot is `None`).
    tabs: Vec<Option<Tab>>,
    active_tab: usize,
    /// Blurred backdrop behind the translucent sidebar, when vibrancy is on and available.
    backdrop: Option<SidebarBackdrop>,
    /// The view pill, search field, tab bar and scrollbar springs. Anything
    /// following direct input (scrolling, hovering, selecting) stays instant.
    motion: Motion<Anim>,
    /// Open, close and minimize zooms of the whole window.
    window: WindowAnim,
    /// The sidebar's hover highlight and current-place selection, gliding from place to place.
    side_glide: Glide,
    sel_glide: Glide,
    scrolled_at: Instant,
    /// The overlay scrollbar is showing (and needs a redraw to fade once scrolling stops).
    scrollbar_up: bool,
    /// Quick Look zoom (0 -> 1), and the panel still shrinking away after closing.
    ql_t: Spring,
    ql_closing: Option<(QuickLook, FileEntry)>,
    /// Window transform this frame: zoom origin (points), scale and opacity.
    xf: (f32, f32, f32, f32),
    /// The transform and body the sidebar backdrop was last placed for.
    backdrop_for: Option<((f32, f32, f32, f32), Rect)>,
    /// Set by `tick` when only this area is animating (the list scrolling, a
    /// sidebar highlight gliding): the next frame repaints just that.
    part: Option<Rect>,
    /// View, sort or hidden files changed: the App saves them as the defaults for new windows.
    pub prefs_dirty: bool,
    pub focused: bool,
    pub tracking: bool,
    pub dirty: bool,
}

impl FinderWindow {
    /// `notify`/`changed_msg`: where to post (with this window's id) when the folder changes on disk.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: &mut Ctx,
        monitor: &Monitor,
        id: u32,
        path: PathBuf,
        cascade: i32,
        notify: isize,
        changed_msg: u32,
        prefs: &FinderCfg,
    ) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let step = (24.0 * s) as i32 * (cascade % 8);
        let left = mon.left + (mon.right - mon.left - w) / 2 - (60.0 * s) as i32 + step;
        let top = mon.top + (mon.bottom - mon.top - h) / 2 - (20.0 * s) as i32 + step;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let hwnd = win::create_window(
            WS_EX_LAYERED | WS_EX_APPWINDOW,
            WS_POPUP | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX,
            rect,
            HWND::default(),
        )?;

        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok_and(|e| !e.kind.is_access()) {
                win::post_wp(notify, changed_msg, id as usize);
            }
        })
        .map_err(|e| log!("finder watcher failed: {e}"))
        .ok();

        let mut f = Self {
            id,
            hwnd,
            canvas: Canvas::new(ctx.gfx, w, h, s)?,
            scale: s,
            cwd: PathBuf::new(),
            history: Vec::new(),
            hist_pos: 0,
            listing: Vec::new(),
            entries: Vec::new(),
            error: None,
            free: None,
            view: prefs.view,
            sort: prefs.sort,
            ascending: prefs.ascending,
            selected: BTreeSet::new(),
            anchor: None,
            cursor: None,
            columns: Vec::new(),
            active_col: 0,
            scroll: Scroll::default(),
            max_scroll: 0.0,
            grid_cols: 1,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            last_click: None,
            rename: None,
            search: TextField::default(),
            search_focus: false,
            search_xs: Vec::new(),
            band: None,
            drag_from: None,
            pending_select: None,
            drop_hit: None,
            quicklook: None,
            ql_bitmap: None,
            typed: String::new(),
            typed_at: Instant::now(),
            bitmaps: HashMap::new(),
            favorites: fs::favorites(),
            drives: fs::drives(),
            watcher,
            zoom_restore: None,
            show_hidden: prefs.show_hidden,
            tabs: vec![None],
            active_tab: 0,
            backdrop: Self::new_backdrop(hwnd, prefs, ctx.theme),
            motion: Motion::default(),
            window: WindowAnim::opening(),
            side_glide: Glide::default(),
            sel_glide: Glide::selection(),
            scrolled_at: Instant::now() - Duration::from_secs(10),
            scrollbar_up: false,
            ql_t: Spring::new(0.0, 0.2, 1.0),
            ql_closing: None,
            xf: (0.0, 0.0, 1.0, 1.0),
            backdrop_for: None,
            part: None,
            prefs_dirty: false,
            focused: true,
            tracking: false,
            dirty: true,
        };
        f.navigate(path, true, ctx.kinds);
        dnd::register(hwnd, id);
        Ok(f)
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn title(&self) -> String {
        self.title_for(&self.cwd)
    }

    fn title_for(&self, dir: &Path) -> String {
        if let Some(p) = self.favorites.iter().chain(&self.drives).find(|p| p.path == dir) {
            return p.name.clone();
        }
        dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.display().to_string())
    }

    /// Settings that new windows start with: view, sort key, ascending, hidden files.
    pub fn prefs(&self) -> (View, SortKey, bool, bool) {
        (self.view, self.sort, self.ascending, self.show_hidden)
    }

    fn new_backdrop(hwnd: HWND, prefs: &crate::config::FinderCfg, theme: &Theme) -> Option<SidebarBackdrop> {
        SidebarBackdrop::new(theme, hwnd, prefs.sidebar)
    }

    /// Recreates the sidebar backdrop after its settings changed.
    pub fn set_sidebar(&mut self, prefs: &crate::config::FinderCfg, theme: &Theme) {
        self.backdrop = None;
        self.backdrop = Self::new_backdrop(self.hwnd, prefs, theme);
        self.dirty = true;
    }

    /// Keeps the sidebar backdrop under the sidebar (call when the window moves, resizes or changes z-order).
    pub fn sync_backdrop(&mut self) {
        let body = self.body();
        self.backdrop_for = Some((self.xf, body));
        let (ox, oy, k, alpha) = self.xf;
        if let Some(bd) = &mut self.backdrop {
            let r = chrome::sidebar_pane(body, SIDEBAR);
            let r = Rect::new(ox + (r.x - ox) * k, oy + (r.y - oy) * k, r.w * k, r.h * k);
            bd.sync(self.hwnd, r, chrome::PANE_RADIUS * k, self.scale);
            bd.set_opacity(alpha);
        }
    }

    // --- animation ---------------------------------------------------------

    /// Advances animations; returns true while frames are still needed.
    pub fn tick(&mut self, dt: f32) -> bool {
        // What moved decides how much of the window the next frame repaints:
        // the list scrolling or a sidebar highlight gliding touch only their own
        // area; anything else (or a redraw already asked for) repaints it all.
        let was_dirty = self.dirty;
        let mut full = self.motion.tick(dt);
        full |= self.window.tick(dt, self.hwnd);
        full |= self.ql_t.step(dt);
        if let Some(ql) = &mut self.quicklook {
            full |= ql.scroll.tick(dt);
        }
        if self.ql_closing.is_some() && self.ql_t.settled() {
            self.ql_closing = None;
            full = true;
        }
        let (body, vr) = (self.body(), self.view_rect());
        let side = Rect::new(body.x, body.y, SIDEBAR, body.h);
        let mut part: Option<Rect> = None;
        let mut touch = |r: Rect| part = Some(part.map_or(r, |p| p.union(r)));
        let mut list = self.scroll.tick(dt);
        for c in &mut self.columns {
            list |= c.scroll.tick(dt);
        }
        if list {
            touch(vr);
        }
        if self.side_glide.tick(dt) | self.sel_glide.tick(dt) {
            touch(side);
        }
        // While the scrollbar lingers, keep ticking without redrawing; once
        // scrolling has stopped long enough, one redraw starts it fading.
        let lingering = self.scrolled_at.elapsed().as_secs_f32() < SCROLLBAR_LINGER;
        if self.scrollbar_up && !lingering {
            self.scrollbar_up = false;
            full = true;
        }
        let moving = full || part.is_some();
        if moving {
            // A pending redraw stays partial only if it was partial too.
            let pending_full = was_dirty && self.part.is_none();
            self.part = match (full || pending_full || self.quicklook.is_some(), part, self.part) {
                (true, ..) => None,
                (false, Some(p), Some(q)) => Some(p.union(q)),
                (false, p, q) => p.or(q),
            };
            self.dirty = true;
        }
        moving || lingering
    }

    /// Whether the last render started something that needs frames.
    pub fn animating(&self) -> bool {
        self.motion.moving()
            || self.window.moving()
            || !self.ql_t.settled()
            || self.scroll.moving()
            || self.side_glide.moving()
            || self.sel_glide.moving()
            || self.columns.iter().any(|c| c.scroll.moving())
            || self.quicklook.as_ref().is_some_and(|q| q.scroll.moving())
            || self.scrollbar_up
    }

    /// Fades and zooms the window away; the App drops it once [`Self::closed`].
    pub fn begin_close(&mut self) {
        self.window.close();
        self.dirty = true;
        unsafe {
            let ex = GetWindowLongW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_TRANSPARENT.0 as i32);
        }
        dnd::revoke(self.hwnd);
    }

    pub fn closed(&self) -> bool {
        self.window.closed()
    }

    fn begin_minimize(&mut self) {
        self.window.minimize();
        self.dirty = true;
    }

    fn close_quicklook(&mut self) {
        let entry = self.ql_entry().cloned();
        if let (Some(ql), Some(e)) = (self.quicklook.take(), entry) {
            self.ql_closing = Some((ql, e));
            self.ql_t.set(0.0);
        }
        self.dirty = true;
    }

    // --- tabs --------------------------------------------------------------

    /// The tab bar slides open with a second tab and closes with the last but one.
    fn tab_h(&self) -> f32 {
        let want = if self.tabs.len() > 1 { TABBAR } else { 0.0 };
        self.motion.get(Anim::TabBar, want, feel::SMOOTH).max(0.0)
    }

    fn tab_dir(&self, i: usize) -> Option<&Path> {
        if i == self.active_tab {
            Some(&self.cwd)
        } else {
            self.tabs.get(i)?.as_ref().map(|t| t.cwd.as_path())
        }
    }

    /// Moves the active tab's state out of the window's fields.
    fn stash(&mut self) -> Tab {
        use std::mem::take;
        Tab {
            cwd: take(&mut self.cwd),
            history: take(&mut self.history),
            hist_pos: self.hist_pos,
            listing: take(&mut self.listing),
            entries: take(&mut self.entries),
            error: self.error.take(),
            free: self.free,
            view: self.view,
            sort: self.sort,
            ascending: self.ascending,
            selected: take(&mut self.selected),
            anchor: self.anchor,
            cursor: self.cursor,
            columns: take(&mut self.columns),
            active_col: self.active_col,
            scroll: self.scroll.target(),
            search: take(&mut self.search),
        }
    }

    fn restore(&mut self, t: Tab) {
        self.cwd = t.cwd;
        self.history = t.history;
        self.hist_pos = t.hist_pos;
        self.listing = t.listing;
        self.entries = t.entries;
        self.error = t.error;
        self.free = t.free;
        self.view = t.view;
        self.sort = t.sort;
        self.ascending = t.ascending;
        self.selected = t.selected;
        self.anchor = t.anchor;
        self.cursor = t.cursor;
        self.columns = t.columns;
        self.active_col = t.active_col;
        self.scroll.jump(t.scroll);
        self.search = t.search;
    }

    /// Ends per-tab transient state (rename, Quick Look, presses) and stops watching the folder.
    fn leave_tab(&mut self) {
        self.end_press();
        self.rename = None;
        self.quicklook = None;
        self.search_focus = false;
        self.drop_hit = None;
        if let Some(w) = &mut self.watcher {
            let _ = w.unwatch(&self.cwd);
        }
    }

    /// Makes the stashed tab `i` current, re-listing it in case it changed while in the background.
    fn enter_tab(&mut self, i: usize, kinds: &mut KindCache) {
        let Some(t) = self.tabs.get_mut(i).and_then(Option::take) else { return };
        self.restore(t);
        self.active_tab = i;
        if let Some(w) = &mut self.watcher {
            let _ = w.watch(&self.cwd, RecursiveMode::NonRecursive);
        }
        self.reload(kinds);
        unsafe {
            let _ = SetWindowTextW(self.hwnd, &HSTRING::from(self.title()));
        }
    }

    /// Opens `path` in a new tab after the current one, with the current view and sort.
    fn new_tab(&mut self, path: PathBuf, kinds: &mut KindCache) {
        self.leave_tab();
        let t = self.stash();
        let (view, sort, ascending) = (t.view, t.sort, t.ascending);
        self.tabs[self.active_tab] = Some(t);
        self.active_tab += 1;
        self.tabs.insert(self.active_tab, None);
        self.hist_pos = 0;
        self.view = view;
        self.sort = sort;
        self.ascending = ascending;
        self.navigate(path, true, kinds);
    }

    fn select_tab(&mut self, i: usize, kinds: &mut KindCache) {
        if i == self.active_tab || i >= self.tabs.len() {
            return;
        }
        self.leave_tab();
        self.tabs[self.active_tab] = Some(self.stash());
        self.enter_tab(i, kinds);
    }

    /// Next (`+1`) or previous (`-1`) tab, wrapping around.
    fn cycle_tab(&mut self, step: isize, kinds: &mut KindCache) {
        let n = self.tabs.len() as isize;
        let i = (self.active_tab as isize + step).rem_euclid(n) as usize;
        self.select_tab(i, kinds);
    }

    /// Closes tab `i`; returns true when it was the last one (so the window should close).
    fn close_tab(&mut self, i: usize, kinds: &mut KindCache) -> bool {
        if self.tabs.len() <= 1 {
            return true;
        }
        if i == self.active_tab {
            self.leave_tab();
            self.tabs.remove(i);
            self.enter_tab(i.min(self.tabs.len() - 1), kinds);
        } else if i < self.tabs.len() {
            self.tabs.remove(i);
            if i < self.active_tab {
                self.active_tab -= 1;
            }
        }
        self.dirty = true;
        false
    }

    // --- navigation & listing ----------------------------------------------

    fn list_sorted(&self, dir: &Path, kinds: &mut KindCache) -> (Vec<FileEntry>, Option<String>) {
        match fs::list(dir, self.show_hidden) {
            Ok(mut entries) => {
                fs::sort(&mut entries, self.sort, self.ascending, kinds);
                (entries, None)
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                (Vec::new(), Some("You don't have permission to see this folder's contents.".into()))
            }
            Err(e) => (Vec::new(), Some(format!("The folder can't be opened: {e}"))),
        }
    }

    pub fn navigate(&mut self, path: PathBuf, push: bool, kinds: &mut KindCache) {
        let (entries, error) = self.list_sorted(&path, kinds);
        if let Some(w) = &mut self.watcher {
            let _ = w.unwatch(&self.cwd);
            let _ = w.watch(&path, RecursiveMode::NonRecursive);
        }
        if push {
            self.history.truncate(self.hist_pos + 1);
            self.history.push(path.clone());
            self.hist_pos = self.history.len() - 1;
        }
        self.free = fs::free_space(&path);
        self.cwd = path;
        self.listing = entries.clone();
        self.entries = entries;
        self.error = error.clone();
        self.selected.clear();
        self.anchor = None;
        self.cursor = None;
        self.scroll.jump(0.0);
        self.rename = None;
        // Like macOS, going somewhere else ends the search and Quick Look.
        self.search.clear();
        self.search_focus = false;
        self.quicklook = None;
        self.columns = vec![Column {
            dir: self.cwd.clone(),
            entries: self.entries.clone(),
            selected: None,
            scroll: Scroll::default(),
            error,
        }];
        self.active_col = 0;
        unsafe {
            let _ = SetWindowTextW(self.hwnd, &HSTRING::from(self.title()));
        }
        self.dirty = true;
    }

    /// Re-lists after on-disk changes, keeping selections by path.
    pub fn reload(&mut self, kinds: &mut KindCache) {
        let (entries, error) = self.list_sorted(&self.cwd.clone(), kinds);
        self.listing = entries;
        self.error = error;
        for c in 0..self.columns.len() {
            let sel = self.columns[c].selected.and_then(|i| self.columns[c].entries.get(i)).map(|e| e.path.clone());
            let (entries, error) = self.list_sorted(&self.columns[c].dir.clone(), kinds);
            let col = &mut self.columns[c];
            col.selected = sel.and_then(|p| entries.iter().position(|e| e.path == p));
            col.entries = entries;
            col.error = error;
        }
        // Drop columns whose parent selection disappeared.
        if let Some(cut) = (0..self.columns.len().saturating_sub(1)).find(|&c| {
            let next = &self.columns[c + 1].dir;
            self.columns[c].selected.and_then(|i| self.columns[c].entries.get(i)).map(|e| &e.path) != Some(next)
        }) {
            self.columns.truncate(cut + 1);
            self.active_col = self.active_col.min(cut);
        }
        self.apply_filter();
        self.free = fs::free_space(&self.cwd);
        self.dirty = true;
    }

    /// Rebuilds `entries` from `listing` and the search text, keeping selections by path.
    fn apply_filter(&mut self) {
        let keep: Vec<PathBuf> = self.selected.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.path.clone()).collect();
        let query = self.search.string().to_lowercase();
        let query = query.trim();
        self.entries = if query.is_empty() {
            self.listing.clone()
        } else {
            self.listing.iter().filter(|e| e.name.to_lowercase().contains(query)).cloned().collect()
        };
        self.reselect(&keep);
        // Column view: the root column shows the same results.
        if let Some(root) = self.columns.first_mut().filter(|c| c.dir == self.cwd) {
            let sel = root.selected.and_then(|j| root.entries.get(j)).map(|e| e.path.clone());
            root.entries = self.entries.clone();
            root.selected = sel.and_then(|p| root.entries.iter().position(|e| e.path == p));
            if root.selected.is_none() {
                self.columns.truncate(1);
                self.active_col = 0;
            }
        }
        self.dirty = true;
    }

    fn focus_search(&mut self) {
        self.search_focus = true;
        self.search.select_all();
        self.dirty = true;
    }

    fn reselect(&mut self, paths: &[PathBuf]) {
        self.selected = self.entries.iter().enumerate().filter(|(_, e)| paths.contains(&e.path)).map(|(i, _)| i).collect();
        self.anchor = self.selected.iter().next().copied();
        self.cursor = self.anchor;
    }

    fn go_back(&mut self, kinds: &mut KindCache) {
        if self.hist_pos > 0 {
            self.hist_pos -= 1;
            let p = self.history[self.hist_pos].clone();
            self.navigate(p, false, kinds);
        }
    }

    fn go_forward(&mut self, kinds: &mut KindCache) {
        if self.hist_pos + 1 < self.history.len() {
            self.hist_pos += 1;
            let p = self.history[self.hist_pos].clone();
            self.navigate(p, false, kinds);
        }
    }

    fn go_parent(&mut self, kinds: &mut KindCache) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            let from = self.cwd.clone();
            self.navigate(parent, true, kinds);
            self.reselect(&[from]);
        }
    }

    fn open_path(&mut self, path: &Path, is_dir: bool, kinds: &mut KindCache) {
        if is_dir {
            self.navigate(path.to_path_buf(), true, kinds);
        } else {
            crate::win::system::open(&path.to_string_lossy());
        }
    }

    fn set_view(&mut self, view: View) {
        if self.view != view {
            // Carry the selection between views.
            if view == View::Columns {
                self.columns.truncate(1);
                self.columns[0].selected = self.selected.iter().next().copied();
            } else if self.view == View::Columns {
                self.selected = self.columns[0].selected.into_iter().collect();
            }
            self.view = view;
            self.scroll.jump(0.0);
            self.prefs_dirty = true;
            self.dirty = true;
        }
    }

    fn set_sort(&mut self, key: SortKey, kinds: &mut KindCache) {
        if self.sort == key {
            self.ascending = !self.ascending;
        } else {
            self.sort = key;
            self.ascending = true;
        }
        self.prefs_dirty = true;
        self.reload(kinds);
    }

    /// Selected paths in the current view.
    fn selection(&self) -> Vec<&FileEntry> {
        if self.view == View::Columns {
            let col = &self.columns[self.active_col.min(self.columns.len() - 1)];
            return col.selected.and_then(|i| col.entries.get(i)).into_iter().collect();
        }
        self.selected.iter().filter_map(|&i| self.entries.get(i)).collect()
    }

    /// Folder that "New Folder" etc. act on.
    fn target_dir(&self) -> PathBuf {
        if self.view == View::Columns {
            self.columns[self.active_col.min(self.columns.len() - 1)].dir.clone()
        } else {
            self.cwd.clone()
        }
    }

    fn select_column(&mut self, c: usize, j: usize, kinds: &mut KindCache) {
        self.columns.truncate(c + 1);
        self.columns[c].selected = Some(j);
        self.active_col = c;
        if let Some(e) = self.columns[c].entries.get(j).filter(|e| e.is_dir).cloned() {
            let (entries, error) = self.list_sorted(&e.path, kinds);
            self.columns.push(Column { dir: e.path, entries, selected: None, scroll: Scroll::default(), error });
        }
        self.dirty = true;
    }

    // --- commands ----------------------------------------------------------

    pub fn command(&mut self, cmd: FinderCmd, kinds: &mut KindCache) {
        match cmd {
            FinderCmd::Open(p) => {
                let is_dir = p.is_dir();
                self.open_path(&p, is_dir, kinds);
            }
            FinderCmd::OpenTab(p) => self.new_tab(p, kinds),
            FinderCmd::Reveal(p) => crate::win::system::reveal(&p.to_string_lossy()),
            FinderCmd::Explorer(p) => crate::win::system::open_in_explorer(&p.to_string_lossy()),
            FinderCmd::CopyPath(paths) => {
                let text: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                fs::copy_text(&text.join("\r\n"));
            }
            FinderCmd::Trash(paths) => {
                if fs::trash(&paths, self.hwnd) {
                    self.reload(kinds);
                    self.selected.clear();
                }
            }
            FinderCmd::NewFolder => self.new_folder(kinds),
            FinderCmd::Rename => self.begin_rename(),
            FinderCmd::SetView(v) => self.set_view(v),
            FinderCmd::Sort(k) => self.set_sort(k, kinds),
            FinderCmd::Terminal(p) => fs::open_terminal(&p),
            FinderCmd::Reload => {
                // A modal drag swallowed the button release.
                self.end_press();
                self.drop_hit = None;
                self.reload(kinds);
            }
            FinderCmd::Drag(_) | FinderCmd::Transfer { .. } => {}
        }
        self.dirty = true;
    }

    /// Forgets the current mouse press (drag candidate, rubber band, capture).
    fn end_press(&mut self) {
        self.pressed = None;
        self.drag_from = None;
        self.pending_select = None;
        self.band = None;
        unsafe {
            if GetCapture() == self.hwnd {
                let _ = ReleaseCapture();
            }
        }
    }

    fn new_folder(&mut self, kinds: &mut KindCache) {
        let dir = self.target_dir();
        match fs::new_folder(&dir) {
            Ok(path) => {
                self.reload(kinds);
                if self.view == View::Columns {
                    let c = self.columns.iter().position(|c| c.dir == dir).unwrap_or(0);
                    if let Some(j) = self.columns[c].entries.iter().position(|e| e.path == path) {
                        self.columns.truncate(c + 1);
                        self.columns[c].selected = Some(j);
                        self.active_col = c;
                    }
                } else {
                    self.reselect(&[path]);
                }
                self.begin_rename();
            }
            Err(e) => log!("new folder failed: {e}"),
        }
    }

    fn begin_rename(&mut self) {
        let Some(entry) = self.selection().first().map(|e| (*e).clone()) else { return };
        let mut field = TextField::new(&entry.name);
        // Like macOS, preselect the name without its extension.
        let stem = if entry.is_dir {
            field.text.len()
        } else {
            entry.name.rfind('.').filter(|&i| i > 0).map(|i| entry.name[..i].chars().count()).unwrap_or(field.text.len())
        };
        field.anchor = 0;
        field.caret = stem;
        self.quicklook = None;
        self.search_focus = false;
        self.rename = Some(Rename { target: entry.path, field });
        self.ensure_visible();
        self.dirty = true;
    }

    fn commit_rename(&mut self, kinds: &mut KindCache) {
        let Some(r) = self.rename.take() else { return };
        let name = r.field.string();
        match fs::rename(&r.target, &name) {
            Ok(new_path) => {
                self.reload(kinds);
                if self.view == View::Columns {
                    for c in 0..self.columns.len() {
                        if let Some(j) = self.columns[c].entries.iter().position(|e| e.path == new_path) {
                            self.columns.truncate(c + 1);
                            self.columns[c].selected = Some(j);
                        }
                    }
                } else {
                    self.reselect(&[new_path]);
                }
            }
            Err(e) => log!("rename failed: {e}"),
        }
        self.dirty = true;
    }

    // --- window management -------------------------------------------------

    pub fn toggle_zoom(&mut self) {
        unsafe {
            let rc = match self.zoom_restore.take() {
                Some(rc) => rc,
                None => {
                    let mut current = RECT::default();
                    let _ = GetWindowRect(self.hwnd, &mut current);
                    self.zoom_restore = Some(current);
                    let mut work = RECT::default();
                    let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
                    let m = (SHADOW * self.scale).round() as i32;
                    RECT { left: work.left - m, top: work.top - m + 4, right: work.right + m, bottom: work.bottom + m - 4 }
                }
            };
            let _ = SetWindowPos(self.hwnd, None, rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_NOZORDER);
        }
        self.dirty = true;
    }

    pub fn min_track_size(&self) -> (i32, i32) {
        (((MIN_W + 2.0 * SHADOW) * self.scale) as i32, ((MIN_H + 2.0 * SHADOW) * self.scale) as i32)
    }

    fn size_pt(&self) -> (f32, f32) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        ((rc.right - rc.left) as f32 / self.scale, (rc.bottom - rc.top) as f32 / self.scale)
    }

    fn body(&self) -> Rect {
        let (w, h) = self.size_pt();
        Rect::new(SHADOW, SHADOW, w - 2.0 * SHADOW, h - 2.0 * SHADOW)
    }

    fn view_rect(&self) -> Rect {
        let b = self.body();
        let top = TOOLBAR + self.tab_h();
        Rect::new(b.x + SIDEBAR, b.y + top, b.w - SIDEBAR, b.h - top - STATUS - PATHBAR)
    }

    /// Hit-test result for WM_NCHITTEST, from window-local points.
    pub fn nc_hit(&self, x: f32, y: f32) -> u32 {
        let b = self.body();
        if !b.inset(-2.0, -2.0).contains(x, y) {
            return HTNOWHERE;
        }
        let (l, r) = (x < b.x + EDGE, x > b.right() - EDGE);
        let (t, bt) = (y < b.y + EDGE, y > b.bottom() - EDGE);
        match (l, r, t, bt) {
            (true, _, true, _) => return HTTOPLEFT,
            (_, true, true, _) => return HTTOPRIGHT,
            (true, _, _, true) => return HTBOTTOMLEFT,
            (_, true, _, true) => return HTBOTTOMRIGHT,
            (true, ..) => return HTLEFT,
            (_, true, ..) => return HTRIGHT,
            (_, _, true, _) => return HTTOP,
            (_, _, _, true) => return HTBOTTOM,
            _ => {}
        }
        if y < b.y + TOOLBAR && self.hit_at(x, y).is_none() {
            return HTCAPTION;
        }
        HTCLIENT
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    // --- input -------------------------------------------------------------

    pub fn on_move(&mut self, x: f32, y: f32) -> FinderEvent {
        let h = self.hit_at(x, y);
        let interesting = |h: Option<Hit>| {
            matches!(
                h,
                Some(
                    Hit::Light(_)
                        | Hit::Back
                        | Hit::Forward
                        | Hit::ViewBtn(_)
                        | Hit::Place(_)
                        | Hit::Header(_)
                        | Hit::SearchClear
                        | Hit::QlClose
                        | Hit::QlOpen
                        | Hit::Tab(_)
                        | Hit::TabClose(_)
                        | Hit::NewTab
                )
            )
        };
        if h != self.hover && (interesting(h) || interesting(self.hover)) {
            self.dirty = true;
        }
        self.hover = h;

        if self.band.is_some() {
            self.update_band(x, y);
        } else if self.pressed == Some(Hit::Search) {
            // Drag-select inside the search field.
            let i = self.search_index(x);
            self.search.set_caret(i, true);
            self.dirty = true;
        } else if let Some((x0, y0)) = self.drag_from {
            let (dx, dy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
            if (x - x0).abs() * self.scale > dx as f32 || (y - y0).abs() * self.scale > dy as f32 {
                let paths: Vec<PathBuf> = self.selection().iter().map(|e| e.path.clone()).collect();
                self.end_press();
                if !paths.is_empty() {
                    return FinderEvent::Run(FinderCmd::Drag(paths));
                }
            }
        }
        FinderEvent::None
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if self.hover.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn on_wheel(&mut self, x: f32, delta: i32) {
        self.scrolled_at = Instant::now();
        if let Some(ql) = &mut self.quicklook {
            ql.scroll.wheel(delta, 60.0, ql.max_scroll);
        } else if self.view == View::Columns {
            let vr = self.view_rect();
            let offset = self.column_offset(vr);
            let c = ((x - vr.x + offset) / COL_W).floor().max(0.0) as usize;
            if let Some(col) = self.columns.get_mut(c) {
                let max = (col.entries.len() as f32 * ROW + 8.0 - vr.h).max(0.0);
                col.scroll.wheel(delta, 60.0, max);
            }
        } else {
            self.scroll.wheel(delta, 60.0, self.max_scroll);
        }
        // Only the list moved: unless a whole redraw is already due, repaint just it.
        if self.quicklook.is_none() && (!self.dirty || self.part.is_some()) {
            let vr = self.view_rect();
            self.part = Some(self.part.map_or(vr, |p| p.union(vr)));
        }
        self.dirty = true;
    }

    /// Starts tracking a press that may become a drag.
    fn begin_press(&mut self, x: f32, y: f32) {
        self.drag_from = Some((x, y));
        unsafe {
            SetCapture(self.hwnd);
        }
    }

    pub fn on_down(&mut self, x: f32, y: f32, ctrl: bool, shift: bool, kinds: &mut KindCache) {
        let hit = self.hit_at(x, y);
        self.end_press();
        self.pressed = hit;
        let now = Instant::now();
        let dbl = Duration::from_millis(unsafe { GetDoubleClickTime() } as u64);
        let double = matches!((self.last_click, hit), (Some((t, h)), Some(hh)) if h == hh && now - t < dbl);
        self.last_click = if double { None } else { hit.map(|h| (now, h)) };

        if self.rename.is_some() {
            self.commit_rename(kinds);
        }
        if !matches!(hit, Some(Hit::Search | Hit::SearchClear)) {
            self.search_focus = false;
        }
        match hit {
            Some(Hit::Item(i)) => {
                if double {
                    if let Some(e) = self.entries.get(i).cloned() {
                        self.open_path(&e.path, e.is_dir, kinds);
                    }
                    return;
                }
                if ctrl {
                    if !self.selected.remove(&i) {
                        self.selected.insert(i);
                    }
                    self.anchor = Some(i);
                } else if shift {
                    let a = self.anchor.unwrap_or(i);
                    self.selected = (a.min(i)..=a.max(i)).collect();
                } else if self.selected.contains(&i) {
                    self.pending_select = Some(i);
                } else {
                    self.selected = [i].into();
                    self.anchor = Some(i);
                }
                self.cursor = Some(i);
                if self.selected.contains(&i) {
                    self.begin_press(x, y);
                }
            }
            Some(Hit::Column(c, j)) => {
                let entry = self.columns.get(c).and_then(|col| col.entries.get(j)).cloned();
                if double {
                    if let Some(e) = entry.filter(|e| !e.is_dir) {
                        crate::win::system::open(&e.path.to_string_lossy());
                    }
                    return;
                }
                self.select_column(c, j, kinds);
                self.begin_press(x, y);
            }
            Some(Hit::ColumnBg(c)) => {
                self.columns.truncate(c + 1);
                self.columns[c].selected = None;
                self.active_col = c;
            }
            Some(Hit::Content) if self.view != View::Columns => {
                // Rubber band: Ctrl toggles items against the current selection, Shift adds to it.
                let base = if ctrl || shift { self.selected.clone() } else { BTreeSet::new() };
                self.selected = base.clone();
                self.band = Some(Band { x0: x, y0: y + self.scroll.offset(), x1: x, y1: y, base, toggle: ctrl });
                unsafe {
                    SetCapture(self.hwnd);
                }
            }
            Some(Hit::Content) => self.selected.clear(),
            Some(Hit::Header(k)) => self.set_sort(k, kinds),
            Some(Hit::Place(i)) => {
                if let Some(p) = self.favorites.iter().chain(&self.drives).nth(i).cloned() {
                    self.navigate(p.path, true, kinds);
                }
            }
            Some(Hit::Search) => {
                self.search_focus = true;
                if double {
                    self.search.select_all();
                } else {
                    let i = self.search_index(x);
                    self.search.set_caret(i, shift);
                    unsafe {
                        SetCapture(self.hwnd);
                    }
                }
            }
            Some(Hit::SearchClear) => {
                self.search.clear();
                self.search_focus = true;
                self.apply_filter();
            }
            Some(Hit::Crumb(depth)) => {
                let target = crumbs(&self.cwd).into_iter().nth(depth);
                if let Some(dir) = target.filter(|d| *d != self.cwd) {
                    self.navigate(dir, true, kinds);
                }
            }
            Some(Hit::QlBackdrop) => self.close_quicklook(),
            // Tabs switch on press, like macOS.
            Some(Hit::Tab(i)) => self.select_tab(i, kinds),
            _ => {}
        }
        self.dirty = true;
    }

    /// Middle click: closes a tab, or opens a folder in a new tab.
    pub fn on_middle(&mut self, x: f32, y: f32, kinds: &mut KindCache) -> FinderEvent {
        if self.quicklook.is_some() {
            return FinderEvent::None;
        }
        match self.hit_at(x, y) {
            Some(Hit::Tab(i) | Hit::TabClose(i)) => {
                if self.close_tab(i, kinds) {
                    return FinderEvent::Close;
                }
            }
            Some(Hit::Item(i)) => {
                if let Some(e) = self.entries.get(i).filter(|e| e.is_dir).cloned() {
                    self.new_tab(e.path, kinds);
                }
            }
            Some(Hit::Column(c, j)) => {
                if let Some(e) = self.columns.get(c).and_then(|col| col.entries.get(j)).filter(|e| e.is_dir).cloned() {
                    self.new_tab(e.path, kinds);
                }
            }
            Some(Hit::Place(i)) => {
                if let Some(p) = self.favorites.iter().chain(&self.drives).nth(i).cloned() {
                    self.new_tab(p.path, kinds);
                }
            }
            _ => {}
        }
        self.dirty = true;
        FinderEvent::None
    }

    pub fn on_up(&mut self, x: f32, y: f32, kinds: &mut KindCache) -> FinderEvent {
        let pressed = self.pressed;
        let pending = self.pending_select;
        let had_band = self.band.is_some();
        self.end_press();
        if had_band {
            self.dirty = true;
            return FinderEvent::None;
        }
        let Some(hit) = self.hit_at(x, y).filter(|h| Some(*h) == pressed) else { return FinderEvent::None };
        match hit {
            Hit::Light(Light::Close) => return FinderEvent::Close,
            Hit::Light(Light::Minimize) => self.begin_minimize(),
            Hit::Light(Light::Zoom) => self.toggle_zoom(),
            Hit::Back => self.go_back(kinds),
            Hit::Forward => self.go_forward(kinds),
            Hit::ViewBtn(v) => self.set_view(v),
            Hit::Item(i) if pending == Some(i) => {
                self.selected = [i].into();
                self.anchor = Some(i);
            }
            Hit::TabClose(i) => {
                if self.close_tab(i, kinds) {
                    return FinderEvent::Close;
                }
            }
            Hit::NewTab => self.new_tab(fs::home(), kinds),
            Hit::QlClose => self.close_quicklook(),
            Hit::QlOpen => {
                if let Some(e) = self.ql_entry().cloned() {
                    self.quicklook = None;
                    self.open_path(&e.path, e.is_dir, kinds);
                }
            }
            _ => {}
        }
        self.dirty = true;
        FinderEvent::None
    }

    /// Caret index in the search text nearest to window x.
    fn search_index(&self, x: f32) -> usize {
        let nearest = self.search_xs.iter().enumerate().min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs()));
        nearest.map(|(i, _)| i).unwrap_or(0).min(self.search.text.len())
    }

    /// Item `i`'s hit areas in content coordinates (scroll 0): icon and label, or the row.
    fn item_rects(&self, i: usize) -> [Rect; 2] {
        let vr = self.view_rect();
        if self.view == View::Icons {
            let cols = self.grid_cols.max(1);
            let cw = (vr.w - 2.0 * GRID_PAD) / cols as f32;
            let cell = Rect::new(vr.x + GRID_PAD + (i % cols) as f32 * cw, vr.y + GRID_PAD + (i / cols) as f32 * CELL_H, cw, CELL_H);
            let icon = Rect::new(cell.cx() - ICON / 2.0, cell.y + 6.0, ICON, ICON);
            [icon, Rect::new(cell.x + 4.0, icon.bottom() + 6.0, cell.w - 8.0, 32.0)]
        } else {
            let row = Rect::new(vr.x + 8.0, vr.y + HEADER + 4.0 + i as f32 * ROW, vr.w - 16.0, ROW);
            [row, row]
        }
    }

    fn update_band(&mut self, x: f32, y: f32) {
        let vr = self.view_rect();
        // Auto-scroll while dragging past the top or bottom edge.
        let top = if self.view == View::List { vr.y + HEADER } else { vr.y };
        if y < top {
            self.scroll.to(self.scroll.target() - (top - y) * 0.5);
        } else if y > vr.bottom() {
            self.scroll.to((self.scroll.target() + (y - vr.bottom()) * 0.5).min(self.max_scroll));
        }
        let Some(band) = &self.band else { return };
        let r = band_rect(band.x0, band.y0, x, y + self.scroll.offset());
        // Only rows the band spans can intersect it.
        let n = self.entries.len();
        let (first, last) = if self.view == View::Icons {
            let cols = self.grid_cols.max(1);
            let row = |y: f32| ((y - vr.y - GRID_PAD) / CELL_H).floor().max(0.0) as usize;
            (row(r.y) * cols, ((row(r.bottom()) + 1) * cols).min(n))
        } else {
            let row = |y: f32| ((y - vr.y - HEADER - 4.0) / ROW).floor().max(0.0) as usize;
            (row(r.y), (row(r.bottom()) + 1).min(n))
        };
        let hits: BTreeSet<usize> =
            (first..last.max(first)).filter(|&i| self.item_rects(i).iter().any(|ir| overlaps(ir, &r))).collect();
        let sel: BTreeSet<usize> =
            if band.toggle { band.base.symmetric_difference(&hits).copied().collect() } else { band.base.union(&hits).copied().collect() };
        self.selected = sel;
        if let Some(&last) = hits.iter().next_back() {
            self.cursor = Some(last);
            self.anchor = hits.iter().next().copied();
        }
        if let Some(band) = &mut self.band {
            band.x1 = x;
            band.y1 = y;
        }
        self.dirty = true;
    }

    /// Drop feedback from [`dnd`]: highlights what's under the point and returns the folder a drop there goes into.
    pub fn on_drag(&mut self, phase: dnd::Phase, paths: &[PathBuf]) -> Option<PathBuf> {
        let (sx, sy, dropping) = match phase {
            dnd::Phase::Leave => {
                if self.drop_hit.take().is_some() {
                    self.dirty = true;
                }
                return None;
            }
            dnd::Phase::Over(x, y) => (x, y, false),
            dnd::Phase::Drop(x, y) => (x, y, true),
        };
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        let (x, y) = ((sx - rc.left) as f32 / self.scale, (sy - rc.top) as f32 / self.scale);
        let hit = self.hit_at(x, y);
        let last = self.columns.len() - 1;
        // Folders take the drop; anywhere else in the view drops into the folder on show.
        let target = match hit {
            Some(Hit::Item(i)) => self.entries.get(i).filter(|e| e.is_dir).map(|e| (Hit::Item(i), e.path.clone())),
            Some(Hit::Column(c, j)) => self.columns[c].entries.get(j).filter(|e| e.is_dir).map(|e| (Hit::Column(c, j), e.path.clone())),
            Some(Hit::Place(i)) => self.favorites.iter().chain(&self.drives).nth(i).map(|p| (Hit::Place(i), p.path.clone())),
            Some(Hit::Tab(i) | Hit::TabClose(i)) => self.tab_dir(i).map(|d| (Hit::Tab(i), d.to_path_buf())),
            _ => None,
        }
        .or_else(|| match hit {
            Some(Hit::Item(_) | Hit::Content | Hit::Header(_)) if self.view != View::Columns => Some((Hit::Content, self.cwd.clone())),
            Some(Hit::Column(c, _) | Hit::ColumnBg(c)) => Some((Hit::ColumnBg(c), self.columns[c].dir.clone())),
            Some(Hit::Content) => Some((Hit::ColumnBg(last), self.columns[last].dir.clone())),
            _ => None,
        })
        .filter(|(_, dir)| !paths.iter().any(|p| dir.starts_with(p)));
        let highlight = if dropping { None } else { target.as_ref().map(|t| t.0) };
        if highlight != self.drop_hit {
            self.drop_hit = highlight;
            self.dirty = true;
        }
        target.map(|t| t.1)
    }

    pub fn on_right_click(&mut self, x: f32, y: f32, kinds: &mut KindCache) -> FinderEvent {
        if self.rename.is_some() {
            self.commit_rename(kinds);
        }
        if self.quicklook.is_some() {
            return FinderEvent::None;
        }
        self.search_focus = false;
        let id = self.id;
        let cmd = |c: FinderCmd| Action::Finder { window: id, cmd: c };
        let hit = self.hit_at(x, y);
        // Right-clicking an unselected item selects it first.
        match hit {
            Some(Hit::Item(i)) if !self.selected.contains(&i) => {
                self.selected = [i].into();
                self.anchor = Some(i);
            }
            Some(Hit::Column(c, j)) if self.columns.get(c).and_then(|c| c.selected) != Some(j) => {
                self.select_column(c, j, kinds)
            }
            _ => {}
        }
        self.dirty = true;
        let on_item = matches!(hit, Some(Hit::Item(_) | Hit::Column(..)));
        let entries = if on_item {
            let sel: Vec<FileEntry> = self.selection().into_iter().cloned().collect();
            let paths: Vec<PathBuf> = sel.iter().map(|e| e.path.clone()).collect();
            let Some(first) = sel.first() else { return FinderEvent::None };
            let mut v = vec![
                Entry::item("Open", cmd(FinderCmd::Open(first.path.clone()))),
                Entry::Separator,
                Entry::item("Move to Trash", cmd(FinderCmd::Trash(paths.clone()))),
                Entry::Separator,
                Entry::item("Rename", cmd(FinderCmd::Rename)),
                Entry::item(if paths.len() > 1 { "Copy Paths" } else { "Copy Path" }, cmd(FinderCmd::CopyPath(paths))),
                Entry::item("Show in File Explorer", cmd(FinderCmd::Reveal(first.path.clone()))),
            ];
            if first.is_dir {
                v.insert(1, Entry::item("Open in New Tab", cmd(FinderCmd::OpenTab(first.path.clone()))));
                v.push(Entry::Separator);
                v.push(Entry::item("New Terminal at Folder", cmd(FinderCmd::Terminal(first.path.clone()))));
            }
            v
        } else {
            let dir = self.target_dir();
            let view = |label: &str, v: View| Entry::item(label, cmd(FinderCmd::SetView(v))).checked(self.view == v);
            let sort = |label: &str, k: SortKey| Entry::item(label, cmd(FinderCmd::Sort(k))).checked(self.sort == k);
            vec![
                Entry::item("New Folder", cmd(FinderCmd::NewFolder)),
                Entry::Separator,
                Entry::item("New Terminal at Folder", cmd(FinderCmd::Terminal(dir.clone()))),
                Entry::item("Open in File Explorer", cmd(FinderCmd::Explorer(dir.clone()))),
                Entry::item("Copy Path", cmd(FinderCmd::CopyPath(vec![dir]))),
                Entry::Separator,
                view("as Icons", View::Icons),
                view("as List", View::List),
                view("as Columns", View::Columns),
                Entry::Separator,
                sort("Sort by Name", SortKey::Name),
                sort("Sort by Date Modified", SortKey::Modified),
                sort("Sort by Size", SortKey::Size),
                sort("Sort by Kind", SortKey::Kind),
            ]
        };
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        FinderEvent::Menu(entries, rc.left as f32 / self.scale + x, rc.top as f32 / self.scale + y)
    }

    /// Keyboard shortcuts mirror macOS with Ctrl in place of ⌘.
    pub fn on_key(&mut self, vk: u16, ctrl: bool, shift: bool, alt: bool, kinds: &mut KindCache) -> FinderEvent {
        self.dirty = true;
        if let Some(r) = &mut self.rename {
            match vk {
                ENTER => self.commit_rename(kinds),
                ESC => self.rename = None,
                _ => {
                    r.field.on_key(vk, ctrl, shift);
                }
            }
            return FinderEvent::None;
        }

        if self.search_focus {
            match vk {
                // Esc clears the search, then leaves the field.
                ESC if !self.search.is_empty() => {
                    self.search.clear();
                    self.apply_filter();
                }
                ESC => self.search_focus = false,
                // Into the results.
                ENTER | TAB | DOWN if !ctrl => {
                    self.search_focus = false;
                    if self.selected.is_empty() && !self.entries.is_empty() {
                        self.selected = [0].into();
                        self.anchor = Some(0);
                        self.cursor = Some(0);
                    }
                }
                _ => match self.search.on_key(vk, ctrl, shift) {
                    KeyResult::Edited => self.apply_filter(),
                    // Unhandled Ctrl shortcuts (close window, views, ...) still work.
                    KeyResult::Ignored if ctrl => return self.on_shortcut(vk, ctrl, shift, alt, kinds),
                    _ => {}
                },
            }
            return FinderEvent::None;
        }

        if self.quicklook.is_some() && matches!(vk, ESC | SPACE) {
            self.close_quicklook();
            return FinderEvent::None;
        }
        self.on_shortcut(vk, ctrl, shift, alt, kinds)
    }

    fn on_shortcut(&mut self, vk: u16, ctrl: bool, shift: bool, alt: bool, kinds: &mut KindCache) -> FinderEvent {
        match (vk, ctrl, alt) {
            (SPACE, false, false) => self.toggle_quicklook(),
            (k, true, false) if k == b'Y' as u16 => self.toggle_quicklook(),
            (k, true, false) if k == b'F' as u16 => self.focus_search(),
            (k, true, false) if k == b'N' as u16 && shift => self.new_folder(kinds),
            (k, true, false) if k == b'N' as u16 => return FinderEvent::NewWindow,
            (k, true, false) if k == b'T' as u16 => self.new_tab(fs::home(), kinds),
            // Ctrl+W closes the tab, or the window with its last tab.
            (k, true, false) if k == b'W' as u16 => {
                if self.close_tab(self.active_tab, kinds) {
                    return FinderEvent::Close;
                }
            }
            (TAB, true, false) => self.cycle_tab(if shift { -1 } else { 1 }, kinds),
            (RBRACKET, true, false) if shift => self.cycle_tab(1, kinds),
            (LBRACKET, true, false) if shift => self.cycle_tab(-1, kinds),
            (k, true, false) if k == b'1' as u16 => self.set_view(View::Icons),
            (k, true, false) if k == b'2' as u16 => self.set_view(View::List),
            (k, true, false) if k == b'3' as u16 => self.set_view(View::Columns),
            (k, true, false) if k == b'A' as u16 => {
                if self.view != View::Columns {
                    self.selected = (0..self.entries.len()).collect();
                }
            }
            (k, true, true) if k == b'C' as u16 => {
                let paths: Vec<PathBuf> = self.selection().iter().map(|e| e.path.clone()).collect();
                self.command(FinderCmd::CopyPath(if paths.is_empty() { vec![self.target_dir()] } else { paths }), kinds);
            }
            (UP, true, _) | (UP, _, true) => self.go_parent(kinds),
            (LEFT, _, true) | (LBRACKET, true, _) | (BACK, false, false) => self.go_back(kinds),
            (RIGHT, _, true) | (RBRACKET, true, _) => self.go_forward(kinds),
            (ENTER, ..) | (DOWN, true, _) => {
                if let Some(e) = self.selection().first().map(|e| (*e).clone()) {
                    self.open_path(&e.path, e.is_dir, kinds);
                }
            }
            (F2, ..) => self.begin_rename(),
            (DEL, ..) | (BACK, true, _) => {
                let paths: Vec<PathBuf> = self.selection().iter().map(|e| e.path.clone()).collect();
                if !paths.is_empty() {
                    self.command(FinderCmd::Trash(paths), kinds);
                }
            }
            (F5, ..) => self.reload(kinds),
            // ⌘⇧. on macOS: toggle hidden files.
            (PERIOD, true, false) if shift => {
                self.show_hidden = !self.show_hidden;
                self.prefs_dirty = true;
                self.reload(kinds);
            }
            (LEFT | RIGHT | UP | DOWN | HOME | END, false, false) => self.move_selection(vk, shift, kinds),
            _ => {}
        }
        FinderEvent::None
    }

    fn move_selection(&mut self, vk: u16, shift: bool, kinds: &mut KindCache) {
        if self.view == View::Columns {
            let c = self.active_col.min(self.columns.len() - 1);
            let n = self.columns[c].entries.len();
            let cur = self.columns[c].selected;
            match vk {
                0x25 if c > 0 => {
                    // Left: focus the parent column; this column stays visible.
                    self.columns.truncate(c + 1);
                    self.columns[c].selected = None;
                    self.active_col = c - 1;
                }
                0x27 => {
                    // Right: into the selected folder's column.
                    if self.columns.len() > c + 1 && !self.columns[c + 1].entries.is_empty() {
                        self.select_column(c + 1, 0, kinds);
                    }
                }
                0x26 | 0x28 | 0x24 | 0x23 if n > 0 => {
                    let j = match (vk, cur) {
                        (0x24, _) => 0,
                        (0x23, _) => n - 1,
                        (0x26, Some(j)) => j.saturating_sub(1),
                        (0x28, Some(j)) => (j + 1).min(n - 1),
                        _ => 0,
                    };
                    self.select_column(c, j, kinds);
                }
                _ => {}
            }
            self.ensure_visible();
            return;
        }
        let n = self.entries.len();
        if n == 0 {
            return;
        }
        let step = if self.view == View::Icons { self.grid_cols.max(1) } else { 1 };
        let cur = self.cursor.filter(|c| self.selected.contains(c)).or_else(|| self.selected.iter().next_back().copied());
        let next = match (vk, cur) {
            (_, None) => 0,
            (0x24, _) => 0,
            (0x23, _) => n - 1,
            (0x25, Some(i)) if self.view == View::Icons => i.saturating_sub(1),
            (0x27, Some(i)) if self.view == View::Icons => (i + 1).min(n - 1),
            (0x26, Some(i)) => i.saturating_sub(step),
            (0x28, Some(i)) => (i + step).min(n - 1),
            (_, Some(i)) => i,
        };
        if shift {
            // Extend from the anchor (where the selection started) to the new cursor.
            let a = self.anchor.unwrap_or(next);
            self.selected = (a.min(next)..=a.max(next)).collect();
        } else {
            self.selected = [next].into();
            self.anchor = Some(next);
        }
        self.cursor = Some(next);
        self.ensure_visible();
    }

    pub fn on_char(&mut self, ch: char) {
        if ch.is_control() {
            return;
        }
        if let Some(r) = &mut self.rename {
            r.field.insert(&ch.to_string());
            self.dirty = true;
            return;
        }
        if self.search_focus {
            self.search.insert(&ch.to_string());
            self.apply_filter();
            return;
        }
        if ch == ' ' {
            // Space is Quick Look (handled as a key).
            return;
        }
        // Type-to-select.
        if self.typed_at.elapsed() > Duration::from_millis(900) {
            self.typed.clear();
        }
        self.typed_at = Instant::now();
        self.typed.extend(ch.to_lowercase());
        let typed = self.typed.clone();
        let matches = |e: &FileEntry| e.name.to_lowercase().starts_with(&typed);
        if self.view == View::Columns {
            let c = self.active_col.min(self.columns.len() - 1);
            if let Some(j) = self.columns[c].entries.iter().position(matches) {
                self.columns[c].selected = Some(j);
                self.columns.truncate(c + 1);
            }
        } else if let Some(i) = self.entries.iter().position(matches) {
            self.selected = [i].into();
            self.anchor = Some(i);
        }
        self.ensure_visible();
        self.dirty = true;
    }

    fn ensure_visible(&mut self) {
        let vr = self.view_rect();
        match self.view {
            View::Icons => {
                let Some(&i) = self.cursor.as_ref().or(self.selected.iter().next()) else { return };
                let top = (i / self.grid_cols.max(1)) as f32 * CELL_H;
                let s = self.scroll.target();
                if top < s {
                    self.scroll.to(top);
                } else if top + CELL_H + 24.0 > s + vr.h {
                    self.scroll.to(top + CELL_H + 24.0 - vr.h);
                }
            }
            View::List => {
                let Some(&i) = self.cursor.as_ref().or(self.selected.iter().next()) else { return };
                let top = i as f32 * ROW;
                let visible = vr.h - HEADER;
                self.scroll.to(self.scroll.target().min(top).max(top + ROW - visible + 4.0));
            }
            View::Columns => {
                let c = self.active_col.min(self.columns.len() - 1);
                let col = &mut self.columns[c];
                if let Some(j) = col.selected {
                    let top = j as f32 * ROW;
                    col.scroll.to(col.scroll.target().min(top).max(top + ROW - vr.h + 8.0));
                }
            }
        }
    }

    /// Horizontal offset that keeps the rightmost column (or preview) in view.
    fn column_offset(&self, vr: Rect) -> f32 {
        let total = self.columns.len() as f32 * COL_W + if self.preview_entry().is_some() { PREVIEW_W } else { 0.0 };
        (total - vr.w).max(0.0)
    }

    /// The item Quick Look shows: the keyboard cursor if selected, else the first selected item.
    fn ql_entry(&self) -> Option<&FileEntry> {
        if self.view == View::Columns {
            return self.selection().first().copied();
        }
        let i = self.cursor.filter(|c| self.selected.contains(c)).or_else(|| self.selected.iter().next().copied())?;
        self.entries.get(i)
    }

    fn toggle_quicklook(&mut self) {
        if self.quicklook.is_some() {
            return self.close_quicklook();
        }
        if self.ql_entry().is_some() {
            self.quicklook = Some(QuickLook::default());
            self.ql_closing = None;
            // Visible from the first frame, then a quick zoom to full size.
            self.ql_t.snap(self.ql_t.value.max(0.4));
            self.ql_t.set(1.0);
        }
        self.dirty = true;
    }

    /// Follows the selection: loads the text preview when the previewed file changes.
    fn sync_quicklook(&mut self) {
        let Some(e) = self.ql_entry().cloned() else {
            self.quicklook = None;
            return;
        };
        if let Some(ql) = self.quicklook.as_mut().filter(|ql| ql.path != e.path) {
            *ql = QuickLook { path: e.path.clone(), text: fs::text_preview(&e), scroll: Scroll::default(), max_scroll: 0.0 };
        }
    }

    fn preview_entry(&self) -> Option<&FileEntry> {
        let last = self.columns.last()?;
        last.selected.and_then(|j| last.entries.get(j)).filter(|e| !e.is_dir)
    }

    // --- drawing -----------------------------------------------------------

    pub fn render(&mut self, ctx: &mut Ctx) {
        self.dirty = false;
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        let resized = (self.canvas.width, self.canvas.height) != (rc.right - rc.left, rc.bottom - rc.top);
        if let Err(e) = self.canvas.resize(rc.right - rc.left, rc.bottom - rc.top) {
            return log!("finder resize failed: {e}");
        }
        if !self.window.begin_render(self.hwnd) {
            return;
        }
        self.motion.sweep();
        if self.quicklook.is_some() {
            self.sync_quicklook();
        }
        // Open, close and minimize zoom the whole window (and its sidebar backdrop).
        self.xf = self.window.transform(self.body());
        let (zoom_x, zoom_y, zoom, window_alpha) = self.xf;

        let (gfx, theme) = (ctx.gfx, ctx.theme);
        let mut pal = chrome::Palette::new(theme);
        if let Some(bd) = &mut self.backdrop {
            bd.set_theme(theme);
            pal.sidebar = SidebarBackdrop::fill();
        }
        // Only when it changed: placing it re-renders its glass.
        if self.backdrop_for != Some((self.xf, self.body())) {
            self.sync_backdrop();
        }
        let floating = self.backdrop.is_some();
        let dark = theme.dark;
        let body = self.body();
        let vr = self.view_rect();
        let content_x = body.x + SIDEBAR;
        let content_w = body.w - SIDEBAR;
        let sel_bg = if self.focused { theme.accent } else if dark { Color::gray(255, 0.18) } else { Color::gray(0, 0.12) };
        let sel_text = if self.focused { Color::WHITE } else { theme.text };
        let soft = if dark { Color::gray(255, 0.08) } else { Color::gray(0, 0.06) };
        let stripe = if dark { Color::gray(255, 0.035) } else { Color::gray(0, 0.035) };
        let field_bg = if dark { Color::rgba(30, 30, 30, 1.0) } else { Color::WHITE };

        let regular = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Leading);
        let right = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Trailing);
        let small = gfx.format(&theme.font, 11.0, Weight::Regular, Align::Leading);
        let small_center = gfx.format(&theme.font, 11.0, Weight::Regular, Align::Center);
        let small_bold = gfx.format(&theme.font, 11.0, Weight::Bold, Align::Leading);
        let title_font = gfx.format(&theme.font, 15.0, Weight::Bold, Align::Leading);
        let label_font = gfx.format_wrapped(&theme.font, 12.0);
        let center = gfx.format(&theme.font, 12.0, Weight::Regular, Align::Center);
        let preview_title = gfx.format(&theme.font, 15.0, Weight::Bold, Align::Center);
        let ql_title = gfx.format(&theme.font, 13.0, Weight::Bold, Align::Center);
        let mono = gfx.format_wrapped("Consolas", 12.0);

        // Grid / scroll metrics for the current view.
        let n = self.entries.len();
        let pad = GRID_PAD;
        self.grid_cols = (((vr.w - 2.0 * pad) / CELL_W).floor() as usize).max(1);
        let content_h = match self.view {
            View::Icons => n.div_ceil(self.grid_cols) as f32 * CELL_H + 2.0 * pad,
            View::List => n as f32 * ROW + 8.0 + HEADER,
            View::Columns => 0.0,
        };
        self.max_scroll = (content_h - vr.h).max(0.0);
        self.scroll.clamp(self.max_scroll);
        // The overlay scrollbar shows at once while scrolling, then fades away.
        let scrollbar_a = {
            let recent = self.scrolled_at.elapsed().as_secs_f32() < SCROLLBAR_LINGER;
            self.scrollbar_up = recent;
            if recent {
                self.motion.fade(Anim::Scrollbar, true)
            } else {
                self.motion.get(Anim::Scrollbar, 0.0, (0.3, 1.0))
            }
        };

        let mut hits: Vec<(Rect, Hit)> = Vec::new();
        let status = {
            let total = if self.view == View::Columns {
                self.columns.get(self.active_col).map(|c| c.entries.len()).unwrap_or(0)
            } else {
                n
            };
            let sel = self.selection().len();
            let items = if sel > 0 {
                format!("{sel} of {total} selected")
            } else {
                format!("{total} item{}", if total == 1 { "" } else { "s" })
            };
            match self.free {
                Some(f) => format!("{items}, {} available", fs::format_size(f)),
                None => items,
            }
        };
        let title = self.title();
        let path_names: Vec<String> = crumbs(&self.cwd).iter().map(|d| self.title_for(d)).collect();
        let preview = self.preview_entry().cloned();
        let column_offset = self.column_offset(vr);
        let ql_live = self.quicklook.is_some();
        let ql_entry = if ql_live { self.ql_entry().cloned() } else { self.ql_closing.as_ref().map(|(_, e)| e.clone()) };
        let ql_t = self.ql_t.value;
        let tab_h = self.tab_h();
        let tab_titles: Vec<String> = if tab_h > 0.5 {
            (0..self.tabs.len()).map(|i| self.tab_dir(i).map(|d| self.title_for(d)).unwrap_or_default()).collect()
        } else {
            Vec::new()
        };
        let active_tab = self.active_tab;

        let kinds = &mut *ctx.kinds;
        let loader = &mut *ctx.loader;
        let bitmaps = &mut self.bitmaps;
        let (entries, columns, selected) = (&self.entries, &self.columns, &self.selected);
        let (view, hover, error, rename) = (self.view, self.hover, &self.error, &self.rename);
        let scroll = self.scroll.offset_px(self.scale);
        let px = self.scale;
        let (max_scroll, grid_cols, active_col) = (self.max_scroll, self.grid_cols, self.active_col);
        let motion = &self.motion;
        let (sort, ascending) = (self.sort, self.ascending);
        let (can_back, can_fwd) = (self.hist_pos > 0, self.hist_pos + 1 < self.history.len());
        let places: Vec<&Place> = self.favorites.iter().chain(&self.drives).collect();
        let fav_count = self.favorites.len();
        let cwd = &self.cwd;
        // Sidebar rows, and the hover highlight gliding between them.
        let side = if floating { chrome::sidebar_pane(body, SIDEBAR) } else { Rect::new(body.x, body.y, SIDEBAR, body.h) };
        let place_rows = place_rows(side, places.len(), fav_count);
        let side_target = match hover {
            Some(Hit::Place(i)) if places.get(i).is_some_and(|p| p.path != *cwd) => place_rows.get(i).copied(),
            _ => None,
        };
        let side_glide = self.side_glide.update(side_target);
        let sel_glide = self.sel_glide.update(places.iter().position(|p| p.path == *cwd).map(|i| place_rows[i]));
        let (search, search_focus, drop_hit) = (&self.search, self.search_focus, self.drop_hit);
        let band = self.band.as_ref().map(|b| band_rect(b.x0, b.y0 - scroll, b.x1, b.y1));
        let ql = match self.quicklook.as_mut() {
            Some(q) => Some(q),
            None => self.ql_closing.as_mut().map(|(q, _)| q),
        }
        .zip(ql_entry);
        let ql_bitmap = &mut self.ql_bitmap;
        let ql_px = ql.as_ref().and_then(|(_, e)| loader.get_large(e));
        let mut search_xs: Vec<f32> = Vec::new();

        // Just the animating area when that's all that changed (and the window
        // itself isn't zooming or fading).
        let part = self.part.take().filter(|_| !resized && zoom == 1.0 && window_alpha == 1.0).and_then(|r| self.canvas.part_px(r));
        let drawn = self.canvas.draw_part(gfx, part, |p| p.scaled_about(zoom_x, zoom_y, zoom, || {
            // Hover highlights fade in and out rather than blinking.
            let hv = |h: Hit| motion.fade(Anim::Hover(h), hover == Some(h));
            let soft_t = |t: f32| soft.alpha(soft.a * t);
            let mut icon_for = |p: &Painter, e: &FileEntry| -> Option<ID2D1Bitmap> {
                let (key, _) = IconLoader::key_for(e);
                if let Some(b) = bitmaps.get(&key) {
                    return b.clone();
                }
                let px = loader.get_entry(e)?;
                let bmp = px.to_bitmap(p.rt);
                bitmaps.insert(key, bmp.clone());
                bmp
            };
            let draw_icon = |p: &Painter, bmp: Option<ID2D1Bitmap>, r: Rect| {
                if let Some(b) = bmp {
                    // Keep aspect ratio for thumbnails.
                    let size = unsafe { b.GetSize() };
                    let (bw, bh) = (size.width.max(1.0), size.height.max(1.0));
                    let k = (r.w / bw).min(r.h / bh);
                    let (w, h) = (bw * k, bh * k);
                    p.bitmap(&b, Rect::new(r.cx() - w / 2.0, r.bottom() - h, w, h), 1.0);
                }
            };
            // Editable text in `r` (scrolled to keep the caret visible); returns each caret index's x.
            let edit_text = |p: &Painter, r: Rect, field: &TextField, focused: bool| -> Vec<f32> {
                let s = field.string();
                let layout = p.gfx.layout(&s, &regular, 100_000.0, r.h);
                let mut xs = Vec::with_capacity(field.text.len() + 1);
                let mut utf16 = 0;
                for i in 0..=field.text.len() {
                    let (mut x, mut y, mut m) = (0.0, 0.0, DWRITE_HIT_TEST_METRICS::default());
                    if let Some(l) = &layout {
                        unsafe {
                            let _ = l.HitTestTextPosition(utf16, false, &mut x, &mut y, &mut m);
                        }
                    }
                    xs.push(x);
                    utf16 += field.text.get(i).map_or(0, |c| c.len_utf16() as u32);
                }
                let shift = (xs[field.caret] - (r.w - 2.0)).max(0.0);
                let xs: Vec<f32> = xs.iter().map(|x| r.x + x - shift).collect();
                p.clipped(r.inset(-1.0, 0.0), || {
                    let (a, b) = field.range();
                    if a != b {
                        p.fill_rect(Rect::new(xs[a], r.y + 3.0, xs[b] - xs[a], r.h - 6.0), theme.accent.alpha(0.35));
                    } else if focused {
                        p.line(xs[a], r.y + 4.0, xs[a], r.bottom() - 4.0, theme.text, 1.0);
                    }
                    p.text(&s, &regular, Rect::new(xs[0], r.y, 100_000.0, r.h), theme.text);
                });
                xs
            };
            let draw_field = |p: &Painter, r: Rect, ren: &Rename| {
                p.fill_round(r, 3.0, field_bg);
                p.stroke_round(r, 3.0, theme.accent, 1.5);
                edit_text(p, Rect::new(r.x + 4.0, r.y, r.w - 8.0, r.h), &ren.field, true);
            };

            if floating {
                chrome::window_floating(p, body, SIDEBAR, theme, &pal);
            } else {
                chrome::window(p, body, SIDEBAR, theme, &pal);
            }

            // Sidebar: monochrome glyphs and rounded selection, inside the floating pane.
            let glyph_c = if dark { Color::gray(255, 0.78) } else { Color::gray(0, 0.72) };
            // Hover and selection glide from place to place; under the selection
            // the glyphs turn accent, clipped to it, so the colour moves with it.
            if let Some((g, a)) = side_glide {
                p.fill_round(g, 8.0, soft_t(a));
            }
            let sel_fill = if dark { Color::gray(255, 0.12) } else { Color::gray(0, 0.075) };
            if let Some((g, a)) = sel_glide {
                p.fill_round(g, 8.0, sel_fill.alpha(sel_fill.a * a));
            }
            let glyph_at = |i: usize| glyphs::Glyph::for_place(i, &places[i].name, i >= fav_count);
            for (i, place) in places.iter().enumerate() {
                let row = place_rows[i];
                if i == 0 || i == fav_count {
                    let header = if i == 0 { "Favorites" } else { "Locations" };
                    p.text(header, &small_bold, Rect::new(side.x + 14.0, row.y - 24.0, side.w - 24.0, 24.0), theme.text_secondary);
                }
                if drop_hit == Some(Hit::Place(i)) {
                    p.fill_round(row, 8.0, theme.accent.alpha(0.3));
                    p.stroke_round(row, 8.0, theme.accent, 1.5);
                }
                glyphs::draw(p, glyph_at(i), row.x + 16.0, row.cy(), glyph_c);
                p.text(&place.name, &regular, Rect::new(row.x + 32.0, row.y, row.w - 36.0, row.h), theme.text);
                hits.push((row, Hit::Place(i)));
            }
            if let Some((g, a)) = sel_glide {
                p.faded(a, || {
                    p.clipped(g, || {
                        for (i, row) in place_rows.iter().enumerate() {
                            if row.bottom() > g.y && row.y < g.bottom() {
                                glyphs::draw(p, glyph_at(i), row.x + 16.0, row.cy(), theme.accent);
                            }
                        }
                    });
                });
            }

            // Toolbar: glass capsules floating over the content, lined up with the traffic lights.
            let cy = body.y + CONTROL_CY;
            let ty = cy - CONTROL_H / 2.0;
            let nav = Rect::new(content_x + 12.0, ty, 76.0, CONTROL_H);
            glyphs::capsule(p, nav, dark, false);
            p.line(nav.cx(), nav.y + 9.0, nav.cx(), nav.bottom() - 9.0, pal.divider, 1.0);
            for (k, (hit, enabled)) in [(Hit::Back, can_back), (Hit::Forward, can_fwd)].into_iter().enumerate() {
                let b = Rect::new(nav.x + k as f32 * nav.w / 2.0, nav.y, nav.w / 2.0, nav.h);
                let t = if enabled { hv(hit) } else { 0.0 };
                if t > 0.0 {
                    p.fill_round(b.inset(3.0, 3.0), (b.h - 6.0) / 2.0, soft_t(t));
                }
                let c = if enabled { theme.text } else { theme.text_disabled };
                glyphs::chevron(p, b.cx(), b.cy(), hit == Hit::Back, 5.5, c, 1.7);
                if enabled {
                    hits.push((b, hit));
                }
            }

            // Search: a circle that expands into a field while in use.
            let right_edge = content_x + content_w - 14.0;
            // The circle grows into a field (and shrinks back) rather than jumping.
            let expanded = search_focus || !search.is_empty();
            let full_w = SEARCH_W.min(content_w * 0.35).max(120.0);
            let sw = motion.get(Anim::SearchW, if expanded { full_w } else { CONTROL_H }, feel::SMOOTH).max(CONTROL_H);
            let open_t = ((sw - CONTROL_H) / (full_w - CONTROL_H).max(1.0)).clamp(0.0, 1.0);
            let sf = Rect::new(right_edge - sw, ty, sw, CONTROL_H);
            glyphs::capsule(p, sf, dark, !expanded && hover == Some(Hit::Search));
            // Before the clear button's: later hits win, so the X stays clickable.
            hits.push((sf, Hit::Search));
            if open_t > 0.01 {
                if search_focus {
                    p.stroke_round(sf.inset(-2.0, -2.0), sf.h / 2.0 + 2.0, theme.accent.alpha(0.55 * open_t), 3.0);
                }
                let glyph_c = theme.text.alpha(lerp(theme.text.a, theme.text_secondary.a, open_t));
                glyphs::magnifier(p, lerp(sf.cx(), sf.x + 17.0, open_t), sf.cy(), glyph_c);
                let text_r = Rect::new(sf.x + 30.0, sf.y, (sf.w - if search.is_empty() { 40.0 } else { 54.0 }).max(0.0), sf.h);
                p.faded(open_t, || {
                    if search.is_empty() {
                        p.clipped(text_r, || p.text("Search", &regular, Rect::new(text_r.x, text_r.y, 200.0, text_r.h), theme.text_secondary));
                    }
                    search_xs = edit_text(p, text_r, search, search_focus);
                });
                if !search.is_empty() {
                    let (cx, cy) = (sf.right() - 16.0, sf.cy());
                    let a = (0.55 + 0.3 * hv(Hit::SearchClear)) * open_t;
                    p.fill_circle(cx, cy, 6.5, theme.text_secondary.alpha(a));
                    p.line(cx - 2.3, cy - 2.3, cx + 2.3, cy + 2.3, field_bg, 1.4);
                    p.line(cx + 2.3, cy - 2.3, cx - 2.3, cy + 2.3, field_bg, 1.4);
                    hits.push((Rect::new(cx - 9.0, cy - 9.0, 18.0, 18.0), Hit::SearchClear));
                }
            } else {
                glyphs::magnifier(p, sf.cx(), sf.cy(), theme.text);
                search_xs = vec![sf.x + 30.0];
            }

            // View switcher capsule, the current view on a darker pill.
            let seg_w = 40.0;
            let outer = Rect::new(sf.x - 10.0 - seg_w * 3.0 - 8.0, ty, seg_w * 3.0 + 8.0, CONTROL_H);
            glyphs::capsule(p, outer, dark, false);
            let views = [View::Icons, View::List, View::Columns];
            let seg_at = |k: f32| Rect::new(outer.x + 4.0 + k * seg_w, outer.y + 4.0, seg_w, outer.h - 8.0);
            for (k, v) in views.into_iter().enumerate() {
                let t = if v == view { 0.0 } else { hv(Hit::ViewBtn(v)) };
                if t > 0.0 {
                    let seg = seg_at(k as f32);
                    p.fill_round(seg, seg.h / 2.0, soft_t(t));
                }
            }
            // The selected pill slides between segments, like a native segmented control.
            let current = views.iter().position(|v| *v == view).unwrap_or(0) as f32;
            let pill = seg_at(motion.get(Anim::ViewPill, current, feel::SLIDE));
            p.fill_round(pill, pill.h / 2.0, if dark { Color::gray(255, 0.16) } else { Color::gray(0, 0.08) });
            for (k, v) in views.into_iter().enumerate() {
                let seg = seg_at(k as f32);
                let c = theme.text;
                let (cx, cy) = (seg.cx(), seg.cy());
                match v {
                    View::Icons => {
                        for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                            p.stroke_round(Rect::new(cx + dx * 3.6 - 2.6, cy + dy * 3.6 - 2.6, 5.2, 5.2), 1.3, c, 1.1);
                        }
                    }
                    View::List => {
                        for dy in [-4.5, 0.0, 4.5] {
                            p.fill_circle(cx - 6.0, cy + dy, 1.1, c);
                            p.line(cx - 3.0, cy + dy, cx + 7.0, cy + dy, c, 1.4);
                        }
                    }
                    View::Columns => {
                        let r = Rect::new(cx - 8.0, cy - 6.0, 16.0, 12.0);
                        p.stroke_round(r, 2.2, c, 1.3);
                        p.line(cx - 2.7, r.y, cx - 2.7, r.bottom(), c, 1.2);
                        p.line(cx + 2.7, r.y, cx + 2.7, r.bottom(), c, 1.2);
                    }
                }
                hits.push((seg, Hit::ViewBtn(v)));
            }

            let title_x = nav.right() + 14.0;
            p.text(&title, &title_font, Rect::new(title_x, ty, (outer.x - 10.0 - title_x).max(0.0), CONTROL_H), theme.text);

            // Tab bar: equal-width tabs, a close button on hover, and + at the end.
            if !tab_titles.is_empty() {
                let bar = Rect::new(content_x, body.y + TOOLBAR, content_w, TABBAR);
                let shade = if dark { Color::gray(0, 0.22) } else { Color::gray(0, 0.05) };
                p.push_clip(Rect::new(bar.x, bar.y, bar.w, tab_h + 1.0));
                p.fill_rect(bar, pal.content);
                let tw = (bar.w - NEW_TAB_W) / tab_titles.len() as f32;
                for (i, t) in tab_titles.iter().enumerate() {
                    let r = Rect::new(bar.x + i as f32 * tw, bar.y, tw, TABBAR);
                    let active = i == active_tab;
                    let tab_t = motion.fade(Anim::Hover(Hit::Tab(i)), matches!(hover, Some(Hit::Tab(h) | Hit::TabClose(h)) if h == i));
                    if !active {
                        p.fill_rect(r, shade);
                        if tab_t > 0.0 {
                            p.fill_rect(r, soft_t(tab_t));
                        }
                    }
                    if drop_hit == Some(Hit::Tab(i)) {
                        p.stroke_round(r.inset(2.0, 2.0), 4.0, theme.accent, 2.0);
                    }
                    p.line(r.right(), r.y, r.right(), r.bottom(), pal.divider, 1.0);
                    let tc = if active { theme.text } else { theme.text_secondary };
                    p.text(t, &center, Rect::new(r.x + 26.0, r.y, r.w - 52.0, r.h), tc);
                    hits.push((r, Hit::Tab(i)));
                    let close = Rect::new(r.x + 6.0, r.cy() - 8.0, 16.0, 16.0);
                    if tab_t > 0.0 {
                        let t = hv(Hit::TabClose(i));
                        if t > 0.0 {
                            p.fill_round(close, 4.0, soft_t(t));
                        }
                        let (cx, cy) = (close.cx(), close.cy());
                        let xc = theme.text_secondary.alpha(theme.text_secondary.a * tab_t);
                        p.line(cx - 3.5, cy - 3.5, cx + 3.5, cy + 3.5, xc, 1.3);
                        p.line(cx + 3.5, cy - 3.5, cx - 3.5, cy + 3.5, xc, 1.3);
                    }
                    hits.push((close, Hit::TabClose(i)));
                }
                let plus = Rect::new(bar.right() - NEW_TAB_W, bar.y, NEW_TAB_W, TABBAR);
                p.fill_rect(plus, shade);
                let t = hv(Hit::NewTab);
                if t > 0.0 {
                    p.fill_rect(plus, soft_t(t));
                }
                let (cx, cy) = (plus.cx(), plus.cy());
                p.line(cx - 5.0, cy, cx + 5.0, cy, theme.text_secondary, 1.4);
                p.line(cx, cy - 5.0, cx, cy + 5.0, theme.text_secondary, 1.4);
                hits.push((plus, Hit::NewTab));
                p.line(bar.x, bar.bottom(), bar.right(), bar.bottom(), pal.divider, 1.0);
                p.pop_clip();
            }

            // Content.
            hits.push((vr, Hit::Content));
            p.clipped(vr, || match view {
                View::Icons => {
                    let cw = (vr.w - 2.0 * pad) / grid_cols as f32;
                    let first_row = ((scroll - pad) / CELL_H).floor().max(0.0) as usize;
                    let last_row = ((scroll + vr.h) / CELL_H).ceil() as usize + 1;
                    for i in (first_row * grid_cols)..(last_row * grid_cols).min(n) {
                        let e = &entries[i];
                        let (col, row) = (i % grid_cols, i / grid_cols);
                        let cell = Rect::new(vr.x + pad + col as f32 * cw, vr.y + pad + row as f32 * CELL_H - scroll, cw, CELL_H);
                        let icon_r = Rect::new(cell.cx() - ICON / 2.0, cell.y + 6.0, ICON, ICON);
                        let sel = selected.contains(&i);
                        if drop_hit == Some(Hit::Item(i)) {
                            p.fill_round(icon_r.inset(-6.0, -5.0), 6.0, theme.accent.alpha(0.3));
                        } else if sel {
                            p.fill_round(icon_r.inset(-6.0, -5.0), 6.0, soft);
                        }
                        let bmp = icon_for(p, e);
                        draw_icon(p, bmp, icon_r);
                        let label = Rect::new(cell.x + 4.0, icon_r.bottom() + 6.0, cell.w - 8.0, 32.0);
                        match rename.as_ref().filter(|r| r.target == e.path) {
                            Some(ren) => draw_field(p, Rect::new(cell.x + 2.0, label.y, cell.w - 4.0, 20.0), ren),
                            None => {
                                if sel {
                                    let tw = p.gfx.measure(&e.name, &label_font).min(label.w);
                                    let lines = if p.gfx.measure(&e.name, &label_font) > label.w { 2.0 } else { 1.0 };
                                    p.fill_round(Rect::new(label.cx() - tw / 2.0 - 4.0, label.y - 1.0, tw + 8.0, lines * 15.5 + 2.0), 4.0, sel_bg);
                                }
                                p.text(&e.name, &label_font, label, if sel { sel_text } else { theme.text });
                            }
                        }
                        hits.extend(intersect(Rect::new(cell.x + 4.0, cell.y, cell.w - 8.0, CELL_H - 4.0), vr).map(|r| (r, Hit::Item(i))));
                    }
                }
                View::List => {
                    let x0 = vr.x + 8.0;
                    let w = vr.w - 16.0;
                    let cols = [
                        (SortKey::Name, "Name", x0, w * 0.46),
                        (SortKey::Modified, "Date Modified", x0 + w * 0.46, w * 0.25),
                        (SortKey::Size, "Size", x0 + w * 0.71, w * 0.11),
                        (SortKey::Kind, "Kind", x0 + w * 0.84, w * 0.16),
                    ];
                    let rows_top = vr.y + HEADER;
                    let first = ((scroll) / ROW).floor().max(0.0) as usize;
                    let last = (((scroll + vr.h) / ROW).ceil() as usize + 1).min(n);
                    let rows_clip = Rect::new(vr.x, rows_top, vr.w, vr.h - HEADER);
                    p.clipped(rows_clip, || {
                        for i in first..last {
                            let e = &entries[i];
                            let r = Rect::new(x0, rows_top + 4.0 + i as f32 * ROW - scroll, w, ROW);
                            let sel = selected.contains(&i);
                            if sel {
                                p.fill_round(r, 7.0, sel_bg);
                            } else if i % 2 == 1 {
                                p.fill_round(r, 7.0, stripe);
                            }
                            if drop_hit == Some(Hit::Item(i)) {
                                p.stroke_round(r.inset(1.0, 1.0), 5.0, theme.accent, 2.0);
                            }
                            let tc = if sel { sel_text } else { theme.text };
                            let sc = if sel { sel_text } else { theme.text_secondary };
                            let bmp = icon_for(p, e);
                            draw_icon(p, bmp, Rect::new(r.x + 6.0, r.cy() - 8.0, 16.0, 16.0));
                            let name_r = Rect::new(r.x + 28.0, r.y, cols[0].3 - 34.0, r.h);
                            match rename.as_ref().filter(|ren| ren.target == e.path) {
                                Some(ren) => draw_field(p, name_r.inset(0.0, 1.0), ren),
                                None => p.text(&e.name, &regular, name_r, tc),
                            }
                            let date = e.modified.map(fs::format_date).unwrap_or_else(|| "--".into());
                            p.text(&date, &regular, Rect::new(cols[1].2, r.y, cols[1].3 - 8.0, r.h), sc);
                            let size = if e.is_dir { "--".to_string() } else { fs::format_size(e.size) };
                            p.text(&size, &right, Rect::new(cols[2].2, r.y, cols[2].3 - 14.0, r.h), sc);
                            p.text(&kinds.kind(e), &regular, Rect::new(cols[3].2, r.y, cols[3].3 - 8.0, r.h), sc);
                            hits.extend(intersect(r, rows_clip).map(|r| (r, Hit::Item(i))));
                        }
                    });
                    // Header (drawn last so rows scroll under it).
                    let header = Rect::new(vr.x, vr.y, vr.w, HEADER);
                    p.fill_rect(header, pal.content);
                    for (k, (key, label, x, cw)) in cols.iter().enumerate() {
                        let r = Rect::new(*x, vr.y, *cw, HEADER);
                        if k > 0 {
                            p.line(r.x - 1.0, r.y + 6.0, r.x - 1.0, r.bottom() - 6.0, pal.divider, 1.0);
                        }
                        let font = if *key == sort { &small_bold } else { &small };
                        p.text(label, font, Rect::new(r.x + 8.0, r.y, r.w - 24.0, r.h), theme.text_secondary);
                        if *key == sort {
                            let (cx, cy) = (r.right() - 14.0, r.cy());
                            let d = if ascending { -1.0 } else { 1.0 };
                            p.line(cx - 3.5, cy - 1.5 * d, cx, cy + 1.5 * d, theme.text_secondary, 1.3);
                            p.line(cx, cy + 1.5 * d, cx + 3.5, cy - 1.5 * d, theme.text_secondary, 1.3);
                        }
                        hits.push((r, Hit::Header(*key)));
                    }
                    p.line(vr.x, header.bottom(), vr.right(), header.bottom(), pal.divider, 1.0);
                }
                View::Columns => {
                    for (c, col) in columns.iter().enumerate() {
                        let cx0 = vr.x + c as f32 * COL_W - column_offset;
                        let col_r = Rect::new(cx0, vr.y, COL_W, vr.h);
                        hits.extend(intersect(col_r, vr).map(|r| (r, Hit::ColumnBg(c))));
                        if let Some(err) = &col.error {
                            p.text(err, &small_center, col_r.inset(10.0, 0.0), theme.text_secondary);
                        }
                        let col_scroll = col.scroll.offset_px(px);
                        let first = (col_scroll / ROW).floor().max(0.0) as usize;
                        let last = (((col_scroll + vr.h) / ROW).ceil() as usize + 1).min(col.entries.len());
                        for j in first..last {
                            let e = &col.entries[j];
                            let r = Rect::new(cx0 + 6.0, vr.y + 4.0 + j as f32 * ROW - col_scroll, COL_W - 12.0, ROW);
                            let sel = col.selected == Some(j);
                            let active = c == active_col;
                            if sel {
                                p.fill_round(r, 5.0, if active { sel_bg } else { soft });
                            }
                            if drop_hit == Some(Hit::Column(c, j)) {
                                p.stroke_round(r.inset(1.0, 1.0), 5.0, theme.accent, 2.0);
                            }
                            let tc = if sel && active { sel_text } else { theme.text };
                            let bmp = icon_for(p, e);
                            draw_icon(p, bmp, Rect::new(r.x + 6.0, r.cy() - 8.0, 16.0, 16.0));
                            let name_r = Rect::new(r.x + 28.0, r.y, r.w - 46.0, r.h);
                            match rename.as_ref().filter(|ren| ren.target == e.path) {
                                Some(ren) => draw_field(p, name_r.inset(0.0, 1.0), ren),
                                None => p.text(&e.name, &regular, name_r, tc),
                            }
                            if e.is_dir {
                                let (x, y) = (r.right() - 10.0, r.cy());
                                p.line(x - 2.0, y - 4.0, x + 2.0, y, tc, 1.4);
                                p.line(x + 2.0, y, x - 2.0, y + 4.0, tc, 1.4);
                            }
                            hits.extend(intersect(r, vr).map(|r| (r, Hit::Column(c, j))));
                        }
                        p.line(col_r.right(), vr.y, col_r.right(), vr.bottom(), pal.divider, 1.0);
                        if drop_hit == Some(Hit::ColumnBg(c)) {
                            p.stroke_round(col_r.inset(2.0, 2.0), 4.0, theme.accent, 2.0);
                        }
                    }
                    if let Some(e) = &preview {
                        let x = vr.x + columns.len() as f32 * COL_W - column_offset;
                        let area = Rect::new(x, vr.y, PREVIEW_W, vr.h);
                        let icon = Rect::new(area.cx() - 64.0, area.y + 30.0, 128.0, 128.0);
                        let bmp = icon_for(p, e);
                        draw_icon(p, bmp, icon);
                        p.text(&e.name, &preview_title, Rect::new(area.x + 16.0, icon.bottom() + 14.0, area.w - 32.0, 22.0), theme.text);
                        let info = format!("{} - {}", kinds.kind(e), fs::format_size(e.size));
                        p.text(&info, &center, Rect::new(area.x + 16.0, icon.bottom() + 36.0, area.w - 32.0, 18.0), theme.text_secondary);
                        if let Some(m) = e.modified {
                            let line = format!("Modified {}", fs::format_date(m));
                            p.text(&line, &center, Rect::new(area.x + 16.0, icon.bottom() + 58.0, area.w - 32.0, 18.0), theme.text_secondary);
                        }
                    }
                }
            });

            if let Some(err) = error.as_ref().filter(|_| view != View::Columns) {
                p.text(err, &center, vr, theme.text_secondary);
            }

            // Dropping into the folder itself: frame the whole view.
            if drop_hit == Some(Hit::Content) {
                p.stroke_round(vr.inset(2.0, 2.0), 4.0, theme.accent, 2.0);
            }
            if let Some(b) = band {
                let top = if view == View::List { vr.y + HEADER } else { vr.y };
                p.clipped(Rect::new(vr.x, top, vr.w, vr.bottom() - top), || {
                    p.fill_rect(b, Color::gray(128, 0.18));
                    p.stroke_round(b, 0.0, Color::gray(128, 0.6), 1.0);
                });
            }

            // Overlay scrollbar.
            // Shows while scrolling, then fades away like macOS.
            if max_scroll > 0.0 && view != View::Columns && scrollbar_a > 0.01 {
                let total = vr.h + max_scroll;
                let th = (vr.h * vr.h / total).max(24.0);
                let ty = vr.y + (vr.h - th) * (scroll / max_scroll).clamp(0.0, 1.0);
                p.fill_round(Rect::new(vr.right() - 8.0, ty + 2.0, 5.0, th - 4.0), 2.5, Color::gray(128, 0.45 * scrollbar_a));
            }

            // Path bar: breadcrumbs from the root, trimmed from the left to fit.
            let pb = Rect::new(content_x, body.bottom() - STATUS - PATHBAR, content_w, PATHBAR);
            p.line(pb.x, pb.y, pb.right(), pb.y, pal.divider, 1.0);
            let widths: Vec<f32> = path_names.iter().map(|n| p.gfx.measure(n, &small) + 36.0).collect();
            let mut first = 0;
            while first + 1 < widths.len() && widths[first..].iter().sum::<f32>() > pb.w - 20.0 {
                first += 1;
            }
            let mut x = pb.x + 10.0;
            for (depth, name) in path_names.iter().enumerate().skip(first) {
                let w = widths[depth] - 14.0;
                let seg = Rect::new(x, pb.y + 2.0, w, pb.h - 4.0);
                let t = hv(Hit::Crumb(depth));
                if t > 0.0 {
                    p.fill_round(seg, 5.0, soft_t(t));
                }
                glyphs::folder(p, seg.x + 10.0, seg.cy(), 0.72, theme.accent);
                p.text(name, &small, Rect::new(seg.x + 20.0, seg.y, seg.w - 20.0, seg.h), theme.text_secondary);
                hits.push((seg, Hit::Crumb(depth)));
                x += w;
                if depth + 1 < path_names.len() {
                    glyphs::chevron(p, x + 7.0, pb.cy(), false, 3.0, theme.text_disabled, 1.1);
                    x += 14.0;
                }
            }

            // Status bar.
            let sb = Rect::new(content_x, body.bottom() - STATUS, content_w, STATUS);
            p.line(sb.x, sb.y, sb.right(), sb.y, pal.divider, 1.0);
            p.text(&status, &small_center, sb, theme.text_secondary);

            if let Some((ql, e)) = ql {
                let key = IconLoader::large_key(&e);
                let large = match ql_bitmap.as_ref().filter(|(k, _)| *k == key) {
                    Some((_, b)) => b.clone(),
                    None => {
                        let b = ql_px.and_then(|px| px.to_bitmap(p.rt));
                        if b.is_some() {
                            *ql_bitmap = Some((key, b.clone()));
                        }
                        b
                    }
                };
                // Until the large preview arrives, show the small icon.
                let image = large.or_else(|| icon_for(p, &e));
                let mut info = kinds.kind(&e);
                if !e.is_dir {
                    info += &format!("  ·  {}", fs::format_size(e.size));
                }
                if let Some(m) = e.modified {
                    info += &format!("  ·  Modified {}", fs::format_date(m));
                }
                let style = QlStyle { theme, dark, title: &ql_title, mono: &mono, center: &center, preview_title: &preview_title };
                // Quick Look zooms up out of the window and shrinks away when closed.
                let t = ql_t.clamp(0.0, 1.0);
                p.fill_round(body, chrome::RADIUS, Color::gray(0, 0.22 * t));
                let mut gone = Vec::new();
                let ql_hits = if ql_live {
                    hits.push((body, Hit::QlBackdrop));
                    &mut hits
                } else {
                    &mut gone
                };
                p.faded(t, || {
                    p.scaled_about(body.cx(), body.cy(), lerp(0.92, 1.0, ql_t), || {
                        quick_look(p, body, ql, &e, image, &info, &style, &hv, ql_hits);
                    })
                });
            }

            // Traffic lights last so they win hit-testing.
            let group_hover = motion.fade(Anim::Lights, matches!(hover, Some(Hit::Light(_))));
            // Lights sit inside the pane, centred on the toolbar row.
            let lights = Rect::new(body.x + 4.0, body.y + CONTROL_CY - 20.0, body.w, body.h);
            for (r, light) in chrome::traffic_lights(p, lights, group_hover, true) {
                hits.push((r, Hit::Light(light)));
            }
        }));
        self.hits = hits;
        self.search_xs = search_xs;
        if let Err(e) = drawn.and_then(|_| self.canvas.present_part(self.hwnd, rc.left, rc.top, window_alpha, part)) {
            log!("finder render failed: {e}");
        }
    }
}

/// The sidebar's place rows inside `side`: Favorites, then Locations, each under a header.
fn place_rows(side: Rect, n: usize, fav_count: usize) -> Vec<Rect> {
    let mut rows = Vec::with_capacity(n);
    let mut y = side.y + 46.0;
    for i in 0..n {
        if i == 0 || i == fav_count {
            y += 24.0;
        }
        rows.push(Rect::new(side.x + 6.0, y, side.w - 12.0, 28.0));
        y += 28.0;
        if i + 1 == fav_count {
            y += 10.0;
        }
    }
    rows
}

/// Normalized rectangle between two corners.
fn band_rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect::new(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs())
}

/// The part of `a` inside `b`, if any. Hit areas use this so rows scrolled out of
/// the view (under the toolbar, sidebar or header) can't steal clicks.
fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
    let (x1, y1) = (a.right().min(b.right()), a.bottom().min(b.bottom()));
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

struct QlStyle<'a> {
    theme: &'a Theme,
    dark: bool,
    title: &'a IDWriteTextFormat,
    mono: &'a IDWriteTextFormat,
    center: &'a IDWriteTextFormat,
    preview_title: &'a IDWriteTextFormat,
}

/// Quick Look: a large preview panel floating over the window. Text files show
/// their contents, images and videos a big thumbnail, everything else a big icon.
#[allow(clippy::too_many_arguments)]
fn quick_look(
    p: &Painter,
    body: Rect,
    ql: &mut QuickLook,
    e: &FileEntry,
    image: Option<ID2D1Bitmap>,
    info: &str,
    s: &QlStyle,
    hover: &dyn Fn(Hit) -> f32,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let theme = s.theme;
    let ink = if s.dark { 255 } else { 0 };

    let w = (body.w - 80.0).min(960.0);
    let h = body.h - 64.0;
    let panel = Rect::new(body.cx() - w / 2.0, body.cy() - h / 2.0, w, h);
    for k in 1..=4 {
        let d = k as f32 * 3.0;
        p.fill_round(panel.inset(-d, -d).offset(0.0, d), 12.0 + d, Color::gray(0, 0.045));
    }
    p.fill_round(panel, 12.0, if s.dark { Color::rgba(44, 44, 46, 1.0) } else { Color::rgba(246, 246, 246, 1.0) });
    p.stroke_round(panel, 12.0, Color::gray(ink, 0.12), 1.0);
    hits.push((panel, Hit::QlPanel));

    // Title bar: close, name, Open.
    let bar = Rect::new(panel.x, panel.y, panel.w, 40.0);
    p.text(&e.name, s.title, Rect::new(bar.x + 100.0, bar.y, bar.w - 200.0, bar.h), theme.text);
    let close = Rect::new(bar.x + 10.0, bar.cy() - 12.0, 24.0, 24.0);
    let t = hover(Hit::QlClose);
    if t > 0.0 {
        p.fill_round(close, 6.0, Color::gray(ink, 0.1 * t));
    }
    let (cx, cy) = (close.cx(), close.cy());
    p.line(cx - 4.5, cy - 4.5, cx + 4.5, cy + 4.5, theme.text_secondary, 1.6);
    p.line(cx + 4.5, cy - 4.5, cx - 4.5, cy + 4.5, theme.text_secondary, 1.6);
    hits.push((close, Hit::QlClose));
    let open = Rect::new(bar.right() - 74.0, bar.cy() - 12.0, 64.0, 24.0);
    p.fill_round(open, 6.0, Color::gray(ink, 0.08 + 0.08 * hover(Hit::QlOpen)));
    p.text("Open", s.center, open, theme.text);
    hits.push((open, Hit::QlOpen));
    p.line(panel.x, bar.bottom(), panel.right(), bar.bottom(), Color::gray(ink, 0.1), 1.0);

    let area = Rect::new(panel.x + 20.0, bar.bottom() + 16.0, panel.w - 40.0, panel.bottom() - bar.bottom() - 32.0);
    if let Some(text) = &ql.text {
        p.fill_round(area, 6.0, if s.dark { Color::rgba(30, 30, 30, 1.0) } else { Color::WHITE });
        let inner = area.inset(14.0, 12.0);
        let Some(layout) = p.gfx.layout(text, s.mono, inner.w, 1_000_000.0) else { return };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            let _ = layout.GetMetrics(&mut m);
        }
        ql.max_scroll = (m.height - inner.h).max(0.0);
        ql.scroll.clamp(ql.max_scroll);
        p.clipped(inner, || unsafe {
            if let Ok(brush) = p.rt.CreateSolidColorBrush(&theme.text.d2d(), None) {
                p.rt.DrawTextLayout(gfx_pt(inner.x, inner.y - ql.scroll.offset()), &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
        });
        return;
    }
    ql.max_scroll = 0.0;
    let thumbnail = IconLoader::key_for(e).1;
    let Some(b) = image else { return };
    let size = unsafe { b.GetSize() };
    let (bw, bh) = (size.width.max(1.0), size.height.max(1.0));
    if thumbnail {
        // Fit the preview, never upscaling it.
        let k = (area.w / bw).min(area.h / bh).min(1.0);
        let (iw, ih) = (bw * k, bh * k);
        p.bitmap(&b, Rect::new(area.cx() - iw / 2.0, area.cy() - ih / 2.0, iw, ih), 1.0);
    } else {
        // Big icon with the name and details under it.
        let side = 192.0_f32.min(area.h - 90.0).max(48.0);
        let top = area.cy() - (side + 70.0) / 2.0;
        let k = (side / bw).min(side / bh);
        let (iw, ih) = (bw * k, bh * k);
        p.bitmap(&b, Rect::new(area.cx() - iw / 2.0, top + side - ih, iw, ih), 1.0);
        p.text(&e.name, s.preview_title, Rect::new(area.x, top + side + 16.0, area.w, 24.0), theme.text);
        p.text(info, s.center, Rect::new(area.x, top + side + 42.0, area.w, 20.0), theme.text_secondary);
    }
}

impl Drop for FinderWindow {
    fn drop(&mut self) {
        self.watcher = None;
        dnd::revoke(self.hwnd);
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// `dir` and its ancestors, root first.
fn crumbs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = dir.ancestors().map(Path::to_path_buf).collect();
    v.reverse();
    v
}

#[cfg(test)]
mod bench {
    use super::*;

    /// On-screen frame rate while scrolling a big folder every frame.
    /// `cargo test --profile fast bench_finder -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_finder() {
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
        let mut loader = IconLoader::new(0, 0);
        let mut kinds = KindCache::default();
        for view in [View::List, View::Icons] {
            let prefs = FinderCfg { view, ..cfg.finder.clone() };
            let mut ctx = Ctx { gfx: &gfx, theme: &theme, loader: &mut loader, kinds: &mut kinds };
            let mut f = FinderWindow::new(&mut ctx, &Monitor::primary(), 1, PathBuf::from(r"C:\Windows\System32"), 0, 0, 0, &prefs).unwrap();
            f.render(&mut ctx);
            unsafe {
                let _ = ShowWindow(f.hwnd, SW_SHOWNOACTIVATE);
            }
            // Let icons arrive, then settle the opening animation.
            for _ in 0..30 {
                f.tick(1.0 / 60.0);
                f.render(&mut ctx);
                std::thread::sleep(Duration::from_millis(15));
            }
            let n = 150;
            let t = Instant::now();
            let mut work = 0.0;
            for k in 0..n {
                f.on_wheel(500.0, if k % 50 < 25 { -120 } else { 120 });
                f.tick(1.0 / 165.0);
                let t0 = Instant::now();
                f.render(&mut ctx);
                work += t0.elapsed().as_secs_f64() * 1000.0;
                unsafe {
                    let _ = windows::Win32::Graphics::Dwm::DwmFlush();
                }
            }
            println!("{view:?}: {:.0} fps on screen, render {:.2} ms", n as f64 / t.elapsed().as_secs_f64(), work / n as f64);

            // Partial frames must leave exactly what a full repaint would.
            f.on_wheel(500.0, -360);
            let mut partial = 0;
            for _ in 0..40 {
                if f.tick(1.0 / 165.0) {
                    partial += f.part.is_some() as u32;
                    f.render(&mut ctx);
                }
            }
            let after_partial = f.canvas.pixels();
            f.dirty = true;
            f.render(&mut ctx);
            let full = f.canvas.pixels();
            f.render(&mut ctx);
            let stable = f.canvas.pixels() == full;
            let differing = after_partial.chunks(4).zip(full.chunks(4)).filter(|(a, b)| a != b).count();
            println!("  {partial} partial frames; pixels differing from a full repaint: {differing} (full repaint deterministic: {stable})");
            // Antialiasing on curves may shift a level or two; nothing may be stale.
            let worst = after_partial.iter().zip(&full).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
            println!("  largest difference: {worst} / 255");
            assert!(partial > 0);
            if stable {
                assert!(worst <= 2);
            }
        }
    }
}
