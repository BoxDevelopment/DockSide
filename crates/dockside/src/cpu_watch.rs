//! Logs what DockSide was busy with whenever its CPU use jumps, so a user's
//! log can say what caused a spike: which threads used the time and, on the
//! UI thread, which window messages and how much glass rendering.
//!
//! A background thread samples the process's CPU time every second. A sample
//! over `THRESHOLD` (as a share of the whole machine, like Task Manager shows
//! it) writes one `cpu:` line.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HMODULE};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetProcAddress, GetModuleHandleW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, GetProcessTimes, GetThreadDescription, GetThreadTimes, OpenThread,
    THREAD_QUERY_INFORMATION, THREAD_QUERY_LIMITED_INFORMATION,
};
use windows::core::{s, w, PCWSTR};

/// How often the process's CPU time is sampled.
const SAMPLE: Duration = Duration::from_secs(1);
/// A sample using more than this share of the whole machine (percent) is logged.
const THRESHOLD: f64 = 8.0;
/// Threads using less than this share (percent of the machine) aren't named in the line.
const THREAD_MIN: f64 = 0.5;
/// Stack samples (see `cpu_stacks`) are taken at most this often while CPU use stays high.
const STACKS_EVERY: Duration = Duration::from_secs(30);
/// Threads using less than this share in the last second aren't stack-sampled.
const STACK_THREAD_MIN: f64 = 0.1;

/// The UI thread's id (0 until `start`).
static UI_THREAD: AtomicU32 = AtomicU32::new(0);
/// Glass work on the UI thread since the last sample: frames that arrived from
/// window captures, panes rendered, and the time spent rendering (microseconds).
static GLASS_FRAMES: AtomicU32 = AtomicU32::new(0);
static GLASS_RENDERS: AtomicU32 = AtomicU32::new(0);
static GLASS_US: AtomicU64 = AtomicU64::new(0);

/// UI-thread window messages since the last sample: (message, timer id) to (count, time).
static MESSAGES: Mutex<Option<HashMap<(u32, usize), (u32, Duration)>>> = Mutex::new(None);

/// Starts watching. Call once, on the UI thread.
pub fn start() {
    UI_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    let spawned = std::thread::Builder::new().name("cpu watch".into()).spawn(run);
    if let Err(e) = spawned {
        log!("cpu watch didn't start: {e}");
    }
}

/// A window message took `took` on the UI thread.
pub fn ui_message(msg: u32, wp: usize, took: Duration) {
    // Timers are told apart by id; other messages' wParams are just data.
    let key = (msg, if msg == 0x0113 { wp } else { 0 });
    if let Ok(mut m) = MESSAGES.lock() {
        let e = m.get_or_insert_with(HashMap::new).entry(key).or_default();
        e.0 += 1;
        e.1 += took;
    }
}

/// A window capture under glass delivered a frame.
pub fn glass_frame() {
    GLASS_FRAMES.fetch_add(1, Ordering::Relaxed);
}

/// Glass rendered `panes` panes in `took`.
pub fn glass_render(panes: u32, took: Duration) {
    GLASS_RENDERS.fetch_add(panes, Ordering::Relaxed);
    GLASS_US.fetch_add(took.as_micros() as u64, Ordering::Relaxed);
}

fn ticks(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// Kernel + user time of the process, in 100 ns ticks.
fn process_time() -> u64 {
    let (mut a, mut b, mut k, mut u) = Default::default();
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u).is_err() {
            return 0;
        }
    }
    ticks(k) + ticks(u)
}

struct Thread {
    /// Its name, or for an unnamed thread the module it started in: threads
    /// of one kind (a thread pool's workers) share it and are counted together.
    kind: String,
    time: u64,
}

