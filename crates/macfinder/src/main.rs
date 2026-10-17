#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod log;

mod anim;
mod app;
mod config;
mod dock;
mod geom;
mod gfx;
mod menu;
mod menubar;
mod surface;
mod theme;
mod win;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

fn main() {
    // Emergency escape hatch: `macfinder --restore-taskbar`
    if std::env::args().any(|a| a == "--restore-taskbar") {
        win::taskbar::restore();
        return;
    }

    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\MacFinder.SingleInstance"));
        if mutex.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
            log!("another instance is already running");
            return;
        }
    }

    log::init();
    std::panic::set_hook(Box::new(|info| {
        win::taskbar::restore();
        log::write(&format!("panic: {info}"));
    }));

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    if let Err(e) = app::run() {
        log!("fatal: {e:?}");
    }
    win::taskbar::restore();
}
