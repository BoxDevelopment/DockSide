//! Java Access Bridge: the menus of Java apps (JetBrains IDEs and other Swing
//! apps), which UI Automation can't see.
//!
//! The bridge talks over window messages, so it runs on a thread of its own
//! with a message loop; the app-menus worker hands it jobs and waits (with a
//! timeout) for the answers. It only works once Access Bridge is switched on
//! in Windows (`jabswitch -enable`, then restart the Java app); until then
//! every read comes back empty and the app gets its curated menus.

use std::collections::VecDeque;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{s, PCWSTR};
use windows::Win32::Foundation::{BOOL, HMODULE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage, MSG, PM_NOREMOVE, PM_REMOVE, WM_APP,
};

use super::appmenu::{Item, Kind};

const MAX_STRING: usize = 1024;
const SHORT_STRING: usize = 256;
const MAX_ACTIONS_TO_DO: usize = 32;
const MAX_KEY_BINDINGS: usize = 10;
/// A job for the bridge thread is waiting.
const WM_JOB: u32 = WM_APP + 1;
/// How long a job may take before the worker stops waiting for it.
const JOB_WAIT: Duration = Duration::from_millis(1500);
/// How long an opened menu's items may take to show up.
const OPEN_WAIT: Duration = Duration::from_millis(600);
/// How far down a window, and over how many elements, its menu bar is looked for.
const BAR_DEPTH: usize = 8;
const BAR_NODES: usize = 300;

#[repr(C)]
struct ContextInfo {
    name: [u16; MAX_STRING],
    description: [u16; MAX_STRING],
    role: [u16; SHORT_STRING],
    role_en_us: [u16; SHORT_STRING],
    states: [u16; SHORT_STRING],
    states_en_us: [u16; SHORT_STRING],
    index_in_parent: i32,
    children_count: i32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    accessible_component: BOOL,
    accessible_action: BOOL,
    accessible_selection: BOOL,
    accessible_text: BOOL,
    accessible_interfaces: BOOL,
}

#[repr(C)]
struct ActionsToDo {
    count: i32,
    actions: [[u16; SHORT_STRING]; MAX_ACTIONS_TO_DO],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyBinding {
    character: u16,
    modifiers: i32,
}

#[repr(C)]
struct KeyBindings {
    count: i32,
    bindings: [KeyBinding; MAX_KEY_BINDINGS],
}

type Obj = i64;

/// The bridge's functions (`WindowsAccessBridge-64.dll`).
struct Api {
    is_java_window: unsafe extern "C" fn(HWND) -> BOOL,
    context_from_hwnd: unsafe extern "C" fn(HWND, *mut i32, *mut Obj) -> BOOL,
    context_info: unsafe extern "C" fn(i32, Obj, *mut ContextInfo) -> BOOL,
    child: unsafe extern "C" fn(i32, Obj, i32) -> Obj,
    release: unsafe extern "C" fn(i32, Obj),
    do_actions: unsafe extern "C" fn(i32, Obj, *mut ActionsToDo, *mut i32) -> BOOL,
    key_bindings: unsafe extern "C" fn(i32, Obj, *mut KeyBindings) -> BOOL,
}

/// A Java object the bridge holds for us (let go with `Api::drop_ctx`).
struct Ctx {
    vm: i32,
    obj: Obj,
}

struct Info {
    name: String,
    role: String,
    states: String,
    children: i32,
}

impl Api {
    fn load(dll: &std::path::Path) -> Option<(Api, unsafe extern "C" fn())> {
        let wide: Vec<u16> = dll.as_os_str().encode_wide_z();
        let m: HMODULE = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.ok()?;
        macro_rules! f {
            ($name:expr) => {
                unsafe { std::mem::transmute(GetProcAddress(m, $name)?) }
            };
        }
        let run: unsafe extern "C" fn() = f!(s!("Windows_run"));
        Some((
            Api {
                is_java_window: f!(s!("isJavaWindow")),
                context_from_hwnd: f!(s!("getAccessibleContextFromHWND")),
                context_info: f!(s!("getAccessibleContextInfo")),
                child: f!(s!("getAccessibleChildFromContext")),
                release: f!(s!("releaseJavaObject")),
                do_actions: f!(s!("doAccessibleActions")),
                key_bindings: f!(s!("getAccessibleKeyBindings")),
            },
            run,
        ))
    }

