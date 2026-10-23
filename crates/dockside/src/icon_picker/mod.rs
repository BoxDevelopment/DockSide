//! Change Icon…: a floating panel for picking a Dock app's icon from an online
//! library (see `library`), an image file, or going back to the app's own.
//!
//! "All Icons" searches Dashboard Icons as you type. "Mac Style" searches
//! macOSicons.com with the user's API key, only when they press Return,
//! since every search spends one of their monthly requests.

pub mod library;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicIsize;

use windows::core::Result;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

use self::library::{Done, Entry, Hit, Library, Source};
use crate::anim::{feel, Glide, Motion, Scroll};
use crate::chrome::SHADOW;
use crate::config::Config;
use crate::finder::text::{KeyResult, TextField};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconPixels;
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::surface::{Fading, Surface};
use crate::theme::Theme;
use crate::win::Monitor;

/// The panel's content window, so the window procedure can close it when it loses focus.
pub static PICKER_HWND: AtomicIsize = AtomicIsize::new(0);

const W: f32 = 640.0;
const SEARCH_H: f32 = 54.0;
const TABS_H: f32 = 42.0;
const COLS: usize = 6;
const ROWS: usize = 4;
const CELL_H: f32 = 104.0;
const GRID_PAD: f32 = 16.0;
const FOOTER_H: f32 = 56.0;
const H: f32 = SEARCH_H + TABS_H + ROWS as f32 * CELL_H + FOOTER_H;
const RADIUS: f32 = 14.0;
const ICON: f32 = 60.0;

/// Which app's icon is being changed.
#[derive(Clone, Debug, PartialEq)]
pub struct IconTarget {
    /// A pinned app's path (its `[[dock.apps]]` entry gets the icon).
    pub pinned: Option<String>,
    /// The app's exe file name (`[app_icons]`, for apps that aren't pinned).
    pub exe: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Tab {
    All,
    Mac,
}

enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Button {
    ChooseFile,
    Default,
    Cancel,
    Set,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Target {
    Cell(usize),
    Tab(Tab),
    Button(Button),
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Anim {
    Hover(Target),
    TabPill,
}

pub enum PickerEvent {
    None,
    Close,
    Run(Action),
}

pub struct IconPicker {
    pub surface: Surface,
    target: IconTarget,
    app: String,
    field: TextField,
    tab: Tab,
    /// macOSicons.com API key ("" when not set).
    key: String,
    library: Library,
    index: Load<Vec<Entry>>,
    all: Vec<Hit>,
    mac: Option<Load<Vec<Hit>>>,
    /// The query the Mac Style results are for, and which search is current.
    mac_query: String,
    mac_generation: u32,
    /// Smooth (eased) scrolling of the grid, in points.
    scroll: Scroll,
    selected: Option<usize>,
    hover: Option<Target>,
    thumbs: HashMap<String, Option<IconPixels>>,
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    hits: Vec<(Rect, Target)>,
    /// The tab pill slides; hovers let go quickly. (Opens instantly, fades out once closed.)
    motion: Motion<Anim>,
    /// The hover and selection highlights, gliding from cell to cell.
    hover_glide: Glide,
    sel_glide: Glide,
    /// Set when only the grid is animating (scrolling, a highlight gliding):
    /// the next frame repaints just that.
    part: Option<Rect>,
    pub tracking: bool,
    pub dirty: bool,
}

impl Fading for IconPicker {
    fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }
}

impl IconPicker {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: &Monitor, target: IconTarget, app: &str, hwnd: isize, msg: u32) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + (mon.bottom - mon.top - h) / 2;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let surface = Surface::new(gfx, rect, s, cfg.menubar.backdrop.for_panels(), theme.menu_tint)?;
        PICKER_HWND.store(surface.content.0 as isize, std::sync::atomic::Ordering::Relaxed);
        let mut field = TextField::new(app);
        // Typing replaces the app's name.
        field.select_all();
        Ok(Self {
            surface,
            target,
            app: app.to_string(),
            field,
            tab: Tab::All,
            key: cfg.dock.macosicons_key.trim().to_string(),
            library: Library::start(hwnd, msg),
            index: Load::Loading,
            all: Vec::new(),
            mac: None,
            mac_query: String::new(),
            mac_generation: 0,
            scroll: Scroll::default(),
            selected: None,
            hover: None,
            thumbs: HashMap::new(),
            bitmaps: HashMap::new(),
            hits: Vec::new(),
            motion: Motion::default(),
            hover_glide: Glide::default(),
            sel_glide: Glide::default(),
            part: None,
            tracking: false,
            dirty: true,
        })
    }

