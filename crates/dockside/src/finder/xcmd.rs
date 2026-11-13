//! Windows 11's packaged context menu verbs: the `IExplorerCommand` handlers
//! apps declare in their package manifest (`windows.fileExplorerContextMenus`),
//! like WinRAR's or 7-Zip's archive commands, "Open in Terminal" and PowerToys'.
//!
//! Explorer shows these only in its new menu, so the classic `IContextMenu`
//! Finder copies under "More Options" never has them. They're found by reading
//! each installed package's manifest (cached, rescanned in the background now
//! and then), then activated through packaged COM and asked for their titles,
//! state and subcommands like Explorer does.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{GUID, HSTRING, PWSTR};
use windows::Win32::Foundation::{BOOL, ERROR_SUCCESS};
use windows::Win32::Storage::Packaging::Appx::GetPackagePathByFullName;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER};
use windows::Win32::System::Registry::{RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ};
use windows::Win32::UI::Shell::{IExplorerCommand, IShellItemArray, SHDefExtractIconW};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, KillTimer, SetTimer, HICON};

use serde::{Deserialize, Serialize};

use crate::gfx::icons::IconPixels;
use crate::menu::{Action, Entry as MenuEntry};

/// Command ids for packaged verbs start here, above the classic menu's (see `shell::invoke`).
pub const FIRST_ID: u32 = 0x1_0000;

const ECF_HASSUBCOMMANDS: u32 = 0x1;
const ECF_ISSEPARATOR: u32 = 0x8;
const ECF_SEPARATORBEFORE: u32 = 0x20;
const ECF_SEPARATORAFTER: u32 = 0x40;
const ECS_DISABLED: u32 = 0x1;
const ECS_HIDDEN: u32 = 0x2;
const ECS_CHECKED: u32 = 0x8;

/// How long a scan of the installed packages is trusted before it's redone (in the background).
const STALE: Duration = Duration::from_secs(120);

/// One packaged verb: its handler and the item types it's declared for
/// ("*", ".png", "Directory", "Directory\Background", ..., lowercased).
#[derive(Clone, Debug, PartialEq)]
pub struct Verb {
    pub clsid: GUID,
    pub types: Vec<String>,
}

static SCAN: Mutex<Option<(Instant, Arc<Vec<Verb>>)>> = Mutex::new(None);
static SCANNING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Activated handlers, kept so their (out-of-process) servers stay running
    /// between menus; `None` for ones that failed, not tried again until the next scan.
    static HANDLERS: RefCell<HashMap<u128, Option<IExplorerCommand>>> = RefCell::new(HashMap::new());
    /// The open menu's commands (by id - FIRST_ID) and the items they act on.
    static OPEN: RefCell<(Option<IShellItemArray>, Vec<IExplorerCommand>)> = const { RefCell::new((None, Vec::new())) };
}

/// Scans the installed packages on a background thread if the last scan is missing or old.
pub fn warm() {
    if elevated() {
        return remote::warm();
    }
    let fresh = SCAN.lock().ok().and_then(|s| s.as_ref().map(|(t, _)| t.elapsed() < STALE)).unwrap_or(false);
    if fresh || SCANNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = std::thread::Builder::new().name("finder-verbs".into()).spawn(|| {
        let verbs = Arc::new(scan());
        if let Ok(mut s) = SCAN.lock() {
            *s = Some((Instant::now(), verbs));
        }
        SCANNING.store(false, Ordering::Release);
    });
}

/// The packaged verbs: the last scan's (rescanned in the background when old), or a scan now if there's none yet.
fn verbs() -> Arc<Vec<Verb>> {
    let cached = SCAN.lock().ok().and_then(|s| s.as_ref().map(|(_, v)| v.clone()));
    match cached {
        Some(v) => {
            warm();
            v
        }
        None => {
            let v = Arc::new(scan());
            if let Ok(mut s) = SCAN.lock() {
                *s = Some((Instant::now(), v.clone()));
            }
            // A fresh scan may list handlers that failed before.
            HANDLERS.with(|h| h.borrow_mut().retain(|_, c| c.is_some()));
            v
        }
    }
}

