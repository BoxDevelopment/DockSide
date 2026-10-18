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
use crate::menu::{self, Action, Entry, MenuCtx, MenuEvent, MenuSource, Placement, PopupMenu};
use crate::menubar::{Hit, MenuBar};
use crate::finder::{self, FinderEvent, FinderWindow, FINDER_PATH};
use crate::settings::{Page, SettingsEvent, SettingsWindow, SETTINGS_HWND};
use crate::spotlight::{self, Spotlight, SpotlightEvent, SPOTLIGHT_HWND};
use crate::theme::Theme;
use crate::win::{self, apps, startup, system, taskbar, Monitor};
use crate::win::{
    WM_APP_APPBAR, WM_APP_CLOSE_MENU, WM_APP_OPEN_SETTINGS, WM_APP_FOREGROUND, WM_APP_FRAME, WM_APP_MENU_KEY, WM_APP_RELOAD,
    WM_APP_WINDOWS_CHANGED, WM_APP_ICONS, WM_APP_FINDER_CHANGED, WM_APP_CLOSE_SPOTLIGHT, WM_APP_INDEX_READY,
};

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static PENDING: RefCell<Vec<Action>> = const { RefCell::new(Vec::new()) };
}

static MSG_HWND: AtomicIsize = AtomicIsize::new(0);
/// Windows the menu mouse hook must not treat as "outside" clicks.
static OPEN_MENU_HWND: AtomicIsize = AtomicIsize::new(0);
static MENUBAR_HWND: AtomicIsize = AtomicIsize::new(0);

/// Lives in the (large) Win32_UI_Controls feature; defined here instead.
const WM_MOUSELEAVE: u32 = 0x02A3;

const TIMER_TICK: usize = 1;
const TIMER_REFRESH: usize = 2;
const TIMER_RELOAD: usize = 3;
const TIMER_REBUILD: usize = 4;
const TIMER_FINDER: usize = 5;

const HOTKEY_SPOTLIGHT: i32 = 1;