    /// Advances the springs; returns true while anything moves.
    pub fn tick(&mut self, dt: f32) -> bool {
        // The grid scrolling or its highlights gliding repaint only the grid.
        let was_dirty = self.dirty;
        let full = self.motion.tick(dt);
        let grid = self.scroll.tick(dt) | self.hover_glide.tick(dt) | self.sel_glide.tick(dt);
        let moving = full || grid;
        if moving {
            let pending_full = was_dirty && self.part.is_none();
            self.part = (!full && !pending_full).then(Self::grid_rect);
            self.dirty = true;
        }
        moving
    }

    /// The scrolling grid, with room for the selection's outline.
    fn grid_rect() -> Rect {
        Rect::new(SHADOW + GRID_PAD - 2.0, SHADOW + SEARCH_H + TABS_H - 2.0, W - 2.0 * GRID_PAD + 4.0, ROWS as f32 * CELL_H + 4.0)
    }

    pub fn animating(&self) -> bool {
        self.motion.moving() || self.scroll.moving() || self.hover_glide.moving() || self.sel_glide.moving()
    }

    /// Scroll range of the grid, in points.
    fn max_scroll(&self) -> f32 {
        (self.results().len().div_ceil(COLS) as f32 * CELL_H - ROWS as f32 * CELL_H).max(0.0)
    }

    /// A result's cell, drawn `offset` points scrolled.
    fn cell_rect(body: Rect, i: usize, offset: f32) -> Rect {
        let grid = Rect::new(body.x + GRID_PAD, body.y + SEARCH_H + TABS_H, W - 2.0 * GRID_PAD, ROWS as f32 * CELL_H);
        let cell_w = grid.w / COLS as f32;
        let (col, row) = (i % COLS, i / COLS);
        Rect::new(grid.x + col as f32 * cell_w, grid.y + row as f32 * CELL_H - offset, cell_w, CELL_H).inset(3.0, 3.0)
    }

    /// Gives the panel keyboard focus (allowed: we're handling a click).
    pub fn focus(&self) {
        unsafe {
            let _ = SetForegroundWindow(self.surface.content);
        }
    }

    fn results(&self) -> &[Hit] {
        match (self.tab, &self.mac) {
            (Tab::All, _) => &self.all,
            (Tab::Mac, Some(Load::Ready(hits))) => hits,
            (Tab::Mac, _) => &[],
        }
    }

    /// Re-runs the Dashboard Icons search for the field's text.
    fn search(&mut self) {
        if let Load::Ready(index) = &self.index {
            self.all = library::search(index, &self.field.string());
        }
        if self.tab == Tab::All {
            self.reset_view();
        }
    }

    fn reset_view(&mut self) {
        self.scroll.jump(0.0);
        // New results: the highlights start where they land.
        self.hover_glide.reset();
        self.sel_glide.reset();
        self.selected = (!self.results().is_empty()).then_some(0);
        self.request_visible();
        self.dirty = true;
    }

    /// Searches macOSicons.com for the field's text (one request).
    fn search_mac(&mut self) {
        let query = self.field.string().trim().to_string();
        if self.key.is_empty() || query.is_empty() {
            return;
        }
        self.mac_generation += 1;
        self.mac_query = query.clone();
        self.mac = Some(Load::Loading);
        self.library.search_macos(&self.key, &query, self.mac_generation);
        self.reset_view();
    }