/// Every installed package's context menu verbs, one entry per handler.
fn scan() -> Vec<Verb> {
    let started = Instant::now();
    let mut out: Vec<Verb> = Vec::new();
    for name in package_names() {
        let Some(dir) = package_path(&name) else { continue };
        let Ok(xml) = std::fs::read_to_string(dir.join("AppxManifest.xml")) else { continue };
        for (ty, clsid) in parse_manifest(&xml) {
            match out.iter_mut().find(|v| v.clsid == clsid) {
                Some(v) if !v.types.contains(&ty) => v.types.push(ty),
                Some(_) => {}
                None => out.push(Verb { clsid, types: vec![ty] }),
            }
        }
    }
    log!("finder: {} packaged context menu verbs ({} ms)", out.len(), started.elapsed().as_millis());
    out
}

/// Full names of the packages registered for this user.
fn package_names() -> Vec<String> {
    let key = HSTRING::from(r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages");
    let mut out = Vec::new();
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, &key, 0, KEY_READ, &mut hkey) != ERROR_SUCCESS {
            return out;
        }
        for i in 0.. {
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            if RegEnumKeyExW(hkey, i, PWSTR(buf.as_mut_ptr()), &mut len, None, PWSTR::null(), None, None) != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        let _ = RegCloseKey(hkey);
    }
    out
}

/// Where a package is installed (None for packages that aren't, like staged leftovers).
fn package_path(full_name: &str) -> Option<PathBuf> {
    let name = HSTRING::from(full_name);
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    unsafe {
        if GetPackagePathByFullName(&name, &mut len, PWSTR(buf.as_mut_ptr())) != ERROR_SUCCESS {
            return None;
        }
    }
    Some(PathBuf::from(crate::win::from_wide(&buf))).filter(|p| !p.as_os_str().is_empty())
}

/// `(item type, handler)` pairs from a manifest's `windows.fileExplorerContextMenus`
/// extensions. A light tag scan: namespace prefixes vary between manifests.
pub fn parse_manifest(xml: &str) -> Vec<(String, GUID)> {
    let mut out = Vec::new();
    let mut in_menus = false;
    let mut item_type: Option<String> = None;
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        if tag.starts_with('!') || tag.starts_with('?') {
            continue;
        }
        let closing = tag.starts_with('/');
        let self_closing = tag.ends_with('/');
        let name = tag.trim_start_matches('/').split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or(name);
        match local {
            "Extension" if closing => in_menus = false,
            "Extension" => in_menus = !self_closing && attr(tag, "Category") == Some("windows.fileExplorerContextMenus"),
            "ItemType" if in_menus && closing => item_type = None,
            "ItemType" if in_menus => item_type = attr(tag, "Type").map(str::to_lowercase).filter(|_| !self_closing),
            "Verb" if in_menus && !closing => {
                if let (Some(ty), Some(clsid)) = (&item_type, attr(tag, "Clsid").and_then(parse_guid)) {
                    out.push((ty.clone(), clsid));
                }
            }
            _ => {}
        }
    }
    out
}

/// An attribute's value in a start tag.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut s = tag;
    while let Some(i) = s.find(name) {
        let before_ok = i > 0 && s[..i].ends_with(|c: char| c.is_whitespace());
        let after = s[i + name.len()..].trim_start();
        if before_ok {
            if let Some(v) = after.strip_prefix('=') {
                let v = v.trim_start();
                let q = v.chars().next()?;
                if q == '"' || q == '\'' {
                    let v = &v[1..];
                    return v.find(q).map(|e| &v[..e]);
                }
            }
        }
        s = &s[i + name.len()..];
    }
    None
}

fn parse_guid(s: &str) -> Option<GUID> {
    let hex: String = s.trim().trim_matches(|c| c == '{' || c == '}').chars().filter(|c| *c != '-').collect();
    (hex.len() == 32).then(|| u128::from_str_radix(&hex, 16).ok()).flatten().map(GUID::from_u128)
}

/// What the menu is for: some items (whether each is a folder, and its extension), or a folder's background.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Target {
    Items(Vec<(bool, String)>),
    Background,
}

