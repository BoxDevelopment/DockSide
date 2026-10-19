#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod log;

mod anim;
mod app;
mod chrome;
mod config;
mod dock;
mod finder;
mod geom;
mod gfx;
mod island;
mod menu;
mod menubar;
mod settings;
mod spotlight;
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
    // Emergency escape hatch: `dockside --restore-taskbar`
    if std::env::args().any(|a| a == "--restore-taskbar") {
        win::taskbar::restore();
        return;
    }

    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\DockSide.SingleInstance"));
        if mutex.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
            // Relaunching (e.g. from the Start menu) opens Settings, like reopening a Mac app.
            win::signal_running_instance(std::env::args().any(|a| a == "--finder"));
            return;
        }
    }

    config::migrate_legacy();
    log::init();
    win::startup::refresh();
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
