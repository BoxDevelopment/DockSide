#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod log;

mod anim;
mod app;
mod calsync;
mod dropdown;
mod chrome;
mod config;
mod controlcenter;
#[cfg(target_arch = "x86_64")]
mod cpu_stacks;
mod cpu_watch;
mod dock;
mod dock_drop;
mod finder;
mod geom;
mod gfx;
mod icon_picker;
mod island;
mod keybind;
mod launchpad;
mod menu;
mod menubar;
mod net;
mod onboarding;
mod settings;
mod screenshot_mode;
mod spotlight;
mod surface;
mod theme;
mod update;
mod widgets;
mod win;
mod genie;

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
    // Claude Code's status line: pass the plan's usage along, print a line, exit.
    if std::env::args().any(|a| a == island::claude::STATUS_ARG) {
        island::claude::status_line();
        return;
    }
    // Now Playing's helper process (see island::media): DockSide starts it.
    if std::env::args().any(|a| a == island::media::HELPER_ARG) {
        island::media::helper_main();
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    // `dockside activity start build --title Build …`: a Live Activity for the island.
    if args.get(1).is_some_and(|a| a == "activity") {
        std::process::exit(island::live::cli(&args[2..]));
    }
    // Run elevated (through UAC) by Settings to add or remove the admin task.
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

    // Before the hand-off below: the elevated instance it starts doesn't wait.
    win::startup::wait_for_shell();

    // "Run as administrator": hand off to the elevated task, releasing the lock first.
    let hand_off = std::time::Instant::now();
    if win::elevation::should_hand_off() {
        release(&mutex);
        if win::elevation::run_task() {
            log!("handed off to the admin task in {:?}", hand_off.elapsed());
            return;
        }
        log!("admin task didn't start: starting without admin rights");
    }

    config::migrate_legacy();
    log::init();
    std::thread::spawn(win::startup::refresh);
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
