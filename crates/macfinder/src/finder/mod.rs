//! Finder: a macOS-style file manager window with a sidebar, back/forward,
//! and icon / list / column views.

pub mod fs;
pub mod loader;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use windows::core::{Result, HSTRING};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::*;

use self::fs::{Entry as FileEntry, KindCache, Place, SortKey};
use self::loader::IconLoader;
use crate::chrome::{self, Light, SHADOW};
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Canvas, Gfx, Painter, Weight};
use crate::menu::{Action, Entry};
use crate::theme::Theme;
use crate::win::{self, Monitor};

/// Dock path that opens a Finder window instead of launching a program.
pub const FINDER_PATH: &str = "macfinder:finder";

const W: f32 = 940.0;
const H: f32 = 580.0;
const MIN_W: f32 = 560.0;
const MIN_H: f32 = 340.0;
const SIDEBAR: f32 = 190.0;
const TOOLBAR: f32 = 52.0;
const STATUS: f32 = 26.0;
const ROW: f32 = 24.0;
const HEADER: f32 = 26.0;
const CELL_W: f32 = 104.0;
const CELL_H: f32 = 108.0;
const ICON: f32 = 64.0;
const COL_W: f32 = 240.0;
const PREVIEW_W: f32 = 300.0;
/// Width of the invisible resize border inside the body edge.
const EDGE: f32 = 6.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Icons,
    List,
    Columns,
}

#[derive(Clone, Debug)]
pub enum FinderCmd {
    Open(PathBuf),
    Reveal(PathBuf),
    CopyPath(Vec<PathBuf>),
    Trash(Vec<PathBuf>),
    NewFolder,
    Rename,
    SetView(View),
    Sort(SortKey),
    Terminal(PathBuf),
}

pub enum FinderEvent {
    None,
    Close,
    NewWindow,
    /// Context menu entries and the screen point (points) to show them at.
    Menu(Vec<Entry>, f32, f32),
}

#[derive(Clone, Copy, PartialEq, Debug)]
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
}

struct Column {
    dir: PathBuf,
    entries: Vec<FileEntry>,
    selected: Option<usize>,
    scroll: f32,
    error: Option<String>,
}

/// Inline rename field. Text is kept as chars so the caret can't split a UTF-8 sequence.
struct Rename {
    target: PathBuf,
    text: Vec<char>,
    caret: usize,
    anchor: usize,
}

impl Rename {
    fn range(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }
    fn insert(&mut self, s: &str) {
        let (a, b) = self.range();
        self.text.splice(a..b, s.chars());
        self.caret = a + s.chars().count();
        self.anchor = self.caret;
    }
    fn string(&self) -> String {
        self.text.iter().collect()
    }
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
    scroll: f32,
    max_scroll: f32,
    grid_cols: usize,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    last_click: Option<(Instant, Hit)>,
    rename: Option<Rename>,
    typed: String,
    typed_at: Instant,
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    favorites: Vec<Place>,
    drives: Vec<Place>,
    watcher: Option<notify::RecommendedWatcher>,
    zoom_restore: Option<RECT>,
    show_hidden: bool,
    pub focused: bool,
    pub tracking: bool,
    pub dirty: bool,
}