    /// Starts downloading the thumbnails in view (and the next row).
    fn request_visible(&mut self) {
        let from = (self.scroll.target() / CELL_H).floor() as usize * COLS;
        let urls: Vec<String> = self.results().iter().skip(from).take(COLS * (ROWS + 1)).map(|h| h.url.clone()).collect();
        for url in urls {
            if !self.thumbs.contains_key(&url) && !self.bitmaps.contains_key(&url) {
                self.thumbs.insert(url.clone(), None);
                self.library.thumb(&url);
            }
        }
    }

    /// Takes in what the background workers finished.
    pub fn poll(&mut self) {
        for done in self.library.take() {
            match done {
                Done::Index(r) => {
                    self.index = match r {
                        Ok(index) => Load::Ready(index),
                        Err(e) => Load::Failed(e),
                    };
                    self.search();
                }
                Done::Thumb(url, px) => {
                    match px {
                        Some(px) => {
                            self.thumbs.insert(url, Some(px));
                        }
                        // Failed: nothing to show, and no point asking again.
                        None => {
                            self.thumbs.remove(&url);
                            self.bitmaps.insert(url, None);
                        }
                    }
                }
                Done::MacSearch(generation, r) if generation == self.mac_generation => {
                    self.mac = Some(match r {
                        Ok(hits) => Load::Ready(hits),
                        Err(e) => Load::Failed(e),
                    });
                    if self.tab == Tab::Mac {
                        self.reset_view();
                    }
                }
                Done::MacSearch(..) => {}
            }
        }
        self.dirty = true;
    }

    fn ready(&self, url: &str) -> bool {
        self.bitmaps.get(url).is_some_and(|b| b.is_some()) || self.thumbs.get(url).is_some_and(|t| t.is_some())
    }