fn run() {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    // DOCKSIDE_CPU_LOG=<percent> sets another threshold (0 logs every second).
    let threshold = std::env::var("DOCKSIDE_CPU_LOG").ok().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(THRESHOLD);
    // DOCKSIDE_CPU_STACKS=0 turns stack samples off.
    let stacks_on = cfg!(target_arch = "x86_64") && std::env::var("DOCKSIDE_CPU_STACKS").map_or(true, |v| v.trim() != "0");
    let mut stacks_at: Option<Instant> = None;
    log!("cpu watch: on (build {:08x}, stack samples {})", crate::cpu_stacks::build_stamp(), if stacks_on { "on" } else { "off" });
    let pid = unsafe { GetCurrentProcessId() };
    let mut threads: HashMap<u32, Thread> = HashMap::new();
    let mut last = (Instant::now(), process_time());
    sample_threads(pid, &mut threads);
    loop {
        std::thread::sleep(SAMPLE);
        let now = (Instant::now(), process_time());
        let wall = now.0.duration_since(last.0).as_secs_f64();
        // Percent of the whole machine: 100 is every core busy.
        let share = |ticks: u64| ticks as f64 / 1e7 / wall / cpus * 100.0;
        let total = share(now.1.saturating_sub(last.1));
        last = now;
        let busy = sample_threads(pid, &mut threads);
        let messages = MESSAGES.lock().ok().and_then(|mut m| m.take()).unwrap_or_default();
        let glass = (GLASS_FRAMES.swap(0, Ordering::Relaxed), GLASS_RENDERS.swap(0, Ordering::Relaxed), GLASS_US.swap(0, Ordering::Relaxed));
        if total < threshold || wall <= 0.0 {
            continue;
        }

        // Threads of a kind (a thread pool's workers, say) count together: many
        // each using a little add up to as much as one busy thread.
        let mut groups: HashMap<&str, (f64, u32)> = HashMap::new();
        for (tid, d) in &busy {
            let name = threads.get(tid).map_or("?", |t| t.kind.as_str());
            let g = groups.entry(name).or_default();
            g.0 += share(*d);
            g.1 += (*d > 0) as u32;
        }
        let mut parts: Vec<(&str, f64, u32)> = groups.into_iter().map(|(n, (p, c))| (n, p, c)).filter(|(_, p, _)| *p >= THREAD_MIN).collect();
        parts.sort_by(|a, b| b.1.total_cmp(&a.1));
        let by_thread = if parts.is_empty() {
            format!("none over {THREAD_MIN}%")
        } else {
            parts
                .iter()
                .take(8)
                .map(|(n, p, c)| if *c > 1 { format!("{n} x{c} {p:.1}%") } else { format!("{n} {p:.1}%") })
                .collect::<Vec<_>>()
                .join(", ")
        };

        let mut msgs: Vec<_> = messages.into_iter().collect();
        msgs.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
        let ui = msgs
            .iter()
            .take(4)
            .filter(|(_, (_, t))| t.as_millis() > 0)
            .map(|((m, wp), (n, t))| format!("{} x{n} {}ms", message_name(*m, *wp), t.as_millis()))
            .collect::<Vec<_>>()
            .join(", ");

        let mut line = format!("cpu: {total:.1}% of the machine over {wall:.1}s ({} cores); threads: {by_thread}", cpus as u32);
        if !ui.is_empty() {
            line += &format!("; UI messages: {ui}");
        }
        if glass.0 > 0 || glass.1 > 0 {
            line += &format!("; glass: {} capture frames, {} pane renders in {}ms", glass.0, glass.1, glass.2 / 1000);
        }
        log!("{line}");

        if stacks_on && stacks_at.is_none_or(|t| t.elapsed() >= STACKS_EVERY) {
            stacks_at = Some(Instant::now());
            let mut targets: Vec<(f64, crate::cpu_stacks::Target)> = busy
                .iter()
                .filter(|(_, d)| share(*d) >= STACK_THREAD_MIN)
                .filter_map(|(tid, d)| Some((share(*d), crate::cpu_stacks::Target { tid: *tid, kind: threads.get(tid)?.kind.clone() })))
                .collect();
            targets.sort_by(|a, b| b.0.total_cmp(&a.0));
            let targets: Vec<_> = targets.into_iter().map(|t| t.1).collect();
            if !targets.is_empty() {
                log!("{}", crate::cpu_stacks::sample(&targets));
            }
            // The sampling took time of its own: start the next sample afresh.
            last = (Instant::now(), process_time());
            sample_threads(pid, &mut threads);
            let _ = MESSAGES.lock().map(|mut m| m.take());
        }
    }
}

