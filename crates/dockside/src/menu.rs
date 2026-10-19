//! macOS-style popup menus (used by the menu bar and Dock context menus) and
//! the actions their items trigger.

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};

use crate::geom::{Color, Rect};
use crate::gfx::{Align, Gfx, Weight};
use crate::surface::Surface;
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
    /// File picker, then pin the chosen app.
    AddDockApp,
    OpenSettings(Option<crate::settings::Page>),
    /// Open a Finder window (at a folder, or Home).
    OpenFinder(Option<std::path::PathBuf>),
    /// Show or hide the Spotlight search panel.
    ToggleSpotlight,
    /// A command for one Finder window (from its context menu).
    Finder { window: u32, cmd: crate::finder::FinderCmd },
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
    Quit,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<Shortcut>,
    pub action: Option<Action>,
    pub checked: bool,
}

#[derive(Clone, Debug)]
pub enum Entry {
    Item(MenuItem),
    Separator,
}

impl Entry {
    pub fn item(label: impl Into<String>, action: Action) -> Entry {
        Entry::Item(MenuItem { label: label.into(), shortcut: None, action: Some(action), checked: false })
    }

    /// An item that sends `sc` to the focused app.
    pub fn keys(label: impl Into<String>, sc: Shortcut) -> Entry {
        Entry::Item(MenuItem { label: label.into(), shortcut: Some(sc), action: Some(Action::Keys(sc)), checked: false })
    }

    pub fn disabled(label: impl Into<String>) -> Entry {
        Entry::Item(MenuItem { label: label.into(), shortcut: None, action: None, checked: false })
    }

    pub fn shortcut(mut self, sc: Shortcut) -> Entry {
        if let Entry::Item(i) = &mut self {
            i.shortcut = Some(sc);
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
        matches!(self, Entry::Item(MenuItem { action: Some(_), .. }))
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
}

pub enum MenuEvent {
    None,
    Activate(Action),
    Close,
    /// Left/Right arrow: move to the neighbouring menu-bar menu.
    Step(i32),
}

const SHADOW: f32 = 20.0;
const ITEM_H: f32 = 22.0;
const SEP_H: f32 = 11.0;
const PAD_Y: f32 = 5.0;
const RADIUS: f32 = 7.0;
const FONT_SIZE: f32 = 13.0;

pub struct PopupMenu {
    pub surface: Surface,
    pub source: MenuSource,
    entries: Vec<Entry>,
    rects: Vec<Rect>,
    body: Rect,
    hovered: Option<usize>,
    mac: bool,
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
                label_w = label_w.max(gfx.measure(&i.label, &font));
                if let Some(sc) = &i.shortcut {
                    short_w = short_w.max(gfx.measure(&sc.label(mac), &font));
                }
            }
        }
        let width = (22.0 + label_w + if short_w > 0.0 { 32.0 + short_w } else { 0.0 } + 18.0).clamp(160.0, 420.0);
        let height = PAD_Y * 2.0
            + entries.iter().map(|e| if matches!(e, Entry::Separator) { SEP_H } else { ITEM_H }).sum::<f32>();

        // Body position in screen points, clamped to the monitor.
        let s = monitor.scale;
        let mon_l = monitor.rect.left as f32 / s;
        let mon_r = monitor.rect.right as f32 / s;
        let (mut x, y) = match placement {
            Placement::Below { x, y } => (x, y),
            Placement::Above { cx, bottom } => (cx - width / 2.0, bottom - height),
        };
        x = x.clamp(mon_l + 4.0, (mon_r - width - 4.0).max(mon_l));

        let rect = RECT {
            left: ((x - SHADOW) * s).round() as i32,
            top: ((y - SHADOW) * s).round() as i32,
            right: ((x + width + SHADOW) * s).round() as i32,
            bottom: ((y + height + SHADOW) * s).round() as i32,
        };
        let tint = theme.menu_tint;
        let mut surface = Surface::new(gfx, rect, s, cfg.menubar.backdrop, tint)?;
        let body = Rect::new(SHADOW, SHADOW, width, height);
        surface.set_backdrop_shape(body, RADIUS);