    fn set_tab(&mut self, tab: Tab) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        // Only search macOSicons again when the text changed since the last search.
        if tab == Tab::Mac && self.field.string().trim() != self.mac_query {
            self.search_mac();
        }
        self.reset_view();
    }

    /// Copies the selected icon next to the config and sets it.
    fn apply(&self) -> PickerEvent {
        let Some(hit) = self.selected.and_then(|i| self.results().get(i)) else { return PickerEvent::None };
        let Some(cached) = library::cached(&hit.url) else { return PickerEvent::None };
        let prefix = match hit.source {
            Source::Dashboard => "dash",
            Source::MacOs => "mac",
        };
        let stem: String = hit.label.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
        let file = cached.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let dest: PathBuf = library::chosen_dir().join(format!("{prefix}-{stem}-{}", &file[..8.min(file.len())])).with_extension("png");
        let copied = std::fs::create_dir_all(library::chosen_dir()).and_then(|_| std::fs::copy(&cached, &dest));
        if let Err(e) = copied {
            log!("could not save icon: {e}");
            return PickerEvent::None;
        }
        PickerEvent::Run(Action::SetAppIcon { target: self.target.clone(), icon: Some(dest.to_string_lossy().into_owned()) })
    }

    fn press(&mut self, t: Target) -> PickerEvent {
        match t {
            Target::Cell(i) => {
                if self.selected == Some(i) {
                    return self.apply();
                }
                self.selected = Some(i);
            }
            Target::Tab(tab) => self.set_tab(tab),
            Target::Settings => return PickerEvent::Run(Action::OpenSettings(Some(crate::settings::Page::Dock))),
            Target::Button(Button::Cancel) => return PickerEvent::Close,
            Target::Button(Button::Set) => return self.apply(),
            Target::Button(Button::Default) => {
                return PickerEvent::Run(Action::SetAppIcon { target: self.target.clone(), icon: None });
            }
            Target::Button(Button::ChooseFile) => return PickerEvent::Run(Action::PickIconFile(self.target.clone())),
        }
        self.dirty = true;
        PickerEvent::None
    }

    fn select_step(&mut self, delta: isize) {
        let n = self.results().len();
        if n == 0 {
            return;
        }
        let i = (self.selected.unwrap_or(0) as isize + delta).clamp(0, n as isize - 1) as usize;
        self.selected = Some(i);
        let top = (i / COLS) as f32 * CELL_H;
        let s = self.scroll.target();
        if top < s {
            self.scroll.to(top);
        } else if top + CELL_H > s + ROWS as f32 * CELL_H {
            self.scroll.to(top + CELL_H - ROWS as f32 * CELL_H);
        }
        self.request_visible();
    }

    pub fn on_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.field.insert(&c.to_string());
        self.search();
        self.dirty = true;
    }

    pub fn on_key(&mut self, vk: u16, ctrl: bool, shift: bool) -> PickerEvent {
        const TAB: u16 = 0x09;
        const ENTER: u16 = 0x0D;
        const ESC: u16 = 0x1B;
        const LEFT: u16 = 0x25;
        const UP: u16 = 0x26;
        const RIGHT: u16 = 0x27;
        const DOWN: u16 = 0x28;
        self.dirty = true;
        match vk {
            ESC => return PickerEvent::Close,
            ENTER if self.tab == Tab::Mac && self.field.string().trim() != self.mac_query => self.search_mac(),
            ENTER => return self.apply(),
            TAB => self.set_tab(if self.tab == Tab::All { Tab::Mac } else { Tab::All }),
            UP => self.select_step(-(COLS as isize)),
            DOWN => self.select_step(COLS as isize),
            // Left / Right move the caret while there's a selection to edit, else pick icons.
            LEFT | RIGHT if self.field.range().0 == self.field.range().1 && self.selected.is_some() && !shift => {
                self.select_step(if vk == LEFT { -1 } else { 1 });
            }
            _ => {
                if self.field.on_key(vk, ctrl, shift) == KeyResult::Edited {
                    self.search();
                }
            }
        }
        PickerEvent::None
    }

    pub fn on_wheel(&mut self, delta: i16) {
        let max = self.max_scroll();
        self.scroll.wheel(delta as i32, CELL_H * 0.75, max);
        self.request_visible();
        // Only the grid moved: unless a whole redraw is already due, repaint just it.
        if !self.dirty || self.part.is_some() {
            self.part = Some(Self::grid_rect());
        }
        self.dirty = true;
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        let hover = self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|&(_, t)| t);
        if hover != self.hover {
            self.hover = hover;
            self.dirty = true;
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if self.hover.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn on_click(&mut self, x: f32, y: f32) -> PickerEvent {
        match self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|&(_, t)| t) {
            Some(t) => self.press(t),
            None => PickerEvent::None,
        }
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        let body = Rect::new(SHADOW, SHADOW, W, H);
        self.surface.set_backdrop_shape(body, RADIUS);

        // Upload thumbnails that arrived since the last frame.
        // (A `None` thumb is still downloading and stays queued.)
        let target = self.surface.canvas.target().clone();
        let arrived: Vec<String> = self.thumbs.iter().filter(|(_, px)| px.is_some()).map(|(u, _)| u.clone()).collect();
        for url in arrived {
            if let Some(Some(px)) = self.thumbs.remove(&url) {
                self.bitmaps.insert(url, px.to_bitmap(&target));
            }
        }

        let big = gfx.format(&theme.font, 20.0, Weight::Regular, Align::Leading);
        let regular = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Center);
        let left_font = gfx.format(&theme.font, 12.0, Weight::Regular, Align::Leading);
        let small = gfx.format(&theme.font, 11.0, Weight::Regular, Align::Center);
        let message = gfx.format_wrapped(&theme.font, 13.0);
        let has_backdrop = self.surface.backdrop.is_some();

        let text = self.field.string();
        let prefix_w = |n: usize| gfx.measure(&self.field.text[..n].iter().collect::<String>(), &big);
        let (a, b) = self.field.range();
        let (sel_x0, sel_x1, caret_x) = (prefix_w(a), prefix_w(b), prefix_w(self.field.caret));

        let results = self.results().to_vec();
        let note: Option<String> = match self.tab {
            Tab::All => match &self.index {
                Load::Loading => Some("Loading the icon library…".into()),
                Load::Failed(e) => Some(format!("{e}. Check your connection, or choose an image file instead.")),
                Load::Ready(_) if text.trim().is_empty() => Some("Type an app's name to find its icon.".into()),
                Load::Ready(_) if results.is_empty() => Some(format!("No icons match \u{201C}{}\u{201D}. Try another name, or Mac Style.", text.trim())),
                Load::Ready(_) => None,
            },
            Tab::Mac if self.key.is_empty() => Some(
                "Search 30,000+ Mac-style icons from macOSicons.com with your own free API key. Add it in Dock Settings.".into(),
            ),
            Tab::Mac => match &self.mac {
                None => Some("Press Return to search macOSicons.com. Each search uses one of your monthly requests.".into()),
                Some(Load::Loading) => Some("Searching macOSicons.com…".into()),
                Some(Load::Failed(e)) => Some(format!("{e}.")),
                Some(Load::Ready(h)) if h.is_empty() => Some(format!("No Mac-style icons for \u{201C}{}\u{201D}.", self.mac_query)),
                Some(Load::Ready(_)) if text.trim() != self.mac_query => {
                    Some("Press Return to search macOSicons.com for the new text.".into())
                }
                Some(Load::Ready(_)) => None,
            },
        };
        let selected_hit = self.selected.and_then(|i| results.get(i));
        let can_set = selected_hit.is_some_and(|h| self.ready(&h.url));
        let credit = match (self.tab, selected_hit.and_then(|h| h.credit.as_deref())) {
            (Tab::All, _) => "Dashboard Icons \u{00B7} Apache-2.0".to_string(),
            (Tab::Mac, Some(c)) => format!("By {c} \u{00B7} macOSicons"),
            (Tab::Mac, None) => "macOSicons.com".to_string(),
        };
        let max = self.max_scroll();
        self.scroll.clamp(max);
        let offset = self.scroll.offset_px(self.surface.scale);
        let hover_target = match self.hover {
            Some(Target::Cell(i)) if Some(i) != self.selected => Some(Self::cell_rect(body, i, offset)),
            _ => None,
        };
        let hover_glide = self.hover_glide.update(hover_target);
        let sel_glide = self.sel_glide.update(self.selected.filter(|&i| i < results.len()).map(|i| Self::cell_rect(body, i, offset)));
        let (tab, hover, app, show_key_button) =
            (self.tab, self.hover, &self.app, self.tab == Tab::Mac && self.key.is_empty());
        let bitmaps = &self.bitmaps;
        let mut hits = Vec::new();
        self.motion.sweep();
        let motion = &self.motion;
        let hv = |t: Target| motion.fade(Anim::Hover(t), hover == Some(t));

        let part = self.part.take();
        let r = self.surface.render_part(gfx, part, |p: &Painter| {
            p.soft_shadow(body, RADIUS, 12, 2.0, 0.03, 6.0, theme.shadow);
            p.clear_rect(body.inset(3.0, 3.0));
            p.fill_round(body, RADIUS, if has_backdrop { theme.menu_tint.alpha(0.3) } else { theme.menu_tint.alpha(1.0) });
            p.stroke_round(body.inset(0.25, 0.25), RADIUS, theme.menu_border, 0.5);

            // Search field.
            let (gx, gy) = (body.x + 26.0, body.y + SEARCH_H / 2.0 - 1.5);
            p.stroke_circle(gx, gy, 8.0, theme.text_secondary, 2.0);
            p.line(gx + 6.0, gy + 6.0, gx + 11.0, gy + 11.0, theme.text_secondary, 2.4);
            let field = Rect::new(body.x + 52.0, body.y, W - 70.0, SEARCH_H);
            if text.is_empty() {
                p.text(&format!("Search icons for {app}"), &big, field, theme.text_disabled);
            } else if a != b {
                p.fill_rect(Rect::new(field.x + sel_x0, field.cy() - 13.0, sel_x1 - sel_x0, 26.0), theme.accent.alpha(0.35));
            }
            p.text(&text, &big, field, theme.text);
            if a == b {
                let cx = field.x + caret_x + 0.5;
                p.line(cx, field.cy() - 12.0, cx, field.cy() + 12.0, theme.accent, 1.5);
            }
            p.line(body.x, body.y + SEARCH_H, body.right(), body.y + SEARCH_H, theme.menu_separator, 1.0);

            // Tabs: a segmented control.
            let seg = Rect::new(body.cx() - 130.0, body.y + SEARCH_H + 8.0, 260.0, 26.0);
            p.fill_round(seg, 7.0, if theme.dark { Color::gray(255, 0.08) } else { Color::gray(0, 0.06) });
            let tabs = [("All Icons", Tab::All), ("Mac Style", Tab::Mac)];
            let current = tabs.iter().position(|(_, t)| *t == tab).unwrap_or(0) as f32;
            let k = motion.get(Anim::TabPill, current, feel::SLIDE);
            let pill = Rect::new(seg.x + 2.0 + k * 128.0, seg.y + 2.0, 128.0, seg.h - 4.0);
            p.fill_round(pill, 6.0, if theme.dark { Color::gray(255, 0.2) } else { Color::WHITE });
            for (i, (label, t)) in tabs.into_iter().enumerate() {
                let r = Rect::new(seg.x + 2.0 + i as f32 * 128.0, seg.y + 2.0, 128.0, seg.h - 4.0);
                p.text(label, &regular, r, theme.text);
                hits.push((r, Target::Tab(t)));
            }

            // Grid.
            let grid = Rect::new(body.x + GRID_PAD, body.y + SEARCH_H + TABS_H, W - 2.0 * GRID_PAD, ROWS as f32 * CELL_H);
            if let Some(note) = &note {
                let r = Rect::new(grid.x + 60.0, grid.y + grid.h * 0.3, grid.w - 120.0, 60.0);
                p.text(note, &message, r, theme.text_secondary);
                if show_key_button {
                    let b = Rect::new(grid.cx() - 80.0, r.bottom() + 12.0, 160.0, 28.0);
                    p.fill_round(b, 7.0, theme.accent);
                    p.text("Open Dock Settings", &regular, b, Color::WHITE);
                    hits.push((b, Target::Settings));
                }
            } else {
                // Rows scroll smoothly under a clip; highlights glide between cells.
                p.push_clip(grid);
                if let Some((g, a)) = hover_glide {
                    p.fill_round(g, 10.0, if theme.dark { Color::gray(255, 0.08 * a) } else { Color::gray(0, 0.05 * a) });
                }
                if let Some((g, a)) = sel_glide {
                    p.fill_round(g, 10.0, theme.accent.alpha(0.22 * a));
                    p.stroke_round(g.inset(0.75, 0.75), 10.0, theme.accent.alpha(a), 1.5);
                }
                let first = (offset / CELL_H).floor().max(0.0) as usize * COLS;
                let last = (((offset + grid.h) / CELL_H).ceil().max(0.0) as usize * COLS).min(results.len());
                for i in first..last {
                    let hit = &results[i];
                    let cell = Self::cell_rect(body, i, offset);
                    let icon = Rect::new(cell.cx() - ICON / 2.0, cell.y + 10.0, ICON, ICON);
                    match bitmaps.get(&hit.url) {
                        Some(Some(bmp)) => {
                            // Keep the image's aspect inside the square.
                            let size = unsafe { bmp.GetSize() };
                            let s = ICON / size.width.max(size.height).max(1.0);
                            let (w, h) = (size.width * s, size.height * s);
                            p.bitmap(bmp, Rect::new(icon.cx() - w / 2.0, icon.cy() - h / 2.0, w, h), 1.0);
                        }
                        Some(None) => p.text("\u{2014}", &regular, icon, theme.text_disabled),
                        None => p.fill_round(icon.inset(8.0, 8.0), 12.0, if theme.dark { Color::gray(255, 0.06) } else { Color::gray(0, 0.05) }),
                    }
                    let label = Rect::new(cell.x + 4.0, icon.bottom() + 4.0, cell.w - 8.0, 18.0);
                    p.text(&hit.label, &small, label, theme.text);
                    let (top, bottom) = (cell.y.max(grid.y), cell.bottom().min(grid.bottom()));
                    if bottom > top {
                        hits.push((Rect::new(cell.x, top, cell.w, bottom - top), Target::Cell(i)));
                    }
                }
                p.pop_clip();
            }

            // Footer.
            let fy = body.bottom() - FOOTER_H;
            p.line(body.x, fy, body.right(), fy, theme.menu_separator, 1.0);
            let mut bx = body.right() - 16.0;
            for (label, button, w) in [("Set Icon", Button::Set, 84.0), ("Cancel", Button::Cancel, 76.0), ("Use Default", Button::Default, 96.0), ("Choose File\u{2026}", Button::ChooseFile, 108.0)] {
                let r = Rect::new(bx - w, fy + (FOOTER_H - 28.0) / 2.0, w, 28.0);
                bx -= w + 8.0;
                let primary = button == Button::Set;
                let enabled = !primary || can_set;
                let t = hv(Target::Button(button));
                let fill = if primary && enabled {
                    theme.accent
                } else if theme.dark {
                    Color::gray(255, 0.1 + 0.06 * t)
                } else {
                    Color::gray(0, 0.06 + 0.04 * t)
                };
                p.fill_round(r, 7.0, fill);
                let ink = if primary && enabled { Color::WHITE } else if enabled { theme.text } else { theme.text_disabled };
                p.text(label, &regular, r, ink);
                if enabled {
                    hits.push((r, Target::Button(button)));
                }
            }
            p.text(&credit, &left_font, Rect::new(body.x + 18.0, fy, bx - body.x - 24.0, FOOTER_H), theme.text_secondary);
        });
        self.hits = hits;
        if let Err(e) = r {
            log!("icon picker render failed: {e}");
        }
    }
}

