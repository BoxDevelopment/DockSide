//! macOS-style popup menus (used by the menu bar and Dock context menus) and
//! the actions their items trigger.

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;

use crate::anim::Glide;
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Gfx, Weight};
use crate::surface::{Fading, Surface};
use crate::theme::Theme;
use crate::win::apps::AppWindow;
use crate::win::system::Shortcut;
use crate::win::Monitor;

#[derive(Clone, Debug)]
pub enum Action {
    Open(String),
    Launch { path: String, args: Option<String> },
    Keys(Shortcut),
    Activate(HWND),
    Minimize(HWND),
    MinimizeWindows(Vec<HWND>),
    Zoom(HWND),
    HideOthers(HWND),
    ShowAll,
    BringToFront(Vec<HWND>),
    CloseWindows(Vec<HWND>),
    Reveal(String),
    TogglePin { path: String, name: String, pin: bool },
    /// Replace the pinned Dock apps (after a drag reorder / removal).
    SetDockApps(Vec<crate::config::DockApp>),
    /// Write one setting (`[table] key = value`) and apply it.
    SetConfig { table: &'static str, key: &'static str, value: crate::config::ConfigValue },
    SetStartAtLogin(bool),
    /// Turn "Run as administrator" on or off (restarts DockSide).
    SetRunAsAdmin(bool),
    /// File picker, then pin the chosen app.
    AddDockApp,
    /// Program (or, with `folder`, folder) picker, then add it to `[windows] no_inject`.
    AddNoInject { folder: bool },
    /// Open the icon picker for a Dock app (with its display name).
    ChangeIcon { target: crate::icon_picker::IconTarget, name: String },
    /// Set (or, with `None`, reset) a Dock app's custom icon.
    SetAppIcon { target: crate::icon_picker::IconTarget, icon: Option<String> },
    /// Image file picker, then use the file as a Dock app's icon.
    PickIconFile(crate::icon_picker::IconTarget),
    OpenSettings(Option<crate::settings::Page>),
    /// Show the first-launch Setup Assistant again.
    OpenOnboarding,
    /// The About This PC window.
    OpenAbout,
    /// Open a Finder window (at a folder, or Home).
    OpenFinder(Option<std::path::PathBuf>),
    /// Show or hide the Spotlight search panel.
    ToggleSpotlight,
    /// Open (or close) Launchpad in the given look: Sequoia's full-screen
    /// Launchpad, or Tahoe's Apps panel in Spotlight.
    OpenLaunchpad(crate::config::LaunchStyle),
    /// Take an app out of Launchpad (and Spotlight's Apps panel).
    LaunchpadHide { name: String, target: String },
    /// Bring a hidden app back; take out an added one; pick an app to add.
    LaunchpadUnhide(String),
    LaunchpadRemoveAdded(String),
    LaunchpadAddApp,
    /// Rename the open Launchpad's folder at this index on its page.
    LaunchpadRenameFolder(usize),
    /// Edit mode in the open Launchpad on (or off); with an app picked; add an app there.
    LaunchpadEdit(bool),
    LaunchpadSelect(String),
    LaunchpadAddHere,
    /// A command for one Finder window (from its context menu).
    Finder { window: u32, cmd: crate::finder::FinderCmd },
    /// Move files to the Recycle Bin (dropped on the Dock's Trash).
    TrashFiles(Vec<std::path::PathBuf>),
    EmptyTrash,
    Sleep,
    Restart,
    ShutDown,
    LogOut,
    Lock,
    OpenConfig,
    /// Timer / stopwatch / shelf commands for the Dynamic Island.
    Island(crate::island::IslandCmd),
    /// Drag files out of the Dynamic Island (modal).
    IslandDrag(Vec<std::path::PathBuf>),
    /// Add / remove the island's hooks in Claude Code's settings.
    ConnectClaude(bool),
    /// Ask Discord for permission (true), or forget it (false).
    ConnectDiscord(bool),
    /// Add, remove or resize a desktop widget.
    Widget(crate::widgets::WidgetCmd),
    /// Switch Windows (and DockSide) to dark or light mode.
    SetDarkMode(bool),
    /// Click an app's notification-area icon.
    TrayClick(crate::win::tray::IconId, crate::win::tray::Button),
    Quit,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<Shortcut>,
    pub action: Option<Action>,
    pub checked: bool,
    /// A small icon before the label.
    pub icon: Option<std::rc::Rc<crate::gfx::icons::IconPixels>>,
    /// What a right click does instead (tray icons open their own menu).
    pub alt_action: Option<Action>,
    /// Items shown beside this one while it's highlighted (empty: none).
    pub submenu: Vec<Entry>,
}

