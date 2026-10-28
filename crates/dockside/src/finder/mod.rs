//! Finder: a macOS-style file manager window with a sidebar, back/forward,
//! tabs, and icon / list / column / gallery views. It covers what File
//! Explorer does: shell locations (This PC, Network, the Recycle Bin, phones),
//! the shell's context menus, Open With, the clipboard, undo and search.

pub mod dnd;
pub mod fs;
mod glyphs;
pub mod loader;
mod menus;
pub mod ops;
pub mod search;
pub mod shell;
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

use self::fs::{Entry as FileEntry, KindCache, Place, PlaceKind, SortKey};
use self::ops::DropOp;
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
const COL_W: f32 = 240.0;
const PREVIEW_W: f32 = 300.0;
/// The preview / details pane beside the icon, list and gallery views.
const PANE_W: f32 = 260.0;
/// A group's heading in grouped icon and list views.
const GROUP_H: f32 = 30.0;
/// Gallery view: the strip of thumbnails under the big preview.
const STRIP_H: f32 = 96.0;
const STRIP_CELL: f32 = 76.0;
/// The scope and filter row under the toolbar while searching.
const SEARCHBAR: f32 = 30.0;
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
const APPS: u16 = 0x5D;
const F2: u16 = 0x71;
const F3: u16 = 0x72;
const F4: u16 = 0x73;
const F5: u16 = 0x74;
const F10: u16 = 0x79;
const F11: u16 = 0x7A;
const PLUS: u16 = 0xBB;
const MINUS: u16 = 0xBD;
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
    Gallery,
}

impl View {
    /// Name used in config.toml.
    pub fn name(self) -> &'static str {
        match self {
            View::Icons => "icons",
            View::List => "list",
            View::Columns => "columns",
            View::Gallery => "gallery",
        }
    }
}

/// Grouping in the icon and list views (macOS "Use Groups", Explorer "Group by").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    None,
    Name,
    Kind,
    Modified,
    Size,
}

impl Group {
    pub fn name(self) -> &'static str {
        match self {
            Group::None => "none",
            Group::Name => "name",
            Group::Kind => "kind",
            Group::Modified => "modified",
            Group::Size => "size",
        }
    }
}

/// A list view column after Name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Col {
    Modified,
    Created,
    Size,
    Kind,
    Location,
    Deleted,
    /// Drives in This PC: free space.
    Space,
}

impl Col {
    pub fn name(self) -> &'static str {
        match self {
            Col::Modified => "modified",
            Col::Created => "created",
            Col::Size => "size",
            Col::Kind => "kind",
            Col::Location => "location",
            Col::Deleted => "deleted",
            Col::Space => "space",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Col::Modified => "Date Modified",
            Col::Created => "Date Created",
            Col::Size => "Size",
            Col::Kind => "Kind",
            Col::Location => "Location",
            Col::Deleted => "Date Deleted",
            Col::Space => "Free Space",
        }
    }

    /// Share of the row's width (Name takes what's left).
    fn weight(self) -> f32 {
        match self {
            Col::Modified | Col::Created | Col::Deleted => 0.22,
            Col::Size => 0.11,
            Col::Kind => 0.16,
            Col::Location => 0.26,
            Col::Space => 0.24,
        }
    }

    fn sort_key(self) -> Option<SortKey> {
        Some(match self {
            Col::Modified => SortKey::Modified,
            Col::Created => SortKey::Created,
            Col::Size => SortKey::Size,
            Col::Kind => SortKey::Kind,
            Col::Location => SortKey::Location,
            Col::Deleted => SortKey::Deleted,
            Col::Space => return None,
        })
    }
}

/// The pane at the right of the icon, list and gallery views.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pane {
    None,
    /// A big preview with the details under it (Alt+P, macOS's Show Preview).
    Preview,
    /// Just the details (Alt+Shift+P, Explorer's details pane).
    Details,
}

impl Pane {
    pub fn name(self) -> &'static str {
        match self {
            Pane::None => "none",
            Pane::Preview => "preview",
            Pane::Details => "details",
        }
    }
}

#[derive(Clone, Debug)]
pub enum FinderCmd {
    Open(PathBuf),
    /// Open a folder in a new tab of this window.
    OpenTab(PathBuf),
    OpenWindow(PathBuf),
    Reveal(PathBuf),
    /// Open a folder in File Explorer itself.
    Explorer(PathBuf),
    /// Copy paths as text; `quoted` like Explorer's "Copy as path".
    CopyPath { paths: Vec<PathBuf>, quoted: bool },
    Copy(Vec<PathBuf>),
    Cut(Vec<PathBuf>),
    Paste(PathBuf),
    PasteShortcut(PathBuf),
    /// To the Recycle Bin, or (`recycle: false`) deleted for good.
    Delete { paths: Vec<PathBuf>, recycle: bool },
    Undo,
    Redo,
    Duplicate(Vec<PathBuf>),
    MakeShortcut(Vec<PathBuf>),
    Compress(Vec<PathBuf>),
    Extract(PathBuf),
    NewFolder,
    /// A file from the New menu ([`shell::new_types`] index).
    NewFile(usize),
    Rename,
    /// Open with handler `n` of the menu's Open With list.
    OpenWith(usize, Vec<PathBuf>),
    ChooseApp(PathBuf),
    RunAsAdmin(PathBuf),
    Properties(Vec<PathBuf>),
    /// A canonical shell verb ("Windows.ModernShare", "eject", "undelete", ...).
    Verb(Vec<PathBuf>, &'static str),
    /// An item of the shell menu under "More Options".
    ShellCommand(u32),
    EmptyTrash,
    Pin(PathBuf),
    Unpin(PathBuf),
    SetView(View),
    Sort(SortKey),
    GroupBy(Group),
    ToggleColumn(Col),
    SetPane(Pane),
    ToggleHidden,
    ToggleExtensions,
    Terminal(PathBuf),
    QuickLook,
    EditAddress,
    /// Set a search filter (`kind:`, `date:`, `size:`), replacing any with the same prefix.
    SearchFilter(&'static str, Option<&'static str>),
    /// Search all of This PC (true) or the folder the search started in.
    SearchScope(bool),
    /// Drag the items out with OLE drag and drop (modal).
    Drag(Vec<PathBuf>),
    /// Copy, move or link dropped items into a folder (modal: the shell shows progress).
    Transfer { paths: Vec<PathBuf>, dest: PathBuf, op: DropOp },
    /// A drop of items without paths (kept by [`ops::keep_dropped`]).
    TransferDropped { dest: PathBuf, copy: bool },
    /// A right-button drop: ask what to do (at screen points x, y).
    DropMenu { paths: Vec<PathBuf>, dest: PathBuf, x: f32, y: f32 },
    /// Re-list after a modal command and reset mouse state, selecting what it produced.
    Settle(Vec<PathBuf>),
}

impl FinderCmd {
    /// Commands that run a modal loop and so must run outside the App borrow (see [`run_modal`]).
    pub fn is_modal(&self) -> bool {
        use FinderCmd::*;
        matches!(
            self,
            Drag(_)
                | Transfer { .. }
                | TransferDropped { .. }
                | Paste(_)
                | Delete { .. }
                | Undo
                | Redo
                | Duplicate(_)
                | Compress(_)
                | Extract(_)
                | OpenWith(..)
                | ChooseApp(_)
                | RunAsAdmin(_)
                | Properties(_)
                | Verb(..)
                | ShellCommand(_)
                | EmptyTrash
        )
    }
}

/// Runs a modal command for the Finder window `hwnd`; returns what to select
/// afterwards. Follow up with [`FinderCmd::Settle`].
pub fn run_modal(hwnd: HWND, cmd: FinderCmd) -> Vec<PathBuf> {
    use FinderCmd::*;
    match cmd {
        Drag(paths) => {
            dnd::drag_out(hwnd, &paths);
        }
        // Dropping onto the Recycle Bin deletes.
        Transfer { paths, dest, .. } if dest.as_os_str().eq_ignore_ascii_case(shell::RECYCLE_BIN) => {
            ops::delete(hwnd, &paths, true);
        }
        Transfer { paths, dest, op } => return ops::transfer(hwnd, &paths, &dest, op),
        TransferDropped { dest, copy } => return ops::transfer_dropped(hwnd, &dest, copy),
        Paste(dest) => return ops::paste(hwnd, &dest),
        Delete { paths, recycle } => {
            ops::delete(hwnd, &paths, recycle);
        }
        Undo => return ops::undo(hwnd),
        Redo => return ops::redo(hwnd),
        Duplicate(paths) => return ops::duplicate(hwnd, &paths),
        Compress(paths) => return ops::compress(hwnd, &paths).into_iter().collect(),
        Extract(path) => return ops::extract(hwnd, &path).into_iter().collect(),
        OpenWith(i, paths) => shell::open_with(i, &paths),
        ChooseApp(path) => shell::choose_app(hwnd, &path),
        RunAsAdmin(path) => shell::run_as_admin(hwnd, &path),
        Properties(paths) => {
            shell::invoke_verb(hwnd, &paths, "properties");
        }
        Verb(paths, verb) => {
            shell::invoke_verb(hwnd, &paths, verb);
        }
        ShellCommand(id) => shell::invoke(hwnd, id, None),
        EmptyTrash => shell::empty_recycle_bin(hwnd),
        _ => {}
    }
    Vec::new()
}

pub enum FinderEvent {
    None,
    Close,
    /// Open another Finder window (at a folder, or Home).
    NewWindow(Option<PathBuf>),
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
    /// The path bar outside its segments (a click edits the path), and the address field itself.
    PathBar,
    Address,
    /// Search scope buttons, and filter pill `n` (kind, date, size).
    ScopeAll,
    ScopeHere,
    Filter(u8),
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

/// View settings new windows start with (saved to config.toml when they change).
#[derive(Clone, PartialEq, Debug)]
pub struct Prefs {
    pub view: View,
    pub sort: SortKey,
    pub ascending: bool,
    pub show_hidden: bool,
    pub show_ext: bool,
    pub group: Group,
    pub columns: Vec<Col>,
    pub pane: Pane,
    pub icon_size: f32,
}

/// Where items sit in the icon, list and gallery views, from the last render:
/// window points with the view scrolled to the top (or, in the gallery, the strip to the left).
#[derive(Default)]
struct Layout {
    /// Each entry's cell (icons), row (list) or thumbnail (gallery strip).
    cells: Vec<Rect>,
    /// Group headings: their top and group index.
    headers: Vec<(f32, usize)>,
    /// Content height (gallery: the strip's width).
    extent: f32,
    cols: usize,
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
    /// The big preview in the pane or gallery, for the item it shows.
    focus_bitmap: Option<(String, Option<ID2D1Bitmap>)>,
    typed: String,
    typed_at: Instant,
    bitmaps: HashMap<(String, (u32, u32)), Option<ID2D1Bitmap>>,
    /// Sidebar rows: Favorites (the first `fav_count`), then Locations.
    places: Vec<Place>,
    fav_count: usize,
    pins: Vec<PathBuf>,
    /// Pinned sidebar folders changed: the App saves them.
    pub pins_dirty: bool,
    side_scroll: Scroll,
    side_max: f32,
    watcher: Option<notify::RecommendedWatcher>,
    /// Where background listings and searches post their results.
    notify: isize,
    /// A shell folder being listed in the background (Network, phones, ...).
    pending: Option<shell::Pending>,
    /// The folder search running below the toolbar's search field.
    searching: Option<search::Search>,
    /// Search all drives rather than the current folder.
    search_all: bool,
    group: Group,
    /// Where each group starts in `entries`, and its heading.
    groups: Vec<(usize, String)>,
    list_cols: Vec<Col>,
    pane: Pane,
    /// The pane's details, for the item they were read for.
    details: Option<(PathBuf, Vec<(String, String)>)>,
    icon_size: f32,
    show_ext: bool,
    /// Item positions from the last render.
    layout: Layout,
    /// The path bar, while it's being edited as an address field.
    address: Option<TextField>,
    address_xs: Vec<f32>,
    /// A message in the status bar for a few seconds ("Windows can't find ...").
    flash: Option<(String, Instant)>,
    /// Events from commands (menus to open, windows to create), for the App to collect.
    outbox: Vec<FinderEvent>,
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
    /// Icons finished loading: the next frame repaints the list to show them.
    icons_ready: bool,
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
            focus_bitmap: None,
            typed: String::new(),
            typed_at: Instant::now(),
            bitmaps: HashMap::new(),
            places: Vec::new(),
            fav_count: 0,
            pins: prefs.sidebar_pins.clone(),
            pins_dirty: false,
            side_scroll: Scroll::default(),
            side_max: 0.0,
            watcher,
            notify,
            pending: None,
            searching: None,
            search_all: false,
            group: prefs.group,
            groups: Vec::new(),
            list_cols: if prefs.columns.is_empty() { vec![Col::Modified, Col::Size, Col::Kind] } else { prefs.columns.clone() },
            pane: prefs.pane,
            details: None,
            icon_size: prefs.icon_size.clamp(32.0, 256.0),
            show_ext: prefs.show_extensions,
            layout: Layout::default(),
            address: None,
            address_xs: Vec::new(),
            flash: None,
            outbox: Vec::new(),
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
            icons_ready: false,
            prefs_dirty: false,
            focused: true,
            tracking: false,
            dirty: true,
        };
        f.refresh_places();
        f.navigate(path, true, ctx.kinds);
        dnd::register(hwnd, id);
        Ok(f)
    }

