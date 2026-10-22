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
mod icon_picker;
mod island;
mod menu;
mod menubar;
mod net;
mod onboarding;
mod settings;
mod spotlight;
mod surface;
mod theme;
mod update;
mod widgets;
mod win;

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex};
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
    // Run elevated (through UAC) by Settings to add or remove the admin task.
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == win::elevation::ADMIN_TASK) {
        let on = args.get(i + 1).is_some_and(|a| a == "on");
        std::process::exit(win::elevation::admin_task_command(on));
    }

    let mutex = unsafe { CreateMutexW(None, true, w!("Local\\DockSide.SingleInstance")) };
    unsafe {
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

    // "Run as administrator": hand off to the elevated task, releasing the lock first.
    if win::elevation::should_hand_off() {
        release(&mutex);
        if win::elevation::run_task() {
            return;
        }
        log!("admin task didn't start: starting without admin rights");
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
    // Turning "Run as administrator" on or off restarts DockSide once this instance is gone.
    if let Some(elevated) = win::elevation::take_relaunch() {
        release(&mutex);
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let started = if elevated { win::elevation::run_task() } else { win::elevation::shell_open(&exe, None, None) };
        log!("relaunch ({}): {started}", if elevated { "elevated" } else { "unelevated" });
    }
    log!("quit: done");
}

/// Lets the next instance start without waiting on this one.
fn release(mutex: &windows::core::Result<HANDLE>) {
    if let Ok(m) = mutex {
        unsafe {
            let _ = ReleaseMutex(*m);
            let _ = CloseHandle(*m);
        }
    }
}