impl MenuItem {
    fn new(label: String, shortcut: Option<Shortcut>, action: Option<Action>) -> Self {
        MenuItem { label, shortcut, action, checked: false, icon: None, alt_action: None, submenu: Vec::new() }
    }
}

#[derive(Clone, Debug)]
pub enum Entry {
    Item(MenuItem),
    Separator,
}

impl Entry {
    pub fn item(label: impl Into<String>, action: Action) -> Entry {
        Entry::Item(MenuItem::new(label.into(), None, Some(action)))
    }

    /// An item with an icon, and a separate right-click action.
    pub fn with_icon(
        label: impl Into<String>,
        icon: Option<crate::gfx::icons::IconPixels>,
        action: Action,
        alt_action: Action,
    ) -> Entry {
        let mut item = MenuItem::new(label.into(), None, Some(action));
        item.icon = icon.map(std::rc::Rc::new);
        item.alt_action = Some(alt_action);
        Entry::Item(item)
    }

    /// An item that sends `sc` to the focused app.
    pub fn keys(label: impl Into<String>, sc: Shortcut) -> Entry {
        Entry::Item(MenuItem::new(label.into(), Some(sc), Some(Action::Keys(sc))))
    }

    pub fn disabled(label: impl Into<String>) -> Entry {
        Entry::Item(MenuItem::new(label.into(), None, None))
    }

    /// An item that opens `entries` beside it (disabled when none of them is enabled).
    pub fn submenu(label: impl Into<String>, entries: Vec<Entry>) -> Entry {
        let mut item = MenuItem::new(label.into(), None, None);
        if entries.iter().any(Entry::enabled) {
            item.submenu = entries;
        }
        Entry::Item(item)
    }

    pub fn shortcut(mut self, sc: Shortcut) -> Entry {
        if let Entry::Item(i) = &mut self {
            i.shortcut = Some(sc);
        }
        self
    }

    /// Adds an icon before the label (shell menu bitmaps, Open With apps).
    pub fn with_pixels(mut self, icon: Option<crate::gfx::icons::IconPixels>) -> Entry {
        if let (Entry::Item(i), Some(px)) = (&mut self, icon) {
            i.icon = Some(std::rc::Rc::new(px));
        }
        self
    }

    pub fn checked(mut self, on: bool) -> Entry {
        if let Entry::Item(i) = &mut self {
            i.checked = on;
        }
        self
    }

    fn enabled(&self) -> bool {
        matches!(self, Entry::Item(i) if i.action.is_some() || !i.submenu.is_empty())
    }

    fn has_submenu(&self) -> bool {
        matches!(self, Entry::Item(i) if !i.submenu.is_empty())
    }
}

/// Who opened the menu, so the opener can highlight itself and react to arrows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuSource {
    MenuBar(crate::menubar::Hit),
    Dock(usize),
    /// A context menu inside one of our app windows (Finder).
    Window,
}

#[derive(Clone, Copy, Debug)]
pub enum Placement {
    /// Top-left corner of the menu body, in screen points.
    Below { x: f32, y: f32 },
    /// Horizontal center and bottom edge of the menu body, in screen points.
    Above { cx: f32, bottom: f32 },
    /// A submenu: right of `right` (or, without room, left of `left`), its
    /// body's top at `top`, in screen points.
    Beside { left: f32, right: f32, top: f32 },
}

pub enum MenuEvent {
    None,
    Activate(Action),
    Close,
    /// Left/Right arrow: move to the neighbouring menu-bar menu.
    Step(i32),
    /// Open the highlighted item's submenu (a click, Enter or the Right arrow).
    Open,
}

const SHADOW: f32 = 20.0;
const ITEM_H: f32 = 22.0;
/// Item icon size (tray icons), in points.
const ICON: f32 = 16.0;
const SEP_H: f32 = 11.0;
const PAD_Y: f32 = 5.0;
const RADIUS: f32 = 7.0;
const FONT_SIZE: f32 = 13.0;
/// Room for a submenu's chevron.
const CHEVRON: f32 = 8.0;

/// Whether moving `from` → `to` heads for a submenu on the `edge` side,
/// between `top` and `bottom`: `to` lies in the triangle from `from` to that
/// side (with a little slack for unsteady hands).
fn toward(from: (f32, f32), to: (f32, f32), edge: f32, top: f32, bottom: f32) -> bool {
    let (dx, ex) = (to.0 - from.0, edge - from.0);
    if dx == 0.0 || dx.signum() != ex.signum() || dx.abs() > ex.abs() {
        return false;
    }
    // How much of the side the triangle spans this far along.
    let t = dx / ex;
    let (lo, hi) = (from.1 + (top - from.1) * t, from.1 + (bottom - from.1) * t);
    (lo - 2.0..=hi + 2.0).contains(&to.1)
}