        let mut rects = Vec::with_capacity(entries.len());
        let mut cy = body.y + PAD_Y;
        for e in &entries {
            let h = if matches!(e, Entry::Separator) { SEP_H } else { ITEM_H };
            rects.push(Rect::new(body.x, cy, body.w, h));
            cy += h;
        }

        Ok(Self { surface, source, entries, rects, body, hovered: None, mac })
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        let font = gfx.format(&theme.font, FONT_SIZE, Weight::Regular, Align::Leading);
        let font_right = gfx.format(&theme.font, FONT_SIZE, Weight::Regular, Align::Trailing);
        let has_backdrop = self.surface.backdrop.is_some();
        let (body, entries, rects, hovered, mac) = (self.body, &self.entries, &self.rects, self.hovered, self.mac);

        let r = self.surface.render(gfx, |p| {
            // Soft drop shadow, layered rounded rects.
            for i in 0..10 {
                let grow = i as f32 * 1.6;
                let a = 0.022 * (10 - i) as f32 / 10.0;
                p.fill_round(body.inset(-grow, -grow).offset(0.0, 4.0), RADIUS + grow, theme.shadow.alpha(a));
            }
            // Keep the shadow from darkening the blurred backdrop under the body.
            p.clear_rect(body.inset(2.0, 2.0));
            p.fill_round(body, RADIUS, if has_backdrop { theme.menu_tint.alpha(0.35) } else { theme.menu_tint.alpha(1.0) });
            p.stroke_round(body.inset(0.25, 0.25), RADIUS, theme.menu_border, 0.5);

            for (i, (e, r)) in entries.iter().zip(rects).enumerate() {
                match e {
                    Entry::Separator => {
                        let y = r.cy();
                        p.line(r.x + 10.0, y, r.right() - 10.0, y, theme.menu_separator, 1.0);
                    }
                    Entry::Item(item) => {
                        let enabled = item.action.is_some();
                        let selected = hovered == Some(i) && enabled;
                        if selected {
                            p.fill_round(r.inset(5.0, 0.0), 4.0, theme.accent);
                        }
                        let color = if selected {
                            Color::WHITE
                        } else if enabled {
                            theme.text
                        } else {
                            theme.text_disabled
                        };
                        if item.checked {
                            p.text("✓", &font, Rect::new(r.x + 8.0, r.y, 14.0, r.h), color);
                        }
                        p.text(&item.label, &font, Rect::new(r.x + 22.0, r.y, r.w - 40.0, r.h), color);
                        if let Some(sc) = &item.shortcut {
                            let c = if selected { Color::WHITE } else { theme.text_secondary };
                            p.text(&sc.label(mac), &font_right, Rect::new(r.x, r.y, r.w - 16.0, r.h), c);
                        }
                    }
                }
            }
        });
        if let Err(e) = r {
            log!("menu render failed: {e}");
        }
    }

    fn index_at(&self, x: f32, y: f32) -> Option<usize> {
        self.rects.iter().position(|r| r.contains(x, y)).filter(|&i| self.entries[i].enabled())
    }

    /// Returns true when the highlight changed.
    pub fn on_move(&mut self, x: f32, y: f32) -> bool {
        let h = self.index_at(x, y);
        let changed = h != self.hovered;
        self.hovered = h;
        changed
    }

    pub fn on_leave(&mut self) -> bool {
        let changed = self.hovered.is_some();
        self.hovered = None;
        changed
    }

    pub fn on_click(&mut self, x: f32, y: f32) -> MenuEvent {
        if !self.body.contains(x, y) {
            return MenuEvent::Close;
        }
        match self.index_at(x, y).map(|i| &self.entries[i]) {
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
        Entry::item("About This PC", Action::Open("ms-settings:about".into())),
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
