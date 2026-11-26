//! Names the window message the UI thread is stuck in.
//!
//! When the shell freezes, the UI thread writes nothing: whatever it is
//! blocked in never returns, so `slow:` lines never appear and a user log
//! can't say what hung. This keeps the message being dispatched (and the
//! window that got it) in atomics, and a small watchdog thread logs them
//! once the dispatch has been running long enough to be felt.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Dispatch running longer than this is logged (once per stuck episode).
const STUCK: Duration = Duration::from_millis(1500);

/// What the UI thread is busy with: a dispatched message, or one of the
/// low-level hooks (those run while messages are being fetched, not inside
/// a dispatch, so the message-based report alone would miss them).
#[derive(Clone, Copy)]
pub enum Kind {
    Dispatch,
    Mouse,
    Keys,
    IslandKeys,
    DragWatch,
    DesktopKeys,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Dispatch => "message",
            Kind::Mouse => "mouse hook",
            Kind::Keys => "key hook",
            Kind::IslandKeys => "island key hook",
            Kind::DragWatch => "drag watch hook",
            Kind::DesktopKeys => "desktop key hook",
        }
    }
}

static KIND: AtomicU32 = AtomicU32::new(0);

static BASE: OnceLock<Instant> = OnceLock::new();
static HWND: AtomicUsize = AtomicUsize::new(0);
static MSG: AtomicU32 = AtomicU32::new(0);
static WPARAM: AtomicUsize = AtomicUsize::new(0);
/// When the current dispatch started (millis since `BASE`); 0 when idle.
static STARTED: AtomicU64 = AtomicU64::new(0);
/// Whether the current episode was already logged.
static LOGGED: AtomicBool = AtomicBool::new(false);

fn now_ms() -> u64 {
    BASE.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// A message is about to be dispatched on the UI thread.
pub fn enter(hwnd: isize, msg: u32, wp: usize) {
    KIND.store(0, Ordering::Relaxed);
    HWND.store(hwnd as usize, Ordering::Relaxed);
    MSG.store(msg, Ordering::Relaxed);
    WPARAM.store(wp, Ordering::Relaxed);
    LOGGED.store(false, Ordering::Relaxed);
    STARTED.store(now_ms().max(1), Ordering::Release);
}

/// A low-level hook callback is starting. Released by dropping the guard, so
/// every early return in the callback still clears it.
pub fn hook(kind: Kind) -> Guard {
    KIND.store(kind as u32, Ordering::Relaxed);
    LOGGED.store(false, Ordering::Relaxed);
    STARTED.store(now_ms().max(1), Ordering::Release);
    Guard
}

pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        leave();
    }
}

/// That message is done.
pub fn leave() {
    STARTED.store(0, Ordering::Relaxed);
}

/// Starts watching. Call once, on the UI thread.
pub fn start() {
    BASE.get_or_init(Instant::now);
    let spawned = std::thread::Builder::new().name("ui watch".into()).spawn(|| loop {
        std::thread::sleep(Duration::from_millis(200));
        let started = STARTED.load(Ordering::Acquire);
        if started == 0 {
            continue;
        }
        let stuck = now_ms().saturating_sub(started);
        if stuck < STUCK.as_millis() as u64 {
            continue;
        }
        // Once per episode, and again whenever it doubles.
        if LOGGED.swap(true, Ordering::Relaxed) && stuck % 5000 > 200 {
            continue;
        }
        let kind = match KIND.load(Ordering::Relaxed) {
            0 => Kind::Dispatch,
            1 => Kind::Mouse,
            2 => Kind::Keys,
            3 => Kind::IslandKeys,
            4 => Kind::DragWatch,
            _ => Kind::DesktopKeys,
        };
        if matches!(kind, Kind::Dispatch) {
            let class = class_of(HWND.load(Ordering::Relaxed));
            log!(
                "ui stuck {:.1}s in message: hwnd {:#x}{} msg {:#06x} (wparam {:#x})",
                stuck as f64 / 1000.0,
                HWND.load(Ordering::Relaxed),
                if class.is_empty() { String::new() } else { format!(" ({class})") },
                MSG.load(Ordering::Relaxed),
                WPARAM.load(Ordering::Relaxed)
            );
        } else {
            log!("ui stuck {:.1}s in the {} callback", stuck as f64 / 1000.0, kind.name());
        }
    });
    if let Err(e) = spawned {
        log!("ui watch didn't start: {e}");
    } else {
        log!("ui watch: on");
    }
}

/// The window's class name (DockSide's surfaces are one letter; other
/// processes' windows have real names).
fn class_of(hwnd: usize) -> String {
    use windows::Win32::Foundation::HWND as H;
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(H(hwnd as _), &mut buf) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}