/// Whether a verb declared for `types` applies to `target`, the way Explorer matches them.
pub fn applies(types: &[String], target: &Target) -> bool {
    let has = |t: &str| types.iter().any(|x| x == t);
    match target {
        Target::Background => has(r"directory\background"),
        Target::Items(items) => {
            !items.is_empty()
                && items.iter().all(|(dir, ext)| {
                    has("allfilesystemobjects")
                        || if *dir { has("directory") || has("folder") } else { has("*") || (!ext.is_empty() && has(&format!(".{ext}"))) }
                })
        }
    }
}

/// One packaged menu item, as plain data: built in this process, or (when
/// DockSide runs elevated) in the unelevated helper and sent over its pipe.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Node {
    Separator,
    /// `id`: its command (`None`: disabled); `icon`: width, height, premultiplied BGRA.
    Item { title: String, icon: Option<(u32, u32, Vec<u8>)>, id: Option<u32>, checked: bool },
    Menu { title: String, icon: Option<(u32, u32, Vec<u8>)>, children: Vec<Node> },
}

/// Whether DockSide runs elevated (packaged handlers then live in the helper: Windows
/// doesn't hand them to elevated processes, "Class not registered").
fn elevated() -> bool {
    static ELEVATED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ELEVATED.get_or_init(crate::win::elevation::is_elevated)
}

/// The packaged verbs for `paths` (the folder itself, for a background menu), as
/// menu entries whose actions come from `action(id)`. Kept for [`invoke`] until
/// the next menu replaces them, or until they're [released](release_later).
pub fn entries(paths: &[PathBuf], target: Target, action: &dyn Fn(u32) -> Action) -> Vec<MenuEntry> {
    release_later();
    let nodes = if elevated() { remote::build(paths, target) } else { build(paths, &target) };
    to_entries(nodes, action)
}

/// How long the shell's menu handlers stay loaded after the last menu closes.
/// Then they're let go: their DLLs (and the `dllhost.exe` each packaged one runs
/// in) go away, and with DockSide elevated, so does the helper. The next menu
/// loads them again (a little slower).
const IDLE: Duration = Duration::from_secs(15);

thread_local! {
    static RELEASE_TIMER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// (Re)starts the countdown to [`release`], on this (the UI) thread.
fn release_later() {
    unsafe {
        let old = RELEASE_TIMER.with(|t| t.get());
        if old != 0 {
            let _ = KillTimer(None, old);
        }
        let id = SetTimer(None, 0, IDLE.as_millis() as u32, Some(release_tick));
        RELEASE_TIMER.with(|t| t.set(id));
    }
}

unsafe extern "system" fn release_tick(_: HWND, _: u32, id: usize, _: u32) {
    // A menu still open may yet run one of them: wait for it.
    if crate::app::menu_open() {
        return;
    }
    let _ = KillTimer(None, id);
    RELEASE_TIMER.with(|t| t.set(0));
    release();
}

/// Lets go of every loaded handler and the last menu's items.
fn release() {
    HANDLERS.with(|h| h.borrow_mut().clear());
    OPEN.with(|o| *o.borrow_mut() = (None, Vec::new()));
    super::shell::release_menu();
}

/// Builds the packaged items for `paths` here, keeping their commands for [`invoke_local`].
fn build(paths: &[PathBuf], target: &Target) -> Vec<Node> {
    let items = super::shell::item_array(paths);
    OPEN.with(|o| *o.borrow_mut() = (items.clone(), Vec::new()));
    let Some(items) = items else { return Vec::new() };
    let mut out = Vec::new();
    for v in verbs().iter().filter(|v| applies(&v.types, target)) {
        let Some(cmd) = handler(v.clsid) else { continue };
        unsafe { add(&cmd, &items, 0, &mut out) };
    }
    while matches!(out.last(), Some(Node::Separator)) {
        out.pop();
    }
    out
}

fn to_entries(nodes: Vec<Node>, action: &dyn Fn(u32) -> Action) -> Vec<MenuEntry> {
    let pixels = |icon: Option<(u32, u32, Vec<u8>)>| {
        icon.filter(|(w, h, d)| *w <= 256 && *h <= 256 && d.len() == (*w * *h * 4) as usize)
            .map(|(width, height, data)| IconPixels { width, height, data })
    };
    nodes
        .into_iter()
        .map(|n| match n {
            Node::Separator => MenuEntry::Separator,
            Node::Item { title, icon, id: Some(id), checked } => MenuEntry::item(title, action(FIRST_ID + id)).checked(checked).with_pixels(pixels(icon)),
            Node::Item { title, icon, id: None, .. } => MenuEntry::disabled(title).with_pixels(pixels(icon)),
            Node::Menu { title, icon, children } => MenuEntry::submenu(title, to_entries(children, action)).with_pixels(pixels(icon)),
        })
        .collect()
}

/// The handler for `clsid`, activated once (packaged COM finds it through the package).
fn handler(clsid: GUID) -> Option<IExplorerCommand> {
    HANDLERS.with(|h| {
        h.borrow_mut()
            .entry(clsid.to_u128())
            .or_insert_with(|| {
                let r: windows::core::Result<IExplorerCommand> =
                    unsafe { CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER | CLSCTX_INPROC_SERVER) };
                r.map_err(|e| log!("finder: packaged verb {clsid:?} unavailable: {e}")).ok()
            })
            .clone()
    })
}

