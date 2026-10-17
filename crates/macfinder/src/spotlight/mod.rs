//! Spotlight: a floating search panel (Alt+Space by default) for apps, files,
//! folders, quick arithmetic and a web search fallback.

pub mod calc;
pub mod index;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicIsize;

use windows::core::Result;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

use self::index::{Index, Item, Kind};
use crate::chrome::SHADOW;
use crate::config::Config;
use crate::finder::loader::IconLoader;
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::surface::Surface;
use crate::theme::Theme;
use crate::win::Monitor;

/// The panel's content window, so the window procedure can close it when it loses focus.
pub static SPOTLIGHT_HWND: AtomicIsize = AtomicIsize::new(0);

const W: f32 = 680.0;
const SEARCH_H: f32 = 54.0;
const ROW_H: f32 = 36.0;
const HEADER_H: f32 = 24.0;
const RADIUS: f32 = 14.0;
const MAX_APPS: usize = 5;
const MAX_FILES: usize = 7;
/// Tallest the panel can get (search field + 12 rows + 4 headers + padding).
const MAX_H: f32 = SEARCH_H + 12.0 * ROW_H + 4.0 * HEADER_H + 24.0;
const ICON_PX: i32 = 64;

#[derive(Clone, Debug)]
enum RowAction {
    Launch(String),
    OpenFile(String),
    OpenFolder(String),
    Copy(String),
    Url(String),
}

#[derive(Clone, Debug)]
struct Row {
    section: &'static str,
    title: String,
    subtitle: String,
    action: RowAction,
    /// Icon loader key and source path.
    icon: Option<(String, String)>,
}

pub enum SpotlightEvent {
    None,
    Close,
    Run(Action),
}

pub struct Spotlight {
    pub surface: Surface,
    text: Vec<char>,
    caret: usize,
    anchor: usize,
    rows: Vec<Row>,
    selected: usize,
    hits: Vec<(Rect, usize)>,
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    pub tracking: bool,
    pub dirty: bool,
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

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Text from the clipboard (for Ctrl+V).
fn clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    unsafe {
        OpenClipboard(None).ok()?;
        let text = GetClipboardData(CF_UNICODETEXT.0 as u32).ok().and_then(|h| {
            let mem = HGLOBAL(h.0);
            let ptr = GlobalLock(mem) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(mem);
            Some(s)
        });
        let _ = CloseClipboard();
        text
    }
}

impl Spotlight {
    pub fn new(gfx: &Gfx, theme: &Theme, cfg: &Config, monitor: &Monitor) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((MAX_H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + ((mon.bottom - mon.top) as f32 * 0.2) as i32 - (SHADOW * s) as i32;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let surface = Surface::new(gfx, rect, s, cfg.menubar.backdrop, theme.menu_tint)?;
        SPOTLIGHT_HWND.store(surface.content.0 as isize, std::sync::atomic::Ordering::Relaxed);
        Ok(Self {
            surface,
            text: Vec::new(),
            caret: 0,
            anchor: 0,
            rows: Vec::new(),
            selected: 0,
            hits: Vec::new(),
            bitmaps: HashMap::new(),
            tracking: false,
            dirty: true,
        })
    }

    /// Gives the panel keyboard focus (allowed: we're handling a hotkey or click).
    pub fn focus(&self) {
        unsafe {
            let _ = SetForegroundWindow(self.surface.content);
        }
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

    /// Recomputes results for the current text.
    pub fn search(&mut self, index: &Index) {
        self.dirty = true;
        self.selected = 0;
        self.rows.clear();
        let raw = self.query();
        let q = raw.trim().to_lowercase();
        if q.is_empty() {
            return;
        }

        let top_n = |items: &[Item], n: usize| -> Vec<(i32, Item)> {
            let mut scored: Vec<(i32, &Item)> = items.iter().filter_map(|it| index::score(it, &q).map(|s| (s, it))).collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.len().cmp(&b.1.name.len())));
            scored.into_iter().take(n).map(|(s, it)| (s, it.clone())).collect()
        };
        let mut apps = top_n(&index.apps, MAX_APPS + 1);
        let mut files = top_n(&index.files, MAX_FILES + 1);

        let row = |section: &'static str, it: &Item| {
            let (subtitle, action) = match it.kind {
                Kind::App => ("Application".to_string(), RowAction::Launch(it.target.clone())),
                Kind::Folder => (it.target.clone(), RowAction::OpenFolder(it.target.clone())),
                Kind::File => (
                    Path::new(&it.target).parent().map(|p| p.display().to_string()).unwrap_or_default(),
                    RowAction::OpenFile(it.target.clone()),
                ),
            };
            Row { section, title: it.name.clone(), subtitle, action, icon: Some(icon_for(it)) }
        };

