//! The focused app's own menu bar, for the menu bar to show in place of the
//! generic File/Edit/View menus.
//!
//! Classic Win32 menus are read straight from the window's menu handle (no
//! flash, well under a millisecond, and nothing to hang on). Other menus (XAML,
//! WPF, Qt) are read through UI Automation, expanding a menu only when it's
//! opened. Everything runs on one worker thread: reads cross into the app, and
//! a hung app must never stall DockSide's UI thread. Elements never leave the
//! worker; the UI thread gets plain [`Item`]s and names a menu by its [`Path`].

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, Result, BSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetMenu, GetMenuItemCount, GetMenuItemInfoW, GetWindowThreadProcessId, IsHungAppWindow, IsMenu, IsWindow,
    PostMessageW, SendMessageTimeoutW, HMENU, MENUITEMINFOW, MFS_CHECKED, MFS_DISABLED, MFT_SEPARATOR, MIIM_FTYPE,
    MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_COMMAND, WM_INITMENU,
    WM_INITMENUPOPUP, WM_NULL, WM_UNINITMENUPOPUP,
};

/// How long UIA waits to reach an app, and for one call into it, before giving up.
const CONNECT_MS: u32 = 800;
const TRANSACT_MS: u32 = 1000;
/// How long a window message to the app may take (a hung app fails it at once).
const SEND_MS: u32 = 250;
/// A bar nested inside the window (XAML islands) is looked for this many
/// levels down, over at most this many elements.
const BAR_DEPTH: usize = 5;
const BAR_NODES: usize = 150;
/// How long an opened menu's items may take to show up.
const EXPAND_WAIT: Duration = Duration::from_millis(600);
/// Chromium apps that draw a menu bar in their page (VS Code's title bar,
/// GitHub Desktop): read live. Other Chromium windows are never asked, since
/// asking switches on their accessibility tree (CPU and memory in the app).
const LIVE_WEB: [&str; 6] = ["code.exe", "code - insiders.exe", "cursor.exe", "windsurf.exe", "vscodium.exe", "githubdesktop.exe"];
/// How deep, and over how many elements, a page is searched for its menu bar
/// (depth first: a title bar comes early in the page).
const WEB_DEPTH: usize = 16;
const WEB_NODES: usize = 400;
/// A window found without a menu bar isn't searched again for this long.
const MISS_TTL: Duration = Duration::from_secs(120);

/// Window classes never searched: Office ribbons (read as apps without menus,
/// the user's choice), Chromium (asking would switch on its accessibility
/// tree, costing CPU inside the browser) and File Explorer (no menu bar).
const SKIP_CLASSES: [&str; 8] =
    ["OpusApp", "XLMAIN", "PPTFrameClass", "rctrl_renwnd32", "Framework::CFrame", "MSWinPub", "CabinetWClass", "Chrome_WidgetWin_1"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Command,
    Submenu,
    Separator,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Where it is in its menu (the step a [`Path`] takes to it).
    pub index: usize,
    pub name: String,
    /// The shortcut as the app spells it ("Ctrl+S"), or empty.
    pub accel: String,
    pub enabled: bool,
    pub checked: bool,
    pub kind: Kind,
}

/// A menu item, by the steps from the bar to it: each step's index and name
/// (checked before acting, so a menu that changed is never acted on blindly).
pub type Path = Vec<(usize, String)>;

pub enum Reply {
    /// A window's menu titles; `None` when it has no menu that can be read.
    Bar { hwnd: isize, titles: Option<Vec<Item>> },
    /// A menu's items (for request `gen`); `None` when it couldn't be read.
    Menu { gen: u64, items: Option<Vec<Item>> },
}

enum Req {
    Bar(isize, bool),
    Menu { gen: u64, hwnd: isize, path: Path },
    Invoke { hwnd: isize, path: Path },
}

/// The UI thread's handle on the worker. Replies are collected and the
/// `notify` window is posted `msg` when there are new ones.
pub struct AppMenus {
    tx: Sender<Req>,
    replies: Arc<Mutex<Vec<Reply>>>,
}

impl AppMenus {
    pub fn start(notify: HWND, msg: u32) -> Self {
        let (tx, rx) = channel();
        let replies = Arc::new(Mutex::new(Vec::new()));
        let out = replies.clone();
        let notify = notify.0 as isize;
        let spawned = std::thread::Builder::new().name("app-menus".into()).spawn(move || worker(rx, out, notify, msg));
        if let Err(e) = spawned {
            log!("app menus: no worker: {e}");
        }
        Self { tx, replies }
    }

    /// Reads `hwnd`'s menu titles (answered with [`Reply::Bar`]). `again`:
    /// look even where none were found lately (a window that just opened may
    /// not have built its menu bar yet).
    pub fn read_bar(&self, hwnd: HWND, again: bool) {
        let _ = self.tx.send(Req::Bar(hwnd.0 as isize, again));
    }

    /// Reads the items of the menu at `path` (answered with [`Reply::Menu`]).
    pub fn read_menu(&self, gen: u64, hwnd: HWND, path: Path) {
        let _ = self.tx.send(Req::Menu { gen, hwnd: hwnd.0 as isize, path });
    }

