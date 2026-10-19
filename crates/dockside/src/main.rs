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
    // Run by Claude Code's hooks: hand the event to the running instance and exit.
    if std::env::args().any(|a| a == "--claude-hook") {
        island::claude::relay();
        return;
    }

    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\DockSide.SingleInstance"));
        if mutex.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
            // Relaunching (e.g. from the Start menu) opens Settings, like reopening a Mac app.
            // An instance that's still starting has no window yet, so give it a moment.
            let finder = std::env::args().any(|a| a == "--finder");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let signalled = loop {
                if win::signal_running_instance(finder) {
                    break true;
                }
                if std::time::Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            };
            // Otherwise the lock outlived its instance (stuck exiting): start anyway.
            if signalled {
                return;
            }
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
    log!("quit: done");
}