pub struct PopupMenu {
    pub surface: Surface,
    pub source: MenuSource,
    entries: Vec<Entry>,
    rects: Vec<Rect>,
    body: Rect,
    hovered: Option<usize>,
    mac: bool,
    /// The selection highlight, gliding between items.
    glide: Glide,
    /// Item icons (tray icons), made once rather than every frame.
    icons: Vec<Option<ID2D1Bitmap>>,
    /// While a submenu is open: the side it's on and its top and bottom, in
    /// surface points. Heading toward it keeps the highlight where it is.
    aim: Option<(f32, f32, f32)>,
    /// The last pointer position, in surface points.
    last: (f32, f32),
    /// How far the items are scrolled up (menus taller than the screen), and the most they can be.
    scroll: f32,
    max_scroll: f32,
    /// Resting on a scroll arrow: points per second it scrolls (negative: up).
    auto: f32,
}

/// The work area a menu must stay inside, in screen points.
#[derive(Clone, Copy, Debug)]
struct Bounds {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

/// The gap kept between a menu and the edges of the work area.
const EDGE: f32 = 4.0;
/// Height of a scroll arrow strip.
const ARROW_H: f32 = 18.0;
/// How fast resting on a scroll arrow scrolls, in points per second.
const ARROW_SPEED: f32 = 360.0;

/// Where a menu body `w` x `h` goes (its top-left, in screen points). A context
/// menu (`context`) opens right of and below the pointer, flipping left or up
/// where it would run off the screen, like macOS; anything still too big slides
/// back inside.
fn place(placement: Placement, w: f32, h: f32, area: Bounds, context: bool) -> (f32, f32) {
    let fit = |pos: f32, size: f32, lo: f32, hi: f32| pos.min(hi - size).max(lo);
    match placement {
        Placement::Below { x, y } => {
            let x = if context && x + w > area.right && x - w >= area.left { x - w } else { x };
            let flipped = context && y + h > area.bottom && y - h >= area.top;
            // Never pushed down from where it was asked to open (a menu bar menu sits on the bar).
            let top = area.top.min(y);
            (fit(x, w, area.left, area.right), fit(if flipped { y - h } else { y }, h, top, area.bottom))
        }
        // Never pushed up from where it was asked to end (a Dock menu sits on the Dock).
        Placement::Above { cx, bottom } => (fit(cx - w / 2.0, w, area.left, area.right), fit(bottom - h, h, area.top, area.bottom.max(bottom))),
        Placement::Beside { left, right, top } => {
            let x = if right + w <= area.right || left - w < area.left { right } else { left - w };
            (fit(x, w, area.left, area.right), fit(top, h, area.top.min(top), area.bottom))
        }
    }
}

/// Menus open instantly; once closed they fade out (see the App's frame loop).
impl Fading for PopupMenu {
    fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }
}

impl PopupMenu {
    pub fn new(
        gfx: &Gfx,
        theme: &Theme,
        cfg: &crate::config::Config,
        entries: Vec<Entry>,
        placement: Placement,
        monitor: &Monitor,
        source: MenuSource,
    ) -> Result<Self> {
        let mac = cfg.general.shortcut_style == crate::config::ShortcutStyle::Mac;
        let font = gfx.format(&theme.font, FONT_SIZE, Weight::Regular, Align::Leading);
        let mut label_w: f32 = 0.0;
        let mut short_w: f32 = 0.0;
        for e in &entries {
            if let Entry::Item(i) = e {
                let icon_w = if i.icon.is_some() { ICON + 6.0 } else { 0.0 };
                label_w = label_w.max(gfx.measure(&i.label, &font) + icon_w);
                if let Some(sc) = &i.shortcut {
                    short_w = short_w.max(gfx.measure(&sc.label(mac), &font));
                }
            }
        }
        if entries.iter().any(Entry::has_submenu) {
            short_w = short_w.max(CHEVRON);
        }
        let width = (22.0 + label_w + if short_w > 0.0 { 32.0 + short_w } else { 0.0 } + 18.0).clamp(160.0, 420.0);
        let content_h = PAD_Y * 2.0
            + entries.iter().map(|e| if matches!(e, Entry::Separator) { SEP_H } else { ITEM_H }).sum::<f32>();

        // Body position in screen points, kept inside the monitor's work area (the
        // taskbar and app bars stay uncovered). A menu taller than that scrolls.
        let s = monitor.scale;
        let work = monitor.work_area();
        let area = Bounds {
            left: work.left as f32 / s + EDGE,
            top: work.top as f32 / s + EDGE,
            right: work.right as f32 / s - EDGE,
            bottom: work.bottom as f32 / s - EDGE,
        };
        let height = content_h.min((area.bottom - area.top).max(ITEM_H * 3.0 + PAD_Y * 2.0));
        let (x, y) = place(placement, width, height, area, source == MenuSource::Window);

        let rect = RECT {
            left: ((x - SHADOW) * s).round() as i32,
            top: ((y - SHADOW) * s).round() as i32,
            right: ((x + width + SHADOW) * s).round() as i32,
            bottom: ((y + height + SHADOW) * s).round() as i32,
        };
        let tint = theme.menu_tint;
        let mut surface = Surface::new(gfx, rect, s, cfg.menubar.backdrop.for_panels(), tint)?;
        let body = Rect::new(SHADOW, SHADOW, width, height);
        surface.set_backdrop_shape(body, RADIUS);

        let mut rects = Vec::with_capacity(entries.len());
        let mut cy = body.y + PAD_Y;
        for e in &entries {
            let h = if matches!(e, Entry::Separator) { SEP_H } else { ITEM_H };
            rects.push(Rect::new(body.x, cy, body.w, h));
            cy += h;
        }

        Ok(Self {
            surface,
            source,
            entries,
            rects,
            body,
            hovered: None,
            mac,
            glide: Glide::default(),
            icons: Vec::new(),
            aim: None,
            last: (0.0, 0.0),
            scroll: 0.0,
            max_scroll: (content_h - height).max(0.0),
            auto: 0.0,
        })
    }