    /// The window came to the front: drives may have changed, and shell places
    /// (the Recycle Bin, phones) have no change notifications, so re-list them.
    pub fn on_focus(&mut self, kinds: &mut KindCache) {
        self.refresh_places();
        if Self::needs_shell(&self.cwd) && self.searching.is_none() {
            self.reload(kinds);
        }
    }

    /// Re-reads the sidebar (drives come and go; pins change).
    pub fn refresh_places(&mut self) {
        let fav = fs::favorites(&self.pins);
        self.fav_count = fav.len();
        self.places = fav;
        self.places.extend(fs::locations());
        self.dirty = true;
    }

    /// Events queued by commands (menus to open, windows to create).
    pub fn take_events(&mut self) -> Vec<FinderEvent> {
        std::mem::take(&mut self.outbox)
    }

    /// Pinned sidebar folders, to save.
    pub fn pins(&self) -> &[PathBuf] {
        &self.pins
    }

    /// Takes new pins from another window's change.
    pub fn set_pins(&mut self, pins: &[PathBuf]) {
        if self.pins != pins {
            self.pins = pins.to_vec();
            self.refresh_places();
        }
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn title(&self) -> String {
        if self.searching.is_some() {
            return format!("Searching “{}”", if self.search_all { "This PC".to_string() } else { self.title_for(&self.cwd) });
        }
        self.title_for(&self.cwd)
    }

    fn title_for(&self, dir: &Path) -> String {
        if let Some(p) = self.places.iter().find(|p| p.path == dir && p.kind != PlaceKind::Pinned) {
            return p.name.clone();
        }
        shell::title(dir)
    }

    /// Settings that new windows start with.
    pub fn prefs(&self) -> Prefs {
        Prefs {
            view: self.view,
            sort: self.sort,
            ascending: self.ascending,
            show_hidden: self.show_hidden,
            show_ext: self.show_ext,
            group: self.group,
            columns: self.list_cols.clone(),
            pane: self.pane,
            icon_size: self.icon_size,
        }
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

    /// On `WM_SIZE`: redraw when restored from minimized, so it zooms back in.
    /// Icons arrived from the loader. Shown on the next frame, so a stream of
    /// them while scrolling a big folder doesn't add whole-window repaints
    /// between frames.
    pub fn on_icons(&mut self) {
        self.icons_ready = true;
    }

    pub fn on_size(&mut self) {
        if self.window.restored(self.hwnd) {
            self.dirty = true;
        }
    }

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
        if list || std::mem::take(&mut self.icons_ready) {
            touch(vr);
        }
        if self.side_glide.tick(dt) | self.sel_glide.tick(dt) | self.side_scroll.tick(dt) {
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
            || self.side_scroll.moving()
            || self.columns.iter().any(|c| c.scroll.moving())
            || self.quicklook.as_ref().is_some_and(|q| q.scroll.moving())
            || self.scrollbar_up
            || self.icons_ready
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
        self.pending = None;
        self.searching = None;
        self.address = None;
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

    /// Shell places, zip files and the like are listed through the shell; real folders with `std::fs`.
    fn needs_shell(dir: &Path) -> bool {
        !shell::is_fs_path(dir) || !dir.is_dir()
    }

    /// Lists `dir` right away (column view, and folders `std::fs` reads).
    fn list_sorted(&self, dir: &Path, kinds: &mut KindCache) -> (Vec<FileEntry>, Option<String>) {
        let listed = if Self::needs_shell(dir) {
            shell::list(dir, self.show_hidden)
        } else {
            fs::list(dir, self.show_hidden).map_err(|e| match e.kind() {
                std::io::ErrorKind::PermissionDenied => "You don't have permission to see this folder's contents.".to_string(),
                _ => format!("The folder can't be opened: {e}"),
            })
        };
        match listed {
            Ok(mut entries) => {
                fs::sort(&mut entries, self.sort, self.ascending, kinds);
                (entries, None)
            }
            Err(e) => (Vec::new(), Some(e)),
        }
    }

    fn start_pending(&mut self) {
        self.pending = Some(shell::Pending::start(self.cwd.clone(), self.show_hidden, self.notify, crate::win::WM_APP_FINDER_ASYNC, self.id));
    }

    /// Background results arrived (a shell listing or search matches).
    pub fn poll_async(&mut self, kinds: &mut KindCache) {
        if let Some(r) = self.pending.as_ref().and_then(|p| p.take().map(|r| (p.dir.clone(), r))) {
            self.pending = None;
            let (dir, result) = r;
            if dir == self.cwd && self.searching.is_none() {
                match result {
                    Ok(mut entries) => {
                        fs::sort(&mut entries, self.sort, self.ascending, kinds);
                        self.listing = entries;
                        self.error = None;
                    }
                    Err(e) => {
                        self.listing.clear();
                        self.error = Some(e);
                    }
                }
                self.apply_filter(kinds);
            }
        }
        if let Some(s) = &self.searching {
            let found = s.take();
            if !found.is_empty() {
                self.listing.extend(found);
                fs::sort(&mut self.listing, self.sort, self.ascending, kinds);
                self.apply_filter(kinds);
            }
        }
        self.dirty = true;
    }

    /// The search field changed: (re)start the search below the folder, or end it.
    fn search_changed(&mut self, kinds: &mut KindCache) {
        let query = search::Query::parse(&self.search.string());
        if query.is_empty() {
            if self.searching.take().is_some() {
                self.reload(kinds);
            } else {
                self.apply_filter(kinds);
            }
            return;
        }
        // Shell places can't be walked: search every drive from them.
        let roots = if self.search_all || !shell::is_fs_path(&self.cwd) {
            fs::drives().into_iter().filter(|d| d.kind == PlaceKind::Drive).map(|d| d.path).collect()
        } else {
            vec![self.cwd.clone()]
        };
        self.searching = Some(search::Search::start(roots, query, self.show_hidden, self.notify, crate::win::WM_APP_FINDER_ASYNC, self.id));
        self.pending = None;
        self.listing.clear();
        self.error = None;
        self.apply_filter(kinds);
        unsafe {
            let _ = SetWindowTextW(self.hwnd, &HSTRING::from(self.title()));
        }
    }

    pub fn navigate(&mut self, path: PathBuf, push: bool, kinds: &mut KindCache) {
        let shell_listed = Self::needs_shell(&path);
        let (entries, error) = if shell_listed { (Vec::new(), None) } else { self.list_sorted(&path, kinds) };
        self.searching = None;
        self.address = None;
        self.details = None;
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
        self.pending = None;
        if shell_listed {
            self.start_pending();
        }
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
        self.apply_filter(kinds);
        unsafe {
            let _ = SetWindowTextW(self.hwnd, &HSTRING::from(self.title()));
        }
        self.dirty = true;
    }

    /// Re-lists after on-disk changes, keeping selections by path.
    pub fn reload(&mut self, kinds: &mut KindCache) {
        if self.searching.is_some() {
            // Results stay; a change on disk doesn't restart the search.
            return;
        }
        if !self.search.is_empty() {
            // Back in a tab that was searching.
            return self.search_changed(kinds);
        }
        if Self::needs_shell(&self.cwd) {
            // Keep showing what's there until the new listing arrives.
            self.start_pending();
        } else {
            let (entries, error) = self.list_sorted(&self.cwd.clone(), kinds);
            self.listing = entries;
            self.error = error;
        }
        for c in 0..self.columns.len() {
            if c == 0 && self.pending.is_some() {
                continue;
            }
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
        self.apply_filter(kinds);
        self.free = fs::free_space(&self.cwd);
        self.dirty = true;
    }

    /// Rebuilds `entries` from `listing`, the search filters and the grouping, keeping selections by path.
    fn apply_filter(&mut self, kinds: &mut KindCache) {
        let keep: Vec<PathBuf> = self.selected.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.path.clone()).collect();
        let query = search::Query::parse(&self.search.string());
        let mut entries: Vec<FileEntry> =
            if query.is_empty() { self.listing.clone() } else { self.listing.iter().filter(|e| query.matches(e)).cloned().collect() };
        self.groups = self.group_entries(&mut entries, kinds);
        self.entries = entries;
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
        if let Some(parent) = shell::parent(&self.cwd) {
            let from = self.cwd.clone();
            self.navigate(parent, true, kinds);
            self.reselect(&[from]);
        }
    }

    fn go_home(&mut self, kind: PlaceKind, kinds: &mut KindCache) {
        if let Some(p) = self.places.iter().find(|p| p.kind == kind).map(|p| p.path.clone()) {
            self.navigate(p, true, kinds);
        }
    }

    /// Opens an item: folders (and zip files, and shortcuts to folders) open
    /// here, like Explorer; everything else with its default app.
    fn open_path(&mut self, path: &Path, is_dir: bool, kinds: &mut KindCache) {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if is_dir || shell::opens_as_folder(&ext) {
            return self.navigate(path.to_path_buf(), true, kinds);
        }
        if ext == "lnk" {
            if let Some(target) = shell::folder_link_target(path) {
                return self.navigate(target, true, kinds);
            }
        }
        shell::open(path);
    }

    fn is_dir(&self, path: &Path) -> bool {
        let known = self.entries.iter().chain(self.columns.iter().flat_map(|c| &c.entries)).find(|e| e.path == path);
        known.map_or_else(|| path.is_dir() || !shell::is_fs_path(path), |e| e.is_dir)
    }

    fn set_view(&mut self, view: View, kinds: &mut KindCache) {
        if self.view != view {
            // Carry the selection between views.
            if view == View::Columns {
                self.columns.truncate(1);
                self.columns[0].selected = self.selected.iter().next().copied();
            } else if self.view == View::Columns {
                self.selected = self.columns[0].selected.into_iter().collect();
            }
            let was_grouped = self.groups_apply();
            self.view = view;
            if was_grouped != self.groups_apply() {
                self.apply_filter(kinds);
            }
            if view == View::Gallery && self.selected.is_empty() && !self.entries.is_empty() {
                self.selected = [0].into();
                self.cursor = Some(0);
                self.anchor = Some(0);
            }
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
            // Dates and sizes start newest / largest first, like Explorer.
            self.ascending = matches!(key, SortKey::Name | SortKey::Kind | SortKey::Location);
        }
        self.prefs_dirty = true;
        fs::sort(&mut self.listing, self.sort, self.ascending, kinds);
        self.apply_filter(kinds);
        self.reload(kinds);
    }

    /// Grouping shows in the icon and list views.
    fn groups_apply(&self) -> bool {
        self.group != Group::None && matches!(self.view, View::Icons | View::List)
    }

    /// Orders entries by group (keeping the sort inside each) and returns where each group starts.
    fn group_entries(&self, entries: &mut Vec<FileEntry>, kinds: &mut KindCache) -> Vec<(usize, String)> {
        if !self.groups_apply() {
            return Vec::new();
        }
        let mut keyed: Vec<((u32, String), FileEntry)> = entries.drain(..).map(|e| (group_key(self.group, &e, kinds), e)).collect();
        keyed.sort_by(|a, b| a.0 .0.cmp(&b.0 .0).then_with(|| fs::natural_cmp(&a.0 .1, &b.0 .1)));
        let mut groups: Vec<(usize, String)> = Vec::new();
        for (i, (key, e)) in keyed.into_iter().enumerate() {
            if groups.last().map(|g| &g.1) != Some(&key.1) {
                groups.push((i, key.1));
            }
            entries.push(e);
        }
        groups
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
                let is_dir = self.is_dir(&p);
                self.open_path(&p, is_dir, kinds);
            }
            FinderCmd::OpenTab(p) => self.new_tab(p, kinds),
            FinderCmd::OpenWindow(p) => self.outbox.push(FinderEvent::NewWindow(Some(p))),
            FinderCmd::Reveal(p) => crate::win::system::reveal(&p.to_string_lossy()),
            FinderCmd::Explorer(p) => crate::win::system::open_in_explorer(&p.to_string_lossy()),
            FinderCmd::CopyPath { paths, quoted } => {
                let text: Vec<String> = paths.iter().map(|p| if quoted { format!("\"{}\"", p.display()) } else { p.display().to_string() }).collect();
                fs::copy_text(&text.join("\r\n"));
            }
            FinderCmd::Copy(paths) => {
                shell::set_clipboard(&paths, false);
            }
            FinderCmd::Cut(paths) => {
                shell::set_clipboard(&paths, true);
            }
            FinderCmd::PasteShortcut(dest) => {
                let paths = shell::clipboard().map(|(d, _)| dnd::hdrop_paths(&d)).unwrap_or_default();
                let made = ops::shortcuts(&paths, &dest);
                self.reload(kinds);
                self.reselect(&made);
            }
            FinderCmd::MakeShortcut(paths) => {
                let dir = paths.first().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_else(|| self.target_dir());
                let made = ops::shortcuts(&paths, &dir);
                self.reload(kinds);
                self.reselect(&made);
            }
            FinderCmd::NewFolder => self.new_folder(kinds),
            FinderCmd::NewFile(i) => self.new_file(i, kinds),
            FinderCmd::Rename => self.begin_rename(),
            FinderCmd::Pin(p) => {
                if !self.pins.contains(&p) {
                    self.pins.push(p);
                    self.pins_dirty = true;
                    self.refresh_places();
                }
            }
            FinderCmd::Unpin(p) => {
                self.pins.retain(|x| *x != p);
                self.pins_dirty = true;
                self.refresh_places();
            }
            FinderCmd::SetView(v) => self.set_view(v, kinds),
            FinderCmd::Sort(k) => self.set_sort(k, kinds),
            FinderCmd::GroupBy(g) => {
                self.group = g;
                self.prefs_dirty = true;
                self.apply_filter(kinds);
            }
            FinderCmd::ToggleColumn(c) => {
                if !self.list_cols.contains(&c) {
                    self.list_cols.push(c);
                    // Keep Explorer's usual order.
                    self.list_cols.sort_by_key(|c| *c as u8);
                } else {
                    self.list_cols.retain(|x| *x != c);
                }
                self.prefs_dirty = true;
            }
            FinderCmd::SetPane(p) => {
                self.pane = if self.pane == p { Pane::None } else { p };
                self.prefs_dirty = true;
            }
            FinderCmd::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.prefs_dirty = true;
                self.reload(kinds);
            }
            FinderCmd::ToggleExtensions => {
                self.show_ext = !self.show_ext;
                self.prefs_dirty = true;
            }
            FinderCmd::Terminal(p) => fs::open_terminal(&p),
            FinderCmd::QuickLook => self.toggle_quicklook(),
            FinderCmd::EditAddress => self.begin_address(),
            FinderCmd::SearchFilter(prefix, value) => {
                let kept: Vec<String> = self.search.string().split_whitespace().filter(|t| !t.to_lowercase().starts_with(prefix)).map(String::from).collect();
                let mut text = kept.join(" ");
                if let Some(v) = value {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text += &format!("{prefix}{v}");
                }
                self.search = TextField::new(&text);
                self.search_focus = true;
                self.search_changed(kinds);
            }
            FinderCmd::SearchScope(all) => {
                if self.search_all != all {
                    self.search_all = all;
                    self.search_changed(kinds);
                }
            }
            FinderCmd::DropMenu { paths, dest, x, y } => {
                let id = self.id;
                let t = |op: DropOp| Action::Finder { window: id, cmd: FinderCmd::Transfer { paths: paths.clone(), dest: dest.clone(), op } };
                let entries = vec![
                    Entry::item("Move Here", t(DropOp::Move)),
                    Entry::item("Copy Here", t(DropOp::Copy)),
                    Entry::item("Create Shortcuts Here", t(DropOp::Link)),
                ];
                self.outbox.push(FinderEvent::Menu(entries, x, y));
            }
            FinderCmd::Settle(select) => {
                // A modal loop swallowed the button release.
                self.end_press();
                self.drop_hit = None;
                self.reload(kinds);
                if !select.is_empty() {
                    if self.view == View::Columns {
                        self.columns.truncate(1);
                        self.columns[0].selected = self.columns[0].entries.iter().position(|e| select.contains(&e.path));
                    }
                    self.reselect(&select);
                    self.ensure_visible();
                }
            }
            // Modal: see `run_modal`.
            _ => {}
        }
        self.dirty = true;
    }