impl FinderWindow {
    /// `notify`/`changed_msg`: where to post (with this window's id) when the folder changes on disk.
    pub fn new(
        ctx: &mut Ctx,
        monitor: &Monitor,
        id: u32,
        path: PathBuf,
        cascade: i32,
        notify: isize,
        changed_msg: u32,
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
            entries: Vec::new(),
            error: None,
            free: None,
            view: View::Icons,
            sort: SortKey::Name,
            ascending: true,
            selected: BTreeSet::new(),
            anchor: None,
            cursor: None,
            columns: Vec::new(),
            active_col: 0,
            scroll: 0.0,
            max_scroll: 0.0,
            grid_cols: 1,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            last_click: None,
            rename: None,
            typed: String::new(),
            typed_at: Instant::now(),
            bitmaps: HashMap::new(),
            favorites: fs::favorites(),
            drives: fs::drives(),
            watcher,
            zoom_restore: None,
            show_hidden: false,
            focused: true,
            tracking: false,
            dirty: true,
        };
        f.navigate(path, true, ctx.kinds);
        Ok(f)
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn title(&self) -> String {
        if let Some(p) = self.favorites.iter().chain(&self.drives).find(|p| p.path == self.cwd) {
            return p.name.clone();
        }
        self.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.cwd.display().to_string())
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
        self.entries = entries;
        self.error = error.clone();
        self.selected.clear();
        self.anchor = None;
        self.cursor = None;
        self.scroll = 0.0;
        self.rename = None;
        self.columns = vec![Column {
            dir: self.cwd.clone(),
            entries: self.entries.clone(),
            selected: None,
            scroll: 0.0,
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
        let keep: Vec<PathBuf> = self.selected.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.path.clone()).collect();
        let (entries, error) = self.list_sorted(&self.cwd.clone(), kinds);
        self.entries = entries;
        self.error = error;
        self.reselect(&keep);
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
        self.free = fs::free_space(&self.cwd);
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
            self.scroll = 0.0;
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
            self.columns.push(Column { dir: e.path, entries, selected: None, scroll: 0.0, error });
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
            FinderCmd::Reveal(p) => crate::win::system::reveal(&p.to_string_lossy()),
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
        }
        self.dirty = true;
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
        let text: Vec<char> = entry.name.chars().collect();
        // Like macOS, preselect the name without its extension.
        let stem = if entry.is_dir {
            text.len()
        } else {
            entry.name.rfind('.').filter(|&i| i > 0).map(|i| entry.name[..i].chars().count()).unwrap_or(text.len())
        };
        self.rename = Some(Rename { target: entry.path, text, caret: stem, anchor: 0 });
        self.ensure_visible();
        self.dirty = true;
    }

    fn commit_rename(&mut self, kinds: &mut KindCache) {
        let Some(r) = self.rename.take() else { return };
        let name = r.string();
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
        Rect::new(b.x + SIDEBAR, b.y + TOOLBAR, b.w - SIDEBAR, b.h - TOOLBAR - STATUS)
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

    pub fn on_move(&mut self, x: f32, y: f32) {
        let h = self.hit_at(x, y);
        let interesting = |h: Option<Hit>| {
            matches!(h, Some(Hit::Light(_) | Hit::Back | Hit::Forward | Hit::ViewBtn(_) | Hit::Place(_) | Hit::Header(_)))
        };
        if h != self.hover && (interesting(h) || interesting(self.hover)) {
            self.dirty = true;
        }
        self.hover = h;
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if self.hover.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn on_wheel(&mut self, x: f32, delta: i32) {
        let amount = delta as f32 / 120.0 * 60.0;
        if self.view == View::Columns {
            let vr = self.view_rect();
            let offset = self.column_offset(vr);
            let c = ((x - vr.x + offset) / COL_W).floor().max(0.0) as usize;
            if let Some(col) = self.columns.get_mut(c) {
                let max = (col.entries.len() as f32 * ROW + 8.0 - vr.h).max(0.0);
                col.scroll = (col.scroll - amount).clamp(0.0, max);
            }
        } else {
            self.scroll = (self.scroll - amount).clamp(0.0, self.max_scroll);
        }
        self.dirty = true;
    }

    pub fn on_down(&mut self, x: f32, y: f32, ctrl: bool, shift: bool, kinds: &mut KindCache) {
        let hit = self.hit_at(x, y);
        self.pressed = hit;
        let now = Instant::now();
        let dbl = Duration::from_millis(unsafe { GetDoubleClickTime() } as u64);
        let double = matches!((self.last_click, hit), (Some((t, h)), Some(hh)) if h == hh && now - t < dbl);
        self.last_click = if double { None } else { hit.map(|h| (now, h)) };

        if self.rename.is_some() {
            self.commit_rename(kinds);
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
                } else {
                    self.selected = [i].into();
                    self.anchor = Some(i);
                }
                self.cursor = Some(i);
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
            }
            Some(Hit::ColumnBg(c)) => {
                self.columns.truncate(c + 1);
                self.columns[c].selected = None;
                self.active_col = c;
            }
            Some(Hit::Content) => self.selected.clear(),
            Some(Hit::Header(k)) => self.set_sort(k, kinds),
            Some(Hit::Place(i)) => {
                if let Some(p) = self.favorites.iter().chain(&self.drives).nth(i).cloned() {
                    self.navigate(p.path, true, kinds);
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32, kinds: &mut KindCache) -> FinderEvent {
        let pressed = self.pressed.take();
        let Some(hit) = self.hit_at(x, y).filter(|h| Some(*h) == pressed) else { return FinderEvent::None };
        match hit {
            Hit::Light(Light::Close) => return FinderEvent::Close,
            Hit::Light(Light::Minimize) => unsafe {
                let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
            },
            Hit::Light(Light::Zoom) => self.toggle_zoom(),
            Hit::Back => self.go_back(kinds),
            Hit::Forward => self.go_forward(kinds),
            Hit::ViewBtn(v) => self.set_view(v),
            _ => {}
        }
        self.dirty = true;
        FinderEvent::None
    }

    pub fn on_right_click(&mut self, x: f32, y: f32, kinds: &mut KindCache) -> FinderEvent {
        if self.rename.is_some() {
            self.commit_rename(kinds);
        }
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
                Entry::item("Open in File Explorer", cmd(FinderCmd::Open(dir.clone()))),
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
        const F2: u16 = 0x71;
        const F5: u16 = 0x74;
        const LBRACKET: u16 = 0xDB;
        const RBRACKET: u16 = 0xDD;
        const PERIOD: u16 = 0xBE;

        self.dirty = true;
        if let Some(r) = &mut self.rename {
            let len = r.text.len();
            match vk {
                ENTER => self.commit_rename(kinds),
                ESC => self.rename = None,
                BACK | DEL => {
                    let (a, b) = r.range();
                    if a != b {
                        r.insert("");
                    } else if vk == BACK && a > 0 {
                        r.text.remove(a - 1);
                        r.caret = a - 1;
                        r.anchor = r.caret;
                    } else if vk == DEL && a < len {
                        r.text.remove(a);
                    }
                }
                LEFT | RIGHT | HOME | END => {
                    let (a, b) = r.range();
                    r.caret = match vk {
                        LEFT if a != b && !shift => a,
                        RIGHT if a != b && !shift => b,
                        LEFT => r.caret.saturating_sub(1),
                        RIGHT => (r.caret + 1).min(len),
                        HOME => 0,
                        _ => len,
                    };
                    if !shift {
                        r.anchor = r.caret;
                    }
                }
                _ if ctrl && vk == b'A' as u16 => {
                    r.anchor = 0;
                    r.caret = len;
                }
                _ => {}
            }
            return FinderEvent::None;
        }

        match (vk, ctrl, alt) {
            (k, true, false) if k == b'N' as u16 && shift => self.new_folder(kinds),
            (k, true, false) if k == b'N' as u16 => return FinderEvent::NewWindow,
            (k, true, false) if k == b'W' as u16 => return FinderEvent::Close,
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
            r.insert(&ch.to_string());
            self.dirty = true;
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
                if top < self.scroll {
                    self.scroll = top;
                } else if top + CELL_H + 24.0 > self.scroll + vr.h {
                    self.scroll = top + CELL_H + 24.0 - vr.h;
                }
            }
            View::List => {
                let Some(&i) = self.cursor.as_ref().or(self.selected.iter().next()) else { return };
                let top = i as f32 * ROW;
                let visible = vr.h - HEADER;
                self.scroll = self.scroll.min(top).max(top + ROW - visible + 4.0).max(0.0);
            }
            View::Columns => {
                let c = self.active_col.min(self.columns.len() - 1);
                let col = &mut self.columns[c];
                if let Some(j) = col.selected {
                    let top = j as f32 * ROW;
                    col.scroll = col.scroll.min(top).max(top + ROW - vr.h + 8.0).max(0.0);
                }
            }
        }
    }

    /// Horizontal offset that keeps the rightmost column (or preview) in view.
    fn column_offset(&self, vr: Rect) -> f32 {
        let total = self.columns.len() as f32 * COL_W + if self.preview_entry().is_some() { PREVIEW_W } else { 0.0 };
        (total - vr.w).max(0.0)
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
        if let Err(e) = self.canvas.resize(rc.right - rc.left, rc.bottom - rc.top) {
            return log!("finder resize failed: {e}");
        }
        if unsafe { IsIconic(self.hwnd).as_bool() } {
            return;
        }

        let (gfx, theme) = (ctx.gfx, ctx.theme);
        let pal = chrome::Palette::new(theme);
        let dark = theme.dark;
        let body = self.body();
        let vr = self.view_rect();
        let content_x = body.x + SIDEBAR;
        let content_w = body.w - SIDEBAR;
        let sel_bg = if self.focused { theme.accent } else if dark { Color::gray(255, 0.18) } else { Color::gray(0, 0.12) };
        let sel_text = if self.focused { Color::WHITE } else { theme.text };
        let soft = if dark { Color::gray(255, 0.08) } else { Color::gray(0, 0.06) };
        let stripe = if dark { Color::gray(255, 0.03) } else { Color::gray(0, 0.025) };
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

        // Grid / scroll metrics for the current view.
        let n = self.entries.len();
        let pad = 12.0;
        self.grid_cols = (((vr.w - 2.0 * pad) / CELL_W).floor() as usize).max(1);
        let content_h = match self.view {
            View::Icons => n.div_ceil(self.grid_cols) as f32 * CELL_H + 2.0 * pad,
            View::List => n as f32 * ROW + 8.0 + HEADER,
            View::Columns => 0.0,
        };
        self.max_scroll = (content_h - vr.h).max(0.0);
        self.scroll = self.scroll.min(self.max_scroll);

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
        let preview = self.preview_entry().cloned();
        let column_offset = self.column_offset(vr);

        let kinds = &mut *ctx.kinds;
        let loader = &mut *ctx.loader;
        let bitmaps = &mut self.bitmaps;
        let (entries, columns, selected) = (&self.entries, &self.columns, &self.selected);
        let (view, hover, error, rename) = (self.view, self.hover, &self.error, &self.rename);
        let (scroll, max_scroll, grid_cols, active_col) = (self.scroll, self.max_scroll, self.grid_cols, self.active_col);
        let (sort, ascending) = (self.sort, self.ascending);
        let (can_back, can_fwd) = (self.hist_pos > 0, self.hist_pos + 1 < self.history.len());
        let places: Vec<&Place> = self.favorites.iter().chain(&self.drives).collect();
        let fav_count = self.favorites.len();
        let cwd = &self.cwd;

        let drawn = self.canvas.draw(gfx, |p| {
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
            let draw_field = |p: &Painter, r: Rect, ren: &Rename| {
                p.fill_round(r, 3.0, field_bg);
                p.stroke_round(r, 3.0, theme.accent, 1.5);
                let text = ren.string();
                let prefix = |i: usize| -> f32 { p.gfx.measure(&ren.text[..i].iter().collect::<String>(), &regular) };
                let (a, b) = ren.range();
                let x0 = r.x + 4.0;
                if a != b {
                    p.fill_rect(Rect::new(x0 + prefix(a), r.y + 3.0, prefix(b) - prefix(a), r.h - 6.0), theme.accent.alpha(0.35));
                } else {
                    let cx = x0 + prefix(a);
                    p.line(cx, r.y + 4.0, cx, r.bottom() - 4.0, theme.text, 1.0);
                }
                p.text(&text, &regular, Rect::new(x0, r.y, r.w - 8.0, r.h), theme.text);
            };

            chrome::window(p, body, SIDEBAR, theme, &pal);

            // Sidebar.
            let mut y = body.y + 52.0;
            for (i, place) in places.iter().enumerate() {
                if i == 0 || i == fav_count {
                    let header = if i == 0 { "Favorites" } else { "Locations" };
                    p.text(header, &small_bold, Rect::new(body.x + 18.0, y, SIDEBAR - 30.0, 22.0), theme.text_secondary);
                    y += 22.0;
                }
                let row = Rect::new(body.x + 10.0, y, SIDEBAR - 20.0, 26.0);
                if place.path == *cwd {
                    p.fill_round(row, 6.0, if dark { Color::gray(255, 0.12) } else { Color::gray(0, 0.09) });
                } else if hover == Some(Hit::Place(i)) {
                    p.fill_round(row, 6.0, soft);
                }
                let probe = FileEntry {
                    name: place.name.clone(),
                    path: place.path.clone(),
                    is_dir: true,
                    size: 0,
                    modified: None,
                    ext: String::new(),
                };
                let bmp = icon_for(p, &probe);
                draw_icon(p, bmp, Rect::new(row.x + 8.0, row.cy() - 8.0, 16.0, 16.0));
                p.text(&place.name, &regular, Rect::new(row.x + 32.0, row.y, row.w - 36.0, row.h), theme.text);
                hits.push((row, Hit::Place(i)));
                y += 26.0;
                if i + 1 == fav_count {
                    y += 8.0;
                }
            }

            // Toolbar.
            let ty = body.y + (TOOLBAR - 28.0) / 2.0;
            for (k, (hit, enabled)) in [(Hit::Back, can_back), (Hit::Forward, can_fwd)].into_iter().enumerate() {
                let b = Rect::new(content_x + 12.0 + k as f32 * 30.0, ty, 28.0, 28.0);
                if enabled && hover == Some(hit) {
                    p.fill_round(b, 6.0, soft);
                }
                let c = if enabled { theme.text_secondary } else { theme.text_disabled };
                let (cx, cy) = (b.cx(), b.cy());
                let d = if hit == Hit::Back { 1.0 } else { -1.0 };
                p.line(cx + 2.5 * d, cy - 6.0, cx - 3.5 * d, cy, c, 1.8);
                p.line(cx - 3.5 * d, cy, cx + 2.5 * d, cy + 6.0, c, 1.8);
                if enabled {
                    hits.push((b, hit));
                }
            }
            p.text(&title, &title_font, Rect::new(content_x + 82.0, body.y, content_w - 220.0, TOOLBAR), theme.text);

            // View switcher.
            let seg_w = 32.0;
            let outer = Rect::new(content_x + content_w - 16.0 - seg_w * 3.0 - 4.0, body.y + (TOOLBAR - 26.0) / 2.0, seg_w * 3.0 + 4.0, 26.0);
            p.fill_round(outer, 7.0, soft);
            for (k, v) in [View::Icons, View::List, View::Columns].into_iter().enumerate() {
                let seg = Rect::new(outer.x + 2.0 + k as f32 * seg_w, outer.y + 2.0, seg_w, 22.0);
                if v == view {
                    p.fill_round(seg, 5.0, if dark { Color::gray(255, 0.2) } else { Color::WHITE });
                }
                let c = theme.text_secondary;
                let (cx, cy) = (seg.cx(), seg.cy());
                match v {
                    View::Icons => {
                        for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                            p.fill_round(Rect::new(cx + dx * 3.2 - 2.5, cy + dy * 3.2 - 2.5, 5.0, 5.0), 1.0, c);
                        }
                    }
                    View::List => {
                        for dy in [-4.0, 0.0, 4.0] {
                            p.fill_circle(cx - 5.0, cy + dy, 1.0, c);
                            p.line(cx - 2.5, cy + dy, cx + 6.0, cy + dy, c, 1.4);
                        }
                    }
                    View::Columns => {
                        let r = Rect::new(cx - 7.0, cy - 5.5, 14.0, 11.0);
                        p.stroke_round(r, 2.0, c, 1.2);
                        p.line(cx - 2.3, r.y, cx - 2.3, r.bottom(), c, 1.2);
                        p.line(cx + 2.3, r.y, cx + 2.3, r.bottom(), c, 1.2);
                    }
                }
                hits.push((seg, Hit::ViewBtn(v)));
            }
            p.line(content_x, body.y + TOOLBAR, body.right(), body.y + TOOLBAR, pal.divider, 1.0);

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
                        if sel {
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
                        hits.push((Rect::new(cell.x + 4.0, cell.y, cell.w - 8.0, CELL_H - 4.0), Hit::Item(i)));
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
                    p.clipped(Rect::new(vr.x, rows_top, vr.w, vr.h - HEADER), || {
                        for i in first..last {
                            let e = &entries[i];
                            let r = Rect::new(x0, rows_top + 4.0 + i as f32 * ROW - scroll, w, ROW);
                            let sel = selected.contains(&i);
                            if sel {
                                p.fill_round(r, 5.0, sel_bg);
                            } else if i % 2 == 1 {
                                p.fill_round(r, 5.0, stripe);
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
                            hits.push((r, Hit::Item(i)));
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
                        hits.push((col_r, Hit::ColumnBg(c)));
                        if let Some(err) = &col.error {
                            p.text(err, &small_center, col_r.inset(10.0, 0.0), theme.text_secondary);
                        }
                        let first = (col.scroll / ROW).floor().max(0.0) as usize;
                        let last = (((col.scroll + vr.h) / ROW).ceil() as usize + 1).min(col.entries.len());
                        for j in first..last {
                            let e = &col.entries[j];
                            let r = Rect::new(cx0 + 6.0, vr.y + 4.0 + j as f32 * ROW - col.scroll, COL_W - 12.0, ROW);
                            let sel = col.selected == Some(j);
                            let active = c == active_col;
                            if sel {
                                p.fill_round(r, 5.0, if active { sel_bg } else { soft });
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
                            hits.push((r, Hit::Column(c, j)));
                        }
                        p.line(col_r.right(), vr.y, col_r.right(), vr.bottom(), pal.divider, 1.0);
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

            // Overlay scrollbar.
            if max_scroll > 0.0 && view != View::Columns {
                let total = vr.h + max_scroll;
                let th = (vr.h * vr.h / total).max(24.0);
                let ty = vr.y + (vr.h - th) * (scroll / max_scroll);
                p.fill_round(Rect::new(vr.right() - 8.0, ty + 2.0, 5.0, th - 4.0), 2.5, Color::gray(128, 0.45));
            }

            // Status bar.
            let sb = Rect::new(content_x, body.bottom() - STATUS, content_w, STATUS);
            p.line(sb.x, sb.y, sb.right(), sb.y, pal.divider, 1.0);
            p.text(&status, &small_center, sb, theme.text_secondary);

            // Traffic lights last so they win hit-testing.
            let group_hover = matches!(hover, Some(Hit::Light(_)));
            for (r, light) in chrome::traffic_lights(p, body, group_hover, true) {
                hits.push((r, Hit::Light(light)));
            }
        });
        self.hits = hits;
        if let Err(e) = drawn.and_then(|_| self.canvas.present(self.hwnd, rc.left, rc.top)) {
            log!("finder render failed: {e}");
        }
    }
}

impl Drop for FinderWindow {
    fn drop(&mut self) {
        self.watcher = None;
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