    /// Item `i`'s rect as shown (scrolled), in surface points.
    fn shown(&self, i: usize) -> Rect {
        self.rects[i].offset(0.0, -self.scroll)
    }

    /// The scroll arrows' strips, when they're showing (only in a menu too tall for the screen).
    fn arrow_up(&self) -> Option<Rect> {
        (self.scroll > 0.5).then(|| Rect::new(self.body.x, self.body.y, self.body.w, ARROW_H))
    }

    fn arrow_down(&self) -> Option<Rect> {
        (self.scroll < self.max_scroll - 0.5).then(|| Rect::new(self.body.x, self.body.bottom() - ARROW_H, self.body.w, ARROW_H))
    }

    /// Scrolls by `dy` points; true when it moved.
    fn scroll_by(&mut self, dy: f32) -> bool {
        let to = (self.scroll + dy).clamp(0.0, self.max_scroll);
        let moved = (to - self.scroll).abs() > 0.01;
        self.scroll = to;
        if moved {
            // What's under the pointer now.
            self.hovered = self.index_at(self.last.0, self.last.1);
        }
        moved
    }

    /// The mouse wheel over the menu (tall menus only); true when it scrolled.
    pub fn on_wheel(&mut self, delta: i32) -> bool {
        self.max_scroll > 0.0 && self.scroll_by(-delta as f32 / 120.0 * ITEM_H * 3.0)
    }

    /// Scrolls item `i` into view (keyboard moves).
    fn reveal(&mut self, i: usize) {
        if self.max_scroll <= 0.0 {
            return;
        }
        let r = self.shown(i);
        let (top, bottom) = (self.body.y + ARROW_H, self.body.bottom() - ARROW_H);
        if r.y < top {
            self.scroll_by(r.y - top);
        } else if r.bottom() > bottom {
            self.scroll_by(r.bottom() - bottom);
        }
    }

    /// Top-left of the surface, in screen points.
    fn origin(&self) -> (f32, f32) {
        let s = self.surface.scale;
        (self.surface.rect.left as f32 / s, self.surface.rect.top as f32 / s)
    }

    /// The menu body, in screen points.
    pub fn screen_body(&self) -> Rect {
        let (ox, oy) = self.origin();
        self.body.offset(ox, oy)
    }

    /// The highlighted item, when it has a submenu.
    pub fn hovered_submenu(&self) -> Option<usize> {
        self.hovered.filter(|&i| self.entries[i].has_submenu())
    }

    pub fn has_hover(&self) -> bool {
        self.hovered.is_some()
    }

    /// Item `i`'s submenu and where it opens: beside the menu, its first item level with `i`.
    pub fn submenu(&self, i: usize) -> Option<(Vec<Entry>, Placement)> {
        let Some(Entry::Item(item)) = self.entries.get(i) else { return None };
        if item.submenu.is_empty() {
            return None;
        }
        let (ox, oy) = self.origin();
        let placement = Placement::Beside { left: ox + self.body.x, right: ox + self.body.right(), top: oy + self.shown(i).y - PAD_Y };
        Some((item.submenu.clone(), placement))
    }

