//! Registers surfaces as AppBars so maximized windows leave room for them.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Shell::*;

struct Bar {
    /// The appbar window, as an isize so `Bar` can live in a shared Mutex
    /// (windows' `HWND` wraps a raw pointer and isn't `Send`).
    hwnd: isize,
    edge: u32,
    monitor: RECT,
    thickness: i32,
    /// The rect last handed to ABM_SETPOS.
    rc: RECT,
}

/// Shared, not thread-local: the shell worker negotiates with Explorer on its
/// own thread, so it must reach the bars from there.
static BARS: Mutex<Vec<Bar>> = Mutex::new(Vec::new());

/// The shell worker's channel, made on first use.
static SHELL_TX: OnceLock<Sender<()>> = OnceLock::new();

fn data(hwnd: isize) -> APPBARDATA {
    APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: HWND(hwnd as _), ..Default::default() }
}

/// Where `bar` should sit given the other appbars (the taskbar among them) right now.
fn query(bar: &Bar) -> RECT {
    let mut abd = data(bar.hwnd);
    abd.uEdge = bar.edge;
    abd.rc = bar.monitor;
    unsafe { SHAppBarMessage(ABM_QUERYPOS, &mut abd) };
    if bar.edge == ABE_TOP {
        abd.rc.bottom = abd.rc.top + bar.thickness;
    } else {
        abd.rc.top = abd.rc.bottom - bar.thickness;
    }
    abd.rc
}

fn set_pos(bar: &Bar) {
    let mut abd = data(bar.hwnd);
    abd.uEdge = bar.edge;
    abd.rc = bar.rc;
    unsafe { SHAppBarMessage(ABM_SETPOS, &mut abd) };
    report(bar, &abd.rc);
}

/// Logs where a bar ended up, and the work area Windows gives its display,
/// when that isn't flush with the display's edge: something else (another
/// appbar, a taskbar we hid) holds space there too, and maximized windows stop
/// short of the bar.
fn report(bar: &Bar, placed: &RECT) {
    let m = bar.monitor;
    let work = super::Monitor { rect: m, scale: 1.0 }.work_area();
    let (edge, flush) = if bar.edge == ABE_TOP {
        ("top", placed.top == m.top && work.top == m.top + bar.thickness)
    } else {
        ("bottom", placed.bottom == m.bottom && work.bottom == m.bottom - bar.thickness)
    };
    if !flush {
        log!(
            "appbar: {edge} {}px on display {},{} {}x{} placed at {},{}-{},{}; work area {},{}-{},{}",
            bar.thickness,
            m.left,
            m.top,
            m.right - m.left,
            m.bottom - m.top,
            placed.left,
            placed.top,
            placed.right,
            placed.bottom,
            work.left,
            work.top,
            work.right,
            work.bottom
        );
    }
}

/// Reserves `thickness` physical pixels along `edge` (ABE_TOP / ABE_BOTTOM) of `monitor`.
pub fn register(hwnd: HWND, callback: u32, edge: u32, monitor: RECT, thickness: i32) {
    // Explorer sends the callback; it runs unelevated.
    super::elevation::allow_messages(hwnd, &[callback]);
    let mut bar = Bar { hwnd: hwnd.0 as isize, edge, monitor, thickness, rc: RECT::default() };
    super::tray::explorer_first(|| unsafe {
        let mut abd = data(bar.hwnd);
        abd.uCallbackMessage = callback;
        SHAppBarMessage(ABM_NEW, &mut abd);
        bar.rc = query(&bar);
        set_pos(&bar);
    });
    if let Ok(mut bars) = BARS.lock() {
        bars.push(bar);
    }
}

/// Re-negotiates every bar's position. Call when another appbar moved or changed state
/// (ABN_POSCHANGED / ABN_STATECHANGE), e.g. the taskbar switching edge or auto-hiding:
/// a reservation made while the taskbar still held that edge would otherwise leave a gap.
pub fn refresh() {
    let Ok(mut bars) = BARS.lock() else { return };
    if bars.is_empty() {
        return;
    }
    super::tray::explorer_first(|| {
        for bar in bars.iter_mut() {
            let rc = query(bar);
            // Only on change: each SETPOS notifies the other bars, which would refresh again.
            if rc != bar.rc {
                bar.rc = rc;
                set_pos(bar);
            }
        }
    });
}

pub fn remove(hwnd: HWND) {
    let id = hwnd.0 as isize;
    if let Ok(mut bars) = BARS.lock() {
        bars.retain(|bar| bar.hwnd != id);
    }
    super::tray::explorer_first(|| unsafe {
        let mut abd = data(id);
        SHAppBarMessage(ABM_REMOVE, &mut abd);
    });
}

// ---------------------------------------------------------------------------
// The shell worker
// ---------------------------------------------------------------------------

/// Tells the shell worker to re-negotiate with Explorer (taskbar state and
/// position, every appbar's place). The caller returns at once: everything
/// below runs on the worker, because `SHAppBarMessage` is a synchronous
/// cross-process call with no timeout, and Explorer busy means the UI thread
/// would freeze with it.
pub fn sync_taskbar() {
    let tx = SHELL_TX.get_or_init(|| {
        let (tx, rx) = channel::<()>();
        let spawned = std::thread::Builder::new().name("shell".into()).spawn(move || {
            while rx.recv().is_ok() {
                // Coalesce: everything asked for while we work is one more pass.
                while rx.try_recv().is_ok() {}
                let edge = super::taskbar::edge();
                super::taskbar::note_edge(edge);
                super::taskbar::enforce();
                super::taskbar::reapply();
                refresh();
            }
        });
        if let Err(e) = spawned {
            log!("shell worker didn't start: {e}");
        }
        tx
    });
    let _ = tx.send(());
}