    fn info(&self, c: &Ctx) -> Option<Info> {
        let mut i: Box<ContextInfo> = unsafe { Box::new(std::mem::zeroed()) };
        unsafe { (self.context_info)(c.vm, c.obj, &mut *i) }.as_bool().then(|| Info {
            name: super::from_wide(&i.name),
            role: super::from_wide(&i.role_en_us),
            states: super::from_wide(&i.states_en_us),
            children: i.children_count,
        })
    }

    fn child(&self, c: &Ctx, i: i32) -> Option<Ctx> {
        let obj = unsafe { (self.child)(c.vm, c.obj, i) };
        (obj != 0).then_some(Ctx { vm: c.vm, obj })
    }

    fn drop_ctx(&self, c: Ctx) {
        unsafe { (self.release)(c.vm, c.obj) };
    }

    fn click(&self, c: &Ctx) -> bool {
        let mut todo: Box<ActionsToDo> = unsafe { Box::new(std::mem::zeroed()) };
        todo.count = 1;
        for (k, ch) in "click".encode_utf16().enumerate() {
            todo.actions[0][k] = ch;
        }
        let mut failed = -1;
        unsafe { (self.do_actions)(c.vm, c.obj, &mut *todo, &mut failed) }.as_bool()
    }

    /// The item's shortcut ("Ctrl+Shift+F"), from its key bindings: the last
    /// one with modifiers (the others are mnemonics).
    fn accel(&self, c: &Ctx) -> String {
        let mut kb: Box<KeyBindings> = unsafe { Box::new(std::mem::zeroed()) };
        if !unsafe { (self.key_bindings)(c.vm, c.obj, &mut *kb) }.as_bool() {
            return String::new();
        }
        const SHIFT: i32 = 1;
        const CTRL: i32 = 2;
        const META: i32 = 4;
        const ALT: i32 = 8;
        const FKEY: i32 = 256;
        const CONTROL_CODE: i32 = 512;
        let n = (kb.count.max(0) as usize).min(MAX_KEY_BINDINGS);
        let Some(b) = kb.bindings[..n].iter().rev().find(|b| b.modifiers & (SHIFT | CTRL | META | ALT | FKEY) != 0) else {
            return String::new();
        };
        let mut s = String::new();
        for (bit, name) in [(CTRL, "Ctrl+"), (META, "Win+"), (ALT, "Alt+"), (SHIFT, "Shift+")] {
            if b.modifiers & bit != 0 {
                s.push_str(name);
            }
        }
        let key = if b.modifiers & FKEY != 0 {
            format!("F{}", b.character)
        } else if b.modifiers & CONTROL_CODE != 0 {
            // Java's virtual key codes for the keys menus use.
            match b.character {
                8 => "Backspace".into(),
                9 => "Tab".into(),
                10 => "Enter".into(),
                27 => "Esc".into(),
                32 => "Space".into(),
                33 => "PgUp".into(),
                34 => "PgDn".into(),
                35 => "End".into(),
                36 => "Home".into(),
                37 => "Left".into(),
                38 => "Up".into(),
                39 => "Right".into(),
                40 => "Down".into(),
                127 => "Delete".into(),
                155 => "Insert".into(),
                _ => return String::new(),
            }
        } else {
            match char::from_u32(b.character as u32) {
                Some(c) if !c.is_control() => c.to_uppercase().to_string(),
                _ => return String::new(),
            }
        };
        s.push_str(&key);
        s
    }
}

trait WideZ {
    fn encode_wide_z(&self) -> Vec<u16>;
}

impl WideZ for std::ffi::OsStr {
    fn encode_wide_z(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(Some(0)).collect()
    }
}

type Job = Box<dyn FnOnce(&Api) + Send>;

/// The bridge thread: started for the first Java window, with the bridge DLL
/// found next to that app's Java runtime (or installed system-wide).
pub struct Bridge {
    jobs: Arc<Mutex<VecDeque<Job>>>,
    thread: u32,
}

static BRIDGE: OnceLock<Option<Bridge>> = OnceLock::new();

/// Whether `hwnd` (of the app at `exe`) is a Java window, starting the bridge if needed.
pub fn bridge_for(exe: &str) -> Option<&'static Bridge> {
    BRIDGE.get_or_init(|| Bridge::start(exe)).as_ref()
}

