//! Gets `dockside_hook.dll` into the GUI threads of the programs allowed to have it.
//!
//! The hooks are per thread, never global: a global hook makes Windows map the DLL
//! into every GUI process, and even a DLL that refuses to stay has then been seen
//! by a kernel anti-cheat (BattlEye, EasyAntiCheat, Vanguard, ...). Instead a
//! worker thread watches windows being created (an out-of-context WinEvent, which
//! loads nothing anywhere), vets each new process in `guard` (anti-cheat, the
//! user's blacklist, 32-bit, protected, higher integrity) and only then hooks the
//! threads of the processes that pass.
//!
//! Windows calls the hook procedures of a thread it can't load the DLL into on the
//! thread that installed them, so the worker keeps pumping messages.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, FreeLibrary, BOOL, HANDLE, HMODULE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId, OpenProcess, OpenThread, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, THREAD_SYNCHRONIZE};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::guard::{self, Rules};
use super::shared::{CBT_PROC, GETMSG_PROC};

/// Posted to the worker: lParam is a `Box<Rules>`.
const WM_RULES: u32 = WM_APP + 1;
/// How long a thread keeps its WH_GETMESSAGE hook: just long enough to load the DLL
/// in a program that is already running (WH_CBT fires only on window events).
const PRELOAD_FOR: Duration = Duration::from_millis(1500);
/// Without a thread handle (OpenThread refused) a verdict is trusted this long.
const BLIND_FOR: Duration = Duration::from_secs(30);

struct ThreadEntry {
    /// SYNCHRONIZE handle: signalled once the thread is gone (its id may come back).
    thread: Option<HANDLE>,
    pid: u32,
    cbt: Option<HHOOK>,
    since: Instant,
}

impl ThreadEntry {
    fn alive(&self) -> bool {
        match self.thread {
            Some(h) => (unsafe { WaitForSingleObject(h, 0) }) != WAIT_OBJECT_0,
            None => self.since.elapsed() < BLIND_FOR,
        }
    }
}

struct ProcEntry {
    process: Option<HANDLE>,
    allowed: bool,
    since: Instant,
}

impl ProcEntry {
    fn alive(&self) -> bool {
        match self.process {
            Some(h) => (unsafe { WaitForSingleObject(h, 0) }) != WAIT_OBJECT_0,
            None => self.since.elapsed() < BLIND_FOR,
        }
    }
}

fn close(h: Option<HANDLE>) {
    if let Some(h) = h {
        unsafe {
            let _ = CloseHandle(h);
        }
    }
}

struct Tracker {
    dll: HMODULE,
    cbt_proc: HOOKPROC,
    getmsg_proc: HOOKPROC,
    rules: Rules,
    own_pid: u32,
    own_integrity: u32,
    threads: HashMap<u32, ThreadEntry>,
    procs: HashMap<u32, ProcEntry>,
    /// WH_GETMESSAGE hooks to remove once their time is up.
    preload: Vec<(HHOOK, Instant)>,
    preload_timer: usize,
}

thread_local! {
    static TRACKER: RefCell<Option<Tracker>> = const { RefCell::new(None) };
}