    /// Tells the menu where its open submenu is (body, in screen points), or that it closed.
    pub fn aim_at(&mut self, child: Option<Rect>) {
        let (ox, oy) = self.origin();
        self.aim = child.map(|c| {
            let edge = if c.cx() >= ox + self.body.cx() { self.body.right() } else { self.body.x };
            (edge, c.y - oy, c.bottom() - oy)
        });
    }

    /// Highlights the first item (a submenu opened from the keyboard).
    pub fn select_first(&mut self) -> bool {
        self.hovered = None;
        self.on_key(0x28).1
    }

    /// Advances the highlight (and a scroll arrow being rested on); returns true while either moves (redraw then).
    pub fn tick(&mut self, dt: f32) -> bool {
        let scrolled = self.auto != 0.0 && self.scroll_by(self.auto * dt.min(0.05));
        if self.auto != 0.0 && !scrolled {
            self.auto = 0.0;
        }
        self.glide.tick(dt) | scrolled
    }

    pub fn animating(&self) -> bool {
        self.glide.moving() || self.auto != 0.0
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        let font = gfx.format(&theme.font, FONT_SIZE, Weight::Regular, Align::Leading);
        let font_right = gfx.format(&theme.font, FONT_SIZE, Weight::Regular, Align::Trailing);
        let has_backdrop = self.surface.backdrop.is_some();
        if self.icons.is_empty() {
            let rt = self.surface.canvas.target().clone();
            self.icons = self
                .entries
                .iter()
                .map(|e| match e {
                    Entry::Item(MenuItem { icon: Some(px), .. }) => px.to_bitmap(&rt),
                    _ => None,
                })
                .collect();
        }
        let target = self.hovered.map(|i| self.shown(i).inset(5.0, 0.0));
        let glide = self.glide.update(target);
        let (up, down) = (self.arrow_up(), self.arrow_down());
        let rects: Vec<Rect> = (0..self.rects.len()).map(|i| self.shown(i)).collect();
        let (body, entries, rects, mac, icons) = (self.body, &self.entries, &rects, self.mac, &self.icons);
        // Items show between the scroll arrows (when there are any).
        let items_clip = Rect::new(body.x, body.y + up.map_or(0.0, |r| r.h), body.w, body.h - up.map_or(0.0, |r| r.h) - down.map_or(0.0, |r| r.h));

        // One item, in its plain colours or (`lit`) white for under the highlight.
        let draw_item = |p: &crate::gfx::Painter, i: usize, lit: bool| {
            let (Entry::Item(item), r) = (&entries[i], rects[i]) else { return };
            // Submenu items have no action of their own, but they're live when they open one.
            let enabled = item.action.is_some() || !item.submenu.is_empty();
            let color = if lit {
                Color::WHITE
            } else if enabled {
                theme.text
            } else {
                theme.text_disabled
            };
            if item.checked {
                p.text("✓", &font, Rect::new(r.x + 8.0, r.y, 14.0, r.h), color);
            }
            let mut lx = r.x + 22.0;
            if item.icon.is_some() {
                if let Some(b) = &icons[i] {
                    p.bitmap(b, Rect::new(lx, r.cy() - ICON / 2.0, ICON, ICON), 1.0);
                }
                lx += ICON + 6.0;
            }
            p.text(&item.label, &font, Rect::new(lx, r.y, r.right() - 18.0 - lx, r.h), color);
            if let Some(sc) = &item.shortcut {
                let c = if lit { Color::WHITE } else { theme.text_secondary };
                p.text(&sc.label(mac), &font_right, Rect::new(r.x, r.y, r.w - 16.0, r.h), c);
            }
            if !item.submenu.is_empty() {
                let (cx, cy, h) = (r.right() - 16.0 - CHEVRON / 2.0, r.cy(), 4.0);
                p.line(cx - h * 0.45, cy - h, cx + h * 0.55, cy, color, 1.5);
                p.line(cx + h * 0.55, cy, cx - h * 0.45, cy + h, color, 1.5);
            }
        };

        let r = self.surface.render(gfx, |p| {
            // Soft drop shadow, layered rounded rects.
            p.soft_shadow(body, RADIUS, 10, 1.6, 0.022, 4.0, theme.shadow);
            // Keep the shadow from darkening the blurred backdrop under the body.
            p.clear_rect(body.inset(2.0, 2.0));
            p.fill_round(body, RADIUS, if has_backdrop { theme.menu_tint.alpha(0.35) } else { theme.menu_tint.alpha(1.0) });
            p.stroke_round(body.inset(0.25, 0.25), RADIUS, theme.menu_border, 0.5);

            p.clipped(items_clip, || {
                for (i, (e, r)) in entries.iter().zip(rects).enumerate() {
                    if r.bottom() < items_clip.y || r.y > items_clip.bottom() {
                        continue;
                    }
                    match e {
                        Entry::Separator => {
                            let y = r.cy();
                            p.line(r.x + 10.0, y, r.right() - 10.0, y, theme.menu_separator, 1.0);
                        }
                        Entry::Item(_) => draw_item(p, i, false),
                    }
                }
                // The highlight glides over the items; whatever it covers is redrawn
                // white, clipped to it, so text turns exactly where the highlight is.
                if let Some((g, a)) = glide {
                    p.faded(a, || {
                        p.fill_round(g, 4.0, theme.accent);
                        p.clipped(g, || {
                            for (i, r) in rects.iter().enumerate() {
                                if r.bottom() > g.y && r.y < g.bottom() {
                                    draw_item(p, i, true);
                                }
                            }
                        });
                    });
                }
            });
            // Scroll arrows: small chevrons pointing where there's more, as on macOS.
            for (r, dir) in [(up, -1.0f32), (down, 1.0)] {
                if let Some(r) = r {
                    let (cx, cy, h) = (r.cx(), r.cy(), 3.5);
                    p.line(cx - h * 1.4, cy - h * dir * 0.5, cx, cy + h * dir * 0.5, theme.text, 1.5);
                    p.line(cx, cy + h * dir * 0.5, cx + h * 1.4, cy - h * dir * 0.5, theme.text, 1.5);
                }
            }
        });
        if let Err(e) = r {
            log!("menu render failed: {e}");
        }
    }