impl Bridge {
    fn start(exe: &str) -> Option<Bridge> {
        // The app's own runtime first (JetBrains ships one), then a system-wide install.
        let dir = std::path::Path::new(exe).parent()?.to_path_buf();
        let candidates = [
            dir.join("..").join("jbr").join("bin").join("windowsaccessbridge-64.dll"),
            dir.join("jbr").join("bin").join("windowsaccessbridge-64.dll"),
            dir.join("windowsaccessbridge-64.dll"),
            std::path::PathBuf::from("WindowsAccessBridge-64.dll"),
        ];
        let dll = candidates.into_iter().find(|p| p.exists() || p.parent().is_none_or(|d| d.as_os_str().is_empty()))?;
        let jobs: Arc<Mutex<VecDeque<Job>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (tx, rx) = channel();
        let queue = jobs.clone();
        let spawned = std::thread::Builder::new().name("java-access-bridge".into()).spawn(move || {
            let Some((api, run)) = Api::load(&dll) else {
                log!("app menus: no Java Access Bridge at {}", dll.display());
                let _ = tx.send(None);
                return;
            };
            let mut msg = MSG::default();
            unsafe {
                // Make this thread's message queue, then say hello to running Java apps.
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                run();
            }
            // Let the Java apps answer before the first question (jobs only
            // start arriving once the thread's id is handed out, below).
            let until = Instant::now() + Duration::from_millis(400);
            while Instant::now() < until {
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = tx.send(Some(unsafe { GetCurrentThreadId() }));
            loop {
                let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                if got.0 <= 0 {
                    break;
                }
                if msg.hwnd.is_invalid() && msg.message == WM_JOB {
                    while let Some(job) = queue.lock().ok().and_then(|mut q| q.pop_front()) {
                        job(&api);
                    }
                    continue;
                }
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });
        if spawned.is_err() {
            return None;
        }
        let thread = rx.recv_timeout(Duration::from_secs(2)).ok().flatten()?;
        Some(Bridge { jobs, thread })
    }

    /// Runs `f` on the bridge thread; `None` if it didn't finish in time.
    fn call<R: Send + 'static>(&self, f: impl FnOnce(&Api) -> R + Send + 'static) -> Option<R> {
        let (tx, rx) = channel::<R>();
        let job: Job = Box::new(move |api| {
            let _ = tx.send(f(api));
        });
        self.jobs.lock().ok()?.push_back(job);
        unsafe { PostThreadMessageW(self.thread, WM_JOB, WPARAM(0), LPARAM(0)) }.ok()?;
        rx.recv_timeout(JOB_WAIT).ok()
    }

    /// `hwnd`'s menu titles, if it's a Java window with a menu bar.
    pub fn titles(&self, hwnd: HWND) -> Option<Vec<Item>> {
        let h = hwnd.0 as isize;
        self.call(move |api| {
            let bar = menu_bar(api, HWND(h as _))?;
            let items = items(api, &bar);
            api.drop_ctx(bar);
            Some(items).filter(|v: &Vec<Item>| !v.is_empty())
        })
        .flatten()
    }

    /// The items of the menu at `path` (indices and names from the bar down).
    pub fn menu(&self, hwnd: HWND, path: Vec<(usize, String)>) -> Option<Vec<Item>> {
        let h = hwnd.0 as isize;
        self.call(move |api| {
            let chain = walk(api, HWND(h as _), &path)?;
            let items = chain.last().map(|(m, _)| items(api, m)).unwrap_or_default();
            close(api, chain);
            Some(items)
        })
        .flatten()
    }

    /// Chooses the item at `path`.
    pub fn invoke(&self, hwnd: HWND, path: Vec<(usize, String)>) -> bool {
        let h = hwnd.0 as isize;
        self.call(move |api| {
            let Some(((last, name), parent)) = path.split_last() else { return false };
            let Some(chain) = walk(api, HWND(h as _), parent) else { return false };
            let item = chain.last().and_then(|(m, _)| api.child(m, *last as i32));
            let ok = item.as_ref().is_some_and(|it| {
                api.info(it).is_some_and(|i| &tidy(&i.name) == name && i.states.contains("enabled")) && api.click(it)
            });
            if let Some(it) = item {
                api.drop_ctx(it);
            }
            if ok {
                // Choosing an item closes its menus.
                for (c, _) in chain {
                    api.drop_ctx(c);
                }
            } else {
                close(api, chain);
            }
            ok
        })
        .unwrap_or(false)
    }
}

fn tidy(s: &str) -> String {
    let s = s.trim();
    match s.strip_suffix("...") {
        Some(head) => format!("{}…", head.trim_end()),
        None => s.to_string(),
    }
}

/// The window's menu bar, depth first (it comes early: root pane, layered pane, menu bar).
fn menu_bar(api: &Api, hwnd: HWND) -> Option<Ctx> {
    if !unsafe { (api.is_java_window)(hwnd) }.as_bool() {
        return None;
    }
    let (mut vm, mut obj) = (0i32, 0 as Obj);
    if !unsafe { (api.context_from_hwnd)(hwnd, &mut vm, &mut obj) }.as_bool() || obj == 0 {
        return None;
    }
    fn go(api: &Api, c: Ctx, left: usize, seen: &mut usize) -> Option<Ctx> {
        let info = api.info(&c);
        if info.as_ref().is_some_and(|i| i.role == "menu bar") {
            return Some(c);
        }
        let n = info.map_or(0, |i| i.children);
        if left > 0 {
            for k in 0..n.min(64) {
                *seen += 1;
                if *seen > BAR_NODES {
                    break;
                }
                if let Some(ch) = api.child(&c, k) {
                    if let Some(found) = go(api, ch, left - 1, seen) {
                        api.drop_ctx(c);
                        return Some(found);
                    }
                }
            }
        }
        api.drop_ctx(c);
        None
    }
    go(api, Ctx { vm, obj }, BAR_DEPTH, &mut 0)
}

/// A menu's (or the bar's) items.
fn items(api: &Api, menu: &Ctx) -> Vec<Item> {
    let n = api.info(menu).map_or(0, |i| i.children);
    let mut out = Vec::new();
    for k in 0..n.min(200) {
        let Some(c) = api.child(menu, k) else { continue };
        if let Some(i) = api.info(&c) {
            let index = k as usize;
            let showing = i.states.contains("visible");
            if i.role == "separator" {
                out.push(Item { index, name: String::new(), accel: String::new(), enabled: false, checked: false, kind: Kind::Separator });
            } else if !i.name.trim().is_empty() && showing {
                let kind = if i.role == "menu" { Kind::Submenu } else { Kind::Command };
                out.push(Item {
                    index,
                    name: tidy(&i.name),
                    accel: if kind == Kind::Command { api.accel(&c) } else { String::new() },
                    enabled: i.states.contains("enabled"),
                    checked: i.states.contains("checked"),
                    kind,
                });
            }
        }
        api.drop_ctx(c);
    }
    out
}

/// Menus along a path, each with whether we opened it; the last is the one asked for.
type Chain = Vec<(Ctx, bool)>;

/// Walks `path` from the bar, opening menus whose items are only made when
/// they open (IntelliJ's). Menus passed through without opening are let go.
fn walk(api: &Api, hwnd: HWND, path: &[(usize, String)]) -> Option<Chain> {
    let mut chain: Chain = vec![(menu_bar(api, hwnd)?, false)];
    for (pos, name) in path {
        let next = chain.last().and_then(|(m, _)| api.child(m, *pos as i32));
        let Some(next) = next else {
            close(api, chain);
            return None;
        };
        let info = api.info(&next);
        if !info.as_ref().is_some_and(|i| &tidy(&i.name) == name) {
            api.drop_ctx(next);
            close(api, chain);
            return None;
        }
        if chain.last().is_some_and(|(_, opened)| !opened) {
            if let Some((c, _)) = chain.pop() {
                api.drop_ctx(c);
            }
        }
        // Empty until opened: open it, and wait for its items.
        let opened = info.is_some_and(|i| i.children == 0) && api.click(&next);
        if opened {
            let until = Instant::now() + OPEN_WAIT;
            while Instant::now() < until && api.info(&next).is_some_and(|i| i.children == 0) {
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        chain.push((next, opened));
    }
    Some(chain)
}

/// Closes the menus a walk opened, innermost first (clicking an open menu
/// closes it), and lets go of them all.
fn close(api: &Api, chain: Chain) {
    for (c, opened) in chain.into_iter().rev() {
        if opened {
            api.click(&c);
        }
        api.drop_ctx(c);
    }
}
