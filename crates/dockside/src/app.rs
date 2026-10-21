//! Application state, the shared window procedure and the event loop.
//!
//! All UI lives on one thread. Window messages are routed to the component
//! that owns the window. Anything that may block or re-enter the message loop
//! (dialogs, launching, focus changes) is queued as an [`Action`] and executed
//! after the current message is handled, outside the `App` borrow.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{SHEmptyRecycleBinW, ABN_FULLSCREENAPP};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{FrameClock, FRAME_PENDING};
use crate::config::{self, Config, DockApp};
use crate::dock::Dock;
use crate::gfx::icons::IconStore;
use crate::gfx::Gfx;
use crate::icon_picker::{IconPicker, IconTarget, PickerEvent, PICKER_HWND};
use crate::island::sources::VolumeKey;
use crate::island::{Island, IslandEvent};
use crate::menu::{self, Action, Entry, MenuCtx, MenuEvent, MenuSource, Placement, PopupMenu};
use crate::menubar::{Hit, MenuBar};
use crate::finder::{self, FinderEvent, FinderWindow, FINDER_PATH};
use crate::onboarding::{Onboarding, OnboardingEvent};
use crate::settings::{Page, SettingsEvent, SettingsWindow, SETTINGS_HWND};
use crate::spotlight::{self, Spotlight, SpotlightEvent, SPOTLIGHT_HWND};
use crate::theme::Theme;
use crate::widgets::{WidgetCmd, WidgetEvent, Widgets};
use crate::win::{self, apps, startup, system, taskbar, Monitor};
use crate::win::{
    WM_APP_APPBAR, WM_APP_CLOSE_MENU, WM_APP_OPEN_SETTINGS, WM_APP_FOREGROUND, WM_APP_FRAME, WM_APP_MENU_KEY, WM_APP_RELOAD,
    WM_APP_WINDOWS_CHANGED, WM_APP_ICONS, WM_APP_FINDER_CHANGED, WM_APP_CLOSE_SPOTLIGHT, WM_APP_INDEX_READY, WM_APP_MEDIA,
    WM_APP_ISLAND, WM_APP_ISLAND_KEY, WM_APP_DESKTOP, WM_APP_DESKTOP_KEY, DESKTOP_LEFT, DESKTOP_RIGHT, WM_APP_PING,
    WM_APP_UPDATE, WM_APP_WIDGETS, WM_APP_WIDGET_MEDIA, WM_APP_ICON_PICKER, WM_APP_CLOSE_PICKER,
};

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static PENDING: RefCell<Vec<Action>> = const { RefCell::new(Vec::new()) };
}

static MSG_HWND: AtomicIsize = AtomicIsize::new(0);
/// Windows the menu mouse hook must not treat as "outside" clicks.
static OPEN_MENU_HWND: AtomicIsize = AtomicIsize::new(0);
static MENUBAR_HWND: AtomicIsize = AtomicIsize::new(0);
/// Whether the island's keyboard hook swallows volume keys (it shows its own HUD).
static ISLAND_VOLUME_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Which desktop shortcuts the desktop key hook takes.
static DESKTOP_ALT_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static DESKTOP_ARROW_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lives in the (large) Win32_UI_Controls feature; defined here instead.
const WM_MOUSELEAVE: u32 = 0x02A3;

const TIMER_TICK: usize = 1;
const TIMER_REFRESH: usize = 2;
const TIMER_RELOAD: usize = 3;
const TIMER_REBUILD: usize = 4;
const TIMER_FINDER: usize = 5;
/// Color-scheme broadcast: rebuild only if light/dark actually flipped.
const TIMER_THEME: usize = 6;
/// Display broadcast: rebuild only if the primary monitor actually changed.
const TIMER_DISPLAY: usize = 7;
/// Destroys replaced surfaces once their replacements are on screen.
const TIMER_RETIRE: usize = 8;
/// Wallpaper broadcast: reload what wallpaper-backed glass refracts.
const TIMER_WALLPAPER: usize = 9;
/// Long enough for DWM to present a new surface's backdrop.
const RETIRE_MS: u32 = 250;

/// Energy Saver and battery saver on/off, reported via WM_POWERBROADCAST.
const GUID_ENERGY_SAVER_STATUS: windows::core::GUID = windows::core::GUID::from_u128(0x550e8400_e29b_41d4_a716_446655440000);
const GUID_POWER_SAVING_STATUS: windows::core::GUID = windows::core::GUID::from_u128(0xe00958c0_c213_4ace_ac77_fecced2eeea5);

/// What decides whether DWM blurs our backdrops. Energy Saver turns blur off
/// and back on without rebuilding the host backdrop brushes, so a change here
/// needs fresh surfaces.
fn backdrop_state() -> (bool, bool) {
    (system::transparency_enabled(), win::controls::low_power_mode())
}

const HOTKEY_SPOTLIGHT: i32 = 1;