    /// Chooses the item at `path` in the app.
    pub fn invoke(&self, hwnd: HWND, path: Path) {
        let _ = self.tx.send(Req::Invoke { hwnd: hwnd.0 as isize, path });
    }

    pub fn take(&self) -> Vec<Reply> {
        self.replies.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

enum Src {
    Hmenu,
    /// A Java app's menus, through Java Access Bridge.
    Jab(&'static super::jab::Bridge),
    /// The bar's title elements.
    Uia(Vec<IUIAutomationElement>),
}

struct Target {
    hwnd: isize,
    src: Src,
    titles: Vec<Item>,
    at: Instant,
}

struct Worker {
    uia: Option<Uia>,
    cur: Option<Target>,
    /// Windows found without a menu bar, and when.
    misses: HashMap<isize, Instant>,
}

fn worker(rx: Receiver<Req>, out: Arc<Mutex<Vec<Reply>>>, notify: isize, msg: u32) {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut w = Worker { uia: None, cur: None, misses: HashMap::new() };
    let reply = |r: Reply| {
        if let Ok(mut v) = out.lock() {
            v.push(r);
        }
        unsafe {
            let _ = PostMessageW(HWND(notify as _), msg, WPARAM(0), LPARAM(0));
        }
    };
    while let Ok(first) = rx.recv() {
        let mut reqs = vec![first];
        reqs.extend(rx.try_iter());
        // Only the newest bar read and menu read still matter; choices all run, in order.
        let bar = reqs.iter().rev().find_map(|r| if let Req::Bar(h, again) = r { Some((*h, *again)) } else { None });
        let menu = reqs.iter().rposition(|r| matches!(r, Req::Menu { .. }));
        if let Some((h, again)) = bar {
            let t = Instant::now();
            if again {
                w.misses.remove(&h);
            }
            let titles = w.target(h, true).map(|t| t.titles.clone());
            slow("bar", t);
            reply(Reply::Bar { hwnd: h, titles });
        }
        for (i, r) in reqs.iter().enumerate() {
            match r {
                Req::Invoke { hwnd, path } => {
                    let t = Instant::now();
                    if !w.invoke(*hwnd, path) {
                        log!("app menus: could not choose {:?}", path.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>());
                    }
                    slow("invoke", t);
                }
                Req::Menu { gen, hwnd, path } if Some(i) == menu => {
                    let t = Instant::now();
                    let items = w.menu(*hwnd, path);
                    slow("menu", t);
                    reply(Reply::Menu { gen: *gen, items });
                }
                _ => {}
            }
        }
    }
}

/// Logs a read that took long enough to notice.
fn slow(what: &str, t: Instant) {
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    if ms > 150.0 {
        log!("app menus: {what} took {ms:.0} ms");
    }
}

/// Whether the app answers messages (a hung one fails at once).
fn responsive(h: HWND) -> bool {
    unsafe {
        IsWindow(h).as_bool()
            && !IsHungAppWindow(h).as_bool()
            && SendMessageTimeoutW(h, WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG | SMTO_BLOCK, SEND_MS, None).0 != 0
    }
}

fn send(h: HWND, msg: u32, wp: usize, lp: isize) {
    unsafe {
        let _ = SendMessageTimeoutW(h, msg, WPARAM(wp), LPARAM(lp), SMTO_ABORTIFHUNG | SMTO_BLOCK, SEND_MS, None);
    }
}

impl Worker {
    /// What's known of `hwnd`'s menu bar (`fresh`: read again unless just read).
    fn target(&mut self, hwnd: isize, fresh: bool) -> Option<&Target> {
        let reuse = self.cur.as_ref().is_some_and(|t| t.hwnd == hwnd && (!fresh || t.at.elapsed() < Duration::from_secs(3)));
        if !reuse {
            self.cur = self.read(hwnd);
        }
        self.cur.as_ref()
    }

    fn read(&mut self, hwnd: isize) -> Option<Target> {
        let h = HWND(hwnd as _);
        if self.misses.get(&hwnd).is_some_and(|t| t.elapsed() < MISS_TTL) || !responsive(h) {
            return None;
        }
        self.misses.retain(|_, t| t.elapsed() < MISS_TTL);
        // A classic menu: right there, no UIA needed.
        if let Some(bar) = hmenu_bar(h) {
            let titles: Vec<Item> = hmenu_items(bar).into_iter().filter(|i| i.kind != Kind::Separator).collect();
            if !titles.is_empty() {
                return Some(Target { hwnd, src: Src::Hmenu, titles, at: Instant::now() });
            }
        }
        let class = super::class_name(h);
        // Java (Swing) windows: only Java Access Bridge can see their menus.
        if class == "SunAwtFrame" || class == "SunAwtDialog" {
            let exe = super::apps::process_path(super::apps::effective_pid(h))?;
            let bridge = super::jab::bridge_for(&exe)?;
            let titles: Vec<Item> = bridge.titles(h)?.into_iter().filter(|i| i.kind != Kind::Separator).collect();
            return (!titles.is_empty()).then(|| Target { hwnd, src: Src::Jab(bridge), titles, at: Instant::now() });
        }
        let web = class.starts_with("Chrome_WidgetWin")
            && super::apps::process_path(super::apps::effective_pid(h))
                .is_some_and(|exe| LIVE_WEB.contains(&super::apps::exe_name(&exe).as_str()));
        if !web && SKIP_CLASSES.contains(&class.as_str()) {
            return None;
        }
        if self.uia.is_none() {
            self.uia = Uia::new().map_err(|e| log!("app menus: UIA unavailable: {e}")).ok();
        }
        let uia = self.uia.as_ref()?;
        let bar = if web {
            unsafe { uia.a.ElementFromHandle(h) }.ok().and_then(|root| uia.find_deep(&root, UIA_MenuBarControlTypeId, WEB_DEPTH, WEB_NODES))
        } else {
            uia.find_bar(h).ok().flatten()
        };
        let found = bar.and_then(|bar| uia.children(&bar).ok());
        let Some(elems) = found.filter(|e| !e.is_empty()) else {
            self.misses.insert(hwnd, Instant::now());
            return None;
        };
        let titles: Vec<Item> =
            elems.iter().enumerate().map(|(i, e)| Uia::item(i, e)).filter(|i| i.kind != Kind::Separator && !i.name.is_empty()).collect();
        if titles.is_empty() {
            self.misses.insert(hwnd, Instant::now());
            return None;
        }
        Some(Target { hwnd, src: Src::Uia(elems), titles, at: Instant::now() })
    }

    /// The items of the menu at `path`.
    fn menu(&mut self, hwnd: isize, path: &Path) -> Option<Vec<Item>> {
        let h = HWND(hwnd as _);
        if !responsive(h) {
            return None;
        }
        self.target(hwnd, false)?;
        let t = self.cur.as_ref()?;
        match &t.src {
            Src::Hmenu => {
                let chain = hmenu_open(h, path)?;
                let items = hmenu_items(*chain.last()?);
                hmenu_close(h, &chain);
                Some(items)
            }
            Src::Jab(bridge) => bridge.menu(h, path.clone()),
            Src::Uia(titles) => {
                let uia = self.uia.as_ref()?;
                let (opened, kids) = uia.open_path(h, titles, path)?;
                let items = kids.iter().enumerate().map(|(i, e)| Uia::item(i, e)).collect();
                uia.collapse_all(&opened);
                Some(items)
            }
        }
    }

    /// Chooses the item at `path`; false when it's gone, changed or disabled.
    fn invoke(&mut self, hwnd: isize, path: &Path) -> bool {
        let h = HWND(hwnd as _);
        let Some(((last, name), parent)) = path.split_last() else { return false };
        if !responsive(h) {
            return false;
        }
        if self.target(hwnd, false).is_none() {
            return false;
        }
        let Some(t) = self.cur.as_ref() else { return false };
        match &t.src {
            Src::Hmenu => {
                let parent: Path = parent.to_vec();
                let Some(chain) = hmenu_open(h, &parent) else { return false };
                let Some(&menu) = chain.last() else { return false };
                let id = hmenu_items(menu)
                    .into_iter()
                    .find(|i| i.index == *last && &i.name == name && i.enabled && i.kind == Kind::Command)
                    .and_then(|_| hmenu_id(menu, *last as u32));
                hmenu_close(h, &chain);
                // What choosing it from the app's own menu sends.
                id.is_some_and(|id| unsafe { PostMessageW(h, WM_COMMAND, WPARAM(id as usize & 0xFFFF), LPARAM(0)).is_ok() })
            }
            Src::Jab(bridge) => bridge.invoke(h, path.clone()),
            Src::Uia(titles) => {
                let Some(uia) = self.uia.as_ref() else { return false };
                let Some((opened, kids)) = uia.open_path(h, titles, &parent.to_vec()) else { return false };
                let done = kids.get(*last).filter(|e| &Uia::item(*last, e).name == name).is_some_and(|e| uia.choose(e));
                if !done {
                    uia.collapse_all(&opened);
                }
                done
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Classic Win32 menus
// ---------------------------------------------------------------------------

fn hmenu_bar(h: HWND) -> Option<HMENU> {
    let m = unsafe { GetMenu(h) };
    (!m.is_invalid() && unsafe { IsMenu(m).as_bool() }).then_some(m)
}

/// One item's type, state, id and submenu (and with `text`, its label).
fn hmenu_info(menu: HMENU, pos: u32, text: bool) -> Option<(MENUITEMINFOW, String)> {
    unsafe {
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | if text { MIIM_STRING } else { Default::default() },
            ..Default::default()
        };
        GetMenuItemInfoW(menu, pos, true, &mut mii).ok()?;
        if !text || mii.cch == 0 {
            return Some((mii, String::new()));
        }
        let mut buf = vec![0u16; mii.cch as usize + 1];
        mii.dwTypeData = PWSTR(buf.as_mut_ptr());
        mii.cch += 1;
        GetMenuItemInfoW(menu, pos, true, &mut mii).ok()?;
        Some((mii, super::from_wide(&buf)))
    }
}

fn hmenu_id(menu: HMENU, pos: u32) -> Option<u32> {
    hmenu_info(menu, pos, false).map(|(mii, _)| mii.wID)
}

/// A menu's items. Items without text (an MDI child's system menu and
/// window buttons, owner-drawn items) are left out.
fn hmenu_items(menu: HMENU) -> Vec<Item> {
    let n = unsafe { GetMenuItemCount(menu) }.max(0) as u32;
    let mut out = Vec::new();
    for pos in 0..n {
        let Some((mii, raw)) = hmenu_info(menu, pos, true) else { continue };
        let index = pos as usize;
        if mii.fType.0 & MFT_SEPARATOR.0 != 0 {
            out.push(Item { index, name: String::new(), accel: String::new(), enabled: false, checked: false, kind: Kind::Separator });
            continue;
        }
        let (name, accel) = split_accel(&raw);
        if name.is_empty() {
            continue;
        }
        out.push(Item {
            index,
            name,
            accel,
            enabled: mii.fState.0 & MFS_DISABLED.0 == 0,
            checked: mii.fState.0 & MFS_CHECKED.0 != 0,
            kind: if mii.hSubMenu.is_invalid() { Kind::Command } else { Kind::Submenu },
        });
    }
    out
}

/// Walks `path` down from the bar, letting the app bring each menu up to date
/// first (as it would before showing it): the bar, then each submenu opened.
fn hmenu_open(h: HWND, path: &Path) -> Option<Vec<HMENU>> {
    let bar = hmenu_bar(h)?;
    send(h, WM_INITMENU, bar.0 as usize, 0);
    let mut chain = vec![bar];
    for (pos, name) in path {
        let menu = *chain.last()?;
        let (mii, raw) = hmenu_info(menu, *pos as u32, true)?;
        if &split_accel(&raw).0 != name || mii.hSubMenu.is_invalid() {
            hmenu_close(h, &chain);
            return None;
        }
        send(h, WM_INITMENUPOPUP, mii.hSubMenu.0 as usize, *pos as isize);
        chain.push(mii.hSubMenu);
    }
    Some(chain)
}

/// Tells the app the menus `hmenu_open` brought up are gone again.
fn hmenu_close(h: HWND, chain: &[HMENU]) {
    for m in chain.iter().skip(1).rev() {
        send(h, WM_UNINITMENUPOPUP, m.0 as usize, 0);
    }
}

/// "&Save\tCtrl+S" → ("Save", "Ctrl+S").
fn split_accel(raw: &str) -> (String, String) {
    let (name, accel) = raw.split_once('\t').unwrap_or((raw, ""));
    (tidy(&strip_mnemonic(name)), accel.trim().to_string())
}

/// "Save As..." → "Save As…", as Mac menus spell it.
fn tidy(s: &str) -> String {
    let s = s.trim();
    match s.strip_suffix("...") {
        Some(head) => format!("{}…", head.trim_end()),
        None => s.to_string(),
    }
}

/// "&File" → "File" ("&&" stays a literal "&").
pub fn strip_mnemonic(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '&' {
            if it.peek() == Some(&'&') {
                out.push('&');
                it.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// UI Automation
// ---------------------------------------------------------------------------

pub struct Uia {
    a: IUIAutomation,
    cache: IUIAutomationCacheRequest,
    walker: IUIAutomationTreeWalker,
    any: IUIAutomationCondition,
    /// The popup the last menu was read from: closing a menu waits for it to
    /// go, so the next one opened is never read from it.
    last_popup: std::cell::Cell<isize>,
}

fn bstr(r: Result<BSTR>) -> String {
    r.map(|b| b.to_string()).unwrap_or_default()
}

fn cached_bool(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> bool {
    unsafe { e.GetCachedPropertyValue(id) }.ok().and_then(|v| bool::try_from(&v).ok()).unwrap_or(false)
}

fn cached_i32(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<i32> {
    unsafe { e.GetCachedPropertyValue(id) }.ok().and_then(|v| i32::try_from(&v).ok())
}

impl Uia {
    pub fn new() -> Result<Self> {
        unsafe {
            let a: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            if let Ok(a2) = a.cast::<IUIAutomation2>() {
                let _ = a2.SetConnectionTimeout(CONNECT_MS);
                let _ = a2.SetTransactionTimeout(TRANSACT_MS);
            }
            let cache = a.CreateCacheRequest()?;
            for id in [
                UIA_NamePropertyId,
                UIA_ControlTypePropertyId,
                UIA_AutomationIdPropertyId,
                UIA_IsEnabledPropertyId,
                UIA_AcceleratorKeyPropertyId,
                UIA_IsExpandCollapsePatternAvailablePropertyId,
                UIA_IsTogglePatternAvailablePropertyId,
                UIA_ToggleToggleStatePropertyId,
                UIA_ExpandCollapseExpandCollapseStatePropertyId,
                UIA_SelectionItemIsSelectedPropertyId,
                UIA_ClassNamePropertyId,
            ] {
                cache.AddProperty(id)?;
            }
            // Separators and the like aren't "control" elements: read the raw tree.
            let any = a.CreateTrueCondition()?;
            cache.SetTreeFilter(&any)?;
            let walker = a.ControlViewWalker()?;
            Ok(Self { a, cache, walker, any, last_popup: std::cell::Cell::new(0) })
        }
    }

    /// The window's own menu bar (never its system menu): a direct child for
    /// classic menus, or nested a few levels down (XAML, WPF, Qt).
    pub fn find_bar(&self, hwnd: HWND) -> Result<Option<IUIAutomationElement>> {
        unsafe {
            let root = self.a.ElementFromHandle(hwnd)?;
            // Breadth first, bounded so a big window can't make it slow.
            let mut level = vec![root];
            let mut seen = 0;
            for _ in 0..BAR_DEPTH {
                let mut next = Vec::new();
                for e in &level {
                    let Ok(mut c) = self.walker.GetFirstChildElementBuildCache(e, &self.cache) else { continue };
                    loop {
                        seen += 1;
                        if c.CachedControlType().ok() == Some(UIA_MenuBarControlTypeId) {
                            let id = bstr(c.CachedAutomationId());
                            let name = bstr(c.CachedName());
                            if id != "SystemMenuBar" && name != "System" {
                                return Ok(Some(c));
                            }
                        } else {
                            next.push(c.clone());
                        }
                        if seen >= BAR_NODES {
                            return Ok(None);
                        }
                        match self.walker.GetNextSiblingElementBuildCache(&c, &self.cache) {
                            Ok(n) => c = n,
                            Err(_) => break,
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                level = next;
            }
            Ok(None)
        }
    }

    /// The first element of type `want` under `root`, depth first, at most
    /// `depth` levels down and over at most `cap` elements (never a system menu bar).
    fn find_deep(&self, root: &IUIAutomationElement, want: UIA_CONTROLTYPE_ID, depth: usize, cap: usize) -> Option<IUIAutomationElement> {
        fn go(u: &Uia, e: &IUIAutomationElement, want: UIA_CONTROLTYPE_ID, left: usize, seen: &mut usize, cap: usize) -> Option<IUIAutomationElement> {
            if left == 0 {
                return None;
            }
            let mut c = unsafe { u.walker.GetFirstChildElementBuildCache(e, &u.cache) }.ok()?;
            loop {
                *seen += 1;
                if *seen > cap {
                    return None;
                }
                if unsafe { c.CachedControlType() }.ok() == Some(want) && bstr(unsafe { c.CachedAutomationId() }) != "SystemMenuBar" {
                    return Some(c);
                }
                if let Some(f) = go(u, &c, want, left - 1, seen, cap) {
                    return Some(f);
                }
                if *seen > cap {
                    return None;
                }
                c = unsafe { u.walker.GetNextSiblingElementBuildCache(&c, &u.cache) }.ok()?;
            }
        }
        go(self, root, want, depth, &mut 0, cap)
    }

    /// `e`'s menu children (items and separators), with their cached properties.
    pub fn children(&self, e: &IUIAutomationElement) -> Result<Vec<IUIAutomationElement>> {
        let mut out = Vec::new();
        self.collect_items(e, 2, &mut out)?;
        Ok(out)
    }

    /// Menu items and separators under `e`, looking through up to `wrappers`
    /// levels of plain containers (web pages wrap each item in one).
    fn collect_items(&self, e: &IUIAutomationElement, wrappers: usize, out: &mut Vec<IUIAutomationElement>) -> Result<()> {
        unsafe {
            let all = e.FindAllBuildCache(TreeScope_Children, &self.any, &self.cache)?;
            for i in 0..all.Length()? {
                let c = all.GetElement(i)?;
                let t = c.CachedControlType().unwrap_or_default();
                if t == UIA_MenuItemControlTypeId || t == UIA_SeparatorControlTypeId {
                    out.push(c);
                } else if wrappers > 0 && (t == UIA_GroupControlTypeId || t == UIA_PaneControlTypeId || t == UIA_CustomControlTypeId) {
                    self.collect_items(&c, wrappers - 1, out)?;
                }
            }
            Ok(())
        }
    }

    pub fn item(index: usize, e: &IUIAutomationElement) -> Item {
        unsafe {
            if e.CachedControlType().unwrap_or_default() == UIA_SeparatorControlTypeId {
                return Item { index, name: String::new(), accel: String::new(), enabled: false, checked: false, kind: Kind::Separator };
            }
            let (name, tail) = split_accel(&bstr(e.CachedName()));
            let accel = Some(bstr(e.CachedAcceleratorKey())).filter(|a| !a.is_empty()).unwrap_or(tail);
            let expands = cached_bool(e, UIA_IsExpandCollapsePatternAvailablePropertyId)
                && cached_i32(e, UIA_ExpandCollapseExpandCollapseStatePropertyId) != Some(ExpandCollapseState_LeafNode.0);
            let checked = (cached_bool(e, UIA_IsTogglePatternAvailablePropertyId)
                && cached_i32(e, UIA_ToggleToggleStatePropertyId) == Some(ToggleState_On.0))
                || cached_bool(e, UIA_SelectionItemIsSelectedPropertyId);
            Item {
                index,
                name,
                accel,
                enabled: e.CachedIsEnabled().map(|b| b.as_bool()).unwrap_or(true),
                checked,
                kind: if expands { Kind::Submenu } else { Kind::Command },
            }
        }
    }

    /// Opens the menus along `path` (from the bar's `titles`), checking each
    /// step's name. Gives the elements opened (to collapse after) and the last
    /// menu's items.
    fn open_path(
        &self,
        app: HWND,
        titles: &[IUIAutomationElement],
        path: &Path,
    ) -> Option<(Vec<IUIAutomationElement>, Vec<IUIAutomationElement>)> {
        let mut opened = Vec::new();
        let mut items: Vec<IUIAutomationElement> = titles.to_vec();
        for (pos, name) in path {
            let ok = items.get(*pos).filter(|e| &Uia::item(*pos, e).name == name).cloned();
            let Some(e) = ok else {
                self.collapse_all(&opened);
                return None;
            };
            match self.expand(app, &e, EXPAND_WAIT) {
                Ok(kids) if !kids.is_empty() => {
                    opened.push(e);
                    items = kids;
                }
                _ => {
                    opened.push(e);
                    self.collapse_all(&opened);
                    return None;
                }
            }
        }
        Some((opened, items))
    }

    /// Expands `e` (an item of `app`'s menus) and returns its menu's items once
    /// they show up, within `wait`. They're usually in a popup window of their own.
    pub fn expand(&self, app: HWND, e: &IUIAutomationElement, wait: Duration) -> Result<Vec<IUIAutomationElement>> {
        unsafe {
            let before = popups(app);
            let p: IUIAutomationExpandCollapsePattern = e.GetCurrentPatternAs(UIA_ExpandCollapsePatternId)?;
            p.Expand()?;
            let deadline = Instant::now() + wait;
            loop {
                // A few frameworks put them under the item itself, or in a
                // menu inside it (web pages).
                let kids = self.children(e)?;
                if !kids.is_empty() {
                    return Ok(kids);
                }
                if let Some(menu) = self.find_deep(e, UIA_MenuControlTypeId, 4, 40) {
                    let kids = self.children(&menu)?;
                    if !kids.is_empty() {
                        return Ok(kids);
                    }
                }
                for h in popups(app).into_iter().filter(|h| !before.contains(h)) {
                    if let Some(kids) = self.menu_in(h)? {
                        self.last_popup.set(h.0 as isize);
                        return Ok(kids);
                    }
                }
                if Instant::now() >= deadline {
                    return Ok(Vec::new());
                }
                std::thread::sleep(Duration::from_millis(8));
            }
        }
    }

    /// The items of the menu in popup window `h` (the popup itself, or a menu a few levels in).
    fn menu_in(&self, h: HWND) -> Result<Option<Vec<IUIAutomationElement>>> {
        unsafe {
            let root = self.a.ElementFromHandleBuildCache(h, &self.cache)?;
            let mut level = vec![root];
            for _ in 0..BAR_DEPTH {
                let mut next = Vec::new();
                for e in &level {
                    if e.CachedControlType().ok() == Some(UIA_MenuControlTypeId) {
                        let kids = self.children(e)?;
                        if !kids.is_empty() {
                            return Ok(Some(kids));
                        }
                    }
                    let Ok(mut c) = self.walker.GetFirstChildElementBuildCache(e, &self.cache) else { continue };
                    loop {
                        if c.CachedControlType().ok() == Some(UIA_MenuItemControlTypeId)
                            && e.CachedControlType().ok() != Some(UIA_MenuBarControlTypeId)
                        {
                            // Items straight under a pane: the pane is the menu.
                            let kids = self.children(e)?;
                            return Ok((!kids.is_empty()).then_some(kids));
                        }
                        next.push(c.clone());
                        match self.walker.GetNextSiblingElementBuildCache(&c, &self.cache) {
                            Ok(n) => c = n,
                            Err(_) => break,
                        }
                    }
                }
                if next.is_empty() || next.len() > BAR_NODES {
                    break;
                }
                level = next;
            }
            Ok(None)
        }
    }

    /// Chooses `e`: Invoke, else Toggle, else Expand (whatever it supports).
    fn choose(&self, e: &IUIAutomationElement) -> bool {
        unsafe {
            if let Ok(p) = e.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
                return p.Invoke().is_ok();
            }
            if let Ok(p) = e.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) {
                return p.Toggle().is_ok();
            }
            if let Ok(p) = e.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId) {
                return p.Expand().is_ok();
            }
            false
        }
    }

    /// Closes menus opened along a path, innermost first, and waits (briefly)
    /// for the last one's popup to go.
    fn collapse_all(&self, opened: &[IUIAutomationElement]) {
        for e in opened.iter().rev() {
            unsafe {
                if let Ok(p) = e.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId) {
                    if p.CurrentExpandCollapseState().is_ok_and(|s| s != ExpandCollapseState_Collapsed) {
                        let _ = p.Collapse();
                    }
                }
            }
        }
        let popup = HWND(self.last_popup.replace(0) as _);
        let deadline = Instant::now() + Duration::from_millis(250);
        while !popup.is_invalid() && unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(popup).as_bool() } && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(8));
        }
    }
}

/// The app's visible popup windows (where its menus open), and any classic
/// menu popup (those open on a system thread). Never a main window: another
/// window of the same app has a menu bar of its own.
fn popups(app: HWND) -> Vec<HWND> {
    use windows::Win32::Foundation::{BOOL, TRUE};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowLongW, IsWindowVisible, GWL_STYLE, WS_POPUP};
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(app, Some(&mut pid)) };
    let mut all: Vec<HWND> = Vec::new();
    unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> BOOL {
        (*(lp.0 as *mut Vec<HWND>)).push(h);
        TRUE
    }
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut all as *mut _ as isize));
    }
    all.into_iter()
        .filter(|&h| h != app && unsafe { IsWindowVisible(h).as_bool() })
        .filter(|&h| unsafe { GetWindowLongW(h, GWL_STYLE) } as u32 & WS_POPUP.0 != 0)
        .filter(|&h| {
            let mut p = 0u32;
            unsafe { GetWindowThreadProcessId(h, Some(&mut p)) };
            p == pid || super::class_name(h) == "#32768"
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Development probe
// ---------------------------------------------------------------------------

/// `dockside --uia-dump <exe or title> [depth] [max nodes]`: prints a window's
/// UIA control tree (type, name, id, class), to see how an app shows its menus.
pub fn dump(args: &[String]) {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let want = args.first().map(|s| s.to_lowercase()).unwrap_or_default();
    let depth: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8);
    let cap: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(400);
    let Ok(uia) = Uia::new() else { return println!("no UIA") };
    for w in super::apps::list_windows().into_iter().filter(|w| w.exe.to_lowercase().contains(&want) || w.title.to_lowercase().contains(&want)) {
        println!("== {} [{}] {}", w.title, super::class_name(w.hwnd), w.exe);
        let t = Instant::now();
        let Ok(root) = (unsafe { uia.a.ElementFromHandleBuildCache(w.hwnd, &uia.cache) }) else { continue };
        let mut n = 0;
        fn walk(uia: &Uia, e: &IUIAutomationElement, d: usize, max: usize, n: &mut usize, cap: usize) {
            unsafe {
                let Ok(mut c) = uia.walker.GetFirstChildElementBuildCache(e, &uia.cache) else { return };
                loop {
                    *n += 1;
                    if *n > cap {
                        return;
                    }
                    let t = c.CachedControlType().unwrap_or_default().0;
                    let name: String = bstr(c.CachedName()).chars().take(60).collect();
                    let cls = bstr(c.GetCachedPropertyValue(UIA_ClassNamePropertyId).and_then(|v| BSTR::try_from(&v)));
                    println!("{}{} '{}' id='{}' cls='{}'", "  ".repeat(d), ctl(t), name, bstr(c.CachedAutomationId()), cls);
                    if d + 1 < max {
                        walk(uia, &c, d + 1, max, n, cap);
                    }
                    match uia.walker.GetNextSiblingElementBuildCache(&c, &uia.cache) {
                        Ok(x) => c = x,
                        Err(_) => break,
                    }
                }
            }
        }
        walk(&uia, &root, 0, depth, &mut n, cap);
        println!("  ({n} nodes, {:.0} ms)", t.elapsed().as_secs_f64() * 1000.0);
    }
}

fn ctl(t: i32) -> String {
    match t {
        50000 => "Button", 50003 => "ComboBox", 50004 => "Edit", 50005 => "Hyperlink", 50006 => "Image",
        50007 => "ListItem", 50008 => "List", 50009 => "Menu", 50010 => "MenuBar", 50011 => "MenuItem",
        50018 => "Tab", 50019 => "TabItem", 50020 => "Text", 50021 => "ToolBar", 50023 => "Tree", 50024 => "TreeItem",
        50025 => "Custom", 50026 => "Group", 50030 => "Document", 50032 => "Window", 50033 => "Pane", 50037 => "TitleBar",
        50038 => "Separator", 50031 => "SplitButton",
        _ => return format!("Type{t}"),
    }
    .to_string()
}

/// CPU time (user + kernel) a process has used, in ms: this one, or `pid`.
fn cpu_ms(pid: Option<u32>) -> f64 {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let (h, own) = match pid {
            Some(p) => match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, p) {
                Ok(h) => (h, false),
                Err(_) => return 0.0,
            },
            None => (GetCurrentProcess(), true),
        };
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok();
        if !own {
            let _ = CloseHandle(h);
        }
        let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 10_000.0;
        if ok {
            ft(k) + ft(u)
        } else {
            0.0
        }
    }
}

/// `dockside --uia-probe <exe, title or "explorer"> [--open N] [--choose N/M]`:
/// prints what DockSide reads from a running app's menu bar, with timings.
/// `--open N` opens title N (the app's real menu may flash for UIA menus);
/// `--choose N/M` chooses item M of title N in the app.
pub fn probe(args: &[String]) {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let want = args.first().map(|s| s.to_lowercase()).unwrap_or_default();
    let flag = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    // `--open N` (a title) or `N/M` (title N's submenu at item M).
    let open_arg = flag("--open");
    let open: Option<usize> = open_arg.as_deref().and_then(|s| s.split('/').next()?.parse().ok());
    let open_sub: Option<usize> = open_arg.as_deref().and_then(|s| s.split('/').nth(1)?.parse().ok());
    // `--choose N/M[/K…]`: title N, then item M of its menu, and so on down.
    let choose: Option<Vec<usize>> =
        flag("--choose").and_then(|s| s.split('/').map(|p| p.parse().ok()).collect::<Option<Vec<usize>>>()).filter(|v| v.len() >= 2);
    // A window handle ("0x1234") works too, for windows the app list leaves out.
    let raw = want.strip_prefix("0x").and_then(|h| isize::from_str_radix(h, 16).ok());
    let wins: Vec<_> = match raw {
        Some(h) => vec![super::apps::AppWindow { hwnd: HWND(h as _), exe: String::new(), title: String::new() }],
        None => super::apps::list_windows()
            .into_iter()
            .filter(|w| match want.as_str() {
                "explorer" => super::class_name(w.hwnd) == "CabinetWClass",
                _ => w.exe.to_lowercase().contains(&want) || w.title.to_lowercase().contains(&want),
            })
            .collect(),
    };
    if wins.is_empty() {
        return println!("no window for {want}");
    }
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    for win in wins {
        let hwnd = win.hwnd.0 as isize;
        println!("== {} [{}] {}", win.title, super::class_name(win.hwnd), win.exe);
        let mut w = Worker { uia: None, cur: None, misses: HashMap::new() };
        for pass in 0..3 {
            w.cur = None;
            w.misses.clear();
            let t = Instant::now();
            let found = w.read(hwnd);
            let took = ms(t);
            match &found {
                Some(t) => println!(
                    "  pass {pass}: {} bar in {took:.1} ms: {:?}",
                    match t.src {
                        Src::Hmenu => "HMENU",
                        Src::Jab(_) => "Java",
                        Src::Uia(_) => "UIA",
                    },
                    t.titles.iter().map(|i| i.name.as_str()).collect::<Vec<_>>()
                ),
                None => println!("  pass {pass}: no menu bar ({took:.1} ms)"),
            }
            w.cur = found;
        }
        // `--bench N`: N fresh bar reads (and N reads of `--open`'s menu), with
        // the CPU they cost DockSide and the app.
        if let Some(n) = flag("--bench").and_then(|s| s.parse::<u32>().ok()).filter(|n| *n > 0) {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(win.hwnd, Some(&mut pid)) };
            let run = |w: &mut Worker, what: &str, f: &mut dyn FnMut(&mut Worker)| {
                let (c0, a0, t) = (cpu_ms(None), cpu_ms(Some(pid)), Instant::now());
                for _ in 0..n {
                    f(w);
                }
                let k = n as f64;
                println!(
                    "  bench {what} x{n}: {:.2} ms each, CPU {:.2} ms DockSide + {:.2} ms app",
                    ms(t) / k,
                    (cpu_ms(None) - c0) / k,
                    (cpu_ms(Some(pid)) - a0) / k
                );
            };
            run(&mut w, "bar", &mut |w| {
                w.cur = None;
                w.misses.clear();
                w.cur = w.read(hwnd);
            });
            if let Some(title) = open.and_then(|i| w.cur.as_ref()?.titles.get(i).cloned()) {
                let path = vec![(title.index, title.name.clone())];
                run(&mut w, "menu", &mut |w| {
                    let t = Instant::now();
                    let n = w.menu(hwnd, &path).map(|v| v.len());
                    println!("    read {n:?} items in {:.1} ms", ms(t));
                });
            }
        }
        let Some(titles) = w.cur.as_ref().map(|t| t.titles.clone()) else { continue };
        if let Some(i) = open {
            let Some(title) = titles.get(i) else { continue };
            let mut path = vec![(title.index, title.name.clone())];
            if let Some(m) = open_sub {
                let sub = w.menu(hwnd, &path).and_then(|v| v.into_iter().find(|i| i.index == m && i.kind == Kind::Submenu));
                let Some(sub) = sub else {
                    println!("  no submenu at {m} in {}", title.name);
                    continue;
                };
                path.push((sub.index, sub.name));
            }
            let t = Instant::now();
            match w.menu(hwnd, &path) {
                Some(items) => {
                    println!("  {} opened in {:.1} ms, {} items:", path.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(" > "), ms(t), items.len());
                    for i in items {
                        println!("    {:>2} {:?} {:?} accel={:?} enabled={} checked={}", i.index, i.kind, i.name, i.accel, i.enabled, i.checked);
                    }
                }
                None => println!("  {} could not be read ({:.1} ms)", title.name, ms(t)),
            }
        }
        if let Some(steps) = &choose {
            let Some(title) = titles.get(steps[0]) else { continue };
            let mut path = vec![(title.index, title.name.clone())];
            let mut found = true;
            for &k in &steps[1..] {
                match w.menu(hwnd, &path).and_then(|v| v.into_iter().find(|i| i.index == k)) {
                    Some(item) => path.push((item.index, item.name)),
                    None => {
                        println!("  no item {k} under {:?}", path.last().map(|(_, n)| n));
                        found = false;
                        break;
                    }
                }
            }
            if found {
                let t = Instant::now();
                let ok = w.invoke(hwnd, &path);
                println!("  chose {}: {ok} in {:.1} ms", path.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(" > "), ms(t));
            }
        }
    }
}