impl Drop for IconPicker {
    fn drop(&mut self) {
        // A picker fading out may outlive the next one to open: only clear our own handle.
        let me = self.surface.content.0 as isize;
        let _ = PICKER_HWND.compare_exchange(me, 0, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the picker (never shown) with live results to `DOCKSIDE_PICKER_OUT`:
    /// `cargo test renders_picker -- --ignored`. Needs the network.
    #[test]
    #[ignore]
    fn renders_picker() {
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        let Ok(out) = std::env::var("DOCKSIDE_PICKER_OUT") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let _ = crate::win::register_class(Some(proc_));
        let gfx = Gfx::new().unwrap();
        let cfg = crate::config::defaults();
        let theme = Theme::new(&cfg, &gfx);
        let target = IconTarget { pinned: None, exe: Some("discord.exe".into()) };
        let mut p = IconPicker::new(&gfx, &theme, &cfg, &Monitor::primary(), target, "Discord", 0, 0).unwrap();
        p.surface.set_hidden(true);
        // Wait for the index, then for the first page of thumbnails.
        for _ in 0..200 {
            p.poll();
            let pending = p.thumbs.values().any(|t| t.is_none());
            if matches!(p.index, Load::Ready(_)) && !p.all.is_empty() && !pending {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        p.render(&gfx, &theme);
        let c = &p.surface.canvas;
        crate::gfx::write_png(&gfx, c.width as u32, c.height as u32, &c.pixels(), &out);
    }
}