impl Tracker {
    fn process_allowed(&mut self, pid: u32) -> bool {
        if pid == self.own_pid {
            // DockSide loaded the DLL itself; it animates its own app windows.
            return true;
        }
        if let Some(e) = self.procs.get(&pid) {
            if e.alive() {
                return e.allowed;
            }
            close(self.procs.remove(&pid).and_then(|e| e.process));
        }
        let (process, verdict) = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(h) => (Some(h), guard::probe(h, pid, &self.rules, self.own_integrity)),
            Err(_) => (None, Some("can't be opened".to_string())),
        };
        if let Some(why) = &verdict {
            let name = process.and_then(guard::image_path).unwrap_or_else(|| format!("pid {pid}"));
            log!("genie: leaving {name} alone: {why}");
        }
        let allowed = verdict.is_none();
        self.procs.insert(pid, ProcEntry { process, allowed, since: Instant::now() });
        allowed
    }

    /// Hooks the thread that owns top-level window `hwnd`, if its process may have
    /// the DLL. `tid` is the window's thread when the caller already knows it.
    fn consider(&mut self, hwnd: HWND, tid: u32) {
        if let Some(e) = self.threads.get(&tid) {
            if e.alive() {
                return;
            }
        }
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if tid == 0 || pid == 0 {
            return;
        }
        if let Some(e) = self.threads.get(&tid) {
            if e.alive() {
                return;
            }
            self.drop_thread(tid);
        }
        let allowed = self.process_allowed(pid);
        let thread = unsafe { OpenThread(THREAD_SYNCHRONIZE, false, tid) }.ok();
        let mut cbt = None;
        if allowed {
            unsafe {
                cbt = SetWindowsHookExW(WH_CBT, self.cbt_proc, self.dll, tid).ok();
                if cbt.is_some() && pid != self.own_pid {
                    // Loads the DLL at the thread's next message rather than its next window event.
                    if let Ok(h) = SetWindowsHookExW(WH_GETMESSAGE, self.getmsg_proc, self.dll, tid) {
                        self.preload.push((h, Instant::now()));
                        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
                        if self.preload_timer == 0 {
                            self.preload_timer = SetTimer(None, 0, 250, None);
                        }
                    }
                }
            }
        }
        self.threads.insert(tid, ThreadEntry { thread, pid, cbt, since: Instant::now() });
    }

    fn drop_thread(&mut self, tid: u32) {
        if let Some(e) = self.threads.remove(&tid) {
            if let Some(h) = e.cbt {
                unsafe {
                    let _ = UnhookWindowsHookEx(h);
                }
            }
            close(e.thread);
        }
    }

    fn scan(&mut self) {
        unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
            (*(lp.0 as *mut Vec<HWND>)).push(hwnd);
            BOOL(1)
        }
        let mut all: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut all as *mut _ as isize));
        }
        for hwnd in all {
            self.consider(hwnd, 0);
        }
    }

    /// New rules: threads of processes they now block lose their hook (the DLL itself
    /// stays in those until they exit: it never unloads mid-flight), and everything
    /// is looked at again.
    fn set_rules(&mut self, rules: Rules) {
        if rules == self.rules {
            return;
        }
        self.rules = rules;
        for (_, e) in self.procs.drain() {
            close(e.process);
        }
        let tids: Vec<u32> = self.threads.keys().copied().collect();
        for tid in tids {
            let pid = self.threads[&tid].pid;
            if !self.process_allowed(pid) || self.threads[&tid].cbt.is_none() {
                self.drop_thread(tid);
            }
        }
        self.scan();
    }

    fn on_timer(&mut self, id: usize) {
        if id != self.preload_timer {
            return;
        }
        let now = Instant::now();
        self.preload.retain(|&(h, at)| {
            let keep = now.duration_since(at) < PRELOAD_FOR;
            if !keep {
                unsafe {
                    let _ = UnhookWindowsHookEx(h);
                }
            }
            keep
        });
        if self.preload.is_empty() {
            unsafe {
                let _ = KillTimer(None, self.preload_timer);
            }
            self.preload_timer = 0;
            // A quiet moment: forget threads and processes that are gone.
            let dead: Vec<u32> = self.threads.iter().filter(|(_, e)| !e.alive()).map(|(&t, _)| t).collect();
            for tid in dead {
                self.drop_thread(tid);
            }
            let dead: Vec<u32> = self.procs.iter().filter(|(_, e)| !e.alive()).map(|(&p, _)| p).collect();
            for pid in dead {
                close(self.procs.remove(&pid).and_then(|e| e.process));
            }
        }
    }

    fn shutdown(&mut self) {
        for (h, _) in self.preload.drain(..) {
            unsafe {
                let _ = UnhookWindowsHookEx(h);
            }
        }
        let tids: Vec<u32> = self.threads.keys().copied().collect();
        for tid in tids {
            if self.threads[&tid].cbt.is_some() {
                // Windows drops its reference to the DLL there the next time the thread
                // handles a message.
                unsafe {
                    let _ = PostThreadMessageW(tid, WM_NULL, WPARAM(0), LPARAM(0));
                }
            }
            self.drop_thread(tid);
        }
        for (_, e) in self.procs.drain() {
            close(e.process);
        }
    }
}

unsafe extern "system" fn on_create(_: HWINEVENTHOOK, _event: u32, hwnd: HWND, id_object: i32, id_child: i32, tid: u32, _: u32) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.is_invalid() {
        return;
    }
    TRACKER.with(|t| {
        let Ok(mut t) = t.try_borrow_mut() else { return };
        let Some(t) = t.as_mut() else { return };
        // Cheapest first: most windows come from threads already looked at.
        if t.threads.get(&tid).is_some_and(|e| e.alive()) {
            return;
        }
        if GetAncestor(hwnd, GA_PARENT) != GetDesktopWindow() {
            return; // a child or message-only window
        }
        t.consider(hwnd, tid);
    });
}

fn with_tracker(f: impl FnOnce(&mut Tracker)) {
    TRACKER.with(|t| {
        if let Ok(mut t) = t.try_borrow_mut() {
            if let Some(t) = t.as_mut() {
                f(t);
            }
        }
    });
}

