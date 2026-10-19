//! Hides the Windows taskbar while we run and guarantees it comes back.
//!
//! The taskbar's original auto-hide state is persisted to disk so that even a
//! hard kill can be undone with `dockside --restore-taskbar` (or by the next launch).

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

static HIDDEN: AtomicBool = AtomicBool::new(false);

fn state_file() -> std::path::PathBuf {
    crate::config::dir().join("taskbar.state")
}

fn tray_windows() -> Vec<HWND> {
    let mut out = Vec::new();
    unsafe {
        // Not FindWindow: DockSide's own tray host shares the class.
        if let Some(h) = crate::win::tray::explorer_tray() {
            out.push(h);
        }
        let mut after = HWND::default();
        while let Ok(h) = FindWindowExW(None, after, w!("Shell_SecondaryTrayWnd"), None) {
            out.push(h);
            after = h;
        }
    }
    out
}

fn appbar_state(tray: HWND) -> u32 {
    let mut abd = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: tray,
        ..Default::default()
    };
    unsafe { SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32 }
}

fn set_appbar_state(tray: HWND, state: u32) {
    let mut abd = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: tray,
        lParam: LPARAM(state as isize),
        ..Default::default()
    };
    unsafe {
        SHAppBarMessage(ABM_SETSTATE, &mut abd);
    }
}

pub fn hide() {
    let trays = tray_windows();
    let Some(&main) = trays.first() else { return };
    if !HIDDEN.swap(true, Ordering::SeqCst) {
        // A leftover state file means a previous run didn't exit cleanly:
        // trust it over the current (auto-hidden) state.
        if !state_file().exists() {
            let _ = std::fs::write(state_file(), appbar_state(main).to_string());
        }
        // Auto-hide releases the taskbar's reserved work area.
        set_appbar_state(main, ABS_AUTOHIDE);
    }
    reapply();
}

/// Explorer re-shows the taskbar on some events (Win key, explorer restart); call periodically.
pub fn reapply() {
    if !HIDDEN.load(Ordering::SeqCst) {
        return;
    }
    for h in tray_windows() {
        unsafe {
            if IsWindowVisible(h).as_bool() {
                let _ = ShowWindow(h, SW_HIDE);
            }
        }
    }
}

pub fn restore() {
    let file = state_file();
    let saved = std::fs::read_to_string(&file).ok().and_then(|s| s.trim().parse::<u32>().ok());
    if !HIDDEN.swap(false, Ordering::SeqCst) && saved.is_none() {
        return;
    }
    let trays = tray_windows();
    if let Some(&main) = trays.first() {
        set_appbar_state(main, saved.unwrap_or(ABS_ALWAYSONTOP));
    }
    for h in trays {
        unsafe {
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        }
    }
    let _ = std::fs::remove_file(file);
}