    /// Whether a point (in surface points) is on the menu itself rather than its shadow.
    pub fn in_body(&self, x: f32, y: f32) -> bool {
        self.body.contains(x, y)
    }

    fn index_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.body.contains(x, y) || [self.arrow_up(), self.arrow_down()].into_iter().flatten().any(|r| r.contains(x, y)) {
            return None;
        }
        (0..self.rects.len()).find(|&i| self.shown(i).contains(x, y)).filter(|&i| self.entries[i].enabled())
    }

    /// Returns true when the highlight changed.
    pub fn on_move(&mut self, x: f32, y: f32) -> bool {
        let from = std::mem::replace(&mut self.last, (x, y));
        // Resting on a scroll arrow scrolls that way (see `tick`).
        self.auto = match (self.arrow_up(), self.arrow_down()) {
            (Some(r), _) if r.contains(x, y) => -ARROW_SPEED,
            (_, Some(r)) if r.contains(x, y) => ARROW_SPEED,
            _ => 0.0,
        };
        // Heading for the open submenu across other items: they don't take the highlight.
        if let Some((edge, top, bottom)) = self.aim {
            if self.hovered.is_some() && toward(from, (x, y), edge, top, bottom) {
                return false;
            }
        }
        let h = self.index_at(x, y);
        let changed = h != self.hovered;
        self.hovered = h;
        changed
    }

    pub fn on_leave(&mut self) -> bool {
        self.auto = 0.0;
        // The open submenu's item stays highlighted while the pointer is in the submenu.
        if self.aim.is_some() {
            return false;
        }
        let changed = self.hovered.is_some();
        self.hovered = None;
        changed
    }

    /// `right`: a right click, which runs an item's `alt_action` when it has one.
    pub fn on_click(&mut self, x: f32, y: f32, right: bool) -> MenuEvent {
        if !self.body.contains(x, y) {
            return MenuEvent::Close;
        }
        match self.index_at(x, y).map(|i| &self.entries[i]) {
            Some(e) if e.has_submenu() => MenuEvent::Open,
            Some(Entry::Item(MenuItem { alt_action: Some(a), .. })) if right => MenuEvent::Activate(a.clone()),
            Some(Entry::Item(MenuItem { action: Some(a), .. })) => MenuEvent::Activate(a.clone()),
            _ => MenuEvent::None,
        }
    }

    pub fn on_key(&mut self, vk: u16) -> (MenuEvent, bool) {
        const VK_RETURN: u16 = 0x0D;
        const VK_ESCAPE: u16 = 0x1B;
        const VK_LEFT: u16 = 0x25;
        const VK_UP: u16 = 0x26;
        const VK_RIGHT: u16 = 0x27;
        const VK_DOWN: u16 = 0x28;
        match vk {
            VK_ESCAPE => (MenuEvent::Close, false),
            VK_LEFT => (MenuEvent::Step(-1), false),
            VK_RIGHT | VK_RETURN if self.hovered_submenu().is_some() => (MenuEvent::Open, false),
            VK_RIGHT => (MenuEvent::Step(1), false),
            VK_RETURN => match self.hovered.map(|i| &self.entries[i]) {
                Some(Entry::Item(MenuItem { action: Some(a), .. })) => (MenuEvent::Activate(a.clone()), false),
                _ => (MenuEvent::None, false),
            },
            VK_UP | VK_DOWN => {
                let n = self.entries.len() as i32;
                let dir = if vk == VK_DOWN { 1 } else { -1 };
                let mut i = self.hovered.map(|h| h as i32).unwrap_or(if dir > 0 { -1 } else { n });
                for _ in 0..n {
                    i = (i + dir).rem_euclid(n);
                    if self.entries[i as usize].enabled() {
                        self.hovered = Some(i as usize);
                        self.reveal(i as usize);
                        return (MenuEvent::None, true);
                    }
                }
                (MenuEvent::None, false)
            }
            _ => (MenuEvent::None, false),
        }
    }
}