fn worker(rules: Rules, ready: mpsc::Sender<Option<u32>>) {
    unsafe {
        // A message queue before anyone posts to us, then DockSide's UI goes on:
        // writing and loading the DLL can take seconds at login (antivirus scans
        // it), and posts wait in the queue meanwhile.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let _ = ready.send(Some(GetCurrentThreadId()));
        let path = match write_dll() {
            Ok(p) => p,
            Err(e) => return log!("genie: writing the hook DLL failed: {e}"),
        };
        let dll = match LoadLibraryExW(&HSTRING::from(path.as_os_str()), None, LOAD_WITH_ALTERED_SEARCH_PATH) {
            Ok(d) => d,
            Err(e) => return log!("genie: loading {} failed: {e}", path.display()),
        };
        let procs = (GetProcAddress(dll, windows::core::PCSTR(CBT_PROC.as_ptr())), GetProcAddress(dll, windows::core::PCSTR(GETMSG_PROC.as_ptr())));
        let (Some(cbt_proc), Some(getmsg_proc)) = procs else {
            log!("genie: hook procedures not exported by {}", path.display());
            let _ = FreeLibrary(dll);
            return;
        };

        TRACKER.with(|t| {
            *t.borrow_mut() = Some(Tracker {
                dll,
                cbt_proc: Some(std::mem::transmute(cbt_proc)),
                getmsg_proc: Some(std::mem::transmute(getmsg_proc)),
                rules,
                own_pid: GetCurrentProcessId(),
                own_integrity: guard::own_integrity(),
                threads: HashMap::new(),
                procs: HashMap::new(),
                preload: Vec::new(),
                preload_timer: 0,
            })
        });
        let event = SetWinEventHook(EVENT_OBJECT_CREATE, EVENT_OBJECT_CREATE, None, Some(on_create), 0, 0, WINEVENT_OUTOFCONTEXT);
        if event.is_invalid() {
            log!("genie: SetWinEventHook(EVENT_OBJECT_CREATE) failed");
        }
        // Programs that are already running are vetted here.
        with_tracker(|t| t.scan());
        with_tracker(|t| log!("genie: hooked {} threads ({})", t.threads.values().filter(|e| e.cbt.is_some()).count(), path.display()));

        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd.is_invalid() || msg.hwnd.0.is_null() {
                match msg.message {
                    WM_RULES => {
                        let rules = Box::from_raw(msg.lParam.0 as *mut Rules);
                        with_tracker(|t| t.set_rules(*rules));
                        continue;
                    }
                    WM_TIMER => {
                        with_tracker(|t| t.on_timer(msg.wParam.0));
                        continue;
                    }
                    _ => {}
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if !event.is_invalid() {
            let _ = UnhookWinEvent(event);
        }
        let tracker = TRACKER.with(|t| t.borrow_mut().take());
        if let Some(mut t) = tracker {
            if t.preload_timer != 0 {
                let _ = KillTimer(None, t.preload_timer);
            }
            t.shutdown();
        }
        // Rules posted after the quit would leak; drain them.
        while PeekMessageW(&mut msg, None, WM_RULES, WM_RULES, PM_REMOVE).as_bool() {
            drop(Box::from_raw(msg.lParam.0 as *mut Rules));
        }
        let _ = FreeLibrary(dll);
        log!("genie: hooks removed");
    }
}

pub struct Injector {
    thread: Option<JoinHandle<()>>,
    tid: u32,
}

impl Injector {
    pub fn start(rules: Rules) -> Option<Injector> {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("genie-injector".into())
            .spawn(move || worker(rules, tx))
            .map_err(|e| log!("genie: injector thread: {e}"))
            .ok()?;
        match rx.recv() {
            Ok(Some(tid)) => Some(Injector { thread: Some(thread), tid }),
            _ => {
                let _ = thread.join();
                None
            }
        }
    }

    /// The blacklist changed: hooks follow it from now on.
    pub fn set_rules(&self, rules: Rules) {
        let ptr = Box::into_raw(Box::new(rules));
        if unsafe { PostThreadMessageW(self.tid, WM_RULES, WPARAM(0), LPARAM(ptr as isize)) }.is_err() {
            drop(unsafe { Box::from_raw(ptr) });
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `dockside_hook.dll`, built by build.rs, so DockSide ships as a single exe.
static HOOK_DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dockside_hook.dll"));

/// Writes the embedded DLL out under a name of its own for this build: every app
/// it's loaded into keeps the file open until it next handles a message after we
/// unhook, so a newer DockSide can't overwrite it in place.
fn write_dll() -> std::io::Result<PathBuf> {
    // FNV-1a, so each distinct DLL gets its own file name.
    let hash = HOOK_DLL.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    let dir = crate::config::dir().join("hook");
    std::fs::create_dir_all(&dir)?;
    let name = format!("dockside_hook-{hash:016x}.dll");
    // Older copies go once no app has them open any more.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.file_name() != name.as_str() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let path = dir.join(name);
    if std::fs::metadata(&path).ok().map(|m| m.len()) != Some(HOOK_DLL.len() as u64) {
        // Written beside and renamed into place, so a half-written DLL is never loaded.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, HOOK_DLL)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}