        if let Some(v) = calc::eval(&raw) {
            let result = calc::format(v);
            self.rows.push(Row {
                section: "Calculator",
                title: format!("= {result}"),
                subtitle: "Press Enter to copy".into(),
                action: RowAction::Copy(result),
                icon: None,
            });
        }

        // Top Hit: a strong app match, else a near-exact file match.
        let best_app = apps.first().map(|a| a.0).unwrap_or(i32::MIN);
        let best_file = files.first().map(|f| f.0).unwrap_or(i32::MIN);
        if best_app >= 700 && best_app >= best_file - 100 {
            let (_, it) = apps.remove(0);
            self.rows.push(row("Top Hit", &it));
        } else if best_file >= 900 {
            let (_, it) = files.remove(0);
            self.rows.push(row("Top Hit", &it));
        }
        for (_, it) in apps.iter().take(MAX_APPS) {
            self.rows.push(row("Applications", it));
        }
        for (_, it) in files.iter().take(MAX_FILES) {
            self.rows.push(row("Folders & Documents", it));
        }
        self.rows.push(Row {
            section: "Web",
            title: format!("Search the web for \u{201C}{}\u{201D}", raw.trim()),
            subtitle: String::new(),
            action: RowAction::Url(format!("https://www.google.com/search?q={}", url_encode(raw.trim()))),
            icon: None,
        });
    }

    fn run(&self, i: usize, reveal: bool) -> SpotlightEvent {
        let Some(row) = self.rows.get(i) else { return SpotlightEvent::None };
        let action = match &row.action {
            RowAction::OpenFile(p) | RowAction::OpenFolder(p) if reveal => {
                Action::OpenFinder(Path::new(p).parent().map(Path::to_path_buf))
            }
            RowAction::Launch(t) => Action::Launch { path: t.clone(), args: None },
            RowAction::OpenFile(p) => Action::Open(p.clone()),
            RowAction::OpenFolder(p) => Action::OpenFinder(Some(PathBuf::from(p))),
            RowAction::Url(u) => Action::Open(u.clone()),
            RowAction::Copy(text) => {
                crate::finder::fs::copy_text(text);
                return SpotlightEvent::Close;
            }
        };
        SpotlightEvent::Run(action)
    }

    pub fn on_char(&mut self, c: char, index: &Index) {
        if c.is_control() {
            return;
        }
        self.insert(&c.to_string());
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
        match vk {
            ESC if !self.text.is_empty() => {
                self.text.clear();
                self.caret = 0;
                self.anchor = 0;
                self.search(index);
            }
            ESC => return SpotlightEvent::Close,
            ENTER => return self.run(self.selected, ctrl),
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
                if let Some(t) = clipboard_text() {
                    self.insert(t.lines().next().unwrap_or(""));
                    self.search(index);
                }
            }
            _ => {}
        }
        SpotlightEvent::None
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        if let Some(&(_, i)) = self.hits.iter().find(|(r, _)| r.contains(x, y)) {
            if i != self.selected {
                self.selected = i;
                self.dirty = true;
            }
        }
    }

    pub fn on_click(&mut self, x: f32, y: f32, ctrl: bool) -> SpotlightEvent {
        match self.hits.iter().find(|(r, _)| r.contains(x, y)) {
            Some(&(_, i)) => self.run(i, ctrl),
            None => SpotlightEvent::None,
        }
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme, loader: &mut IconLoader) {
        self.dirty = false;
        let headers = {
            let mut n = 0;
            let mut last = "";
            for r in &self.rows {
                if r.section != last {
                    n += 1;
                    last = r.section;
                }
            }
            n
        };
        let list_h = if self.rows.is_empty() { 0.0 } else { 8.0 + self.rows.len() as f32 * ROW_H + headers as f32 * HEADER_H + 8.0 };
        let body = Rect::new(SHADOW, SHADOW, W, (SEARCH_H + list_h).min(MAX_H));
        self.surface.set_backdrop_shape(body, RADIUS);

        let big = gfx.format(&theme.font, 22.0, Weight::Regular, Align::Leading);
        let title_font = gfx.format(&theme.font, 14.0, Weight::Regular, Align::Leading);
        let sub_font = gfx.format(&theme.font, 12.0, Weight::Regular, Align::Trailing);
        let header_font = gfx.format(&theme.font, 11.0, Weight::Bold, Align::Leading);
        let has_backdrop = self.surface.backdrop.is_some();

        let query = self.query();
        let prefix_w = |n: usize| gfx.measure(&self.text[..n].iter().collect::<String>(), &big);
        let (a, b) = self.range();
        let (sel_x0, sel_x1, caret_x) = (prefix_w(a), prefix_w(b), prefix_w(self.caret));

        let (rows, selected) = (&self.rows, self.selected);
        let bitmaps = &mut self.bitmaps;
        let mut hits = Vec::new();

        let r = self.surface.render(gfx, |p: &Painter| {
            // Shadow, then the tinted body over the blurred backdrop.
            for i in 0..12 {
                let grow = i as f32 * 2.0;
                let alpha = 0.03 * (12 - i) as f32 / 12.0;
                p.fill_round(body.inset(-grow, -grow).offset(0.0, 6.0), RADIUS + grow, theme.shadow.alpha(alpha));
            }
            p.clear_rect(body.inset(3.0, 3.0));
            p.fill_round(body, RADIUS, if has_backdrop { theme.menu_tint.alpha(0.3) } else { theme.menu_tint.alpha(1.0) });
            p.stroke_round(body.inset(0.25, 0.25), RADIUS, theme.menu_border, 0.5);

            // Search field.
            let (gx, gy) = (body.x + 26.0, body.y + SEARCH_H / 2.0 - 1.5);
            p.stroke_circle(gx, gy, 8.0, theme.text_secondary, 2.0);
            p.line(gx + 6.0, gy + 6.0, gx + 11.0, gy + 11.0, theme.text_secondary, 2.4);
            let field = Rect::new(body.x + 52.0, body.y, W - 70.0, SEARCH_H);
            if query.is_empty() {
                p.text("Spotlight Search", &big, field, theme.text_disabled);
            } else if a != b {
                p.fill_rect(Rect::new(field.x + sel_x0, field.cy() - 14.0, sel_x1 - sel_x0, 28.0), theme.accent.alpha(0.35));
            }
            p.text(&query, &big, field, theme.text);
            if a == b {
                let cx = field.x + caret_x + 0.5;
                p.line(cx, field.cy() - 13.0, cx, field.cy() + 13.0, theme.accent, 1.5);
            }

            if rows.is_empty() {
                return;
            }
            p.line(body.x, body.y + SEARCH_H, body.right(), body.y + SEARCH_H, theme.menu_separator, 1.0);

            // Results.
            let mut y = body.y + SEARCH_H + 8.0;
            let mut last = "";
            for (i, row) in rows.iter().enumerate() {
                if y + ROW_H > body.bottom() {
                    break;
                }
                if row.section != last {
                    last = row.section;
                    p.text(row.section, &header_font, Rect::new(body.x + 18.0, y, W - 36.0, HEADER_H), theme.text_secondary);
                    y += HEADER_H;
                }
                let r = Rect::new(body.x + 8.0, y, W - 16.0, ROW_H);
                let sel = i == selected;
                if sel {
                    p.fill_round(r, 7.0, theme.accent);
                }
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
                    None if row.section == "Calculator" => {
                        p.fill_round(icon_r.inset(1.0, 1.0), 5.0, Color::rgba(255, 149, 0, 1.0));
                        p.line(icon_r.cx() - 5.0, icon_r.cy() - 2.5, icon_r.cx() + 5.0, icon_r.cy() - 2.5, Color::WHITE, 1.8);
                        p.line(icon_r.cx() - 5.0, icon_r.cy() + 2.5, icon_r.cx() + 5.0, icon_r.cy() + 2.5, Color::WHITE, 1.8);
                    }
                    None => {
                        // Web: a globe.
                        let (cx, cy) = (icon_r.cx(), icon_r.cy());
                        p.stroke_circle(cx, cy, 10.0, if sel { Color::WHITE } else { theme.accent }, 1.6);
                        p.line(cx - 10.0, cy, cx + 10.0, cy, if sel { Color::WHITE } else { theme.accent }, 1.2);
                        p.line(cx, cy - 10.0, cx, cy + 10.0, if sel { Color::WHITE } else { theme.accent }, 1.2);
                    }
                }
                let title_w = if row.subtitle.is_empty() { r.w - 60.0 } else { r.w * 0.55 };
                p.text(&row.title, &title_font, Rect::new(icon_r.right() + 12.0, r.y, title_w, r.h), tc);
                if !row.subtitle.is_empty() {
                    let sub = Rect::new(icon_r.right() + 12.0 + title_w + 8.0, r.y, r.right() - 12.0 - (icon_r.right() + 20.0 + title_w), r.h);
                    p.text(&row.subtitle, &sub_font, sub, sc);
                }
                hits.push((r, i));
                y += ROW_H;
            }
        });
        self.hits = hits;
        if let Err(e) = r {
            log!("spotlight render failed: {e}");
        }
    }
}

impl Drop for Spotlight {
    fn drop(&mut self) {
        SPOTLIGHT_HWND.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}