    fn new_file(&mut self, i: usize, kinds: &mut KindCache) {
        let Some(t) = shell::new_types().into_iter().nth(i) else { return };
        let dir = self.target_dir();
        match shell::create_new(&dir, &t) {
            Ok(path) => {
                ops::record_created(&format!("New {}", t.name), &[path.clone()]);
                self.after_create(&dir, path, kinds);
            }
            Err(e) => self.show_flash(format!("The file can't be created: {e}")),
        }
    }

    /// Selects a just-created item and starts renaming it, like Explorer.
    fn after_create(&mut self, dir: &Path, path: PathBuf, kinds: &mut KindCache) {
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

    fn show_flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
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
                ops::record_created("New Folder", &[path.clone()]);
                self.after_create(&dir, path, kinds);
            }
            Err(e) => self.show_flash(format!("The folder can't be created: {e}")),
        }
    }

    fn begin_rename(&mut self) {
        let Some(entry) = self.selection().first().map(|e| (*e).clone()) else { return };
        if !entry.fs {
            // Drives, phones' files and recycled items rename through their own properties.
            return;
        }
        let shown = entry.label(self.show_ext).to_string();
        let mut field = TextField::new(&shown);
        // Like macOS, preselect the name without its extension.
        let stem = if entry.is_dir {
            field.text.len()
        } else {
            shown.rfind('.').filter(|&i| i > 0).map(|i| shown[..i].chars().count()).unwrap_or(field.text.len())
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
        let old_name = r.target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // Renaming without the extension showing keeps the extension.
        let hidden_ext = self.entries.iter().chain(self.columns.iter().flat_map(|c| &c.entries)).find(|e| e.path == r.target)
            .filter(|e| e.label(self.show_ext).len() < e.name.len())
            .map(|e| e.name[e.label(self.show_ext).len()..].to_string());
        let name = match hidden_ext {
            Some(ext) if !name.to_lowercase().ends_with(&ext.to_lowercase()) => format!("{name}{ext}"),
            _ => name,
        };
        match fs::rename(&r.target, &name) {
            Ok(new_path) => {
                if new_path != r.target {
                    ops::record_rename(&new_path, &old_name);
                }
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
            Err(e) => self.show_flash(format!("“{name}” can't be used: {e}")),
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
        let top = TOOLBAR + self.tab_h() + self.search_h();
        let pane = if self.pane_shown() { PANE_W } else { 0.0 };
        Rect::new(b.x + SIDEBAR, b.y + top, b.w - SIDEBAR - pane, b.h - top - STATUS - PATHBAR)
    }

    /// The scope and filter bar shows while searching.
    fn search_h(&self) -> f32 {
        if self.searching.is_some() || !self.search.is_empty() {
            SEARCHBAR
        } else {
            0.0
        }
    }

    /// The preview / details pane, when on and there's room (column view has its own preview).
    fn pane_shown(&self) -> bool {
        self.pane != Pane::None && self.view != View::Columns && self.body().w - SIDEBAR > 2.0 * PANE_W
    }

    /// List columns after Name: the Recycle Bin and This PC have their own, and search adds Location.
    fn shown_cols(&self) -> Vec<Col> {
        let here = self.cwd.as_os_str();
        if here.eq_ignore_ascii_case(shell::RECYCLE_BIN) {
            return vec![Col::Location, Col::Deleted, Col::Size, Col::Kind];
        }
        if here.eq_ignore_ascii_case(shell::THIS_PC) {
            return vec![Col::Kind, Col::Size, Col::Space];
        }
        let mut c = self.list_cols.clone();
        if self.searching.is_some() && !c.contains(&Col::Location) {
            c.insert(0, Col::Location);
        }
        c
    }

    /// Icon view cell size for the current icon size.
    fn cell_size(&self) -> (f32, f32) {
        ((self.icon_size + 40.0).max(84.0), self.icon_size + 44.0)
    }

    /// Places every entry for the icon, list and gallery views.
    fn compute_layout(&mut self) {
        let vr = self.view_rect();
        let n = self.entries.len();
        let mut l = Layout { cells: Vec::with_capacity(n), headers: Vec::new(), extent: 0.0, cols: 1 };
        // Ungrouped views are one group without a heading.
        let groups: Vec<usize> = if self.groups.is_empty() { vec![0] } else { self.groups.iter().map(|g| g.0).collect() };
        let headed = !self.groups.is_empty();
        let ends: Vec<usize> = groups.iter().skip(1).copied().chain(std::iter::once(n)).collect();
        match self.view {
            View::Icons => {
                let (cell_w, cell_h) = self.cell_size();
                let cols = (((vr.w - 2.0 * GRID_PAD) / cell_w).floor() as usize).max(1);
                let cw = (vr.w - 2.0 * GRID_PAD) / cols as f32;
                let mut y = vr.y + GRID_PAD;
                for (k, (&start, &end)) in groups.iter().zip(&ends).enumerate() {
                    if headed {
                        l.headers.push((y, k));
                        y += GROUP_H;
                    }
                    for i in start..end {
                        let local = i - start;
                        l.cells.push(Rect::new(vr.x + GRID_PAD + (local % cols) as f32 * cw, y + (local / cols) as f32 * cell_h, cw, cell_h));
                    }
                    y += (end - start).div_ceil(cols) as f32 * cell_h;
                }
                l.cols = cols;
                l.extent = y - vr.y + GRID_PAD;
            }
            View::List => {
                let mut y = vr.y + HEADER + 4.0;
                for (k, (&start, &end)) in groups.iter().zip(&ends).enumerate() {
                    if headed {
                        l.headers.push((y, k));
                        y += GROUP_H;
                    }
                    for _ in start..end {
                        l.cells.push(Rect::new(vr.x + 8.0, y, vr.w - 16.0, ROW));
                        y += ROW;
                    }
                }
                l.extent = y - vr.y + 4.0;
            }
            View::Gallery => {
                let top = vr.bottom() - STRIP_H;
                for i in 0..n {
                    l.cells.push(Rect::new(vr.x + 12.0 + i as f32 * STRIP_CELL, top + 8.0, STRIP_CELL, STRIP_H - 16.0));
                }
                l.extent = n as f32 * STRIP_CELL + 24.0;
            }
            View::Columns => {}
        }
        self.grid_cols = l.cols;
        self.layout = l;
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
                        | Hit::Crumb(_)
                        | Hit::ScopeAll
                        | Hit::ScopeHere
                        | Hit::Filter(_)
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
        } else if self.pressed == Some(Hit::Address) {
            let i = nearest(&self.address_xs, x);
            if let Some(a) = &mut self.address {
                a.set_caret(i.min(a.text.len()), true);
                self.dirty = true;
            }
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

    /// `x`: window points. Ctrl+wheel changes the icon size, like Explorer.
    pub fn on_wheel(&mut self, x: f32, delta: i32, ctrl: bool) {
        self.scrolled_at = Instant::now();
        let body = self.body();
        if ctrl && self.quicklook.is_none() {
            self.zoom_icons(if delta > 0 { 1 } else { -1 });
            return;
        }
        if self.quicklook.is_none() && x < body.x + SIDEBAR {
            self.side_scroll.wheel_smooth(delta, 40.0, self.side_max);
            self.dirty = true;
            return;
        }
        if let Some(ql) = &mut self.quicklook {
            ql.scroll.wheel_smooth(delta, 60.0, ql.max_scroll);
        } else if self.view == View::Columns {
            let vr = self.view_rect();
            let offset = self.column_offset(vr);
            let c = ((x - vr.x + offset) / COL_W).floor().max(0.0) as usize;
            if let Some(col) = self.columns.get_mut(c) {
                let max = (col.entries.len() as f32 * ROW + 8.0 - vr.h).max(0.0);
                col.scroll.wheel_smooth(delta, 60.0, max);
            }
        } else {
            self.scroll.wheel_smooth(delta, 60.0, self.max_scroll);
        }
        // Only the list moved: unless a whole redraw is already due, repaint just it.
        if self.quicklook.is_none() && (!self.dirty || self.part.is_some()) {
            let vr = self.view_rect();
            self.part = Some(self.part.map_or(vr, |p| p.union(vr)));
        }
        self.dirty = true;
    }

    /// Steps the icon size (Ctrl+wheel, Ctrl+Plus / Minus).
    fn zoom_icons(&mut self, step: i32) {
        const SIZES: [f32; 8] = [32.0, 48.0, 64.0, 96.0, 128.0, 160.0, 192.0, 256.0];
        let i = SIZES.iter().position(|s| *s >= self.icon_size - 0.5).unwrap_or(2) as i32;
        self.icon_size = SIZES[(i + step).clamp(0, SIZES.len() as i32 - 1) as usize];
        self.prefs_dirty = true;
        self.dirty = true;
        self.ensure_visible();
    }

    /// Starts tracking a press that may become a drag.
    fn begin_press(&mut self, x: f32, y: f32) {
        self.drag_from = Some((x, y));
        unsafe {
            SetCapture(self.hwnd);
        }
    }

    pub fn on_down(&mut self, x: f32, y: f32, ctrl: bool, shift: bool, kinds: &mut KindCache) -> FinderEvent {
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
        if !matches!(hit, Some(Hit::Search | Hit::SearchClear | Hit::Filter(_) | Hit::ScopeAll | Hit::ScopeHere)) {
            self.search_focus = false;
        }
        // Clicking away from the address field leaves the path as it was, like Explorer.
        if !matches!(hit, Some(Hit::Address)) {
            self.address = None;
        }
        match hit {
            Some(Hit::Item(i)) => {
                if double {
                    if let Some(e) = self.entries.get(i).cloned() {
                        self.open_path(&e.path, e.is_dir, kinds);
                    }
                    return FinderEvent::None;
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
                        self.open_path(&e.path, false, kinds);
                    }
                    return FinderEvent::None;
                }
                self.select_column(c, j, kinds);
                self.begin_press(x, y);
            }
            Some(Hit::ColumnBg(c)) => {
                self.columns.truncate(c + 1);
                self.columns[c].selected = None;
                self.active_col = c;
            }
            Some(Hit::Content) if !matches!(self.view, View::Columns | View::Gallery) => {
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
                if let Some(p) = self.places.get(i).cloned() {
                    self.navigate(p.path, true, kinds);
                }
            }
            Some(Hit::PathBar) => self.begin_address(),
            Some(Hit::Address) => {
                if let Some(a) = &mut self.address {
                    if double {
                        a.select_all();
                    } else {
                        let i = nearest(&self.address_xs, x).min(a.text.len());
                        a.set_caret(i, shift);
                        unsafe {
                            SetCapture(self.hwnd);
                        }
                    }
                }
            }
            Some(Hit::ScopeAll) => self.command(FinderCmd::SearchScope(true), kinds),
            Some(Hit::ScopeHere) => self.command(FinderCmd::SearchScope(false), kinds),
            Some(Hit::Filter(n)) => {
                let r = self.hits.iter().find(|(_, h)| *h == Hit::Filter(n)).map(|(r, _)| *r).unwrap_or_default();
                let entries = self.filter_menu(n);
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetWindowRect(self.hwnd, &mut rc);
                }
                self.dirty = true;
                return FinderEvent::Menu(entries, rc.left as f32 / self.scale + r.x, rc.top as f32 / self.scale + r.bottom() + 4.0);
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
                self.search_changed(kinds);
            }
            Some(Hit::Crumb(depth)) => {
                let target = shell::crumbs(&self.cwd).into_iter().nth(depth);
                match target {
                    Some(dir) if dir != self.cwd => self.navigate(dir, true, kinds),
                    // The current folder's own segment: double-click edits the path.
                    Some(_) if double => self.begin_address(),
                    _ => {}
                }
            }
            Some(Hit::QlBackdrop) => self.close_quicklook(),
            // Tabs switch on press, like macOS.
            Some(Hit::Tab(i)) => self.select_tab(i, kinds),
            _ => {}
        }
        self.dirty = true;
        FinderEvent::None
    }

    /// The search bar's Kind / Date Modified / Size menus.
    fn filter_menu(&self, n: u8) -> Vec<Entry> {
        let id = self.id;
        let q = search::Query::parse(&self.search.string());
        let f = |prefix: &'static str, v: Option<&'static str>| Action::Finder { window: id, cmd: FinderCmd::SearchFilter(prefix, v) };
        let mut v = Vec::new();
        match n {
            0 => {
                v.push(Entry::item("Any Kind", f("kind:", None)).checked(q.kind.is_none()));
                v.push(Entry::Separator);
                for (k, key, label) in search::Kind::ALL {
                    v.push(Entry::item(label, f("kind:", Some(key))).checked(q.kind == Some(k)));
                }
            }
            1 => {
                v.push(Entry::item("Any Date", f("date:", None)).checked(q.when.is_none()));
                v.push(Entry::Separator);
                for (w, key, label) in search::When::ALL {
                    v.push(Entry::item(label, f("date:", Some(key))).checked(q.when == Some(w)));
                }
            }
            _ => {
                v.push(Entry::item("Any Size", f("size:", None)).checked(q.size.is_none()));
                v.push(Entry::Separator);
                for (s, key, label) in search::Size::ALL {
                    v.push(Entry::item(label, f("size:", Some(key))).checked(q.size == Some(s)));
                }
            }
        }
        v
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
                if let Some(p) = self.places.get(i).cloned() {
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
            Hit::ViewBtn(v) => self.set_view(v, kinds),
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
        let Some(cell) = self.layout.cells.get(i).copied() else { return [Rect::new(0.0, 0.0, 0.0, 0.0); 2] };
        if self.view == View::Icons {
            let s = self.icon_size;
            let icon = Rect::new(cell.cx() - s / 2.0, cell.y + 6.0, s, s);
            [icon, Rect::new(cell.x + 4.0, icon.bottom() + 6.0, cell.w - 8.0, 32.0)]
        } else {
            [cell, cell]
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
        // Cells are in order down the view: only those between the band's top and bottom can meet it.
        let cells = &self.layout.cells;
        let first = cells.partition_point(|c| c.bottom() < r.y);
        let last = cells.partition_point(|c| c.y <= r.bottom()).max(first);
        let hits: BTreeSet<usize> = (first..last).filter(|&i| self.item_rects(i).iter().any(|ir| overlaps(ir, &r))).collect();
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
            Some(Hit::Place(i)) => self.places.get(i).map(|p| (Hit::Place(i), p.path.clone())),
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

    /// Keyboard shortcuts: macOS's with Ctrl in place of ⌘, plus File Explorer's.
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

        if let Some(a) = &mut self.address {
            match vk {
                ENTER => self.go_address(kinds),
                ESC => self.address = None,
                TAB => self.complete_address(),
                _ => {
                    a.on_key(vk, ctrl, shift);
                }
            }
            return FinderEvent::None;
        }

        if self.search_focus {
            match vk {
                // Esc clears the search, then leaves the field.
                ESC if !self.search.is_empty() => {
                    self.search.clear();
                    self.search_changed(kinds);
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
                    KeyResult::Edited => self.search_changed(kinds),
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
        let paths: Vec<PathBuf> = self.selection().iter().map(|e| e.path.clone()).collect();
        let is = |c: char| vk == c as u16;
        let in_trash = self.cwd.as_os_str().eq_ignore_ascii_case(shell::RECYCLE_BIN);
        match (vk, ctrl, alt) {
            (SPACE, false, false) => self.toggle_quicklook(),
            _ if ctrl && !alt && is('Y') && shift => self.toggle_quicklook(),
            // Search: Ctrl+F (macOS), Ctrl+E and F3 (Explorer).
            _ if ctrl && !alt && (is('F') || is('E')) => self.focus_search(),
            (F3, false, false) => self.focus_search(),
            // The address field: Ctrl+L, Alt+D, F4, and macOS's Go to Folder.
            _ if (ctrl && !alt && is('L')) || (alt && !ctrl && is('D')) || (ctrl && shift && is('G')) => self.begin_address(),
            (F4, false, false) => self.begin_address(),
            _ if ctrl && !alt && is('N') && shift => self.new_folder(kinds),
            // A new window at this folder, like Explorer.
            _ if ctrl && !alt && is('N') => return FinderEvent::NewWindow(Some(self.cwd.clone())),
            _ if ctrl && !alt && is('T') => self.new_tab(fs::home(), kinds),
            // Ctrl+W closes the tab, or the window with its last tab.
            _ if ctrl && !alt && is('W') => {
                if self.close_tab(self.active_tab, kinds) {
                    return FinderEvent::Close;
                }
            }
            (TAB, true, false) => self.cycle_tab(if shift { -1 } else { 1 }, kinds),
            (RBRACKET, true, false) if shift => self.cycle_tab(1, kinds),
            (LBRACKET, true, false) if shift => self.cycle_tab(-1, kinds),
            // Views: Ctrl+1-4 (macOS); Ctrl+Shift+1-8 (Explorer: icon sizes, list, details, tiles, content).
            _ if ctrl && shift && !alt && ('1'..='8').any(is) => {
                let n = vk - b'0' as u16;
                if n <= 4 {
                    self.icon_size = [256.0, 128.0, 64.0, 32.0][n as usize - 1];
                    self.prefs_dirty = true;
                    self.set_view(View::Icons, kinds);
                } else {
                    self.set_view(View::List, kinds);
                }
            }
            _ if ctrl && !alt && is('1') => self.set_view(View::Icons, kinds),
            _ if ctrl && !alt && is('2') => self.set_view(View::List, kinds),
            _ if ctrl && !alt && is('3') => self.set_view(View::Columns, kinds),
            _ if ctrl && !alt && is('4') => self.set_view(View::Gallery, kinds),
            (PLUS, true, false) => self.zoom_icons(1),
            (MINUS, true, false) => self.zoom_icons(-1),
            _ if ctrl && !alt && is('A') => {
                if self.view != View::Columns {
                    self.selected = (0..self.entries.len()).collect();
                }
            }
            // Clipboard and undo.
            _ if ctrl && !alt && !shift && is('C') && !paths.is_empty() => self.command(FinderCmd::Copy(paths), kinds),
            _ if ctrl && !alt && is('X') && !paths.is_empty() => self.command(FinderCmd::Cut(paths), kinds),
            _ if ctrl && !alt && is('V') => return FinderEvent::Run(FinderCmd::Paste(self.target_dir())),
            _ if ctrl && !alt && is('Z') && shift => return FinderEvent::Run(FinderCmd::Redo),
            _ if ctrl && !alt && is('Z') => return FinderEvent::Run(FinderCmd::Undo),
            _ if ctrl && !alt && is('Y') => return FinderEvent::Run(FinderCmd::Redo),
            _ if ctrl && !alt && is('D') && !paths.is_empty() => return FinderEvent::Run(FinderCmd::Duplicate(paths)),
            // Copy as path: Ctrl+Shift+C (Windows 11) or Ctrl+Alt+C (macOS's ⌥⌘C).
            _ if ctrl && (shift || alt) && is('C') => {
                let paths = if paths.is_empty() { vec![self.target_dir()] } else { paths };
                self.command(FinderCmd::CopyPath { paths, quoted: shift }, kinds);
            }
            // Properties: Alt+Enter (Explorer) or Ctrl+I (macOS's Get Info).
            (ENTER, false, true) => {
                let paths = if paths.is_empty() { vec![self.target_dir()] } else { paths };
                return FinderEvent::Run(FinderCmd::Properties(paths));
            }
            _ if ctrl && !alt && is('I') => {
                let paths = if paths.is_empty() { vec![self.target_dir()] } else { paths };
                return FinderEvent::Run(FinderCmd::Properties(paths));
            }
            // Panes: Alt+P preview, Alt+Shift+P details (Explorer); Ctrl+Shift+P (macOS's Show Preview).
            _ if alt && !ctrl && is('P') => self.command(FinderCmd::SetPane(if shift { Pane::Details } else { Pane::Preview }), kinds),
            _ if ctrl && shift && !alt && is('P') => self.command(FinderCmd::SetPane(Pane::Preview), kinds),
            // macOS's Go menu.
            _ if ctrl && shift && !alt && is('H') => self.go_home(PlaceKind::Home, kinds),
            _ if ctrl && shift && !alt && is('D') => self.go_home(PlaceKind::Desktop, kinds),
            _ if ctrl && shift && !alt && is('O') => self.go_home(PlaceKind::Documents, kinds),
            _ if ctrl && shift && !alt && is('R') => self.go_home(PlaceKind::Recents, kinds),
            _ if ctrl && shift && !alt && is('K') => self.go_home(PlaceKind::Network, kinds),
            _ if ctrl && alt && is('L') => self.go_home(PlaceKind::Downloads, kinds),
            (UP, true, _) | (UP, _, true) => self.go_parent(kinds),
            (LEFT, _, true) | (LBRACKET, true, _) | (BACK, false, false) => self.go_back(kinds),
            (RIGHT, _, true) | (RBRACKET, true, _) => self.go_forward(kinds),
            (HOME, false, true) => self.go_home(PlaceKind::Home, kinds),
            (ENTER, false, false) | (DOWN, true, _) => {
                // Enter opens, like Explorer (macOS renames with Return; F2 does that here).
                for e in self.selection().into_iter().cloned().collect::<Vec<_>>() {
                    self.open_path(&e.path, e.is_dir, kinds);
                    if e.is_dir {
                        break;
                    }
                }
            }
            (F2, ..) => self.begin_rename(),
            // Delete recycles, Shift+Delete deletes for good (as it always does in the Recycle Bin).
            (DEL, false, false) | (BACK, true, false) if !paths.is_empty() => {
                return FinderEvent::Run(FinderCmd::Delete { paths, recycle: !shift && !in_trash });
            }
            (F5, ..) => {
                self.refresh_places();
                self.reload(kinds);
            }
            (F11, false, false) => self.toggle_zoom(),
            (F10, false, false) if shift => return self.key_menu(),
            (APPS, ..) => return self.key_menu(),
            // ⌘⇧. on macOS: toggle hidden files.
            (PERIOD, true, false) if shift => self.command(FinderCmd::ToggleHidden, kinds),
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
                LEFT if c > 0 => {
                    // Left: focus the parent column; this column stays visible.
                    self.columns.truncate(c + 1);
                    self.columns[c].selected = None;
                    self.active_col = c - 1;
                }
                RIGHT => {
                    // Right: into the selected folder's column.
                    if self.columns.len() > c + 1 && !self.columns[c + 1].entries.is_empty() {
                        self.select_column(c + 1, 0, kinds);
                    }
                }
                UP | DOWN | HOME | END if n > 0 => {
                    let j = match (vk, cur) {
                        (HOME, _) => 0,
                        (END, _) => n - 1,
                        (UP, Some(j)) => j.saturating_sub(1),
                        (DOWN, Some(j)) => (j + 1).min(n - 1),
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
        let cur = self.cursor.filter(|c| self.selected.contains(c)).or_else(|| self.selected.iter().next_back().copied());
        let next = match (vk, cur) {
            (_, None) => 0,
            (HOME, _) => 0,
            (END, _) => n - 1,
            (LEFT, Some(i)) if self.view != View::List => i.saturating_sub(1),
            (RIGHT, Some(i)) if self.view != View::List => (i + 1).min(n - 1),
            (UP, Some(i)) if self.view == View::Icons => self.vertical_neighbour(i, false),
            (DOWN, Some(i)) if self.view == View::Icons => self.vertical_neighbour(i, true),
            (UP, Some(i)) => i.saturating_sub(1),
            (DOWN, Some(i)) => (i + 1).min(n - 1),
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

    /// The icon in the row above or below item `i` nearest it horizontally (rows differ across groups).
    fn vertical_neighbour(&self, i: usize, down: bool) -> usize {
        let cells = &self.layout.cells;
        let Some(cur) = cells.get(i).copied() else { return i };
        let row_of = |y: f32| cells.iter().enumerate().filter(move |(_, c)| (c.y - y).abs() < 0.5);
        let target_y = if down {
            cells[i..].iter().map(|c| c.y).find(|y| *y > cur.y + 0.5)
        } else {
            cells[..i].iter().rev().map(|c| c.y).find(|y| *y < cur.y - 0.5)
        };
        let Some(y) = target_y else { return i };
        row_of(y).min_by(|a, b| (a.1.cx() - cur.cx()).abs().total_cmp(&(b.1.cx() - cur.cx()).abs())).map(|(j, _)| j).unwrap_or(i)
    }

    pub fn on_char(&mut self, ch: char, kinds: &mut KindCache) {
        if ch.is_control() {
            return;
        }
        if let Some(r) = &mut self.rename {
            r.field.insert(&ch.to_string());
            self.dirty = true;
            return;
        }
        if let Some(a) = &mut self.address {
            a.insert(&ch.to_string());
            self.dirty = true;
            return;
        }
        if self.search_focus {
            self.search.insert(&ch.to_string());
            self.search_changed(kinds);
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
            self.cursor = Some(i);
        }
        self.ensure_visible();
        self.dirty = true;
    }

    fn ensure_visible(&mut self) {
        let vr = self.view_rect();
        let cell = self.cursor.or(self.selected.iter().next().copied()).and_then(|i| self.layout.cells.get(i)).copied();
        match self.view {
            View::Icons | View::List => {
                let Some(c) = cell else { return };
                // Content offsets of the item's top and bottom, and the part of the view rows show in.
                let top_pad = if self.view == View::List { HEADER + 4.0 } else { 0.0 };
                let (top, bottom) = (c.y - vr.y - top_pad, c.bottom() - vr.y);
                let visible = vr.h - top_pad;
                let s = self.scroll.target();
                if top < s {
                    self.scroll.to(top.max(0.0));
                } else if bottom + 8.0 > s + top_pad + visible {
                    self.scroll.to((bottom + 8.0 - visible - top_pad).min(self.max_scroll.max(bottom + 8.0 - vr.h)));
                }
            }
            View::Gallery => {
                let Some(c) = cell else { return };
                let (left, right) = (c.x - vr.x - 12.0, c.right() - vr.x + 12.0);
                let s = self.scroll.target();
                if left < s {
                    self.scroll.to(left.max(0.0));
                } else if right > s + vr.w {
                    self.scroll.to(right - vr.w);
                }
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

    // --- address field -----------------------------------------------------------

    /// The path bar turns into a field showing the path, selected, like Explorer's address bar.
    fn begin_address(&mut self) {
        self.rename = None;
        self.search_focus = false;
        let text = if shell::is_fs_path(&self.cwd) { self.cwd.display().to_string() } else { self.title_for(&self.cwd.clone()) };
        let mut field = TextField::new(&text);
        field.select_all();
        self.address = Some(field);
        self.dirty = true;
    }

    /// Where typed text leads: a folder, a file, or nothing (Explorer then runs it as a command).
    fn resolve(&self, text: &str) -> Option<PathBuf> {
        let t = text.trim().trim_matches('"');
        if t.is_empty() {
            return None;
        }
        let named = match t.to_lowercase().as_str() {
            "this pc" | "computer" | "my computer" => Some(PathBuf::from(shell::THIS_PC)),
            "network" => Some(PathBuf::from(shell::NETWORK)),
            "recycle bin" | "trash" => Some(PathBuf::from(shell::RECYCLE_BIN)),
            "libraries" => Some(PathBuf::from(shell::LIBRARIES)),
            "~" | "home" => Some(fs::home()),
            lower => self.places.iter().find(|p| p.name.to_lowercase() == lower).map(|p| p.path.clone()),
        };
        if named.is_some() {
            return named;
        }
        // %VARIABLES% and ~\ expand, like Explorer and shells.
        let mut expanded = String::new();
        let mut rest = t;
        while let Some(a) = rest.find('%') {
            let Some(b) = rest[a + 1..].find('%') else { break };
            let name = &rest[a + 1..a + 1 + b];
            expanded += &rest[..a];
            expanded += &std::env::var(name).unwrap_or_else(|_| format!("%{name}%"));
            rest = &rest[a + b + 2..];
        }
        expanded += rest;
        let expanded = match expanded.strip_prefix("~\\").or_else(|| expanded.strip_prefix("~/")) {
            Some(r) => fs::home().join(r).display().to_string(),
            None => expanded,
        };
        let p = PathBuf::from(expanded.replace('/', "\\"));
        let s = p.to_string_lossy().to_string();
        let candidate = if shell::is_fs_path(&p) || s.starts_with("::") || s.to_lowercase().starts_with("shell:") {
            p
        } else if shell::is_fs_path(&self.cwd) {
            self.cwd.join(&p)
        } else {
            p
        };
        // The shell understands everything else (shell:Downloads, ::{GUID}, UNC, ...).
        shell::item(&candidate).and_then(|i| shell::parsing_name(&i)).map(PathBuf::from)
    }

    fn go_address(&mut self, kinds: &mut KindCache) {
        let Some(field) = self.address.take() else { return };
        let text = field.string();
        match self.resolve(&text) {
            Some(p) => {
                let is_dir = p.is_dir() || !shell::is_fs_path(&p) && shell::list(&p, false).is_ok();
                self.open_path(&p, is_dir, kinds);
            }
            // Explorer runs what it can't find ("notepad", "cmd").
            None if crate::win::elevation::shell_open(text.trim(), None, shell::is_fs_path(&self.cwd).then(|| self.cwd.to_string_lossy().into_owned()).as_deref()) => {}
            None => {
                self.show_flash(format!("Windows can't find “{}”. Check the spelling and try again.", text.trim()));
                self.address = Some(field);
            }
        }
        self.dirty = true;
    }

    /// Tab completes the folder name being typed.
    fn complete_address(&mut self) {
        let Some(a) = &mut self.address else { return };
        let text = a.string();
        let (dir, prefix) = match text.rfind('\\') {
            Some(i) => (text[..=i].to_string(), text[i + 1..].to_lowercase()),
            None => return,
        };
        let Ok(items) = fs::list(Path::new(&dir), self.show_hidden) else { return };
        let mut names: Vec<String> = items.into_iter().filter(|e| e.is_dir && e.name.to_lowercase().starts_with(&prefix)).map(|e| e.name).collect();
        names.sort_by(|a, b| fs::natural_cmp(a, b));
        // Cycle to the next match if this one is already complete.
        let current = text[dir.len()..].trim_end_matches('\\').to_string();
        let next = names.iter().position(|n| *n == current).map(|i| (i + 1) % names.len()).unwrap_or(0);
        if let Some(name) = names.get(next) {
            *a = TextField::new(&format!("{dir}{name}\\"));
            self.dirty = true;
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

        // Layout and scroll metrics for the current view (the gallery's strip scrolls sideways).
        let n = self.entries.len();
        self.compute_layout();
        let span = if self.view == View::Gallery { vr.w } else { vr.h };
        self.max_scroll = if self.view == View::Columns { 0.0 } else { (self.layout.extent - span).max(0.0) };
        self.scroll.clamp(self.max_scroll);
        if self.flash.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(6)) {
            self.flash = None;
        }
        // The pane's details follow the selection.
        let pane_on = self.pane_shown();
        let focus_entry = if pane_on || self.view == View::Gallery { self.ql_entry().cloned() } else { None };
        match &focus_entry {
            Some(e) if pane_on && self.details.as_ref().map(|d| &d.0) != Some(&e.path) => {
                self.details = Some((e.path.clone(), shell::details(&e.path)));
            }
            None => self.details = None,
            _ => {}
        }
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
            if let Some((msg, _)) = &self.flash {
                msg.clone()
            } else if let Some(s) = &self.searching {
                let state = if s.finished() { "found" } else { "found so far…" };
                format!("{total} item{} {state}", if total == 1 { "" } else { "s" })
            } else if self.pending.is_some() && n == 0 {
                "Loading…".to_string()
            } else {
                match self.free {
                    Some(f) => format!("{items}, {} available", fs::format_size(f)),
                    None => items,
                }
            }
        };
        let title = self.title();
        let path_names: Vec<String> = shell::crumbs(&self.cwd).iter().map(|d| self.title_for(d)).collect();
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

        let shown_cols = self.shown_cols();
        let search_bar = self.search_h();
        let kinds = &mut *ctx.kinds;
        let loader = &mut *ctx.loader;
        let bitmaps = &mut self.bitmaps;
        let (entries, columns, selected) = (&self.entries, &self.columns, &self.selected);
        let (view, hover, error, rename) = (self.view, self.hover, &self.error, &self.rename);
        let scroll = self.scroll.offset_px(self.scale);
        let px = self.scale;
        let (max_scroll, active_col) = (self.max_scroll, self.active_col);
        let motion = &self.motion;
        let (sort, ascending) = (self.sort, self.ascending);
        let (can_back, can_fwd) = (self.hist_pos > 0, self.hist_pos + 1 < self.history.len());
        let places: Vec<&Place> = self.places.iter().collect();
        let fav_count = self.fav_count;
        let cwd = &self.cwd;
        // Sidebar rows (scrolled when they don't fit), and the hover highlight gliding between them.
        let side = if floating { chrome::sidebar_pane(body, SIDEBAR) } else { Rect::new(body.x, body.y, SIDEBAR, body.h) };
        let unscrolled = place_rows(side, places.len(), fav_count, 0.0);
        self.side_max = unscrolled.last().map_or(0.0, |r| (r.bottom() + 12.0 - side.bottom()).max(0.0));
        self.side_scroll.clamp(self.side_max);
        let side_off = self.side_scroll.offset();
        let place_rows = place_rows(side, places.len(), fav_count, side_off);
        let side_clip = Rect::new(side.x, side.y + 40.0, side.w, side.h - 40.0);
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
        let mut address_xs: Vec<f32> = Vec::new();
        let show_ext = self.show_ext;
        let layout = &self.layout;
        let groups = &self.groups;
        let icon_size = self.icon_size;
        let address = self.address.as_ref();
        let search_all = self.search_all;
        let scope_here = if shell::is_fs_path(cwd) { format!("“{}”", shell::title(cwd)) } else { String::new() };
        let query = search::Query::parse(&self.search.string());
        let pane = if pane_on { self.pane } else { Pane::None };
        let details = self.details.as_ref().map(|d| d.1.as_slice()).unwrap_or(&[]);
        let big_px = focus_entry.as_ref().filter(|_| pane == Pane::Preview || view == View::Gallery).and_then(|e| loader.get_large(e));
        let focus_bitmap = &mut self.focus_bitmap;
        let multi = if view == View::Columns { 0 } else { selected.len() };

        // Just the animating area when that's all that changed (and the window
        // itself isn't zooming or fading).
        let part = self.part.take().filter(|_| !resized && zoom == 1.0 && window_alpha == 1.0).and_then(|r| self.canvas.part_px(r));
        let drawn = self.canvas.draw_part(gfx, part, |p| p.scaled_about(zoom_x, zoom_y, zoom, || {
            // Hover highlights fade in and out rather than blinking.
            let hv = |h: Hit| motion.fade(Anim::Hover(h), hover == Some(h));
            let soft_t = |t: f32| soft.alpha(soft.a * t);
            // An item's icon, pre-scaled to fit `fit` (or full size for Quick Look).
            let mut icon_for = |p: &Painter, e: &FileEntry, fit: Option<Rect>| -> Option<ID2D1Bitmap> {
                let (key, _) = IconLoader::key_for(e);
                let size = fit.map_or((0, 0), |r| ((r.w * px).round() as u32, (r.h * px).round() as u32));
                let key = (key, size);
                if let Some(b) = bitmaps.get(&key) {
                    return b.clone();
                }
                let full = loader.get_entry(e)?.to_bitmap(p.rt);
                let bmp = match fit {
                    Some(r) => full.and_then(|b| p.fitted(&b, r.w, r.h)),
                    None => full,
                };
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
                    if (k - 1.0).abs() < 0.01 {
                        p.stamp(&b, r.cx() - w / 2.0, r.bottom() - h, px);
                    } else {
                        p.bitmap(&b, Rect::new(r.cx() - w / 2.0, r.bottom() - h, w, h), 1.0);
                    }
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
            let sel_fill = if dark { Color::gray(255, 0.12) } else { Color::gray(0, 0.075) };
            let glyph_at = |i: usize| glyphs::Glyph::for_place(places[i].kind);
            p.push_clip(side_clip);
            if let Some((g, a)) = side_glide {
                p.fill_round(g, 8.0, soft_t(a));
            }
            if let Some((g, a)) = sel_glide {
                p.fill_round(g, 8.0, sel_fill.alpha(sel_fill.a * a));
            }
            for (i, place) in places.iter().enumerate() {
                let row = place_rows[i];
                if i == 0 || i == fav_count {
                    let header = if i == 0 { "Favorites" } else { "Locations" };
                    p.text(header, &small_bold, Rect::new(side.x + 14.0, row.y - 24.0, side.w - 24.0, 24.0), theme.text_secondary);
                }
                if row.bottom() < side_clip.y || row.y > side_clip.bottom() {
                    continue;
                }
                if drop_hit == Some(Hit::Place(i)) {
                    p.fill_round(row, 8.0, theme.accent.alpha(0.3));
                    p.stroke_round(row, 8.0, theme.accent, 1.5);
                }
                glyphs::draw(p, glyph_at(i), row.x + 16.0, row.cy(), glyph_c);
                p.text(&place.name, &regular, Rect::new(row.x + 32.0, row.y, row.w - 36.0, row.h), theme.text);
                hits.extend(intersect(row, side_clip).map(|r| (r, Hit::Place(i))));
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
            p.pop_clip();

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
            let outer = Rect::new(sf.x - 10.0 - seg_w * 4.0 - 8.0, ty, seg_w * 4.0 + 8.0, CONTROL_H);
            glyphs::capsule(p, outer, dark, false);
            let views = [View::Icons, View::List, View::Columns, View::Gallery];
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
                    View::Gallery => {
                        p.stroke_round(Rect::new(cx - 8.0, cy - 6.5, 16.0, 9.0), 2.0, c, 1.3);
                        for dx in [-6.0, -1.5, 3.0] {
                            p.fill_round(Rect::new(cx + dx, cy + 4.0, 3.0, 2.6), 0.8, c);
                        }
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

            // Search scope and filters, under the toolbar (and tabs), like macOS's search bar.
            if search_bar > 0.0 {
                let bar = Rect::new(content_x, body.y + TOOLBAR + tab_h, content_w, SEARCHBAR);
                p.fill_rect(bar, pal.content);
                p.line(bar.x, bar.bottom(), bar.right(), bar.bottom(), pal.divider, 1.0);
                p.text("Search:", &small, Rect::new(bar.x + 12.0, bar.y, 60.0, bar.h), theme.text_secondary);
                let kind = query.kind.and_then(|k| search::Kind::ALL.iter().find(|x| x.0 == k)).map(|x| x.2);
                let when = query.when.and_then(|w| search::When::ALL.iter().find(|x| x.0 == w)).map(|x| x.2);
                let size = query.size.and_then(|s| search::Size::ALL.iter().find(|x| x.0 == s)).map(|x| x.2);
                let mut pills: Vec<(String, bool, Hit)> = vec![("This PC".into(), search_all, Hit::ScopeAll)];
                if !scope_here.is_empty() {
                    pills.push((scope_here.clone(), !search_all, Hit::ScopeHere));
                }
                pills.push((kind.map_or("Kind".into(), |k| format!("Kind: {k}")), kind.is_some(), Hit::Filter(0)));
                pills.push((when.map_or("Date Modified".into(), |w| format!("Modified: {w}")), when.is_some(), Hit::Filter(1)));
                pills.push((size.map_or("Size".into(), |s| format!("Size: {s}")), size.is_some(), Hit::Filter(2)));
                let mut x = bar.x + 12.0 + p.gfx.measure("Search:", &small) + 8.0;
                for (k, (label, on, hit)) in pills.iter().enumerate() {
                    // A gap between the scope and the filters.
                    if k > 0 && matches!(hit, Hit::Filter(0)) {
                        x += 12.0;
                    }
                    let chevron = matches!(hit, Hit::Filter(_));
                    let w = p.gfx.measure(label, &small) + if chevron { 30.0 } else { 20.0 };
                    let r = Rect::new(x, bar.y + 5.0, w, bar.h - 10.0);
                    if x + w > bar.right() - 8.0 {
                        break;
                    }
                    if *on {
                        p.fill_round(r, r.h / 2.0, theme.accent.alpha(0.16));
                    } else {
                        let t = hv(*hit);
                        p.fill_round(r, r.h / 2.0, soft_t(0.6 + 0.4 * t));
                    }
                    let tc = if *on { theme.accent } else { theme.text };
                    p.text(label, &small, Rect::new(r.x + 10.0, r.y, r.w - 10.0, r.h), tc);
                    if chevron {
                        let (cx, cy) = (r.right() - 11.0, r.cy());
                        p.line(cx - 3.0, cy - 1.5, cx, cy + 1.5, tc, 1.2);
                        p.line(cx, cy + 1.5, cx + 3.0, cy - 1.5, tc, 1.2);
                    }
                    hits.push((r, *hit));
                    x += w + 6.0;
                }
            }

            // Group headings, in content coordinates.
            let group_heading = |p: &Painter, y: f32, k: usize| {
                let r = Rect::new(vr.x + 12.0, y - scroll, vr.w - 24.0, GROUP_H);
                if r.bottom() < vr.y || r.y > vr.bottom() {
                    return;
                }
                let label = groups.get(k).map(|g| g.1.as_str()).unwrap_or("");
                p.text(label, &small_bold, Rect::new(r.x + 2.0, r.y + 6.0, r.w, r.h - 8.0), theme.text);
                p.line(r.x, r.bottom() - 3.0, r.right(), r.bottom() - 3.0, pal.divider, 1.0);
            };
            // Hidden and cut items show faded, like Explorer.
            let dim = |sel: bool, e: &FileEntry| if (e.hidden || shell::is_cut(&e.path)) && !sel { 0.5 } else { 1.0 };

            // Content.
            hits.push((vr, Hit::Content));
            p.clipped(vr, || match view {
                View::Icons => {
                    let cells = &layout.cells;
                    // Cells run down the view: draw only those in sight.
                    let first = cells.partition_point(|c| c.bottom() - scroll < vr.y);
                    let last = cells.partition_point(|c| c.y - scroll <= vr.bottom());
                    for &(y, k) in &layout.headers {
                        group_heading(p, y, k);
                    }
                    for i in first..last.max(first).min(n) {
                        let e = &entries[i];
                        let cell = cells[i].offset(0.0, -scroll);
                        let icon_r = Rect::new(cell.cx() - icon_size / 2.0, cell.y + 6.0, icon_size, icon_size);
                        let sel = selected.contains(&i);
                        if drop_hit == Some(Hit::Item(i)) {
                            p.fill_round(icon_r.inset(-6.0, -5.0), 6.0, theme.accent.alpha(0.3));
                        } else if sel {
                            p.fill_round(icon_r.inset(-6.0, -5.0), 6.0, soft);
                        }
                        let bmp = icon_for(p, e, Some(icon_r));
                        p.faded(dim(sel, e), || draw_icon(p, bmp, icon_r));
                        let label = Rect::new(cell.x + 4.0, icon_r.bottom() + 6.0, cell.w - 8.0, 32.0);
                        let name = e.label(show_ext);
                        match rename.as_ref().filter(|r| r.target == e.path) {
                            Some(ren) => draw_field(p, Rect::new(cell.x + 2.0, label.y, cell.w - 4.0, 20.0), ren),
                            None => {
                                if sel {
                                    let tw = p.gfx.measure(name, &label_font).min(label.w);
                                    let lines = if p.gfx.measure(name, &label_font) > label.w { 2.0 } else { 1.0 };
                                    p.fill_round(Rect::new(label.cx() - tw / 2.0 - 4.0, label.y - 1.0, tw + 8.0, lines * 15.5 + 2.0), 4.0, sel_bg);
                                }
                                p.text(name, &label_font, label, if sel { sel_text } else { theme.text });
                            }
                        }
                        hits.extend(intersect(Rect::new(cell.x + 4.0, cell.y, cell.w - 8.0, cell.h - 4.0), vr).map(|r| (r, Hit::Item(i))));
                    }
                }
                View::List => {
                    let x0 = vr.x + 8.0;
                    let w = vr.w - 16.0;
                    // Name takes what the other columns leave (but at least a third).
                    let rest: f32 = shown_cols.iter().map(|c| c.weight()).sum::<f32>().min(0.66);
                    let scale_w = if rest > 0.0 { rest / shown_cols.iter().map(|c| c.weight()).sum::<f32>().max(0.01) } else { 1.0 };
                    let name_w = w * (1.0 - rest);
                    let mut cols: Vec<(Option<Col>, Option<SortKey>, &str, f32, f32)> = vec![(None, Some(SortKey::Name), "Name", x0, name_w)];
                    let mut x = x0 + name_w;
                    for c in &shown_cols {
                        let cw = w * c.weight() * scale_w;
                        cols.push((Some(*c), c.sort_key(), c.label(), x, cw));
                        x += cw;
                    }
                    let rows_top = vr.y + HEADER;
                    let rows_clip = Rect::new(vr.x, rows_top, vr.w, vr.h - HEADER);
                    let cells = &layout.cells;
                    let first = cells.partition_point(|c| c.bottom() - scroll < rows_top);
                    let last = cells.partition_point(|c| c.y - scroll <= vr.bottom());
                    p.clipped(rows_clip, || {
                        for &(y, k) in &layout.headers {
                            group_heading(p, y, k);
                        }
                        for i in first..last.max(first).min(n) {
                            let e = &entries[i];
                            let r = cells[i].offset(0.0, -scroll);
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
                            let icon_r = Rect::new(r.x + 6.0, r.cy() - 8.0, 16.0, 16.0);
                            let bmp = icon_for(p, e, Some(icon_r));
                            p.faded(dim(sel, e), || draw_icon(p, bmp, icon_r));
                            let name_r = Rect::new(r.x + 28.0, r.y, cols[0].4 - 34.0, r.h);
                            match rename.as_ref().filter(|ren| ren.target == e.path) {
                                Some(ren) => draw_field(p, name_r.inset(0.0, 1.0), ren),
                                None => p.text(e.label(show_ext), &regular, name_r, tc),
                            }
                            for &(col, _, _, cx, cw) in &cols[1..] {
                                let date = |t: Option<std::time::SystemTime>| t.map(fs::format_date).unwrap_or_else(|| "--".into());
                                let (text, align_right) = match col {
                                    Some(Col::Modified) => (date(e.modified), false),
                                    Some(Col::Created) => (date(e.created), false),
                                    Some(Col::Deleted) => (date(e.deleted), false),
                                    Some(Col::Size) if e.is_dir && e.extra.is_none() => ("--".into(), true),
                                    Some(Col::Size) => (fs::format_size(e.size), true),
                                    Some(Col::Kind) => (kinds.kind(e), false),
                                    Some(Col::Location) => (e.folder(), false),
                                    Some(Col::Space) => (e.extra.clone().unwrap_or_default(), false),
                                    None => (String::new(), false),
                                };
                                if align_right {
                                    p.text(&text, &right, Rect::new(cx, r.y, cw - 14.0, r.h), sc);
                                } else {
                                    p.text(&text, &regular, Rect::new(cx + 8.0, r.y, cw - 12.0, r.h), sc);
                                }
                            }
                            hits.extend(intersect(r, rows_clip).map(|r| (r, Hit::Item(i))));
                        }
                    });
                    // Header (drawn last so rows scroll under it).
                    let header = Rect::new(vr.x, vr.y, vr.w, HEADER);
                    p.fill_rect(header, pal.content);
                    for (k, &(_, key, label, x, cw)) in cols.iter().enumerate() {
                        let r = Rect::new(x, vr.y, cw, HEADER);
                        if k > 0 {
                            p.line(r.x - 1.0, r.y + 6.0, r.x - 1.0, r.bottom() - 6.0, pal.divider, 1.0);
                        }
                        let current = key == Some(sort);
                        let font = if current { &small_bold } else { &small };
                        p.text(label, font, Rect::new(r.x + 8.0, r.y, r.w - 24.0, r.h), theme.text_secondary);
                        if current {
                            let (cx, cy) = (r.right() - 14.0, r.cy());
                            let d = if ascending { -1.0 } else { 1.0 };
                            p.line(cx - 3.5, cy - 1.5 * d, cx, cy + 1.5 * d, theme.text_secondary, 1.3);
                            p.line(cx, cy + 1.5 * d, cx + 3.5, cy - 1.5 * d, theme.text_secondary, 1.3);
                        }
                        // Columns without a sort (Free Space) still take the header's menu.
                        hits.push((r, Hit::Header(key.unwrap_or(sort))));
                    }
                    p.line(vr.x, header.bottom(), vr.right(), header.bottom(), pal.divider, 1.0);
                }
                View::Gallery => {
                    // The selected item big, its name and details, and a strip of thumbnails below.
                    let strip = Rect::new(vr.x, vr.bottom() - STRIP_H, vr.w, STRIP_H);
                    let stage = Rect::new(vr.x + 20.0, vr.y + 16.0, vr.w - 40.0, strip.y - vr.y - 60.0);
                    if let Some(e) = &focus_entry {
                        let key = IconLoader::large_key(e);
                        let big = match focus_bitmap.as_ref().filter(|(k, _)| *k == key) {
                            Some((_, b)) => b.clone(),
                            None => {
                                let b = big_px.as_ref().and_then(|px| px.to_bitmap(p.rt));
                                if b.is_some() {
                                    *focus_bitmap = Some((key, b.clone()));
                                }
                                b
                            }
                        };
                        let image = big.or_else(|| icon_for(p, e, None));
                        if let Some(b) = image {
                            let size = unsafe { b.GetSize() };
                            let (bw, bh) = (size.width.max(1.0), size.height.max(1.0));
                            let k = (stage.w / bw).min(stage.h / bh).min(1.0);
                            let (iw, ih) = (bw * k, bh * k);
                            p.bitmap(&b, Rect::new(stage.cx() - iw / 2.0, stage.cy() - ih / 2.0, iw, ih), 1.0);
                        }
                        p.text(e.label(show_ext), &preview_title, Rect::new(stage.x, stage.bottom() + 8.0, stage.w, 22.0), theme.text);
                        let mut info = kinds.kind(e);
                        if !e.is_dir {
                            info += &format!(" — {}", fs::format_size(e.size));
                        }
                        if let Some(m) = e.modified {
                            info += &format!(" — {}", fs::format_date(m));
                        }
                        p.text(&info, &center, Rect::new(stage.x, stage.bottom() + 30.0, stage.w, 18.0), theme.text_secondary);
                    }
                    p.line(strip.x, strip.y, strip.right(), strip.y, pal.divider, 1.0);
                    let cells = &layout.cells;
                    let first = cells.partition_point(|c| c.right() - scroll < vr.x);
                    let last = cells.partition_point(|c| c.x - scroll <= vr.right());
                    for i in first..last.max(first).min(n) {
                        let e = &entries[i];
                        let cell = cells[i].offset(-scroll, 0.0);
                        let thumb = Rect::new(cell.cx() - 28.0, cell.y + 6.0, 56.0, 56.0);
                        if selected.contains(&i) {
                            p.fill_round(cell.inset(4.0, 0.0), 8.0, theme.accent.alpha(0.25));
                            p.stroke_round(cell.inset(4.0, 0.0), 8.0, theme.accent, 2.0);
                        } else if drop_hit == Some(Hit::Item(i)) {
                            p.stroke_round(cell.inset(4.0, 0.0), 8.0, theme.accent, 2.0);
                        }
                        let bmp = icon_for(p, e, Some(thumb));
                        draw_icon(p, bmp, thumb);
                        hits.extend(intersect(cell, vr).map(|r| (r, Hit::Item(i))));
                    }
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
                            let icon_r = Rect::new(r.x + 6.0, r.cy() - 8.0, 16.0, 16.0);
                            let bmp = icon_for(p, e, Some(icon_r));
                            draw_icon(p, bmp, icon_r);
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
                        let bmp = icon_for(p, e, Some(icon));
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
            if max_scroll > 0.0 && !matches!(view, View::Columns | View::Gallery) && scrollbar_a > 0.01 {
                let total = vr.h + max_scroll;
                let th = (vr.h * vr.h / total).max(24.0);
                let ty = vr.y + (vr.h - th) * (scroll / max_scroll).clamp(0.0, 1.0);
                p.fill_round(Rect::new(vr.right() - 8.0, ty + 2.0, 5.0, th - 4.0), 2.5, Color::gray(128, 0.45 * scrollbar_a));
            }

            // Preview / details pane at the right of the view.
            if pane != Pane::None {
                let pr = Rect::new(vr.right(), vr.y, PANE_W, vr.h);
                p.fill_rect(pr, pal.content);
                p.line(pr.x, pr.y, pr.x, pr.bottom(), pal.divider, 1.0);
                let inner = pr.inset(16.0, 14.0);
                let mut y = inner.y;
                p.clipped(pr, || {
                    match &focus_entry {
                        Some(e) if multi <= 1 => {
                            let side = if pane == Pane::Preview { inner.w.min(inner.h * 0.45) } else { 64.0 };
                            let box_r = Rect::new(inner.cx() - side / 2.0, y, side, side);
                            let mut image = None;
                            if pane == Pane::Preview {
                                let key = IconLoader::large_key(e);
                                image = match focus_bitmap.as_ref().filter(|(k, _)| *k == key) {
                                    Some((_, b)) => b.clone(),
                                    None => {
                                        let b = big_px.as_ref().and_then(|px| px.to_bitmap(p.rt));
                                        if b.is_some() {
                                            *focus_bitmap = Some((key, b.clone()));
                                        }
                                        b
                                    }
                                };
                            }
                            let image = image.or_else(|| icon_for(p, e, Some(box_r)));
                            if let Some(b) = image {
                                let size = unsafe { b.GetSize() };
                                let (bw, bh) = (size.width.max(1.0), size.height.max(1.0));
                                let k = (box_r.w / bw).min(box_r.h / bh);
                                let (iw, ih) = (bw * k, bh * k);
                                p.bitmap(&b, Rect::new(box_r.cx() - iw / 2.0, box_r.bottom() - ih, iw, ih), 1.0);
                            }
                            y = box_r.bottom() + 12.0;
                            p.text(e.label(show_ext), &preview_title, Rect::new(inner.x, y, inner.w, 22.0), theme.text);
                            y += 22.0;
                            let mut sub = kinds.kind(e);
                            if !e.is_dir {
                                sub += &format!(" — {}", fs::format_size(e.size));
                            }
                            p.text(&sub, &center, Rect::new(inner.x, y, inner.w, 18.0), theme.text_secondary);
                            y += 30.0;
                            p.text("Information", &small_bold, Rect::new(inner.x, y, inner.w, 16.0), theme.text);
                            y += 20.0;
                            for (label, value) in details {
                                if y > inner.bottom() - 16.0 {
                                    break;
                                }
                                p.text(label, &small, Rect::new(inner.x, y, inner.w * 0.42, 16.0), theme.text_secondary);
                                p.text(value, &small, Rect::new(inner.x + inner.w * 0.45, y, inner.w * 0.55, 16.0), theme.text);
                                y += 18.0;
                            }
                        }
                        _ => {
                            let total: u64 = selected.iter().filter_map(|&i| entries.get(i)).map(|e| e.size).sum();
                            let (head, sub) = if multi > 1 {
                                (format!("{multi} items"), fs::format_size(total))
                            } else {
                                (title.clone(), format!("{n} item{}", if n == 1 { "" } else { "s" }))
                            };
                            let mid = inner.cy() - 30.0;
                            glyphs::folder(p, inner.cx(), mid - 20.0, 2.4, theme.text_secondary);
                            p.text(&head, &preview_title, Rect::new(inner.x, mid + 8.0, inner.w, 22.0), theme.text);
                            p.text(&sub, &center, Rect::new(inner.x, mid + 30.0, inner.w, 18.0), theme.text_secondary);
                        }
                    }
                });
            }

            // Path bar: breadcrumbs from the root, trimmed from the left to fit. A
            // click beside them (or Ctrl+L) turns it into an address field.
            let pb = Rect::new(content_x, body.bottom() - STATUS - PATHBAR, content_w, PATHBAR);
            p.line(pb.x, pb.y, pb.right(), pb.y, pal.divider, 1.0);
            if let Some(field) = address {
                let r = pb.inset(6.0, 2.0);
                p.fill_round(r, 5.0, field_bg);
                p.stroke_round(r, 5.0, theme.accent, 1.5);
                address_xs = edit_text(p, Rect::new(r.x + 8.0, r.y, r.w - 16.0, r.h), field, true);
                hits.push((r, Hit::Address));
            } else {
                hits.push((pb, Hit::PathBar));
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
                let image = large.or_else(|| icon_for(p, &e, None));
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
        self.address_xs = address_xs;
        if let Err(e) = drawn.and_then(|_| self.canvas.present_part(self.hwnd, rc.left, rc.top, window_alpha, part)) {
            log!("finder render failed: {e}");
        }
    }
}

/// The group an entry falls in: an order rank and the heading.
fn group_key(group: Group, e: &FileEntry, kinds: &mut KindCache) -> (u32, String) {
    match group {
        Group::None => (0, String::new()),
        Group::Name => {
            let c = e.name.chars().next().map(|c| c.to_uppercase().next().unwrap_or(c)).unwrap_or('#');
            let label = if c.is_alphabetic() {
                c.to_string()
            } else if c.is_ascii_digit() {
                "0–9".to_string()
            } else {
                "#".to_string()
            };
            (0, label)
        }
        Group::Kind => (!e.is_dir as u32, kinds.kind(e)),
        Group::Size if e.is_dir => (0, "Folders".into()),
        Group::Size => {
            let s = search::Size::of(e.size);
            let i = search::Size::ALL.iter().position(|x| x.0 == s).unwrap_or(0);
            (1 + i as u32, search::Size::ALL[i].2.to_string())
        }
        Group::Modified => {
            let Some(t) = e.modified else { return (u32::MAX, "Unknown".into()) };
            let dt: chrono::DateTime<chrono::Local> = t.into();
            let now = chrono::Local::now();
            let days = (now.date_naive() - dt.date_naive()).num_days();
            use chrono::Datelike;
            match days {
                i64::MIN..=0 => (0, "Today".into()),
                1 => (1, "Yesterday".into()),
                2..=6 => (2, "Previous 7 Days".into()),
                7..=29 => (3, "Previous 30 Days".into()),
                _ if dt.year() == now.year() => (100 + 12 - dt.month(), dt.format("%B").to_string()),
                _ => (1000 + (3000 - dt.year().clamp(0, 3000)) as u32, dt.year().to_string()),
            }
        }
    }
}

/// The sidebar's place rows inside `side`: Favorites, then Locations, each under a header.
fn place_rows(side: Rect, n: usize, fav_count: usize, scroll: f32) -> Vec<Rect> {
    let mut rows = Vec::with_capacity(n);
    let mut y = side.y + 46.0 - scroll;
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

/// The index of the caret position in `xs` nearest to `x`.
fn nearest(xs: &[f32], x: f32) -> usize {
    xs.iter().enumerate().min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs())).map(|(i, _)| i).unwrap_or(0)
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

#[cfg(test)]
mod bench {
    use super::*;

    /// Drives a Finder window through places, views, menus, search and file
    /// operations, saving a PNG of each step to `TOUR_DIR`.
    /// `cargo test --profile fast finder_tour -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn finder_tour() {
        use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        dnd::ensure_ole();
        let _ = crate::win::register_class(Some(proc_));
        let out = PathBuf::from(std::env::var("TOUR_DIR").unwrap_or_else(|_| std::env::temp_dir().display().to_string()));
        let gfx = Gfx::new().unwrap();
        let cfg = crate::config::defaults();
        let theme = Theme::new(&cfg, &gfx);
        let mut loader = IconLoader::new(0, 0);
        let mut kinds = KindCache::default();
        let prefs = FinderCfg { view: View::List, show_hidden: false, ..cfg.finder.clone() };
        let mut ctx = Ctx { gfx: &gfx, theme: &theme, loader: &mut loader, kinds: &mut kinds };
        let mut f = FinderWindow::new(&mut ctx, &Monitor::primary(), 1, fs::home(), 0, 0, 0, &prefs).unwrap();
        // Settles background work and icons, then saves a picture.
        let snap = |f: &mut FinderWindow, ctx: &mut Ctx, name: &str| {
            for _ in 0..150 {
                f.poll_async(ctx.kinds);
                let busy = f.pending.is_some() || f.searching.as_ref().is_some_and(|s| !s.finished());
                ctx.loader.drain();
                f.icons_ready = true;
                f.tick(0.05);
                f.dirty = true;
                f.render(ctx);
                if !busy {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            for _ in 0..12 {
                std::thread::sleep(Duration::from_millis(60));
                ctx.loader.drain();
                f.tick(0.05);
                f.dirty = true;
                f.render(ctx);
            }
            let px = f.canvas.pixels();
            crate::gfx::write_png(ctx.gfx, f.canvas.width as u32, f.canvas.height as u32, &px, &out.join(format!("{name}.png")).to_string_lossy());
            println!("{name}: {} entries, error {:?}, cwd {}", f.entries.len(), f.error, f.cwd.display());
        };
        let print_menu = |name: &str, ev: FinderEvent| {
            fn walk(v: &[Entry], depth: usize) {
                for e in v {
                    match e {
                        Entry::Separator => println!("{}--", "  ".repeat(depth)),
                        Entry::Item(i) => {
                            let tag = if i.action.is_none() && i.submenu.is_empty() { " (disabled)" } else { "" };
                            println!("{}{}{tag}", "  ".repeat(depth), i.label);
                            if depth < 2 {
                                walk(&i.submenu, depth + 1);
                            }
                        }
                    }
                }
            }
            println!("== {name}");
            if let FinderEvent::Menu(v, ..) = ev {
                walk(&v, 1);
            }
        };

        snap(&mut f, &mut ctx, "01-home-list");
        f.navigate(PathBuf::from(shell::THIS_PC), true, ctx.kinds);
        snap(&mut f, &mut ctx, "02-this-pc");
        f.navigate(PathBuf::from(shell::RECYCLE_BIN), true, ctx.kinds);
        snap(&mut f, &mut ctx, "03-recycle-bin");
        f.navigate(PathBuf::from(shell::LIBRARIES), true, ctx.kinds);
        snap(&mut f, &mut ctx, "04-libraries");
        f.navigate(fs::home(), true, ctx.kinds);
        f.command(FinderCmd::SetView(View::Icons), ctx.kinds);
        f.command(FinderCmd::GroupBy(Group::Kind), ctx.kinds);
        snap(&mut f, &mut ctx, "05-icons-grouped");
        f.command(FinderCmd::GroupBy(Group::None), ctx.kinds);
        if let Some(pics) = fs::known_folder(&windows::Win32::UI::Shell::FOLDERID_Pictures) {
            f.navigate(pics, true, ctx.kinds);
        }
        f.command(FinderCmd::SetView(View::Gallery), ctx.kinds);
        snap(&mut f, &mut ctx, "06-gallery");
        f.navigate(fs::home().join("Documents"), true, ctx.kinds);
        f.command(FinderCmd::SetView(View::List), ctx.kinds);
        f.command(FinderCmd::SetPane(Pane::Preview), ctx.kinds);
        if let Some(i) = f.entries.iter().position(|e| !e.is_dir) {
            f.selected = [i].into();
            f.cursor = Some(i);
        }
        snap(&mut f, &mut ctx, "07-preview-pane");
        f.command(FinderCmd::SetPane(Pane::Preview), ctx.kinds);

        // Context menus: on the selected file, and on the background.
        if let Some(i) = f.entries.iter().position(|e| !e.is_dir) {
            let r = f.layout.cells[i];
            print_menu("item menu", f.on_right_click(r.x + 40.0, r.cy() - f.scroll.offset(), false, ctx.kinds));
        }
        f.dirty = true;
        f.render(&mut ctx);
        let vr = f.view_rect();
        print_menu("background menu", f.on_right_click(vr.right() - 20.0, vr.bottom() - 6.0, false, ctx.kinds));
        println!("new types: {:?}", shell::new_types().iter().map(|t| t.name.clone()).collect::<Vec<_>>());

        // Address field.
        f.on_key(b'L' as u16, true, false, false, ctx.kinds);
        snap(&mut f, &mut ctx, "08-address");
        f.address = Some(TextField::new("%USERPROFILE%\\Desktop"));
        f.on_key(ENTER, false, false, false, ctx.kinds);
        println!("address went to {}", f.cwd.display());

        // Search under the repo with a filter.
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let repo = PathBuf::from(repo.canonicalize().unwrap().display().to_string().trim_start_matches(r"\\?\"));
        f.navigate(repo, true, ctx.kinds);
        f.search = TextField::new("finder ext:rs");
        f.search_changed(ctx.kinds);
        snap(&mut f, &mut ctx, "09-search");
        f.search.clear();
        f.search_changed(ctx.kinds);

        // File operations and undo in a scratch folder.
        let scratch = std::env::temp_dir().join("dockside-finder-tour");
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("a.txt"), "hello").unwrap();
        let a = scratch.join("a.txt");
        let names = |d: &Path| {
            let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        };
        let dup = ops::duplicate(HWND::default(), &[a.clone()]);
        println!("duplicate -> {dup:?}: {:?}", names(&scratch));
        let zip = ops::compress(HWND::default(), &[a.clone()]);
        // The zip folder compresses in the background.
        std::thread::sleep(Duration::from_millis(1500));
        println!("compress -> {zip:?}: {:?} ({} bytes)", names(&scratch), zip.as_ref().and_then(|z| std::fs::metadata(z).ok()).map_or(0, |m| m.len()));
        if let Some(z) = &zip {
            std::fs::rename(z, scratch.join("packed.zip")).unwrap();
            let x = ops::extract(HWND::default(), &scratch.join("packed.zip"));
            println!("extract -> {x:?}: {:?} / {:?}", names(&scratch), x.as_ref().map(|d| names(d)));
        }
        let sub = scratch.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let moved = ops::transfer(HWND::default(), &[a.clone()], &sub, DropOp::Move);
        println!("move -> {moved:?}: {:?} / {:?}", names(&scratch), names(&sub));
        let back = ops::undo(HWND::default());
        println!("undo move -> {back:?}: {:?} / {:?}", names(&scratch), names(&sub));
        let again = ops::redo(HWND::default());
        println!("redo move -> {again:?}: {:?} / {:?}", names(&scratch), names(&sub));
        ops::undo(HWND::default());
        ops::delete(HWND::default(), &[a.clone()], true);
        println!("trash: {:?} (label {:?})", names(&scratch), ops::undo_label());
        let restored = ops::undo(HWND::default());
        println!("undo trash -> {restored:?}: {:?}", names(&scratch));
        let links = ops::shortcuts(&[a.clone()], &scratch);
        println!("shortcut -> {links:?}");
        shell::set_clipboard(&[a.clone()], false);
        let pasted = ops::paste(HWND::default(), &sub);
        println!("paste -> {pasted:?}: {:?}", names(&sub));
        f.navigate(scratch.clone(), true, ctx.kinds);
        f.command(FinderCmd::NewFile(0), ctx.kinds);
        println!("new file ({:?}): renaming {:?}", shell::new_types().first().map(|t| t.name.clone()), f.rename.as_ref().map(|r| r.target.clone()));
        f.rename = None;
        snap(&mut f, &mut ctx, "10-scratch");
        let _ = std::fs::remove_dir_all(&scratch);
    }

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
            let mut f = FinderWindow::new(&mut ctx, &Monitor::primary(), 1, PathBuf::from(std::env::var("BENCH_DIR").unwrap_or(r"C:\Windows\System32".into())), 0, 0, 0, &prefs).unwrap();
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
            let mut input = 0.0;
            for k in 0..n {
                let t0 = Instant::now();
                f.on_wheel(500.0, if k % 50 < 25 { -120 } else { 120 }, false);
                f.tick(1.0 / 165.0);
                input += t0.elapsed().as_secs_f64() * 1000.0;
                let t0 = Instant::now();
                f.render(&mut ctx);
                work += t0.elapsed().as_secs_f64() * 1000.0;
                unsafe {
                    let _ = windows::Win32::Graphics::Dwm::DwmFlush();
                }
            }
            println!("{view:?} ({} items): {:.0} fps on screen, render {:.2} ms, wheel+tick {:.2} ms", f.entries.len(), n as f64 / t.elapsed().as_secs_f64(), work / n as f64, input / n as f64);

            // A steady spin from top to bottom, a notch every few frames, with
            // icons for newly revealed rows streaming in as it goes.
            f.scroll.jump(0.0);
            let (mut frames, mut worst, mut slow) = (0, 0.0f64, 0);
            let t = Instant::now();
            while f.scroll.offset() < f.max_scroll && frames < 3000 {
                let t0 = Instant::now();
                if frames % 4 == 0 {
                    f.on_wheel(500.0, -120, false);
                }
                if ctx.loader.drain() {
                    f.on_icons();
                }
                f.tick(1.0 / 165.0);
                if f.dirty {
                    f.render(&mut ctx);
                }
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                worst = worst.max(ms);
                slow += (ms > 1000.0 / 165.0) as u32;
                frames += 1;
                unsafe {
                    let _ = windows::Win32::Graphics::Dwm::DwmFlush();
                }
            }
            println!("  spin: {frames} frames, {:.0} fps on screen, worst frame {worst:.2} ms, {slow} over budget", frames as f64 / t.elapsed().as_secs_f64());

            // Partial frames must leave exactly what a full repaint would.
            f.on_wheel(500.0, -360, false);
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
