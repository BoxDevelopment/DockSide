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
use crate::settings::{Page, SettingsEvent, SettingsWindow, SETTINGS_HWND};
use crate::theme::Theme;
use crate::win::{self, apps, startup, system, taskbar, Monitor};
use crate::win::{
    WM_APP_APPBAR, WM_APP_CLOSE_MENU, WM_APP_OPEN_SETTINGS, WM_APP_FOREGROUND, WM_APP_FRAME, WM_APP_MENU_KEY, WM_APP_RELOAD,
    WM_APP_WINDOWS_CHANGED,
};

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static PENDING: RefCell<Vec<Action>> = const { RefCell::new(Vec::new()) };
}

static MSG_HWND: AtomicIsize = AtomicIsize::new(0);

/// Lives in the (large) Win32_UI_Controls feature; defined here instead.
const WM_MOUSELEAVE: u32 = 0x02A3;

const TIMER_TICK: usize = 1;
const TIMER_REFRESH: usize = 2;
const TIMER_RELOAD: usize = 3;
const TIMER_REBUILD: usize = 4;

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
        // Shell surfaces never take focus; the settings window is a normal app window.
        WM_MOUSEACTIVATE if hwnd.0 as isize != SETTINGS_HWND.load(Ordering::Relaxed) => {
            return LRESULT(MA_NOACTIVATE as isize)
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
        if apps::window_pid(target) != GetCurrentProcessId() {
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
                Ok(m) => self.menubar = Some(m),
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
                Ok(m) => self.menubar = Some(m),
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
        let windows = apps::list_windows();
        if let Some(d) = &mut self.dock {
            d.update_running(&windows, &mut self.names, &self.cfg);
        }
        self.update_foreground();
    }

    fn update_foreground(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
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
                }
            }
            Err(e) => log!("menu failed: {e}"),
        }
    }

    fn close_menu(&mut self) {
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
                WM_APP_OPEN_SETTINGS => self.open_settings(None),
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
                    if let Some(a) = MenuBar::status_action(h) {
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