/// Reads every thread's CPU time, naming new ones. Returns each thread's time
/// since the last call (100 ns ticks), and forgets threads that ended.
fn sample_threads(pid: u32, threads: &mut HashMap<u32, Thread>) -> Vec<(u32, u64)> {
    let mut busy = Vec::new();
    let mut seen = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else { return busy };
        let mut e = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut ok = Thread32First(snap, &mut e).is_ok();
        while ok {
            if e.th32OwnerProcessID == pid {
                let tid = e.th32ThreadID;
                seen.push(tid);
                if let Ok(h) = OpenThread(THREAD_QUERY_INFORMATION | THREAD_QUERY_LIMITED_INFORMATION, false, tid) {
                    let (mut a, mut b, mut k, mut u) = Default::default();
                    if GetThreadTimes(h, &mut a, &mut b, &mut k, &mut u).is_ok() {
                        let time = ticks(k) + ticks(u);
                        match threads.get_mut(&tid) {
                            Some(t) => {
                                busy.push((tid, time.saturating_sub(t.time)));
                                t.time = time;
                            }
                            None => {
                                threads.insert(tid, Thread { kind: thread_kind(h, tid), time });
                            }
                        }
                    }
                    let _ = CloseHandle(h);
                }
            }
            ok = Thread32Next(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    threads.retain(|tid, _| seen.contains(tid));
    busy
}

/// "UI", the thread's own name, or else the module its code started in
/// ("nvwgf2umx.dll" is the graphics driver, "ntdll.dll" a Windows thread pool).
fn thread_kind(h: HANDLE, tid: u32) -> String {
    if tid == UI_THREAD.load(Ordering::Relaxed) {
        return "UI".into();
    }
    unsafe {
        if let Ok(p) = GetThreadDescription(h) {
            let name = p.to_string().unwrap_or_default();
            let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(p.0 as *mut c_void));
            if !name.is_empty() {
                return name;
            }
        }
    }
    start_module(h).map_or_else(|| format!("thread {tid}"), |m| if m == "ntdll.dll" { "thread pool".into() } else { m })
}

/// The file name of the module a thread's start address is in.
fn start_module(h: HANDLE) -> Option<String> {
    type Query = unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;
    const THREAD_QUERY_SET_WIN32_START_ADDRESS: u32 = 9;
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let query: Query = std::mem::transmute(GetProcAddress(ntdll, s!("NtQueryInformationThread"))?);
        let mut start: usize = 0;
        if query(h, THREAD_QUERY_SET_WIN32_START_ADDRESS, &mut start as *mut usize as *mut c_void, std::mem::size_of::<usize>() as u32, std::ptr::null_mut()) != 0
            || start == 0
        {
            return None;
        }
        let mut module = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(start as *const u16),
            &mut module,
        )
        .ok()?;
        let mut buf = [0u16; 260];
        let n = GetModuleFileNameW(module, &mut buf) as usize;
        let path = String::from_utf16_lossy(&buf[..n]);
        path.rsplit('\\').next().map(str::to_lowercase)
    }
}

/// DockSide's own messages (`WM_APP` + n), by n.
const APP_MESSAGES: [&str; 36] = [
    "", "frame", "reload", "close menu", "menu key", "foreground", "windows changed", "appbar", "open settings", "icons",
    "finder changed", "close spotlight", "index ready", "media", "island", "island key", "desktop key", "desktop", "ping",
    "update", "widgets", "foreground moved", "icon picker", "close picker", "admin task", "finder async", "record key",
    "spotlight tap", "drag watch", "shelf key", "cloaked", "close launchpad", "control center", "wallpaper", "calendar",
    "battery",
];

/// The app's timers (`TIMER_*` in app.rs), by id.
const TIMERS: [&str; 19] = [
    "", "tick", "refresh", "reload", "rebuild", "finder", "theme", "display", "retire", "wallpaper", "appbar", "taskbar",
    "fullscreen", "", "drag watch", "dock follow", "spotlight poll", "launchpad blink", "power",
];

/// A window message by name where it's a common one, else its number.
fn message_name(msg: u32, wp: usize) -> String {
    const WM_APP: u32 = 0x8000;
    if let Some(name) = msg.checked_sub(WM_APP).and_then(|n| APP_MESSAGES.get(n as usize)).filter(|n| !n.is_empty()) {
        return format!("app:{name}");
    }
    let name = match msg {
        0x0113 => return TIMERS.get(wp).filter(|n| !n.is_empty()).map_or_else(|| format!("timer {wp}"), |n| format!("timer:{n}")),
        0x0084 => "WM_NCHITTEST",
        0x0200 => "WM_MOUSEMOVE",
        0x0201 => "WM_LBUTTONDOWN",
        0x0202 => "WM_LBUTTONUP",
        0x0204 => "WM_RBUTTONDOWN",
        0x0205 => "WM_RBUTTONUP",
        0x020A => "WM_MOUSEWHEEL",
        0x0218 => "WM_POWERBROADCAST",
        0x02A3 => "WM_MOUSELEAVE",
        0x001A => "WM_SETTINGCHANGE",
        0x004A => "WM_COPYDATA",
        _ => return format!("{msg:#06x}"),
    };
    name.into()
}