unsafe fn take(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

/// Appends `cmd` (and, for a submenu, its subcommands) to `out`, as Explorer would show it.
unsafe fn add(cmd: &IExplorerCommand, items: &IShellItemArray, depth: u32, out: &mut Vec<Node>) {
    let flags = cmd.GetFlags().unwrap_or(0);
    let sep = |out: &mut Vec<Node>| {
        if !matches!(out.last(), None | Some(Node::Separator)) {
            out.push(Node::Separator);
        }
    };
    if flags & ECF_ISSEPARATOR != 0 {
        sep(out);
        return;
    }
    let state = cmd.GetState(items, BOOL::from(true)).unwrap_or(0);
    if state & ECS_HIDDEN != 0 {
        return;
    }
    let title = cmd.GetTitle(items).map(|p| take(p)).unwrap_or_default();
    let title = title.replace('&', "").trim().to_string();
    if title.is_empty() {
        return;
    }
    let icon = cmd.GetIcon(items).ok().map(|p| take(p)).and_then(|loc| icon(&loc)).map(|px| (px.width, px.height, px.data));
    let node = if flags & ECF_HASSUBCOMMANDS != 0 {
        let mut children = Vec::new();
        if depth < 4 {
            if let Ok(en) = cmd.EnumSubCommands() {
                loop {
                    let mut next = [None];
                    let mut got = 0u32;
                    if en.Next(&mut next, Some(&mut got)).is_err() || got == 0 {
                        break;
                    }
                    if let Some(c) = next[0].take() {
                        add(&c, items, depth + 1, &mut children);
                    }
                }
            }
        }
        while matches!(children.last(), Some(Node::Separator)) {
            children.pop();
        }
        if children.is_empty() {
            return;
        }
        Node::Menu { title, icon, children }
    } else if state & ECS_DISABLED != 0 {
        Node::Item { title, icon, id: None, checked: false }
    } else {
        let n = OPEN.with(|o| {
            let mut o = o.borrow_mut();
            o.1.push(cmd.clone());
            o.1.len() - 1
        });
        Node::Item { title, icon, id: Some(n as u32), checked: state & ECS_CHECKED != 0 }
    };
    if flags & ECF_SEPARATORBEFORE != 0 {
        sep(out);
    }
    out.push(node);
    if flags & ECF_SEPARATORAFTER != 0 {
        out.push(Node::Separator);
    }
}

/// A command's icon from its "file,index" location.
fn icon(loc: &str) -> Option<IconPixels> {
    let loc = loc.trim();
    if loc.is_empty() || loc.starts_with('@') {
        return None;
    }
    let (file, index) = match loc.rsplit_once(',') {
        Some((f, i)) if i.trim().parse::<i32>().is_ok() => (f.trim(), i.trim().parse::<i32>().unwrap_or(0)),
        _ => (loc, 0),
    };
    let file = file.trim_matches('"');
    const SIZE: i32 = 32;
    unsafe {
        let mut large = HICON::default();
        if SHDefExtractIconW(&HSTRING::from(file), index, 0, Some(&mut large), None, SIZE as u32).is_err() || large.is_invalid() {
            return None;
        }
        let px = crate::win::tray::icon_pixels(large, SIZE);
        let _ = DestroyIcon(large);
        px
    }
}

/// Runs command `id` of the open menu's packaged verbs. Modal for some: call outside the App borrow.
pub fn invoke(id: u32) -> bool {
    let Some(n) = id.checked_sub(FIRST_ID) else { return false };
    release_later();
    if elevated() {
        return remote::invoke(n);
    }
    invoke_local(n)
}

fn invoke_local(n: u32) -> bool {
    let Some((items, cmd)) = OPEN.with(|o| {
        let o = o.borrow();
        Some((o.0.clone()?, o.1.get(n as usize)?.clone()))
    }) else {
        return false;
    };
    unsafe {
        if let Err(e) = cmd.Invoke(&items, None) {
            log!("finder: packaged verb {n} failed: {e}");
        }
    }
    true
}

/// The unelevated helper (`dockside --shell-verbs <pipe>`): Windows only hands
/// packaged handlers to unelevated processes, so an elevated DockSide asks a
/// copy of itself, started through Explorer, to build and run them. That also
/// starts what they launch (Terminal, Notepad) unelevated, as from Explorer.
///
/// The helper owns the pipe (an unelevated process couldn't open one an
/// elevated one made); requests and replies are JSON lines. It quits when
/// DockSide disconnects or exits.
pub mod remote {
    use std::io::{BufRead, BufReader, Write};
    use std::os::windows::io::FromRawHandle;
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use serde::{Deserialize, Serialize};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    use super::{Node, Target};

    pub const ARG: &str = "--shell-verbs";
    /// How long a menu waits for the helper's items before opening without them.
    const WAIT: Duration = Duration::from_millis(2500);

    #[derive(Serialize, Deserialize)]
    pub(super) enum Request {
        /// Scan the packages, ready for the first menu.
        Warm,
        Build { paths: Vec<PathBuf>, target: Target },
        Invoke(u32),
    }

    struct Link {
        tx: Sender<(Request, Option<Sender<Vec<Node>>>)>,
    }

    static LINK: Mutex<Option<Link>> = Mutex::new(None);
    /// When the helper last failed to start (it's not retried on every menu then).
    static LAST_FAIL: Mutex<Option<Instant>> = Mutex::new(None);

    /// The running helper's request queue, starting the helper when there's none.
    fn link() -> Option<Sender<(Request, Option<Sender<Vec<Node>>>)>> {
        let mut l = LINK.lock().ok()?;
        if let Some(link) = l.as_ref() {
            return Some(link.tx.clone());
        }
        if LAST_FAIL.lock().ok()?.is_some_and(|t| t.elapsed() < Duration::from_secs(15)) {
            return None;
        }
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let name = format!(r"\\.\pipe\dockside-verbs-{}-{nonce:x}", std::process::id());
        let exe = std::env::current_exe().ok()?.display().to_string();
        if !crate::win::elevation::shell_open(&exe, Some(&format!("{ARG} {name}")), None) {
            log!("finder: couldn't start the shell verbs helper");
            failed();
            return None;
        }
        let (tx, rx) = channel();
        let _ = std::thread::Builder::new().name("finder-verbs-link".into()).spawn(move || serve_link(name, rx));
        *l = Some(Link { tx: tx.clone() });
        Some(tx)
    }

    /// Connects to the helper's pipe, then passes requests along until it goes away.
    pub(super) fn serve_link(name: String, rx: Receiver<(Request, Option<Sender<Vec<Node>>>)>) {
        let started = Instant::now();
        let wide = HSTRING::from(name.as_str());
        let handle = loop {
            let h = unsafe {
                CreateFileW(&wide, (GENERIC_READ | GENERIC_WRITE).0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None)
            };
            match h {
                Ok(h) => break Some(h),
                Err(_) if started.elapsed() < Duration::from_secs(15) => unsafe {
                    if !WaitNamedPipeW(&wide, 200).as_bool() {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                },
                Err(e) => {
                    log!("finder: shell verbs helper didn't come up: {e}");
                    failed();
                    break None;
                }
            }
        };
        if let Some(handle) = handle {
            let mut pid = 0u32;
            unsafe {
                let _ = GetNamedPipeServerProcessId(handle, &mut pid);
            }
            let file = unsafe { std::fs::File::from_raw_handle(handle.0) };
            if let Ok(read) = file.try_clone() {
                let mut reader = BufReader::new(read);
                let mut writer = file;
                loop {
                    // Idle with no menu open: hang up; the helper exits, and its handlers with it.
                    let (req, reply) = match rx.recv_timeout(super::IDLE) {
                        Ok(r) => r,
                        Err(RecvTimeoutError::Timeout) if crate::app::menu_open() => continue,
                        Err(RecvTimeoutError::Timeout) => {
                            let mut l = LINK.lock().unwrap_or_else(|e| e.into_inner());
                            match rx.try_recv() {
                                // Something came in just now: serve it after all.
                                Ok(r) => r,
                                Err(_) => {
                                    *l = None;
                                    return;
                                }
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    if matches!(req, Request::Invoke(_)) && pid != 0 {
                        // What it starts may come to the front (we took the click).
                        unsafe {
                            let _ = AllowSetForegroundWindow(pid);
                        }
                    }
                    let Ok(mut line) = serde_json::to_string(&req) else { continue };
                    line.push('\n');
                    if writer.write_all(line.as_bytes()).and_then(|_| writer.flush()).is_err() {
                        break;
                    }
                    if let Some(reply) = reply {
                        let mut buf = String::new();
                        if reader.read_line(&mut buf).is_ok_and(|n| n == 0) || buf.is_empty() {
                            break;
                        }
                        let _ = reply.send(serde_json::from_str(&buf).unwrap_or_default());
                    }
                }
            }
        }
        // Gone (or never came): the next menu starts a new one.
        if let Ok(mut l) = LINK.lock() {
            *l = None;
        }
    }

    fn failed() {
        if let Ok(mut f) = LAST_FAIL.lock() {
            *f = Some(Instant::now());
        }
    }

    /// Starts the helper and has it scan the packages.
    pub fn warm() {
        if let Some(tx) = link() {
            let _ = tx.send((Request::Warm, None));
        }
    }

    /// The helper's items for `paths`, or none if it doesn't answer in time.
    pub fn build(paths: &[PathBuf], target: Target) -> Vec<Node> {
        // Twice: the link may hang up for idleness just as this asks (then it starts a new helper).
        for _ in 0..2 {
            let Some(tx) = link() else { return Vec::new() };
            let (reply_tx, reply_rx) = channel();
            let req = Request::Build { paths: paths.to_vec(), target: target.clone() };
            if tx.send((req, Some(reply_tx))).is_err() {
                continue;
            }
            match reply_rx.recv_timeout(WAIT) {
                Ok(nodes) => return nodes,
                Err(RecvTimeoutError::Disconnected) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    log!("finder: shell verbs helper didn't answer in time");
                    return Vec::new();
                }
            }
        }
        Vec::new()
    }

    /// Runs item `n` of the helper's last menu (never starts a helper: a new one wouldn't have it).
    pub fn invoke(n: u32) -> bool {
        let tx = LINK.lock().ok().and_then(|l| l.as_ref().map(|l| l.tx.clone()));
        tx.is_some_and(|tx| tx.send((Request::Invoke(n), None)).is_ok())
    }

    /// The helper process: serves one DockSide over pipe `name`, then exits.
    pub fn helper_main(name: &str) {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        if !name.starts_with(r"\\.\pipe\dockside-verbs-") {
            return;
        }
        let pipe = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(name),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                1 << 16,
                1 << 16,
                0,
                None,
            )
        };
        if pipe.is_invalid() {
            return;
        }
        // Nobody connecting within a while: DockSide gave up; don't linger.
        let _ = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(30));
            if !CONNECTED.load(std::sync::atomic::Ordering::Relaxed) {
                quit();
            }
        });
        // ERROR_PIPE_CONNECTED: DockSide got in before we started waiting, which is fine.
        if let Err(e) = unsafe { ConnectNamedPipe(pipe, None) } {
            if e.code().0 as u32 != 0x8007_0217 {
                return;
            }
        }
        CONNECTED.store(true, std::sync::atomic::Ordering::Relaxed);
        serve(pipe);
    }

    static CONNECTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Ends the helper process at once. A normal exit runs every loaded DLL's
    /// shutdown code, and the shell extensions loaded here can hang in it (the
    /// helper then never went away); there's nothing to flush.
    pub fn quit() -> ! {
        unsafe {
            use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
            let _ = TerminateProcess(GetCurrentProcess(), 0);
        }
        std::process::exit(0)
    }

    fn serve(pipe: HANDLE) {
        let file = unsafe { std::fs::File::from_raw_handle(pipe.0) };
        let Ok(read) = file.try_clone() else { return };
        let mut writer = file;
        for line in BufReader::new(read).lines() {
            let Ok(line) = line else { return };
            let Ok(req) = serde_json::from_str::<Request>(&line) else { continue };
            match req {
                // Only the scan: handlers load when a menu needs them.
                Request::Warm => drop(super::verbs()),
                Request::Build { paths, target } => {
                    let nodes = super::build(&paths, &target);
                    let Ok(mut out) = serde_json::to_string(&nodes) else { return };
                    out.push('\n');
                    if writer.write_all(out.as_bytes()).and_then(|_| writer.flush()).is_err() {
                        return;
                    }
                }
                Request::Invoke(n) => {
                    super::invoke_local(n);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns:desktop4="http://schemas.microsoft.com/appx/manifest/desktop/windows10/4">
  <!-- a comment -->
  <Applications><Application><Extensions>
    <com:Extension Category="windows.comServer"><com:SurrogateServer DisplayName="X"><com:Class Id="b41db860-64e4-11d2-9906-e49fadc173ca" Path="x.dll"/></com:SurrogateServer></com:Extension>
    <desktop4:Extension Category="windows.fileExplorerContextMenus">
      <desktop4:FileExplorerContextMenus>
        <desktop5:ItemType Type="*">
          <desktop5:Verb Id="Top" Clsid="b41db860-64e4-11d2-9906-e49fadc173ca" />
        </desktop5:ItemType>
        <desktop5:ItemType Type="Directory\Background">
          <desktop5:Verb Id="Here" Clsid="{9F156763-7844-4DC4-B2B1-901F640F5155}"/>
        </desktop5:ItemType>
      </desktop4:FileExplorerContextMenus>
    </desktop4:Extension>
    <uap:Extension Category="windows.fileTypeAssociation"><uap:SupportedFileTypes><uap:FileType>.rar</uap:FileType></uap:SupportedFileTypes></uap:Extension>
  </Extensions></Application></Applications>
</Package>"#;

    #[test]
    fn manifests_list_their_verbs() {
        let v = parse_manifest(MANIFEST);
        assert_eq!(
            v,
            vec![
                ("*".to_string(), GUID::from_u128(0xb41db860_64e4_11d2_9906_e49fadc173ca)),
                (r"directory\background".to_string(), GUID::from_u128(0x9f156763_7844_4dc4_b2b1_901f640f5155)),
            ]
        );
    }

    #[test]
    fn verbs_match_item_types() {
        let types = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let file = [(false, "png".to_string())];
        let dir = [(true, String::new())];
        let both = [(false, "png".to_string()), (true, String::new())];
        let items = |v: &[(bool, String)]| Target::Items(v.to_vec());
        assert!(applies(&types(&["*"]), &items(&file)));
        assert!(!applies(&types(&["*"]), &items(&dir)));
        assert!(applies(&types(&[".png"]), &items(&file)));
        assert!(!applies(&types(&[".jpg"]), &items(&file)));
        assert!(applies(&types(&["*", "directory"]), &items(&both)));
        assert!(!applies(&types(&["directory"]), &items(&both)));
        assert!(applies(&types(&[r"directory\background"]), &Target::Background));
        assert!(!applies(&types(&["directory"]), &Target::Background));
    }

    /// Lists what this machine's packaged handlers offer for a file and a folder
    /// (`cargo test -p dockside xcmd::tests::live -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn live() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let all = scan();
        for v in &all {
            println!("{:?} {:?}", v.clsid, v.types);
        }
        fn dump(e: &[MenuEntry], depth: usize) {
            for e in e {
                match e {
                    MenuEntry::Separator => println!("{}----", "  ".repeat(depth)),
                    MenuEntry::Item(i) => {
                        println!("{}{}{}", "  ".repeat(depth), i.label, if i.action.is_some() { "" } else { " (disabled)" });
                        dump(&i.submenu, depth + 1);
                    }
                }
            }
        }
        let file = std::env::temp_dir().join("dockside-xcmd-test.txt");
        let _ = std::fs::write(&file, "x");
        let act = |n: u32| Action::Open(n.to_string());
        super::super::shell::warm_menus();
        std::thread::sleep(Duration::from_secs(3));
        for pass in 0..2 {
            let t = Instant::now();
            println!("== file, classic menu, pass {pass}");
            if let Some(cm) = super::super::shell::item_menu(&[file.clone()]) {
                dump(&super::super::shell::menu_entries(cm, false, &act).entries, 1);
            }
            println!("   ({} ms)", t.elapsed().as_millis());
        }
        println!("== file");
        dump(&entries(&[file.clone()], Target::Items(vec![(false, "txt".into())]), &act), 1);
        println!("== folder");
        let dir = std::env::temp_dir();
        dump(&entries(&[dir.clone()], Target::Items(vec![(true, String::new())]), &act), 1);
        println!("== background");
        dump(&entries(&[dir.clone()], Target::Background, &act), 1);
    }

    /// Handlers load for a menu and are let go once the menu's been idle a while.
    /// (`cargo test -p dockside xcmd::tests::released_when_idle -- --ignored --nocapture`)
    #[test]
    #[ignore]
    fn released_when_idle() {
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, MSG, PM_REMOVE};
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let file = std::env::temp_dir().join("dockside-xcmd-idle.txt");
        let _ = std::fs::write(&file, "x");
        let act = |n: u32| Action::Open(n.to_string());
        let loaded = |what: &str| {
            let (h, o) = (HANDLERS.with(|h| h.borrow().values().filter(|c| c.is_some()).count()), OPEN.with(|o| o.borrow().1.len()));
            println!("{what}: {h} handlers loaded, {o} commands kept");
            h + o
        };
        assert!(!entries(&[file.clone()], Target::Items(vec![(false, "txt".into())]), &act).is_empty());
        assert!(loaded("after the menu") > 0);
        let start = Instant::now();
        while start.elapsed() < IDLE + Duration::from_secs(2) {
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(&file);
        assert_eq!(loaded(&format!("{} s later", start.elapsed().as_secs())), 0);
    }

    /// The helper's pipe protocol end to end, in this process: one thread
    /// serves as the helper, the other asks it for a file's items.
    /// (`cargo test -p dockside xcmd::tests::helper_round_trip -- --ignored --nocapture`)
    #[test]
    #[ignore]
    fn helper_round_trip() {
        let name = format!(r"\\.\pipe\dockside-verbs-test-{}", std::process::id());
        let n2 = name.clone();
        let helper = std::thread::spawn(move || remote::helper_main(&n2));
        let (tx, rx) = std::sync::mpsc::channel();
        let n3 = name.clone();
        std::thread::spawn(move || remote::serve_link(n3, rx));
        let file = std::env::temp_dir().join("dockside-xcmd-test.txt");
        let _ = std::fs::write(&file, "x");
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        tx.send((remote::Request::Build { paths: vec![file.clone()], target: Target::Items(vec![(false, "txt".into())]) }, Some(reply_tx))).unwrap();
        let nodes = reply_rx.recv_timeout(Duration::from_secs(20)).unwrap();
        let _ = std::fs::remove_file(&file);
        fn titles(n: &[Node], out: &mut Vec<String>) {
            for n in n {
                match n {
                    Node::Item { title, icon, .. } => out.push(format!("{title} (icon: {})", icon.is_some())),
                    Node::Menu { title, children, .. } => {
                        out.push(format!("{title} >"));
                        titles(children, out);
                    }
                    Node::Separator => {}
                }
            }
        }
        let mut t = Vec::new();
        titles(&nodes, &mut t);
        println!("{t:#?}");
        assert!(t.iter().any(|t| t.starts_with("WinRAR")), "WinRAR came over the pipe");
        // Idle: the link hangs up and the helper's serving ends (in the real helper, so does its process).
        let _keep = tx;
        let start = Instant::now();
        while !helper.is_finished() && start.elapsed() < IDLE + Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(100));
        }
        println!("helper finished: {} after {} ms", helper.is_finished(), start.elapsed().as_millis());
        assert!(helper.is_finished());
    }
}