pub fn run() -> anyhow::Result<()> {
    win::register_class(Some(wndproc))?;
    let msg_hwnd = win::create_window(WINDOW_EX_STYLE(0), WINDOW_STYLE(0), RECT::default(), HWND_MESSAGE)?;
    MSG_HWND.store(msg_hwnd.0 as isize, Ordering::Relaxed);

    let app = App::new(msg_hwnd)?;
    for guid in [GUID_ENERGY_SAVER_STATUS, GUID_POWER_SAVING_STATUS] {
        if let Err(e) = unsafe { windows::Win32::System::Power::RegisterPowerSettingNotification(HANDLE(msg_hwnd.0), &guid, DEVICE_NOTIFY_WINDOW_HANDLE) } {
            log!("power notification failed: {e}");
        }
    }
    APP.with(|a| *a.borrow_mut() = Some(app));
    with_app(|app| {
        app.build();
        if !app.cfg.general.onboarding_done {
            app.open_onboarding();
        }
    });
    crate::update::watch(msg_hwnd.0 as isize, WM_APP_UPDATE);

    unsafe {
        SetTimer(msg_hwnd, TIMER_TICK, 1000, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Drop outside the borrow: destroying windows dispatches messages.
    let app = APP.with(|a| a.borrow_mut().take());
    drop(app);
    log!("quit: surfaces closed");
    // Graphics last, once nothing draws with them.
    crate::win::glass::shutdown();
    log!("quit: glass stopped");
    crate::win::composition::shutdown();
    log!("quit: composition stopped");
    Ok(())
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| cell.try_borrow_mut().ok().and_then(|mut guard| guard.as_mut().map(f)))
}

fn push(action: Action) {
    PENDING.with(|p| p.borrow_mut().push(action));
}

/// Drag-and-drop feedback from a Finder window's OLE drop target: returns the folder a drop would go into.
pub fn finder_drag(id: u32, phase: finder::dnd::Phase, paths: &[PathBuf]) -> Option<PathBuf> {
    with_app(|app| {
        let target = app.finders.iter_mut().find(|f| f.id == id)?.on_drag(phase, paths);
        app.flush();
        target
    })
    .flatten()
}

/// Queues the copy / move for a drop; it runs once the drop callback has returned.
pub fn finder_transfer(id: u32, paths: Vec<PathBuf>, dest: PathBuf, copy: bool) {
    push(Action::Finder { window: id, cmd: finder::FinderCmd::Transfer { paths, dest, copy } });
    win::post(MSG_HWND.load(Ordering::Relaxed), WM_NULL);
}

fn drain_actions() {
    loop {
        // Never run actions from a nested (re-entrant) dispatch.
        if APP.with(|a| a.try_borrow_mut().is_err()) {
            return;
        }
        let next = PENDING.with(|p| {
            let mut p = p.borrow_mut();
            (!p.is_empty()).then(|| p.remove(0))
        });
        match next {
            Some(action) => execute(action),
            None => return,
        }
    }
}

fn execute(action: Action) {
    log!("action: {action:?}");
    match action {
        Action::Open(target) => system::open(&target),
        Action::Launch { path, .. } if path == FINDER_PATH => {
            with_app(|app| app.open_finder(None));
        }
        Action::OpenFinder(path) => {
            with_app(|app| app.open_finder(path));
        }
        // Drags and file operations run modal loops: keep them outside the App borrow.
        Action::Finder { window, cmd } if cmd.is_modal() => {
            let hwnd = with_app(|app| app.finders.iter().find(|f| f.id == window).map(|f| f.hwnd)).flatten();
            finder::run_modal(hwnd.unwrap_or_default(), cmd);
            with_app(|app| app.finder_cmd(window, finder::FinderCmd::Reload));
        }
        Action::Finder { window, cmd } => {
            with_app(|app| app.finder_cmd(window, cmd));
        }
        Action::Launch { path, args } => {
            with_app(|app| {
                if app.dock.as_mut().map(|d| d.bounce(&path)).unwrap_or(false) {
                    app.animate();
                }
            });
            apps::launch(&path, args.as_deref());
        }
        Action::Keys(sc) => sc.send(),
        Action::Activate(h) => apps::activate(h),
        Action::Minimize(h) => apps::minimize(h),
        Action::MinimizeWindows(hwnds) => hwnds.into_iter().for_each(apps::minimize),
        Action::Zoom(h) => apps::toggle_zoom(h),
        Action::HideOthers(keep) => {
            let keep_pid = apps::effective_pid(keep);
            for w in apps::list_windows() {
                if apps::effective_pid(w.hwnd) != keep_pid {
                    apps::minimize(w.hwnd);
                }
            }
        }
        Action::ShowAll => {
            for w in apps::list_windows() {
                if apps::is_minimized(w.hwnd) {
                    unsafe {
                        let _ = ShowWindow(w.hwnd, SW_SHOWNOACTIVATE);
                    }
                }
            }
        }
        Action::BringToFront(hwnds) => hwnds.into_iter().rev().for_each(apps::activate),
        Action::CloseWindows(hwnds) => hwnds.into_iter().for_each(apps::close),
        Action::Reveal(path) => system::reveal(&path),
        Action::TogglePin { path, name, pin } => {
            with_app(|app| app.toggle_pin(&path, &name, pin));
        }
        Action::SetConfig { table, key, value } => {
            match config::save_value(&config::path(), table, key, &value) {
                // Apply right away instead of waiting for the file watcher.
                Ok(()) => {
                    with_app(|app| app.reload());
                }
                Err(e) => log!("could not update {table}.{key}: {e:#}"),
            }
        }
        Action::SetStartAtLogin(on) => {
            if let Err(e) = startup::set_enabled(on) {
                log!("could not change start at login: {e}");
            }
            with_app(|app| app.mark_settings_dirty());
        }
        Action::AddDockApp => {
            let owner = HWND(SETTINGS_HWND.load(Ordering::Relaxed) as _);
            if let Some(path) = system::pick_app(owner) {
                let name = if path.to_lowercase().ends_with(".exe") {
                    apps::file_description(&path)
                } else {
                    None
                }
                .or_else(|| std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| path.clone());
                with_app(|app| app.toggle_pin(&path, &name, true));
            }
        }
        Action::ToggleSpotlight => {
            with_app(|app| app.toggle_spotlight());
        }
        Action::ChangeIcon { target, name } => {
            with_app(|app| app.open_icon_picker(target, &name));
        }
        Action::SetAppIcon { target, icon } => {
            with_app(|app| app.set_app_icon(&target, icon));
        }
        // The file dialog runs a modal loop: outside the App borrow.
        Action::PickIconFile(target) => {
            with_app(|app| app.icon_picker = None);
            if let Some(path) = system::pick_image(HWND::default()) {
                with_app(|app| app.set_app_icon(&target, Some(path)));
            }
        }
        Action::OpenSettings(page) => {
            with_app(|app| app.open_settings(page));
        }
        Action::OpenOnboarding => {
            with_app(|app| app.open_onboarding());
        }
        Action::SetDockApps(list) => {
            match config::save_dock_apps(&config::path(), &list) {
                Ok(()) => {
                    with_app(|app| app.reload());
                }
                Err(e) => log!("could not update dock apps: {e:#}"),
            }
        }
        Action::EmptyTrash => unsafe {
            let _ = SHEmptyRecycleBinW(None, PCWSTR::null(), 0);
        },
        Action::Sleep => system::sleep(),
        Action::Restart => {
            if system::confirm("Restart", "Are you sure you want to restart your computer now?") {
                system::restart();
            }
        }
        Action::ShutDown => {
            if system::confirm("Shut Down", "Are you sure you want to shut down your computer now?") {
                system::shut_down();
            }
        }
        Action::LogOut => {
            if system::confirm("Log Out", "Are you sure you want to quit all applications and log out now?") {
                system::log_out();
            }
        }
        Action::Lock => system::lock(),
        Action::TrayClick(id, button) => crate::win::tray::click(id, button),
        Action::SetDarkMode(dark) => {
            crate::win::controls::set_dark_mode(dark);
            // Don't wait for the system-wide broadcast to come back around.
            with_app(|app| app.timer(TIMER_REBUILD, 50));
        }
        Action::OpenConfig => system::open(&config::path().to_string_lossy()),
        Action::ConnectClaude(on) => {
            use crate::island::claude;
            let path = claude::settings_path();
            if on {
                let ask = format!(
                    "DockSide will add hooks to {} so the Dynamic Island can show what Claude Code is doing.\n\n\
                     Each step Claude takes runs DockSide briefly to pass the event along. Your other settings are kept, \
                     and a backup is saved next to the file.",
                    path.display()
                );
                if system::confirm("Connect Claude Code", &ask) {
                    let exe = std::env::current_exe().unwrap_or_default();
                    match claude::connect(&exe) {
                        Ok(_) => system::notice(
                            "Claude Code connected",
                            "New Claude Code sessions will show up in the Dynamic Island. Sessions that are already open pick it up after a restart.",
                        ),
                        Err(e) => system::notice("Couldn't connect Claude Code", &format!("{e:#}")),
                    }
                }
            } else if let Err(e) = claude::disconnect() {
                system::notice("Couldn't disconnect Claude Code", &format!("{e:#}"));
            }
            with_app(|app| app.mark_settings_dirty());
        }
        Action::Island(cmd) => {
            with_app(|app| {
                if let Some(i) = &mut app.island {
                    i.command(cmd);
                    app.animate();
                }
            });
        }
        // Modal drag loop: outside the App borrow.
        Action::IslandDrag(paths) => {
            let hwnd = with_app(|app| app.island.as_ref().map(|i| i.surface.content)).flatten();
            let dropped = hwnd.is_some_and(|h| finder::dnd::drag_out(h, &paths));
            with_app(|app| {
                if let Some(i) = &mut app.island {
                    i.drag_done(&paths, dropped);
                    app.animate();
                }
                app.flush();
            });
        }
        // Confirmed outside the App borrow: the message box runs a modal loop.
        Action::Widget(WidgetCmd::Reset) => {
            let ask = "Put the default widgets back where they started? Widgets you've added, moved or resized                        are replaced, and the text in Notes widgets is cleared. Your reminders and widget settings are kept.";
            if system::confirm("Reset Widgets", ask) {
                with_app(|app| app.widget_cmd(WidgetCmd::Reset));
            }
        }
        Action::Widget(cmd) => {
            with_app(|app| app.widget_cmd(cmd));
        }
        Action::Quit => unsafe { PostQuitMessage(0) },
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_FRAME => FRAME_PENDING.store(false, Ordering::Release),
        WM_APP_PING => return LRESULT(win::PING_REPLY as isize),
        // Spotlight closes as soon as it loses focus (posted: we may be mid-dispatch).
        WM_ACTIVATE if wp.0 & 0xFFFF == 0 && hwnd.0 as isize == SPOTLIGHT_HWND.load(Ordering::Relaxed) => {
            win::post(MSG_HWND.load(Ordering::Relaxed), WM_APP_CLOSE_SPOTLIGHT);
        }
        WM_ACTIVATE if wp.0 & 0xFFFF == 0 && hwnd.0 as isize == PICKER_HWND.load(Ordering::Relaxed) => {
            win::post(MSG_HWND.load(Ordering::Relaxed), WM_APP_CLOSE_PICKER);
        }
        // Shell surfaces never take focus; app windows (Finder, Settings) behave normally.
        WM_MOUSEACTIVATE if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_APPWINDOW.0 == 0 => {
            return LRESULT(MA_NOACTIVATE as isize)
        }
        // Resizable app windows (Finder) draw their own frame: the client area is the whole
        // window. Handled here, not in App, because the first one arrives during CreateWindow,
        // while App is borrowed; DefWindowProc would leave the client inset by the resize border.
        // (Both forms: leaving the proposed rect untouched makes it the client rect.)
        WM_NCCALCSIZE if GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_THICKFRAME.0 != 0 => {
            return LRESULT(0)
        }
        // Desktop widgets stay on the desktop layer: nothing else may restack them
        // (clicking or typing into one would otherwise lift it over app windows).
        WM_WINDOWPOSCHANGING
            if GetWindowLongPtrW(hwnd, GWLP_USERDATA) == crate::widgets::TAG && !crate::widgets::placing() =>
        {
            let pos = &mut *(lp.0 as *mut WINDOWPOS);
            pos.flags |= SWP_NOZORDER;
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            // Backdrop windows: black is fully transparent under a DWM accent.
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            FillRect(hdc, &ps.rcPaint, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        _ => {}
    }
    let handled = with_app(|app| app.on_message(hwnd, msg, wp, lp)).flatten();
    drain_actions();
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != 0 {
        return;
    }
    let msg = if event == EVENT_SYSTEM_FOREGROUND { WM_APP_FOREGROUND } else { WM_APP_WINDOWS_CHANGED };
    win::post(MSG_HWND.load(Ordering::Relaxed), msg);
}

/// While a menu is open: clicks outside our windows close it.
unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 && matches!(wp.0 as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
        let info = &*(lp.0 as *const MSLLHOOKSTRUCT);
        let target = WindowFromPoint(info.pt);
        let root = GetAncestor(target, GA_ROOT).0 as isize;
        // Close on any click except on the menu itself, or on the menu bar
        // (which switches between its menus on its own).
        let keep = root != 0
            && (root == OPEN_MENU_HWND.load(Ordering::Relaxed) || root == MENUBAR_HWND.load(Ordering::Relaxed));
        if apps::window_pid(target) != GetCurrentProcessId() || !keep {
            win::post(MSG_HWND.load(Ordering::Relaxed), WM_APP_CLOSE_MENU);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// While the island runs: volume keys become the island's HUD (swallowed so the
/// Windows flyout stays away), and Caps Lock presses are reported.
unsafe extern "system" fn island_key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    const VK_VOLUME_MUTE: u16 = 0xAD;
    const VK_VOLUME_UP: u16 = 0xAF;
    const VK_CAPITAL: u16 = 0x14;
    if code >= 0 {
        let info = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let vk = info.vkCode as u16;
        let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let injected = info.flags.0 & LLKHF_INJECTED.0 != 0;
        if (VK_VOLUME_MUTE..=VK_VOLUME_UP).contains(&vk) && ISLAND_VOLUME_KEYS.load(Ordering::Relaxed) && !injected {
            if down {
                win::post_wp(MSG_HWND.load(Ordering::Relaxed), WM_APP_ISLAND_KEY, vk as usize);
            }
            return LRESULT(1);
        }
        // Key repeat sends more key-downs; only the first press of a hold toggles Caps Lock.
        static CAPS_HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if vk == VK_CAPITAL {
            if down && !CAPS_HELD.swap(true, Ordering::Relaxed) {
                win::post_wp(MSG_HWND.load(Ordering::Relaxed), WM_APP_ISLAND_KEY, vk as usize);
            } else if !down {
                CAPS_HELD.store(false, Ordering::Relaxed);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// Desktop shortcuts: Alt+1…9, and Win+Ctrl+←/→ in place of Windows' own (which
/// shows a popup). Swallowed, with a masking key so releasing Alt / Win doesn't
/// open an app's menu or Start.
unsafe extern "system" fn desktop_key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    if code >= 0 {
        let info = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let vk = info.vkCode as u16;
        // Our own masking key comes back through here; leave it alone.
        if vk != 0xE8 {
            let held = |k: VIRTUAL_KEY| GetAsyncKeyState(k.0 as i32) < 0;
            let (alt, ctrl, shift) = (held(VK_MENU), held(VK_CONTROL), held(VK_SHIFT));
            let win = held(VK_LWIN) || held(VK_RWIN);
            let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let target = if DESKTOP_ALT_KEYS.load(Ordering::Relaxed) && alt && !ctrl && !shift && !win && (0x31..=0x39).contains(&vk) {
                Some((vk - 0x30) as usize)
            } else if DESKTOP_ARROW_KEYS.load(Ordering::Relaxed) && win && ctrl && !alt && !shift && (vk == VK_LEFT.0 || vk == VK_RIGHT.0) {
                Some(if vk == VK_LEFT.0 { DESKTOP_LEFT } else { DESKTOP_RIGHT })
            } else {
                None
            };
            if let Some(t) = target {
                if down {
                    // vkE8 is unassigned: it breaks up the Alt / Win "tap" without doing anything.
                    let mask = |flags| INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), dwFlags: flags, ..Default::default() } },
                    };
                    SendInput(&[mask(KEYBD_EVENT_FLAGS(0)), mask(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32);
                    win::post_wp(MSG_HWND.load(Ordering::Relaxed), WM_APP_DESKTOP_KEY, t);
                }
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// While a menu is open: arrows / Enter / Esc drive the menu instead of the focused app.
unsafe extern "system" fn key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let vk = info.vkCode as u16;
        if matches!(vk, 0x0D | 0x1B | 0x25..=0x28) {
            if matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                win::post_wp(MSG_HWND.load(Ordering::Relaxed), WM_APP_MENU_KEY, vk as usize);
            }
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

fn mouse_pt(lp: LPARAM, scale: f32) -> (f32, f32) {
    let x = (lp.0 & 0xFFFF) as u16 as i16 as f32;
    let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
    (x / scale, y / scale)
}

fn track_leave(hwnd: HWND) {
    let mut tme = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    unsafe {
        let _ = TrackMouseEvent(&mut tme);
    }
}

/// File drag feedback for the island's shelf (OLE drop target).
pub fn island_drag(phase: finder::dnd::Phase, paths: &[PathBuf]) -> bool {
    with_app(|app| {
        let ok = app.island.as_mut().is_some_and(|i| i.on_drag(phase, paths));
        app.animate();
        app.flush();
        ok
    })
    .unwrap_or(false)
}

/// Brings the app behind a media session forward: a running window of that exe,
/// otherwise the app by its AppUserModelID (Store apps, browsers).
fn open_media_app(app_id: &str) -> Action {
    let exe = app_id.to_lowercase();
    if exe.ends_with(".exe") {
        if let Some(w) = apps::list_windows().into_iter().find(|w| w.exe_name() == exe) {
            return Action::Activate(w.hwnd);
        }
    }
    Action::Open(format!("shell:AppsFolder\\{app_id}"))
}

fn dock_eq_except_apps(a: &config::DockCfg, b: &config::DockCfg) -> bool {
    config::DockCfg { apps: Vec::new(), ..a.clone() } == config::DockCfg { apps: Vec::new(), ..b.clone() }
}

pub struct App {
    gfx: Gfx,
    cfg: Config,
    cfg_path: PathBuf,
    theme: Theme,
    monitor: Monitor,
    msg_hwnd: HWND,
    pid: u32,
    clock: FrameClock,
    animating: bool,
    last_frame: Instant,
    icons: IconStore,
    names: apps::AppNames,
    menubar: Option<MenuBar>,
    dock: Option<Dock>,
    island: Option<Island>,
    widgets: Option<Widgets>,
    menu: Option<PopupMenu>,
    settings: Option<SettingsWindow>,
    /// The Setup Assistant, on first launch or when reopened.
    onboarding: Option<Onboarding>,
    finders: Vec<FinderWindow>,
    next_finder_id: u32,
    loader: finder::loader::IconLoader,
    kinds: finder::fs::KindCache,
    /// Finder windows whose folder changed on disk (reloaded on a short timer).
    finder_changed: Vec<u32>,
    spotlight: Option<Spotlight>,
    /// Change Icon… for a Dock app.
    icon_picker: Option<IconPicker>,
    index: spotlight::index::Shared,
    hooks: Vec<HHOOK>,
    island_hook: Option<HHOOK>,
    /// Alt+number / Win+Ctrl+arrow desktop shortcuts.
    desktop_hook: Option<HHOOK>,
    /// Reports desktop switches made anywhere, for the island.
    desktop_watch: Option<win::desktops::Watcher>,
    /// Hides Windows' own desktop switch popup.
    desktop_popup: Option<win::desktops::PopupHider>,
    win_hooks: Vec<HWINEVENTHOOK>,
    _watcher: Option<notify::RecommendedWatcher>,
    taskbar_created: u32,
    /// Windows' last ABN_FULLSCREENAPP report (checked against the foreground window).
    fullscreen_reported: bool,
    /// Shell surfaces are currently hidden for a fullscreen app.
    fullscreen_hidden: bool,
    /// `backdrop_state()` when the surfaces were last built.
    backdrop_state: (bool, bool),
    /// Replaced surfaces, left on screen under their replacements until
    /// `TIMER_RETIRE` so a rebuild never shows a gap.
    retired: Vec<Box<dyn std::any::Any>>,
}

impl App {
    fn new(msg_hwnd: HWND) -> anyhow::Result<Self> {
        let gfx = Gfx::new()?;
        let cfg_path = config::ensure_exists().unwrap_or_else(|e| {
            log!("could not create config: {e:#}");
            config::path()
        });
        let cfg = config::load(&cfg_path).unwrap_or_else(|e| {
            log!("config error, using defaults: {e:#}");
            config::defaults()
        });
        let theme = Theme::new(&cfg, &gfx);
        let watcher = config::watch(&cfg_path, msg_hwnd.0 as isize, WM_APP_RELOAD)
            .map_err(|e| log!("config watcher failed: {e}"))
            .ok();

        let mut win_hooks = Vec::new();
        unsafe {
            for (min, max) in [
                (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
                (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
                (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
                (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
            ] {
                let h = SetWinEventHook(
                    min,
                    max,
                    None,
                    Some(win_event),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                );
                if !h.is_invalid() {
                    win_hooks.push(h);
                }
            }
        }

        Ok(Self {
            gfx,
            cfg,
            cfg_path,
            theme,
            monitor: Monitor::primary(),
            msg_hwnd,
            pid: unsafe { GetCurrentProcessId() },
            clock: FrameClock::new(msg_hwnd.0 as isize),
            animating: false,
            last_frame: Instant::now(),
            icons: IconStore::default(),
            names: apps::AppNames::default(),
            menubar: None,
            dock: None,
            island: None,
            widgets: None,
            menu: None,
            settings: None,
            onboarding: None,
            finders: Vec::new(),
            next_finder_id: 1,
            loader: finder::loader::IconLoader::new(msg_hwnd.0 as isize, WM_APP_ICONS),
            kinds: finder::fs::KindCache::default(),
            finder_changed: Vec::new(),
            spotlight: None,
            icon_picker: None,
            index: Default::default(),
            hooks: Vec::new(),
            island_hook: None,
            desktop_hook: None,
            desktop_watch: None,
            desktop_popup: None,
            win_hooks,
            _watcher: watcher,
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            fullscreen_reported: false,
            fullscreen_hidden: false,
            backdrop_state: backdrop_state(),
            retired: Vec::new(),
        })
    }

    /// (Re)creates every surface from the current config.
    fn build(&mut self) {
        crate::win::glass::set_enabled(self.cfg.general.glass_refraction);
        self.close_menu();
        self.retire_island();
        self.retire_widgets();
        self.retire_dock();
        self.retire_menubar();
        self.theme = Theme::new(&self.cfg, &self.gfx);
        self.backdrop_state = backdrop_state();
        self.monitor = Monitor::primary();

        if self.cfg.general.hide_taskbar {
            taskbar::hide();
        } else {
            taskbar::restore();
        }
        if self.cfg.menubar.enabled && self.cfg.menubar.show_tray {
            if let Err(e) = crate::win::tray::start() {
                log!("tray host failed: {e}");
            }
        } else {
            crate::win::tray::stop();
        }
        if self.cfg.menubar.enabled {
            match MenuBar::new(&self.gfx, &self.theme, &self.cfg, self.monitor) {
                Ok(m) => {
                    MENUBAR_HWND.store(m.surface.content.0 as isize, Ordering::Relaxed);
                    self.menubar = Some(m);
                }
                Err(e) => log!("menu bar failed: {e}"),
            }
        }
        if self.cfg.dock.enabled {
            match Dock::new(&self.gfx, &self.theme, &self.cfg, self.monitor) {
                Ok(d) => self.dock = Some(d),
                Err(e) => log!("dock failed: {e}"),
            }
        }
        // After the menu bar, so it stacks above it.
        self.rebuild_island();
        self.rebuild_widgets();
        // The new surfaces start shown; hide them again if a fullscreen app is still up.
        self.fullscreen_hidden = false;
        self.update_fullscreen();
        self.setup_desktops();
        self.refresh_windows();
        self.flush();
        self.register_hotkey();
        if self.index.lock().is_ok_and(|i| i.built.is_none()) {
            self.rebuild_index();
        }
        log!("built surfaces (scale {}, font {})", self.monitor.scale, self.theme.font);
    }

    /// Applies a changed config with the least disruption: unchanged surfaces
    /// stay put, and a pinned-apps change updates the Dock in place.
    fn reload(&mut self) {
        let cfg = match config::load(&self.cfg_path) {
            Ok(cfg) => cfg,
            Err(e) => return log!("config not applied: {e:#}"),
        };
        if cfg == self.cfg {
            return;
        }
        let old = std::mem::replace(&mut self.cfg, cfg);
        self.mark_settings_dirty();
        let (g0, g1) = (&old.general, &self.cfg.general);
        let rebuild = g0.appearance != g1.appearance || g0.hide_taskbar != g1.hide_taskbar
            || g0.glass_refraction != g1.glass_refraction;
        let sidebar_changed = g0.appearance != g1.appearance
            || g0.glass_refraction != g1.glass_refraction
            || old.finder.sidebar != self.cfg.finder.sidebar;
        let general_changed = g0 != g1;
        if rebuild {
            // Backdrop tints depend on light/dark, and glass on refraction; recreate everything.
            self.build();
        }
        if sidebar_changed {
            for f in &mut self.finders {
                f.set_sidebar(&self.cfg.finder, &self.theme);
            }
        }
        if rebuild {
            return;
        }
        if general_changed {
            // Font / accent / shortcut hints: same surfaces, new paint.
            self.theme = Theme::new(&self.cfg, &self.gfx);
            if let Some(mb) = &mut self.menubar {
                mb.dirty = true;
            }
            if let Some(d) = &mut self.dock {
                d.dirty = true;
            }
            if let Some(w) = &mut self.widgets {
                w.invalidate();
            }
        }
        if old.spotlight != self.cfg.spotlight {
            self.register_hotkey();
        }
        if old.desktops != self.cfg.desktops {
            self.setup_desktops();
        }
        if old.menubar != self.cfg.menubar {
            self.rebuild_menubar();
        }
        if old.island != self.cfg.island || old.menubar.enabled != self.cfg.menubar.enabled
            || old.menubar.height != self.cfg.menubar.height
        {
            self.rebuild_island();
        }
        let (w0, w1) = (&old.widgets, &self.cfg.widgets);
        if w0.enabled != w1.enabled || w0.backdrop != w1.backdrop || old.menubar.enabled != self.cfg.menubar.enabled
            || old.menubar.height != self.cfg.menubar.height
        {
            self.rebuild_widgets();
        } else if w0 != w1 || old.island.calendar_ics != self.cfg.island.calendar_ics
            || old.menubar.clock_format != self.cfg.menubar.clock_format
        {
            if let Some(w) = &mut self.widgets {
                w.sync(&self.gfx, &self.theme, &self.cfg);
            }
        }
        if old.dock != self.cfg.dock || old.app_icons != self.cfg.app_icons {
            let only_apps = dock_eq_except_apps(&old.dock, &self.cfg.dock) && old.app_icons == self.cfg.app_icons;
            match &mut self.dock {
                Some(d) if only_apps => {
                    d.set_apps(&self.cfg.dock.apps);
                    self.animate();
                }
                _ => self.rebuild_dock(),
            }
        }
        self.refresh_windows();
        self.flush();
    }

    fn rebuild_menubar(&mut self) {
        if self.menu.as_ref().is_some_and(|m| matches!(m.source, MenuSource::MenuBar(_))) {
            self.close_menu();
        }
        self.retire_menubar();
        if self.cfg.menubar.enabled {
            match MenuBar::new(&self.gfx, &self.theme, &self.cfg, self.monitor) {
                Ok(m) => {
                    MENUBAR_HWND.store(m.surface.content.0 as isize, Ordering::Relaxed);
                    self.menubar = Some(m);
                }
                Err(e) => log!("menu bar failed: {e}"),
            }
        }
    }

    /// Keeps `old` on screen for `RETIRE_MS`, under whatever replaces it.
    fn retire(&mut self, old: impl std::any::Any) {
        self.retired.push(Box::new(old));
        self.timer(TIMER_RETIRE, RETIRE_MS);
    }

    fn retire_menubar(&mut self) {
        if let Some(mut m) = self.menubar.take() {
            m.release_space();
            self.retire(m);
        }
    }

    fn retire_dock(&mut self) {
        if let Some(mut d) = self.dock.take() {
            d.release_space();
            self.retire(d);
        }
    }

    fn retire_island(&mut self) {
        if let Some(i) = self.island.take() {
            self.retire(i);
        }
    }

    fn retire_widgets(&mut self) {
        if let Some(mut w) = self.widgets.take() {
            w.save_state();
            self.retire(w);
        }
    }

    fn rebuild_island(&mut self) {
        self.retire_island();
        if self.cfg.island.enabled {
            let hwnd = self.msg_hwnd.0 as isize;
            match Island::new(&self.gfx, &self.theme, &self.cfg, self.monitor, hwnd, WM_APP_MEDIA, WM_APP_ISLAND) {
                Ok(i) => self.island = Some(i),
                Err(e) => log!("island failed: {e}"),
            }
        }
        // One low-level hook for volume keys and Caps Lock while the island runs.
        ISLAND_VOLUME_KEYS.store(self.island.is_some() && self.cfg.island.volume, Ordering::Relaxed);
        let want = self.island.is_some();
        match (want, self.island_hook) {
            (true, None) => unsafe {
                match SetWindowsHookExW(WH_KEYBOARD_LL, Some(island_key_hook), win::hinstance(), 0) {
                    Ok(h) => self.island_hook = Some(h),
                    Err(e) => log!("island key hook failed: {e}"),
                }
            },
            (false, Some(h)) => unsafe {
                let _ = UnhookWindowsHookEx(h);
                self.island_hook = None;
            },
            _ => {}
        }
        self.animate();
    }

    fn rebuild_widgets(&mut self) {
        self.retire_widgets();
        if self.cfg.widgets.enabled {
            let hwnd = self.msg_hwnd.0 as isize;
            self.widgets = Some(Widgets::new(&self.gfx, &self.theme, &self.cfg, self.monitor, hwnd, WM_APP_WIDGETS, WM_APP_WIDGET_MEDIA));
        }
    }

    /// Adds, removes or resizes a widget: written to the config, then applied.
    fn widget_cmd(&mut self, cmd: WidgetCmd) {
        if self.widgets.is_none() && matches!(cmd, WidgetCmd::Add(_) | WidgetCmd::Reset) {
            // Adding (or resetting) widgets while they're off turns them on.
            let on = config::ConfigValue::Bool(true);
            if let Err(e) = config::save_value(&self.cfg_path, "widgets", "enabled", &on) {
                return log!("could not turn widgets on: {e:#}");
            }
            self.reload();
        }
        let Some(items) = self.widgets.as_mut().and_then(|w| w.command(cmd)) else { return };
        self.save_widgets(&items);
    }

    fn save_widgets(&mut self, items: &[config::WidgetItem]) {
        match config::save_widgets(&self.cfg_path, items) {
            Ok(()) => self.reload(),
            Err(e) => log!("could not save widgets: {e:#}"),
        }
    }

    /// Installs or removes the desktop shortcuts and switch watcher to match the config.
    fn setup_desktops(&mut self) {
        let d = &self.cfg.desktops;
        DESKTOP_ALT_KEYS.store(d.alt_number, Ordering::Relaxed);
        DESKTOP_ARROW_KEYS.store(d.replace_windows_switcher, Ordering::Relaxed);
        let want_hook = d.alt_number || d.replace_windows_switcher;
        match (want_hook, self.desktop_hook) {
            (true, None) => unsafe {
                match SetWindowsHookExW(WH_KEYBOARD_LL, Some(desktop_key_hook), win::hinstance(), 0) {
                    Ok(h) => self.desktop_hook = Some(h),
                    Err(e) => log!("desktop key hook failed: {e}"),
                }
            },
            (false, Some(h)) => unsafe {
                let _ = UnhookWindowsHookEx(h);
                self.desktop_hook = None;
            },
            _ => {}
        }
        // Re-hooked on every build: Explorer may have restarted.
        self.desktop_popup = None;
        if d.replace_windows_switcher {
            self.desktop_popup = win::desktops::hide_switch_popup();
        }
        // The watcher also moves focus after a switch, so run it whenever shortcuts are on.
        if want_hook || d.island {
            if self.desktop_watch.is_none() {
                self.desktop_watch = win::desktops::watch(self.msg_hwnd.0 as isize, WM_APP_DESKTOP);
            }
        } else {
            self.desktop_watch = None;
        self.desktop_popup = None;
        }
    }

    /// After any desktop switch: focus that desktop's top window (the internal
    /// switch leaves focus behind), and show it in the island.
    fn on_desktop_changed(&mut self) {
        if let Some(top) = apps::list_windows().first() {
            push(Action::Activate(top.hwnd));
        }
        if !self.cfg.desktops.island {
            return;
        }
        if let (Some(i), Some(info)) = (&mut self.island, win::desktops::current()) {
            i.on_desktop(info.index, info.count, info.name);
            self.animate();
        }
    }

    /// Repaints the windows that show settings (System Settings, Setup Assistant).
    fn mark_settings_dirty(&mut self) {
        if let Some(s) = &mut self.settings {
            s.dirty = true;
        }
        if let Some(o) = &mut self.onboarding {
            o.dirty = true;
        }
    }

    fn open_onboarding(&mut self) {
        match &self.onboarding {
            Some(o) => o.show(),
            None => match Onboarding::new(&self.gfx, &self.monitor) {
                Ok(o) => self.onboarding = Some(o),
                Err(e) => log!("setup assistant failed: {e}"),
            },
        }
        self.flush();
    }

    /// Closes the Setup Assistant and remembers not to open it on launch again.
    fn finish_onboarding(&mut self) {
        self.onboarding = None;
        if !self.cfg.general.onboarding_done {
            push(Action::SetConfig { table: "general", key: "onboarding_done", value: config::ConfigValue::Bool(true) });
        }
    }

    fn open_settings(&mut self, page: Option<Page>) {
        match &mut self.settings {
            Some(s) => s.show_page(page),
            None => match SettingsWindow::new(&self.gfx, &self.monitor, page.unwrap_or(Page::General)) {
                Ok(s) => self.settings = Some(s),
                Err(e) => log!("settings window failed: {e}"),
            },
        }
        self.flush();
    }

    fn rebuild_dock(&mut self) {
        if self.menu.as_ref().is_some_and(|m| matches!(m.source, MenuSource::Dock(_))) {
            self.close_menu();
        }
        self.retire_dock();
        if self.cfg.dock.enabled {
            match Dock::new(&self.gfx, &self.theme, &self.cfg, self.monitor) {
                Ok(d) => self.dock = Some(d),
                Err(e) => log!("dock failed: {e}"),
            }
        }
    }

    fn animate(&mut self) {
        if !self.animating {
            self.animating = true;
            self.last_frame = Instant::now();
            self.clock.start();
        }
    }

    fn timer(&self, id: usize, ms: u32) {
        unsafe {
            SetTimer(self.msg_hwnd, id, ms, None);
        }
    }

    fn on_frame(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;
        let mut still = false;
        if let Some(d) = &mut self.dock {
            still |= d.tick(dt);
        }
        if let Some(i) = &mut self.island {
            still |= i.tick(dt);
        }
        if !still {
            self.animating = false;
            self.clock.stop();
        }
    }

    fn on_tick(&mut self) {
        if let Some(mb) = &mut self.menubar {
            mb.tick();
        }
        crate::win::tray::keep_on_top();
        if self.island.as_mut().is_some_and(|i| i.on_tick(system::battery())) {
            self.animate();
        }
        if let Some(w) = &mut self.widgets {
            w.on_tick();
        }
        if self.cfg.general.hide_taskbar {
            taskbar::reapply();
        }
        self.refresh_windows();
    }

    fn refresh_windows(&mut self) {
        let mut windows = apps::list_windows();
        windows.extend(self.finder_app_windows());
        if let Some(d) = &mut self.dock {
            d.update_running(&windows, &mut self.names, &self.cfg);
        }
        self.update_foreground();
    }

    /// Keeps the shell above app windows: Windows may lift a newly focused,
    /// maximized window over the topmost band. Raised bottom to top, so the
    /// island sits above the menu bar and an open menu / Spotlight above both.
    fn raise_surfaces(&self) {
        let surfaces = [
            self.dock.as_ref().map(|d| &d.surface),
            self.menubar.as_ref().map(|m| &m.surface),
            self.island.as_ref().map(|i| &i.surface),
            self.menu.as_ref().map(|m| &m.surface),
            self.spotlight.as_ref().map(|s| &s.surface),
            self.icon_picker.as_ref().map(|p| &p.surface),
        ];
        for s in surfaces.into_iter().flatten() {
            s.raise();
        }
    }

    /// Hides the shell surfaces while a real app fills the screen. Windows'
    /// report alone isn't trusted: live wallpapers and desktop overlays trigger
    /// it too, and it can go stale when focus moves on.
    fn update_fullscreen(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
        let hide = self.fullscreen_reported && apps::is_fullscreen_app(fg, self.monitor.rect);
        if hide == self.fullscreen_hidden {
            return;
        }
        self.fullscreen_hidden = hide;
        if let Some(mb) = &mut self.menubar {
            mb.surface.set_hidden(hide);
        }
        if let Some(d) = &mut self.dock {
            d.surface.set_hidden(hide);
        }
        if let Some(i) = &mut self.island {
            i.set_fullscreen(hide);
        }
    }

    fn update_foreground(&mut self) {
        self.raise_surfaces();
        self.update_fullscreen();
        let fg = unsafe { GetForegroundWindow() };
        // Our own shell surfaces (Dock, menu bar, menus) leave the state as it was.
        let ours = self.settings.as_ref().is_some_and(|s| s.hwnd == fg) || self.onboarding.as_ref().is_some_and(|o| o.hwnd == fg);
        let on_desktop = if self.finders.iter().any(|f| f.hwnd == fg) || ours {
            Some(false)
        } else if fg.is_invalid() || apps::window_pid(fg) == self.pid {
            None
        } else {
            Some(apps::is_desktop(fg) || win::class_name(fg) == "Shell_TrayWnd")
        };
        if let Some(on) = on_desktop {
            if self.dock.as_mut().is_some_and(|d| d.set_on_desktop(on)) {
                self.animate();
            }
            // Show Desktop (Win+D) restacks the desktop; keep widgets right above it.
            if on {
                if let Some(w) = &self.widgets {
                    w.place();
                }
            }
        }
        if self.finders.iter().any(|f| f.hwnd == fg) {
            if let Some(mb) = &mut self.menubar {
                mb.set_app("Finder".into(), fg, Some(FINDER_PATH.to_string()));
            }
            return;
        }
        if self.settings.as_ref().is_some_and(|s| s.hwnd == fg) {
            if let Some(mb) = &mut self.menubar {
                mb.set_app("System Settings".into(), fg, None);
            }
            return;
        }
        if self.onboarding.as_ref().is_some_and(|o| o.hwnd == fg) {
            if let Some(mb) = &mut self.menubar {
                mb.set_app("Setup Assistant".into(), fg, None);
            }
            return;
        }
        if fg.is_invalid() || apps::window_pid(fg) == self.pid {
            return;
        }
        let (name, exe) = if apps::is_desktop(fg) || win::class_name(fg) == "Shell_TrayWnd" {
            (self.cfg.menubar.desktop_app_name.clone(), None)
        } else {
            match apps::process_path(apps::effective_pid(fg)) {
                Some(exe) => (self.names.name_for(&exe, &self.cfg.app_names), Some(exe)),
                None => (apps::window_title(fg), None),
            }
        };
        if let Some(mb) = &mut self.menubar {
            mb.set_app(name, fg, exe);
        }
    }

    fn toggle_pin(&mut self, path: &str, name: &str, pin: bool) {
        let mut list = self.cfg.dock.apps.clone();
        if pin {
            if !list.iter().any(|a| a.path.eq_ignore_ascii_case(path)) {
                list.push(DockApp {
                    name: Some(name.to_string()),
                    path: path.to_string(),
                    args: None,
                    icon: None,
                    match_exe: None,
                });
            }
        } else {
            list.retain(|a| !a.path.eq_ignore_ascii_case(path));
        }
        // The file watcher picks up the change and rebuilds.
        if let Err(e) = config::save_dock_apps(&self.cfg_path, &list) {
            log!("could not update dock apps: {e:#}");
        }
    }

    /// Renders whatever changed since the last frame.
    fn flush(&mut self) {
        if let Some(mb) = &mut self.menubar {
            if mb.dirty {
                mb.render(&self.gfx, &self.theme, &mut self.icons);
            }
        }
        if let Some(d) = &mut self.dock {
            if d.dirty {
                d.render(&self.gfx, &self.theme, &mut self.icons);
            }
        }
        if let Some(i) = self.island.as_mut().filter(|i| i.dirty) {
            i.render(&self.gfx, &self.theme);
        }
        if let Some(w) = self.widgets.as_mut().filter(|w| w.needs_render()) {
            w.render(&self.gfx, &self.theme);
        }
        if let Some(s) = &mut self.settings {
            if s.dirty {
                s.render(&self.gfx, &self.theme, &self.cfg, &mut self.icons);
            }
        }
        if let Some(o) = self.onboarding.as_mut().filter(|o| o.dirty) {
            o.render(&self.gfx, &self.theme, &self.cfg);
        }
        if let Some(p) = self.icon_picker.as_mut().filter(|p| p.dirty) {
            p.render(&self.gfx, &self.theme);
        }
        if let Some(s) = self.spotlight.as_mut().filter(|s| s.dirty) {
            s.render(&self.gfx, &self.theme, &mut self.loader);
        }
        for f in self.finders.iter_mut().filter(|f| f.dirty) {
            let mut ctx = finder::Ctx { gfx: &self.gfx, theme: &self.theme, loader: &mut self.loader, kinds: &mut self.kinds };
            f.render(&mut ctx);
        }
    }

    // --- Spotlight ---------------------------------------------------------

    fn toggle_spotlight(&mut self) {
        if self.spotlight.take().is_some() || !self.cfg.spotlight.enabled {
            return;
        }
        self.close_menu();
        let stale = self.index.lock().map(|i| i.built.is_none_or(|t| t.elapsed() > spotlight::index::STALE_AFTER));
        if stale.unwrap_or(true) {
            self.rebuild_index();
        }
        match Spotlight::new(&self.gfx, &self.theme, &self.cfg, &self.monitor) {
            Ok(mut s) => {
                s.render(&self.gfx, &self.theme, &mut self.loader);
                s.focus();
                self.spotlight = Some(s);
            }
            Err(e) => log!("spotlight failed: {e}"),
        }
    }

    /// Re-indexes apps and the user's folders in the background.
    fn rebuild_index(&self) {
        let home = finder::fs::home();
        let roots = finder::fs::favorites().into_iter().map(|p| p.path).filter(|p| *p != home).collect();
        spotlight::index::rebuild(self.index.clone(), roots, self.msg_hwnd.0 as isize, WM_APP_INDEX_READY);
    }

    fn register_hotkey(&mut self) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT};
        unsafe {
            let _ = UnregisterHotKey(self.msg_hwnd, HOTKEY_SPOTLIGHT);
        }
        if !self.cfg.spotlight.enabled {
            return;
        }
        let spec = &self.cfg.spotlight.hotkey;
        let Some((mods, vk)) = win::parse_hotkey(spec) else {
            return log!("spotlight hotkey '{spec}' isn't valid");
        };
        if unsafe { RegisterHotKey(self.msg_hwnd, HOTKEY_SPOTLIGHT, mods | MOD_NOREPEAT, vk) }.is_err() {
            log!("spotlight hotkey '{spec}' is already used by another app");
        }
    }

    fn on_spotlight_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
        let s = self.spotlight.as_mut()?;
        let idx = self.index.lock().ok()?;
        let (x, y) = mouse_pt(lp, s.surface.scale);
        let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe { GetKeyState(vk.0 as i32) < 0 };
        let event = match msg {
            WM_KEYDOWN | WM_SYSKEYDOWN => s.on_key(wp.0 as u16, down(VK_CONTROL), down(VK_SHIFT), &idx),
            WM_CHAR => {
                if let Some(c) = char::from_u32(wp.0 as u32) {
                    s.on_char(c, &idx);
                }
                SpotlightEvent::None
            }
            // Swallow Alt+key menu beeps while Alt is still held from the hotkey.
            WM_SYSCHAR => SpotlightEvent::None,
            WM_MOUSEMOVE => {
                if !s.tracking {
                    s.tracking = true;
                    track_leave(s.surface.content);
                }
                s.on_move(x, y);
                SpotlightEvent::None
            }
            WM_MOUSELEAVE => {
                s.tracking = false;
                SpotlightEvent::None
            }
            WM_LBUTTONUP => s.on_click(x, y, down(VK_CONTROL)),
            _ => return None,
        };
        drop(idx);
        match event {
            SpotlightEvent::None => {}
            SpotlightEvent::Close => self.spotlight = None,
            SpotlightEvent::Run(action) => {
                self.spotlight = None;
                push(action);
            }
        }
        Some(LRESULT(0))
    }

    // --- Icon picker -------------------------------------------------------

    fn open_icon_picker(&mut self, target: IconTarget, name: &str) {
        self.close_menu();
        self.spotlight = None;
        let hwnd = self.msg_hwnd.0 as isize;
        match IconPicker::new(&self.gfx, &self.theme, &self.cfg, &self.monitor, target, name, hwnd, WM_APP_ICON_PICKER) {
            Ok(mut p) => {
                p.render(&self.gfx, &self.theme);
                p.focus();
                self.icon_picker = Some(p);
            }
            Err(e) => log!("icon picker failed: {e}"),
        }
    }

    /// Writes a Dock app's custom icon (`None` resets it) and applies it. A
    /// pinned app keeps it in its `[[dock.apps]]` entry; others in `[app_icons]`.
    fn set_app_icon(&mut self, target: &IconTarget, icon: Option<String>) {
        self.icon_picker = None;
        let path = self.cfg_path.clone();
        let written = (|| -> anyhow::Result<()> {
            if let Some(pinned) = &target.pinned {
                let mut apps = self.cfg.dock.apps.clone();
                for a in apps.iter_mut().filter(|a| a.path == *pinned) {
                    a.icon = icon.clone();
                }
                config::save_dock_apps(&path, &apps)?;
            }
            if let Some(exe) = &target.exe {
                match (&icon, target.pinned.is_some()) {
                    (Some(i), false) => config::save_value(&path, "app_icons", exe, &config::ConfigValue::Str(i.clone()))?,
                    // A pinned app's own entry wins; drop any older per-exe icon.
                    _ => config::remove_value(&path, "app_icons", exe)?,
                }
            }
            Ok(())
        })();
        match written {
            Ok(()) => self.reload(),
            Err(e) => log!("could not save the icon: {e:#}"),
        }
    }

    fn on_picker_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
        let p = self.icon_picker.as_mut()?;
        let (x, y) = mouse_pt(lp, p.surface.scale);
        let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe { GetKeyState(vk.0 as i32) < 0 };
        let event = match msg {
            WM_KEYDOWN | WM_SYSKEYDOWN => p.on_key(wp.0 as u16, down(VK_CONTROL), down(VK_SHIFT)),
            WM_CHAR => {
                if let Some(c) = char::from_u32(wp.0 as u32) {
                    p.on_char(c);
                }
                PickerEvent::None
            }
            WM_SYSCHAR => PickerEvent::None,
            WM_MOUSEWHEEL => {
                p.on_wheel((wp.0 >> 16) as u16 as i16);
                PickerEvent::None
            }
            WM_MOUSEMOVE => {
                if !p.tracking {
                    p.tracking = true;
                    track_leave(p.surface.content);
                }
                p.on_move(x, y);
                PickerEvent::None
            }
            WM_MOUSELEAVE => {
                p.on_leave();
                PickerEvent::None
            }
            WM_LBUTTONUP => p.on_click(x, y),
            _ => return None,
        };
        match event {
            PickerEvent::None => {}
            PickerEvent::Close => self.icon_picker = None,
            PickerEvent::Run(action) => push(action),
        }
        Some(LRESULT(0))
    }

    // --- Finder ------------------------------------------------------------

    fn open_finder(&mut self, path: Option<PathBuf>) {
        let id = self.next_finder_id;
        self.next_finder_id += 1;
        let path = path.unwrap_or_else(finder::fs::home);
        let cascade = self.finders.len() as i32;
        let mut ctx = finder::Ctx { gfx: &self.gfx, theme: &self.theme, loader: &mut self.loader, kinds: &mut self.kinds };
        let prefs = &self.cfg.finder;
        match FinderWindow::new(&mut ctx, &self.monitor, id, path, cascade, self.msg_hwnd.0 as isize, WM_APP_FINDER_CHANGED, prefs) {
            Ok(mut f) => {
                f.render(&mut ctx);
                unsafe {
                    let _ = ShowWindow(f.hwnd, SW_SHOW);
                    let _ = SetForegroundWindow(f.hwnd);
                }
                self.finders.push(f);
                self.refresh_windows();
            }
            Err(e) => log!("finder window failed: {e}"),
        }
    }

    fn close_finder(&mut self, id: u32) {
        self.finders.retain(|f| f.id != id);
        self.refresh_windows();
    }

    fn finder_cmd(&mut self, id: u32, cmd: finder::FinderCmd) {
        if let Some(f) = self.finders.iter_mut().find(|f| f.id == id) {
            f.command(cmd, &mut self.kinds);
        }
        self.save_finder_prefs();
        self.flush();
    }

    /// Saves a Finder window's changed view / sort / hidden-files choice as the default for new windows.
    fn save_finder_prefs(&mut self) {
        use config::ConfigValue::{Bool, Str};
        let Some(f) = self.finders.iter_mut().find(|f| f.prefs_dirty) else { return };
        f.prefs_dirty = false;
        let (view, sort, ascending, show_hidden) = f.prefs();
        let c = &mut self.cfg.finder;
        if (c.view, c.sort, c.ascending, c.show_hidden) == (view, sort, ascending, show_hidden) {
            return;
        }
        (c.view, c.sort, c.ascending, c.show_hidden) = (view, sort, ascending, show_hidden);
        let values = [
            ("view", Str(view.name().into())),
            ("sort", Str(sort.name().into())),
            ("ascending", Bool(ascending)),
            ("show_hidden", Bool(show_hidden)),
        ];
        if let Err(e) = config::save_values(&self.cfg_path, "finder", &values) {
            log!("could not save Finder settings: {e:#}");
        }
    }

    /// Our Finder windows, shaped like other apps' windows for the Dock and menus.
    fn finder_app_windows(&self) -> Vec<apps::AppWindow> {
        self.finders
            .iter()
            .map(|f| apps::AppWindow { hwnd: f.hwnd, exe: FINDER_PATH.to_string(), title: f.title() })
            .collect()
    }

    fn on_finder_message(&mut self, idx: usize, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
        let f = &mut self.finders[idx];
        let scale = f.scale();
        let (x, y) = mouse_pt(lp, scale);
        let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe { GetKeyState(vk.0 as i32) < 0 };
        let event = match msg {
            WM_NCHITTEST => {
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetWindowRect(hwnd, &mut rc);
                }
                let sx = (lp.0 & 0xFFFF) as u16 as i16 as i32;
                let sy = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let hit = f.nc_hit((sx - rc.left) as f32 / scale, (sy - rc.top) as f32 / scale);
                return Some(LRESULT(hit as isize));
            }
            WM_GETMINMAXINFO => {
                let info = unsafe { &mut *(lp.0 as *mut MINMAXINFO) };
                let (w, h) = f.min_track_size();
                info.ptMinTrackSize = POINT { x: w, y: h };
                return Some(LRESULT(0));
            }
            WM_WINDOWPOSCHANGED => {
                f.sync_backdrop();
                f.dirty = true;
                return None;
            }
            WM_ACTIVATE => {
                f.focused = (wp.0 & 0xFFFF) != 0;
                f.dirty = true;
                return None;
            }
            WM_MOUSEMOVE => {
                if !f.tracking {
                    f.tracking = true;
                    track_leave(hwnd);
                }
                f.on_move(x, y)
            }
            WM_MOUSELEAVE => {
                f.on_leave();
                FinderEvent::None
            }
            WM_LBUTTONDOWN => {
                f.on_down(x, y, down(VK_CONTROL), down(VK_SHIFT), &mut self.kinds);
                FinderEvent::None
            }
            WM_LBUTTONUP => f.on_up(x, y, &mut self.kinds),
            WM_RBUTTONUP => f.on_right_click(x, y, &mut self.kinds),
            WM_MBUTTONUP => f.on_middle(x, y, &mut self.kinds),
            WM_MOUSEWHEEL => {
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetWindowRect(hwnd, &mut rc);
                }
                let sx = (lp.0 & 0xFFFF) as u16 as i16 as i32;
                f.on_wheel((sx - rc.left) as f32 / scale, ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32);
                FinderEvent::None
            }
            // Alt+F4 keeps its default meaning (WM_CLOSE).
            WM_SYSKEYDOWN if wp.0 == 0x73 => return None,
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                f.on_key(wp.0 as u16, down(VK_CONTROL), down(VK_SHIFT), down(VK_MENU), &mut self.kinds)
            }
            WM_CHAR => {
                if let Some(c) = char::from_u32(wp.0 as u32) {
                    f.on_char(c);
                }
                FinderEvent::None
            }
            WM_SYSCHAR => return Some(LRESULT(0)),
            WM_CLOSE => FinderEvent::Close,
            _ => return None,
        };
        let id = f.id;
        match event {
            FinderEvent::None => {}
            FinderEvent::Close => self.close_finder(id),
            FinderEvent::NewWindow => self.open_finder(None),
            FinderEvent::Menu(entries, x, y) => self.open_menu(entries, Placement::Below { x, y }, MenuSource::Window),
            FinderEvent::Run(cmd) => push(Action::Finder { window: id, cmd }),
        }
        self.save_finder_prefs();
        Some(LRESULT(0))
    }

    // --- menus -------------------------------------------------------------

    fn open_menu(&mut self, entries: Vec<Entry>, placement: Placement, source: MenuSource) {
        self.close_menu();
        if entries.is_empty() {
            return;
        }
        match PopupMenu::new(&self.gfx, &self.theme, &self.cfg, entries, placement, &self.monitor, source) {
            Ok(mut m) => {
                m.render(&self.gfx, &self.theme);
                OPEN_MENU_HWND.store(m.surface.content.0 as isize, Ordering::Relaxed);
                self.menu = Some(m);
                self.install_hooks();
                match source {
                    MenuSource::MenuBar(hit) => {
                        if let Some(mb) = &mut self.menubar {
                            mb.set_open(Some(hit));
                        }
                    }
                    MenuSource::Dock(i) => {
                        if let Some(d) = &mut self.dock {
                            d.menu_item = Some(i);
                            d.dirty = true;
                        }
                    }
                    MenuSource::Window => {}
                }
            }
            Err(e) => log!("menu failed: {e}"),
        }
    }

    fn close_menu(&mut self) {
        OPEN_MENU_HWND.store(0, Ordering::Relaxed);
        if self.menu.take().is_none() {
            return;
        }
        self.remove_hooks();
        if let Some(mb) = &mut self.menubar {
            mb.set_open(None);
        }
        if let Some(d) = &mut self.dock {
            if d.menu_item.take().is_some() {
                d.on_leave();
                self.animate();
            }
        }
    }

    fn open_menubar_menu(&mut self, hit: Hit) {
        let Some(mb) = &self.menubar else { return };
        let Some(placement) = mb.menu_placement(hit) else { return };
        let exe = mb.target_exe.clone();
        let app_name = mb.app_name.clone();
        let target = mb.target;
        let app_windows: Vec<apps::AppWindow> = match &exe {
            Some(exe) if exe == FINDER_PATH => self.finder_app_windows(),
            Some(exe) => apps::list_windows().into_iter().filter(|w| w.exe.eq_ignore_ascii_case(exe)).collect(),
            None => Vec::new(),
        };
        let ctx = MenuCtx { app_name: &app_name, target, app_windows: &app_windows };
        let entries = match hit {
            Hit::Logo => menu::apple_menu(),
            Hit::AppName => menu::app_menu(&ctx),
            Hit::Title(i) => menu::title_menu(i, &ctx),
            Hit::Tray => {
                use crate::win::tray::{self, Button};
                let px = (16.0 * self.monitor.scale).round() as i32;
                let icons = tray::icons(px);
                if icons.is_empty() {
                    vec![Entry::disabled("No tray icons")]
                } else {
                    icons
                        .into_iter()
                        .map(|(id, tip, pixels)| {
                            Entry::with_icon(tip, pixels, Action::TrayClick(id, Button::Left), Action::TrayClick(id, Button::Right))
                        })
                        .collect()
                }
            }
            _ => return,
        };
        self.open_menu(entries, placement, MenuSource::MenuBar(hit));
    }

    fn install_hooks(&mut self) {
        if !self.hooks.is_empty() {
            return;
        }
        unsafe {
            let procs: [(WINDOWS_HOOK_ID, HOOKPROC); 2] =
                [(WH_MOUSE_LL, Some(mouse_hook)), (WH_KEYBOARD_LL, Some(key_hook))];
            for (id, proc_) in procs {
                match SetWindowsHookExW(id, proc_, win::hinstance(), 0) {
                    Ok(h) => self.hooks.push(h),
                    Err(e) => log!("hook failed: {e}"),
                }
            }
        }
    }

    fn remove_hooks(&mut self) {
        for h in self.hooks.drain(..) {
            unsafe {
                let _ = UnhookWindowsHookEx(h);
            }
        }
    }

    fn on_menu_key(&mut self, vk: u16) {
        let Some(m) = &mut self.menu else { return };
        let (event, redraw) = m.on_key(vk);
        if redraw {
            m.render(&self.gfx, &self.theme);
        }
        let source = m.source;
        match event {
            MenuEvent::Activate(a) => {
                self.close_menu();
                push(a);
            }
            MenuEvent::Close => self.close_menu(),
            MenuEvent::Step(dir) => {
                if let MenuSource::MenuBar(hit) = source {
                    self.open_menubar_menu(hit.step(dir));
                }
            }
            MenuEvent::None => {}
        }
    }

    // --- message routing ---------------------------------------------------

    fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if hwnd == self.msg_hwnd {
            match msg {
                WM_COPYDATA => {
                    if let (Some(i), Some(json)) = (self.island.as_mut(), crate::island::claude::receive(lp)) {
                        i.on_claude(&json);
                        self.animate();
                    }
                }
                WM_APP_FRAME => self.on_frame(),
                WM_POWERBROADCAST => {
                    if wp.0 as u32 == PBT_POWERSETTINGCHANGE {
                        // Give DWM a moment to switch its effects first.
                        self.timer(TIMER_THEME, 800);
                    }
                    return Some(LRESULT(1));
                }
                WM_TIMER => {
                    let id = wp.0;
                    if id != TIMER_TICK {
                        unsafe {
                            let _ = KillTimer(self.msg_hwnd, id);
                        }
                    }
                    match id {
                        TIMER_TICK => self.on_tick(),
                        TIMER_REFRESH => self.refresh_windows(),
                        TIMER_RELOAD => self.reload(),
                        TIMER_REBUILD => self.build(),
                        TIMER_RETIRE => self.retired.clear(),
                        TIMER_WALLPAPER => win::glass::wallpaper_changed(),
                        // Wallpaper Engine and similar re-tint the accent color constantly;
                        // a full rebuild each time makes everything flicker away.
                        TIMER_THEME
                            if crate::theme::wants_dark(&self.cfg) != self.theme.dark
                                || backdrop_state() != self.backdrop_state =>
                        {
                            self.build()
                        }
                        TIMER_DISPLAY if Monitor::primary() != self.monitor => self.build(),
                        TIMER_DISPLAY => {
                            self.raise_surfaces();
                            if let Some(w) = &self.widgets {
                                w.place();
                            }
                        }
                        TIMER_FINDER => {
                            for id in std::mem::take(&mut self.finder_changed) {
                                if let Some(f) = self.finders.iter_mut().find(|f| f.id == id) {
                                    f.reload(&mut self.kinds);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                WM_APP_RELOAD => self.timer(TIMER_RELOAD, 150),
                WM_APP_FOREGROUND => {
                    self.update_foreground();
                    self.timer(TIMER_REFRESH, 100);
                }
                WM_APP_WINDOWS_CHANGED => self.timer(TIMER_REFRESH, 150),
                WM_APP_CLOSE_MENU => self.close_menu(),
                WM_APP_OPEN_SETTINGS if wp.0 == 1 => self.open_finder(None),
                WM_APP_OPEN_SETTINGS => self.open_settings(None),
                WM_HOTKEY if wp.0 as i32 == HOTKEY_SPOTLIGHT => self.toggle_spotlight(),
                WM_APP_CLOSE_SPOTLIGHT => self.spotlight = None,
                WM_APP_CLOSE_PICKER => self.icon_picker = None,
                WM_APP_ICON_PICKER => {
                    if let Some(p) = &mut self.icon_picker {
                        p.poll();
                    }
                }
                WM_APP_INDEX_READY => {
                    if let (Some(s), Ok(idx)) = (&mut self.spotlight, self.index.lock()) {
                        s.search(&idx);
                    }
                }
                WM_APP_ICONS => {
                    if self.loader.drain() {
                        self.finders.iter_mut().for_each(|f| f.dirty = true);
                        if let Some(s) = &mut self.spotlight {
                            s.dirty = true;
                        }
                    }
                }
                WM_APP_FINDER_CHANGED => {
                    let id = wp.0 as u32;
                    if !self.finder_changed.contains(&id) {
                        self.finder_changed.push(id);
                    }
                    self.timer(TIMER_FINDER, 200);
                }
                WM_APP_MENU_KEY => self.on_menu_key(wp.0 as u16),
                WM_APP_MEDIA => {
                    if let Some(i) = &mut self.island {
                        i.on_media();
                        self.animate();
                    }
                }
                WM_APP_ISLAND => {
                    if let Some(i) = &mut self.island {
                        i.on_signals();
                        self.animate();
                    }
                }
                WM_APP_WIDGETS => {
                    if let Some(w) = &mut self.widgets {
                        w.on_data();
                    }
                }
                WM_APP_WIDGET_MEDIA => {
                    if let Some(w) = &mut self.widgets {
                        w.on_media();
                    }
                }
                WM_APP_UPDATE => {
                    if let (Some(i), Some(r)) = (&mut self.island, crate::update::available()) {
                        i.show_update(&r);
                        self.animate();
                    }
                    self.mark_settings_dirty();
                }
                WM_APP_DESKTOP_KEY => match wp.0 {
                    DESKTOP_LEFT => win::desktops::step(-1),
                    DESKTOP_RIGHT => win::desktops::step(1),
                    n => {
                        if !win::desktops::switch_to(n - 1) {
                            if let (Some(i), Some(cur)) = (self.island.as_mut().filter(|_| self.cfg.desktops.island), win::desktops::current()) {
                                i.on_desktop_missing(n, cur.index, cur.count);
                                self.animate();
                            }
                        }
                    }
                },
                WM_APP_DESKTOP => self.on_desktop_changed(),
                WM_APP_ISLAND_KEY => {
                    if let Some(i) = &mut self.island {
                        match wp.0 as u16 {
                            0x14 => i.on_caps(),
                            0xAD => i.on_volume_key(VolumeKey::Mute),
                            0xAE => i.on_volume_key(VolumeKey::Down),
                            _ => i.on_volume_key(VolumeKey::Up),
                        }
                        self.animate();
                    }
                }
                _ => return None,
            }
            self.flush();
            return Some(LRESULT(0));
        }

        // Broadcasts reach every one of our top-level windows; debounce into one rebuild.
        if msg == self.taskbar_created {
            self.timer(TIMER_REBUILD, 400);
            return None;
        }
        if msg == WM_DISPLAYCHANGE || msg == WM_DPICHANGED {
            self.timer(TIMER_DISPLAY, 400);
            return None;
        }
        if msg == WM_SETTINGCHANGE {
            if wp.0 as u32 == SPI_SETDESKWALLPAPER.0 {
                self.timer(TIMER_WALLPAPER, 300);
            }
            let area = if lp.0 != 0 { unsafe { PCWSTR(lp.0 as *const u16).to_string().unwrap_or_default() } } else { String::new() };
            if area == "ImmersiveColorSet" {
                self.timer(TIMER_THEME, 400);
            }
            return None;
        }
        if msg == WM_APP_APPBAR {
            if wp.0 as u32 == ABN_FULLSCREENAPP {
                self.fullscreen_reported = lp.0 != 0;
                self.update_fullscreen();
                self.flush();
            }
            return Some(LRESULT(0));
        }

        if self.settings.as_ref().is_some_and(|s| s.hwnd == hwnd) {
            let r = self.on_settings_message(hwnd, msg, wp, lp);
            self.flush();
            return r;
        }
        if self.onboarding.as_ref().is_some_and(|o| o.hwnd == hwnd) {
            let r = self.on_onboarding_message(hwnd, msg, wp, lp);
            self.flush();
            return r;
        }
        if self.spotlight.as_ref().is_some_and(|s| s.surface.content == hwnd) {
            let r = self.on_spotlight_message(msg, wp, lp);
            self.flush();
            return r;
        }
        if self.icon_picker.as_ref().is_some_and(|p| p.surface.content == hwnd) {
            let r = self.on_picker_message(msg, wp, lp);
            self.flush();
            return r;
        }
        if let Some(idx) = self.finders.iter().position(|f| f.hwnd == hwnd) {
            let r = self.on_finder_message(idx, hwnd, msg, wp, lp);
            self.flush();
            return r;
        }

        let result = if self.menu.as_ref().is_some_and(|m| m.surface.content == hwnd) {
            self.on_menu_mouse(hwnd, msg, lp)
        } else if self.menubar.as_ref().is_some_and(|m| m.surface.content == hwnd) {
            self.on_menubar_mouse(hwnd, msg, lp)
        } else if self.dock.as_ref().is_some_and(|d| d.surface.content == hwnd) {
            self.on_dock_mouse(hwnd, msg, lp)
        } else if self.island.as_ref().is_some_and(|i| i.surface.content == hwnd) {
            self.on_island_mouse(hwnd, msg, wp, lp)
        } else if self.widgets.as_ref().is_some_and(|w| w.owns(hwnd)) {
            self.on_widget_message(hwnd, msg, wp, lp)
        } else {
            None
        };
        self.flush();
        result
    }

    fn on_settings_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let s = self.settings.as_mut()?;
        let scale = s.scale();
        let (x, y) = mouse_pt(lp, scale);
        match msg {
            WM_NCHITTEST => {
                // lParam is in screen coordinates here.
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetWindowRect(hwnd, &mut rc);
                }
                let sx = (lp.0 & 0xFFFF) as u16 as i16 as i32;
                let sy = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let (lx, ly) = ((sx - rc.left) as f32 / scale, (sy - rc.top) as f32 / scale);
                let hit = if s.is_caption(lx, ly) { HTCAPTION } else { HTCLIENT };
                return Some(LRESULT(hit as isize));
            }
            WM_MOUSEMOVE => {
                if !s.tracking {
                    s.tracking = true;
                    track_leave(hwnd);
                }
                s.on_move(x, y);
            }
            WM_MOUSELEAVE => s.on_leave(),
            WM_LBUTTONDOWN => {
                s.on_down(x, y, &self.cfg);
                unsafe { SetCapture(hwnd) };
            }
            WM_LBUTTONUP => {
                unsafe {
                    let _ = ReleaseCapture();
                }
                match s.on_up(x, y, &self.cfg) {
                    SettingsEvent::Action(a) => push(a),
                    SettingsEvent::Close => self.settings = None,
                    SettingsEvent::None => {}
                }
            }
            WM_MOUSEWHEEL => s.on_wheel(((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32),
            WM_KEYDOWN => {
                use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
                let (ctrl, shift) = unsafe { (GetKeyState(VK_CONTROL.0 as i32) < 0, GetKeyState(VK_SHIFT.0 as i32) < 0) };
                match s.on_key(wp.0 as u16, ctrl, shift, &self.cfg) {
                    SettingsEvent::Action(a) => push(a),
                    SettingsEvent::Close => self.settings = None,
                    SettingsEvent::None => {}
                }
            }
            WM_CHAR => {
                if let Some(c) = char::from_u32(wp.0 as u32) {
                    s.on_char(c);
                }
            }
            WM_CLOSE => self.settings = None,
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn on_onboarding_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let o = self.onboarding.as_mut()?;
        let scale = o.scale();
        let (x, y) = mouse_pt(lp, scale);
        let event = match msg {
            WM_NCHITTEST => {
                // lParam is in screen coordinates here.
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetWindowRect(hwnd, &mut rc);
                }
                let sx = (lp.0 & 0xFFFF) as u16 as i16 as i32;
                let sy = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let (lx, ly) = ((sx - rc.left) as f32 / scale, (sy - rc.top) as f32 / scale);
                let hit = if o.is_caption(lx, ly) { HTCAPTION } else { HTCLIENT };
                return Some(LRESULT(hit as isize));
            }
            WM_MOUSEMOVE => {
                if !o.tracking {
                    o.tracking = true;
                    track_leave(hwnd);
                }
                o.on_move(x, y);
                OnboardingEvent::None
            }
            WM_MOUSELEAVE => {
                o.on_leave();
                OnboardingEvent::None
            }
            WM_LBUTTONDOWN => {
                o.on_down(x, y, &self.cfg);
                unsafe { SetCapture(hwnd) };
                OnboardingEvent::None
            }
            WM_LBUTTONUP => {
                unsafe {
                    let _ = ReleaseCapture();
                }
                o.on_up(x, y, &self.cfg)
            }
            WM_KEYDOWN => o.on_key(wp.0 as u16),
            WM_CLOSE => OnboardingEvent::Finish(None),
            _ => return None,
        };
        match event {
            OnboardingEvent::None => {}
            OnboardingEvent::Action(a) => push(a),
            OnboardingEvent::Finish(a) => {
                self.finish_onboarding();
                if let Some(a) = a {
                    push(a);
                }
            }
        }
        Some(LRESULT(0))
    }

    fn on_menu_mouse(&mut self, hwnd: HWND, msg: u32, lp: LPARAM) -> Option<LRESULT> {
        let m = self.menu.as_mut()?;
        let (x, y) = mouse_pt(lp, m.surface.scale);
        match msg {
            WM_MOUSEMOVE => {
                track_leave(hwnd);
                if m.on_move(x, y) {
                    m.render(&self.gfx, &self.theme);
                }
            }
            WM_MOUSELEAVE => {
                if m.on_leave() {
                    m.render(&self.gfx, &self.theme);
                }
            }
            WM_LBUTTONUP | WM_RBUTTONUP => match m.on_click(x, y, msg == WM_RBUTTONUP) {
                MenuEvent::Activate(a) => {
                    self.close_menu();
                    push(a);
                }
                MenuEvent::Close => self.close_menu(),
                _ => {}
            },
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn on_menubar_mouse(&mut self, hwnd: HWND, msg: u32, lp: LPARAM) -> Option<LRESULT> {
        let mb = self.menubar.as_mut()?;
        let (x, y) = mouse_pt(lp, mb.surface.scale);
        let hit = mb.hit_at(x, y);
        match msg {
            WM_MOUSEMOVE => {
                track_leave(hwnd);
                // With a menu open, sliding across titles switches menus.
                if let (Some(open), Some(h)) = (mb.open, hit) {
                    if h.opens_menu() && h != open {
                        self.open_menubar_menu(h);
                    }
                }
            }
            WM_MOUSELEAVE => mb.set_pressed(None),
            WM_LBUTTONDOWN => match hit {
                Some(h) if h.opens_menu() => {
                    if mb.open == Some(h) {
                        self.close_menu();
                    } else {
                        self.open_menubar_menu(h);
                    }
                }
                Some(h) => {
                    self.close_menu();
                    if let Some(mb) = &mut self.menubar {
                        mb.set_pressed(Some(h));
                    }
                    if let Some(a) = MenuBar::status_action(h, self.cfg.spotlight.enabled) {
                        push(a);
                    }
                }
                None => self.close_menu(),
            },
            WM_LBUTTONUP => mb.set_pressed(None),
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn island_event(&mut self, e: IslandEvent) {
        match e {
            IslandEvent::OpenApp(id) => push(open_media_app(&id)),
            IslandEvent::Run(a) => push(a),
            IslandEvent::Menu(entries, placement) => self.open_menu(entries, placement, MenuSource::Window),
            IslandEvent::DragOut(paths) => push(Action::IslandDrag(paths)),
        }
    }

    fn on_island_mouse(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let i = self.island.as_mut()?;
        let (x, y) = mouse_pt(lp, i.surface.scale);
        match msg {
            WM_MOUSEMOVE => {
                if !i.tracking {
                    i.tracking = true;
                    track_leave(hwnd);
                }
                if let Some(e) = i.on_move(x, y) {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    self.island_event(e);
                }
            }
            WM_MOUSELEAVE => i.on_leave(),
            WM_MOUSEWHEEL => {
                use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
                let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                // Shift+wheel pages like a horizontal swipe (wheel up = previous).
                if unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0 {
                    i.on_hwheel(-delta);
                } else {
                    i.on_wheel(delta);
                }
            }
            WM_MOUSEHWHEEL => i.on_hwheel(((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32),
            WM_RBUTTONUP => {
                let e = i.context_menu();
                self.island_event(e);
            }
            WM_LBUTTONDOWN => {
                i.on_down(x, y);
                // Keep scrubbing when the pointer leaves the island.
                unsafe { SetCapture(hwnd) };
                self.close_menu();
            }
            WM_LBUTTONUP => {
                unsafe {
                    let _ = ReleaseCapture();
                }
                if let Some(e) = i.on_up(x, y) {
                    self.island_event(e);
                }
            }
            _ => return None,
        }
        self.animate();
        Some(LRESULT(0))
    }

    fn on_widget_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
        let w = self.widgets.as_mut()?;
        let (x, y) = mouse_pt(lp, w.scale());
        let event = match msg {
            WM_MOUSEMOVE => {
                if w.on_move(hwnd, x, y) {
                    track_leave(hwnd);
                }
                WidgetEvent::None
            }
            WM_MOUSELEAVE => {
                w.on_leave(hwnd);
                WidgetEvent::None
            }
            WM_LBUTTONDOWN => {
                w.on_down(hwnd, x, y);
                unsafe { SetCapture(hwnd) };
                WidgetEvent::None
            }
            WM_LBUTTONUP => {
                // (The capture change this sends arrives while we're busy, so it's ignored.)
                unsafe {
                    let _ = ReleaseCapture();
                }
                w.on_up(hwnd, x, y, &self.gfx, &self.theme)
            }
            WM_CAPTURECHANGED => w.on_capture_lost(),
            WM_RBUTTONUP => {
                if let Some((entries, placement)) = w.context_menu(hwnd, x, y) {
                    self.open_menu(entries, placement, MenuSource::Window);
                }
                WidgetEvent::None
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe { GetKeyState(vk.0 as i32) < 0 };
                if !w.on_key(hwnd, wp.0 as u16, down(VK_CONTROL), down(VK_SHIFT), &self.gfx, &self.theme) {
                    return None;
                }
                WidgetEvent::None
            }
            WM_CHAR => {
                if let Some(c) = char::from_u32(wp.0 as u32) {
                    w.on_char(hwnd, c, &self.gfx, &self.theme);
                }
                WidgetEvent::None
            }
            // Typing ends when the widget loses the keyboard; DefWindowProc still runs.
            WM_ACTIVATE => {
                if wp.0 & 0xFFFF == 0 {
                    w.on_deactivate(hwnd);
                }
                return None;
            }
            _ => return None,
        };
        match event {
            WidgetEvent::None => {}
            WidgetEvent::Action(a) => push(a),
            WidgetEvent::Save(items) => self.save_widgets(&items),
            WidgetEvent::OpenMedia(id) => push(open_media_app(&id)),
            WidgetEvent::Focus(h) => unsafe {
                let _ = SetForegroundWindow(h);
            },
        }
        Some(LRESULT(0))
    }

    fn on_dock_mouse(&mut self, hwnd: HWND, msg: u32, lp: LPARAM) -> Option<LRESULT> {
        let d = self.dock.as_mut()?;
        let (x, y) = mouse_pt(lp, d.surface.scale);
        let mut animate = false;
        match msg {
            WM_MOUSEMOVE => {
                if !d.tracking {
                    d.tracking = true;
                    track_leave(hwnd);
                }
                animate = d.on_move(x, y);
                // Keep receiving the pointer while a drag leaves the window.
                if d.dragging() && unsafe { GetCapture() } != hwnd {
                    unsafe { SetCapture(hwnd) };
                }
            }
            WM_MOUSELEAVE => {
                d.tracking = false;
                if d.menu_item.is_none() {
                    animate = d.on_leave();
                }
            }
            WM_CAPTURECHANGED => {
                if lp.0 != hwnd.0 as isize {
                    d.cancel_drag();
                    animate = true;
                }
            }
            WM_LBUTTONDOWN => {
                self.close_menu();
                if let Some(d) = &mut self.dock {
                    d.on_down(x, y);
                }
            }
            WM_LBUTTONUP => {
                let was_dragging = d.dragging();
                if let Some(a) = d.on_up(x, y) {
                    push(a);
                }
                if was_dragging {
                    animate = true;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
            }
            WM_RBUTTONUP => {
                if let Some((idx, entries, placement)) = d.context_menu(x, y) {
                    self.open_menu(entries, placement, MenuSource::Dock(idx));
                }
            }
            _ => return None,
        }
        if animate {
            self.animate();
        }
        Some(LRESULT(0))
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.remove_hooks();
        if let Some(h) = self.island_hook.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(h);
            }
        }
        if let Some(h) = self.desktop_hook.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(h);
            }
        }
        self.desktop_watch = None;
        for h in self.win_hooks.drain(..) {
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
        self.menu = None;
        self.island = None;
        self.widgets = None;
        self.dock = None;
        self.menubar = None;
        crate::win::tray::stop();
        taskbar::restore();
    }
}