pub fn run() -> anyhow::Result<()> {
    win::register_class(Some(wndproc))?;
    let msg_hwnd = win::create_window(WINDOW_EX_STYLE(0), WINDOW_STYLE(0), RECT::default(), HWND_MESSAGE)?;
    MSG_HWND.store(msg_hwnd.0 as isize, Ordering::Relaxed);

    let app = App::new(msg_hwnd)?;
    APP.with(|a| *a.borrow_mut() = Some(app));
    with_app(|app| app.build());

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
        Action::OpenSettings(page) => {
            with_app(|app| app.open_settings(page));
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
        Action::OpenConfig => system::open(&config::path().to_string_lossy()),
        Action::Quit => unsafe { PostQuitMessage(0) },
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_FRAME => FRAME_PENDING.store(false, Ordering::Release),
        // Spotlight closes as soon as it loses focus (posted: we may be mid-dispatch).
        WM_ACTIVATE if wp.0 & 0xFFFF == 0 && hwnd.0 as isize == SPOTLIGHT_HWND.load(Ordering::Relaxed) => {
            win::post(MSG_HWND.load(Ordering::Relaxed), WM_APP_CLOSE_SPOTLIGHT);
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
    menu: Option<PopupMenu>,
    settings: Option<SettingsWindow>,
    finders: Vec<FinderWindow>,
    next_finder_id: u32,
    loader: finder::loader::IconLoader,
    kinds: finder::fs::KindCache,
    /// Finder windows whose folder changed on disk (reloaded on a short timer).
    finder_changed: Vec<u32>,
    spotlight: Option<Spotlight>,
    index: spotlight::index::Shared,
    hooks: Vec<HHOOK>,
    win_hooks: Vec<HWINEVENTHOOK>,
    _watcher: Option<notify::RecommendedWatcher>,
    taskbar_created: u32,
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
            menu: None,
            settings: None,
            finders: Vec::new(),
            next_finder_id: 1,
            loader: finder::loader::IconLoader::new(msg_hwnd.0 as isize, WM_APP_ICONS),
            kinds: finder::fs::KindCache::default(),
            finder_changed: Vec::new(),
            spotlight: None,
            index: Default::default(),
            hooks: Vec::new(),
            win_hooks,
            _watcher: watcher,
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        })
    }

    /// (Re)creates every surface from the current config.
    fn build(&mut self) {
        self.close_menu();
        self.dock = None;
        self.menubar = None;
        self.theme = Theme::new(&self.cfg, &self.gfx);
        self.monitor = Monitor::primary();

        if self.cfg.general.hide_taskbar {
            taskbar::hide();
        } else {
            taskbar::restore();
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
        if g0.appearance != g1.appearance || g0.hide_taskbar != g1.hide_taskbar {
            // Backdrop tints depend on light/dark; recreate everything.
            return self.build();
        }
        if g0 != g1 {
            // Font / accent / shortcut hints: same surfaces, new paint.
            self.theme = Theme::new(&self.cfg, &self.gfx);
            if let Some(mb) = &mut self.menubar {
                mb.dirty = true;
            }
            if let Some(d) = &mut self.dock {
                d.dirty = true;
            }
        }
        if old.spotlight != self.cfg.spotlight {
            self.register_hotkey();
        }
        if old.menubar != self.cfg.menubar {
            self.rebuild_menubar();
        }
        if old.dock != self.cfg.dock {
            let only_apps = dock_eq_except_apps(&old.dock, &self.cfg.dock);
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
        self.menubar = None;
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

    fn mark_settings_dirty(&mut self) {
        if let Some(s) = &mut self.settings {
            s.dirty = true;
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
        self.dock = None;
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
        if !still {
            self.animating = false;
            self.clock.stop();
        }
    }

    fn on_tick(&mut self) {
        if let Some(mb) = &mut self.menubar {
            mb.tick();
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

    fn update_foreground(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
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
        if let Some(s) = &mut self.settings {
            if s.dirty {
                s.render(&self.gfx, &self.theme, &self.cfg, &mut self.icons);
            }
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
                WM_APP_FRAME => self.on_frame(),
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
                _ => return None,
            }
            self.flush();
            return Some(LRESULT(0));
        }

        // Broadcasts reach every one of our top-level windows; debounce into one rebuild.
        if msg == self.taskbar_created || msg == WM_DISPLAYCHANGE || msg == WM_DPICHANGED {
            self.timer(TIMER_REBUILD, 400);
            return None;
        }
        if msg == WM_SETTINGCHANGE && lp.0 != 0 {
            let area = unsafe { PCWSTR(lp.0 as *const u16).to_string().unwrap_or_default() };
            if area == "ImmersiveColorSet" {
                self.timer(TIMER_REBUILD, 400);
            }
            return None;
        }
        if msg == WM_APP_APPBAR {
            if wp.0 as u32 == ABN_FULLSCREENAPP {
                let hide = lp.0 != 0;
                if let Some(mb) = &mut self.menubar {
                    mb.surface.set_hidden(hide);
                }
                if let Some(d) = &mut self.dock {
                    d.surface.set_hidden(hide);
                }
            }
            return Some(LRESULT(0));
        }

        if self.settings.as_ref().is_some_and(|s| s.hwnd == hwnd) {
            let r = self.on_settings_message(hwnd, msg, wp, lp);
            self.flush();
            return r;
        }
        if self.spotlight.as_ref().is_some_and(|s| s.surface.content == hwnd) {
            let r = self.on_spotlight_message(msg, wp, lp);
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
            WM_KEYDOWN if wp.0 == 0x1B => self.settings = None, // Esc
            WM_CLOSE => self.settings = None,
            _ => return None,
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
            WM_LBUTTONUP | WM_RBUTTONUP => match m.on_click(x, y) {
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
        for h in self.win_hooks.drain(..) {
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
        self.menu = None;
        self.dock = None;
        self.menubar = None;
        taskbar::restore();
    }
}