// ---------------------------------------------------------------------------
// Standard menu contents
// ---------------------------------------------------------------------------

const VK_ESC: u16 = 0x1B;
const VK_F1: u16 = 0x70;
const VK_F11: u16 = 0x7A;
const VK_LEFT: u16 = 0x25;
const VK_RIGHT: u16 = 0x27;
const VK_PLUS: u16 = 0xBB;
const VK_MINUS: u16 = 0xBD;
const VK_PERIOD: u16 = 0xBE;
const fn ch(c: u8) -> u16 {
    c as u16
}

pub const MENU_TITLES: [&str; 5] = ["File", "Edit", "View", "Window", "Help"];

pub struct MenuCtx<'a> {
    pub app_name: &'a str,
    pub target: HWND,
    /// The focused app's windows, topmost first.
    pub app_windows: &'a [AppWindow],
}

pub fn apple_menu() -> Vec<Entry> {
    vec![
        Entry::item("About This PC", Action::OpenAbout),
        Entry::Separator,
        Entry::item("System Settings…", Action::Open("ms-settings:".into())),
        Entry::item("App Store…", Action::Open("ms-windows-store:".into())),
        Entry::Separator,
        Entry::keys("Force Quit…", Shortcut::new("cs", VK_ESC, "⎋")),
        Entry::Separator,
        Entry::item("Sleep", Action::Sleep),
        Entry::item("Restart…", Action::Restart),
        Entry::item("Shut Down…", Action::ShutDown),
        Entry::Separator,
        Entry::item("Lock Screen", Action::Lock).shortcut(Shortcut::new("w", ch(b'L'), "L")),
        Entry::item("Log Out…", Action::LogOut),
        Entry::Separator,
        Entry::item("DockSide Settings…", Action::OpenSettings(None)),
        Entry::item("Quit DockSide", Action::Quit),
    ]
}

pub fn app_menu(ctx: &MenuCtx) -> Vec<Entry> {
    let name = ctx.app_name;
    let mut v = vec![
        Entry::disabled(format!("About {name}")),
        Entry::Separator,
        Entry::item(format!("Hide {name}"), Action::Minimize(ctx.target)).shortcut(Shortcut::new("c", ch(b'H'), "H")),
        Entry::item("Hide Others", Action::HideOthers(ctx.target)).shortcut(Shortcut::new("ca", ch(b'H'), "H")),
        Entry::item("Show All", Action::ShowAll),
        Entry::Separator,
    ];
    let windows: Vec<HWND> = ctx.app_windows.iter().map(|w| w.hwnd).collect();
    let quit = if windows.is_empty() { Action::CloseWindows(vec![ctx.target]) } else { Action::CloseWindows(windows) };
    v.push(Entry::item(format!("Quit {name}"), quit).shortcut(Shortcut::new("c", ch(b'Q'), "Q")));
    v
}

pub fn title_menu(index: usize, ctx: &MenuCtx) -> Vec<Entry> {
    match MENU_TITLES.get(index).copied() {
        Some("File") => vec![
            Entry::keys("New Window", Shortcut::new("c", ch(b'N'), "N")),
            Entry::keys("New Tab", Shortcut::new("c", ch(b'T'), "T")),
            Entry::keys("Open…", Shortcut::new("c", ch(b'O'), "O")),
            Entry::Separator,
            Entry::keys("Close Window", Shortcut::new("c", ch(b'W'), "W")),
            Entry::keys("Save", Shortcut::new("c", ch(b'S'), "S")),
            Entry::keys("Save As…", Shortcut::new("cs", ch(b'S'), "S")),
            Entry::Separator,
            Entry::keys("Print…", Shortcut::new("c", ch(b'P'), "P")),
        ],
        Some("Edit") => vec![
            Entry::keys("Undo", Shortcut::new("c", ch(b'Z'), "Z")),
            Entry::keys("Redo", Shortcut::new("c", ch(b'Y'), "Y")),
            Entry::Separator,
            Entry::keys("Cut", Shortcut::new("c", ch(b'X'), "X")),
            Entry::keys("Copy", Shortcut::new("c", ch(b'C'), "C")),
            Entry::keys("Paste", Shortcut::new("c", ch(b'V'), "V")),
            Entry::keys("Select All", Shortcut::new("c", ch(b'A'), "A")),
            Entry::Separator,
            Entry::keys("Find…", Shortcut::new("c", ch(b'F'), "F")),
            Entry::Separator,
            Entry::keys("Emoji & Symbols", Shortcut::new("w", VK_PERIOD, ".")),
        ],
        Some("View") => vec![
            Entry::keys("Reload", Shortcut::new("c", ch(b'R'), "R")),
            Entry::Separator,
            Entry::keys("Actual Size", Shortcut::new("c", ch(b'0'), "0")),
            Entry::keys("Zoom In", Shortcut::new("c", VK_PLUS, "+")),
            Entry::keys("Zoom Out", Shortcut::new("c", VK_MINUS, "-")),
            Entry::Separator,
            Entry::keys("Enter Full Screen", Shortcut::new("", VK_F11, "F11")),
        ],
        Some("Window") => {
            let mut v = vec![
                Entry::item("Minimize", Action::Minimize(ctx.target)).shortcut(Shortcut::new("c", ch(b'M'), "M")),
                Entry::item("Zoom", Action::Zoom(ctx.target)),
                Entry::Separator,
                Entry::keys("Tile Window to Left of Screen", Shortcut::new("w", VK_LEFT, "←")),
                Entry::keys("Tile Window to Right of Screen", Shortcut::new("w", VK_RIGHT, "→")),
                Entry::Separator,
                Entry::item(
                    "Bring All to Front",
                    Action::BringToFront(ctx.app_windows.iter().map(|w| w.hwnd).collect()),
                ),
            ];
            if !ctx.app_windows.is_empty() {
                v.push(Entry::Separator);
                for w in ctx.app_windows {
                    v.push(Entry::item(w.title.clone(), Action::Activate(w.hwnd)).checked(w.hwnd == ctx.target));
                }
            }
            v
        }
        Some("Help") => vec![
            Entry::keys(format!("{} Help", ctx.app_name), Shortcut::new("", VK_F1, "F1")),
            Entry::Separator,
            Entry::keys("Search", Shortcut::new("w", ch(b'S'), "S")),
        ],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Bounds = Bounds { left: 4.0, top: 34.0, right: 1916.0, bottom: 1036.0 };

    #[test]
    fn context_menus_open_down_and_right_where_they_fit() {
        assert_eq!(place(Placement::Below { x: 500.0, y: 300.0 }, 200.0, 400.0, AREA, true), (500.0, 300.0));
    }

    #[test]
    fn context_menus_flip_up_near_the_bottom() {
        // 400 tall at y = 900 would end at 1300: it opens upward, ending at the pointer.
        assert_eq!(place(Placement::Below { x: 500.0, y: 900.0 }, 200.0, 400.0, AREA, true), (500.0, 500.0));
    }

    #[test]
    fn context_menus_flip_left_near_the_right_edge() {
        assert_eq!(place(Placement::Below { x: 1850.0, y: 300.0 }, 200.0, 100.0, AREA, true), (1650.0, 300.0));
    }

    #[test]
    fn menus_that_fit_neither_way_slide_inside() {
        // 900 tall at y = 500: no room below (ends 1400) or above (starts -400): pinned to the bottom.
        assert_eq!(place(Placement::Below { x: 500.0, y: 500.0 }, 200.0, 900.0, AREA, true), (500.0, 136.0));
    }

    #[test]
    fn menu_bar_menus_stay_on_the_bar() {
        // A menu bar menu opens right under the bar even where that's above the work area's edge.
        assert_eq!(place(Placement::Below { x: 10.0, y: 30.0 }, 200.0, 300.0, AREA, false), (10.0, 30.0));
    }

    #[test]
    fn dock_menus_stay_on_the_dock() {
        let (_, y) = place(Placement::Above { cx: 960.0, bottom: 1040.0 }, 200.0, 300.0, AREA, false);
        assert_eq!(y, 740.0);
    }

    #[test]
    fn submenus_open_left_without_room_and_shift_up_near_the_bottom() {
        let (x, y) = place(Placement::Beside { left: 1700.0, right: 1900.0, top: 900.0 }, 200.0, 300.0, AREA, true);
        assert_eq!((x, y), (1500.0, 736.0));
    }
}
