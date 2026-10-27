//! macOS window animations for other apps' windows: the genie
//! into the Dock, opening and closing effects. `dockside_hook.dll`, loaded into
//! every app by a global hook, catches the window as it minimizes, opens or closes,
//! takes a snapshot and asks us (WM_COPYDATA) to animate it; we draw the animation
//! on our own render thread and tell the DLL when to reveal or let go of the real
//! window. The DLL also switches Windows' own transitions off for the window it
//! animates.
//!
//! What the DLLs need from us lives in named objects: the settings section, a
//! shutdown event and a mutex we hold for our whole life (if we crash it is
//! abandoned and the DLLs unload themselves).

mod effect;
mod injector;
mod ipc;
mod render;
#[path = "../../../dockside-hook/src/shared.rs"]
mod shared;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, ERROR_ACCESS_DENIED, HANDLE, HLOCAL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, GetCurrentProcess, GetCurrentProcessId, OpenProcessToken, ReleaseMutex, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{AnimSpeed, MinimizeEffect, WindowEffect, WindowsCfg};
use shared::*;

const PRELOAD_TIMER: usize = 1;

/// Windows the genie is animating, until when. The DLL cloaks a window while the
/// animation stands in for it, which would otherwise make it look gone (the Dock
/// would show its app closed for a moment).
static STANDING_IN: Mutex<Option<HashMap<isize, Instant>>> = Mutex::new(None);

/// Marks `hwnd` as animated for `duration_ms` (plus the DLL's reveal afterwards).
fn stand_in(hwnd: HWND, duration_ms: i32) {
    let now = Instant::now();
    let mut m = STANDING_IN.lock().unwrap_or_else(|e| e.into_inner());
    let m = m.get_or_insert_with(HashMap::new);
    m.retain(|_, until| *until > now);
    m.insert(hwnd.0 as isize, now + Duration::from_millis(duration_ms.max(0) as u64 + 1000));
}

/// Whether the genie is (or was a moment ago) animating `hwnd`.
pub fn stands_in_for(hwnd: HWND) -> bool {
    let m = STANDING_IN.lock().unwrap_or_else(|e| e.into_inner());
    m.as_ref().and_then(|m| m.get(&(hwnd.0 as isize))).is_some_and(|until| *until > Instant::now())
}

/// What the IPC handler goes by, from `[windows]` in the config.
#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub enabled: bool,
    /// Minimize and restore with one of ours (not Windows' own).
    pub minimize: bool,
    pub style: i32, // effect::STYLE_*
    pub duration_ms: i32,
    pub restore: bool,
    pub open_effect: i32, // EFFECT_*
    pub close_effect: i32,
    pub open_duration_ms: i32,
    pub close_duration_ms: i32,
    pub multi_monitor: bool,
    pub mesh_quality: i32,
    pub corner_radius: i32,
    pub corner_smoothing: i32,
}

fn effect_id(e: WindowEffect) -> i32 {
    match e {
        WindowEffect::Windows => EFFECT_SYSTEM,
        WindowEffect::Genie => EFFECT_GENIE,
        WindowEffect::Shatter => EFFECT_SHATTER,
        WindowEffect::Fragments => EFFECT_FRAGMENTS,
    }
}

impl Settings {
    pub fn from_cfg(cfg: &WindowsCfg) -> Settings {
        let minimize = cfg.minimize_effect != MinimizeEffect::Windows;
        let (open_effect, close_effect) = (effect_id(cfg.open_effect), effect_id(cfg.close_effect));
        // The base durations are the normal speed.
        let speed = |ms: f32| {
            (ms * match cfg.speed {
                AnimSpeed::Slow => 1.6,
                AnimSpeed::Normal => 1.0,
                AnimSpeed::Fast => 0.65,
            }) as i32
        };
        Settings {
            enabled: minimize || open_effect != EFFECT_SYSTEM || close_effect != EFFECT_SYSTEM,
            minimize,
            style: match cfg.minimize_effect {
                MinimizeEffect::Classic => effect::STYLE_CLASSIC,
                MinimizeEffect::Scale => effect::STYLE_SCALE,
                _ => effect::STYLE_MODERN,
            },
            duration_ms: speed(450.0),
            restore: minimize && cfg.animate_restore,
            open_effect,
            close_effect,
            open_duration_ms: speed(450.0),
            close_duration_ms: speed(400.0),
            multi_monitor: false,
            mesh_quality: 48,
            corner_radius: 15,
            corner_smoothing: 100,
        }
    }
}

/// Programs that must never get the DLL: games whose anti-cheat reacts to foreign
/// modules, and the folders games are installed to.
const NO_INJECT: &[&str] = &[
    "robloxplayerbeta.exe",
    "valorant-win64-shipping.exe",
    "fortniteclient-win64-shipping.exe",
    "r5apex.exe",
    "r5apex_dx12.exe",
    "cs2.exe",
    "overwatch.exe",
    "cod.exe",
    "wow.exe",
    "\\steamapps\\common\\",
    "\\epic games\\",
    "\\riot games\\",
    "\\xboxgames\\",
    "\\ea games\\",
    "\\ubisoft game launcher\\games\\",
    "\\gog galaxy\\games\\",
];

/// Entries separated by NUL, ended by an empty entry; never overruns the field.
fn write_list(dst: &mut [u16; LIST_CHARS], items: &[&str]) {
    let mut pos = 0;
    for item in items {
        let item: Vec<u16> = item.to_lowercase().encode_utf16().collect();
        if item.is_empty() {
            continue;
        }
        if pos + item.len() + 1 > LIST_CHARS - 2 {
            break;
        }
        dst[pos..pos + item.len()].copy_from_slice(&item);
        pos += item.len();
        dst[pos] = 0;
        pos += 1;
    }
    dst[pos..].fill(0);
}

// --- shared objects' security (elevation.cpp) ------------------------------------

enum SharedObject {
    Settings,
    ShutdownEvent,
    AliveMutex,
}

fn current_user_sid() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut size = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
        let mut buf = vec![0u8; size.max(1) as usize];
        let got = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as _), size, &mut size);
        let _ = CloseHandle(token);
        got.ok()?;
        let mut s = PWSTR::null();
        ConvertSidToStringSidW((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, &mut s).ok()?;
        let sid = s.to_string().ok();
        let _ = LocalFree(HLOCAL(s.0 as _));
        sid
    }
}

/// Who may open our named objects. Ordinary programs must be able to read them
/// even while we run elevated, and must never be able to change them.
struct Security {
    sd: PSECURITY_DESCRIPTOR,
    attrs: SECURITY_ATTRIBUTES,
}

impl Security {
    fn new(kind: SharedObject, elevated: bool) -> Option<Security> {
        let mut sddl = String::from("D:(A;;GA;;;SY)(A;;GA;;;BA)");
        if let Some(sid) = current_user_sid() {
            sddl += &format!("(A;;GA;;;{sid})");
        }
        match kind {
            SharedObject::Settings => {
                sddl += "(A;;GR;;;AU)";
                if elevated {
                    sddl += "S:(ML;;NW;;;HI)";
                }
            }
            SharedObject::ShutdownEvent => {
                sddl += "(A;;0x00100000;;;AU)"; // SYNCHRONIZE
                if elevated {
                    sddl += "S:(ML;;NW;;;HI)";
                }
            }
            SharedObject::AliveMutex => {
                sddl += "(A;;0x00100001;;;AU)"; // SYNCHRONIZE | MUTEX_MODIFY_STATE
                if elevated {
                    sddl += "S:(ML;;NW;;;ME)";
                }
            }
        }
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let text = wide(&sddl);
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(text.as_ptr()), SDDL_REVISION_1, &mut sd, None) }.ok()?;
        Some(Security {
            sd,
            attrs: SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() },
        })
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(HLOCAL(self.sd.0));
        }
    }
}

// --- the host ----------------------------------------------------------------------

/// What the IPC window reaches while it handles messages. Borrowed there only, so a
/// request that comes in while it is being changed is left to Windows.
struct Inner {
    server: Option<ipc::Server>,
    injector: Option<injector::Injector>,
}

pub struct Genie {
    instance: u32,
    shutdown: HANDLE,
    alive: HANDLE,
    map: HANDLE,
    view: *mut SharedSettings,
    ipc_hwnd: HWND,
    inner: Box<RefCell<Inner>>,
    settings: Settings,
}

unsafe extern "system" fn ipc_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let inner = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Inner>;
    if let Some(inner) = inner.as_ref() {
        match msg {
            WM_COPYDATA => {
                let reply = match inner.try_borrow() {
                    Ok(i) => i.server.as_ref().map_or(REPLY_FAIL, |s| s.handle(lp.0 as *const COPYDATASTRUCT)),
                    Err(_) => REPLY_FAIL,
                };
                return LRESULT(reply as isize);
            }
            WM_TIMER if wp.0 == PRELOAD_TIMER => {
                let _ = KillTimer(hwnd, PRELOAD_TIMER);
                if let Ok(mut i) = inner.try_borrow_mut() {
                    if let Some(inj) = &mut i.injector {
                        inj.end_preload();
                    }
                }
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

impl Genie {
    /// None when it can't run (no graphics device, no DLL, ...): windows then keep
    /// Windows' own animations.
    pub fn new(cfg: &WindowsCfg, dock_icon: fn(HWND) -> Option<RECT>) -> Option<Genie> {
        let settings = Settings::from_cfg(cfg);
        let instance = unsafe { GetCurrentProcessId() }.wrapping_mul(2_654_435_761) ^ render::qpc() as u32;
        let elevated = crate::win::elevation::is_elevated();
        log!("genie: starting (instance {instance:08X}{})", if elevated { ", elevated" } else { "" });

        let mut g = Genie {
            instance,
            shutdown: HANDLE::default(),
            alive: HANDLE::default(),
            map: HANDLE::default(),
            view: std::ptr::null_mut(),
            ipc_hwnd: HWND::default(),
            inner: Box::new(RefCell::new(Inner { server: None, injector: None })),
            settings: settings.clone(),
        };
        // From here on, a failure drops `g`, which undoes whatever was made.
        unsafe {
            let sa = Security::new(SharedObject::ShutdownEvent, elevated)?;
            g.shutdown = CreateEventW(Some(&sa.attrs), true, false, PCWSTR(wide(&shutdown_event_name(instance)).as_ptr()))
                .map_err(|e| log!("genie: shutdown event: {e}"))
                .ok()?;
            let sa = Security::new(SharedObject::AliveMutex, elevated)?;
            g.alive = CreateMutexW(Some(&sa.attrs), true, PCWSTR(wide(&alive_mutex_name(instance)).as_ptr()))
                .map_err(|e| log!("genie: alive mutex: {e}"))
                .ok()?;

            // Read-only for the DLLs; may already exist while programs still have the
            // previous one mapped - then it is simply reused. Access denied = the section
            // of a previous (elevated) DockSide is still held by DLLs that are shutting
            // down; they let go of it within moments.
            let sa = Security::new(SharedObject::Settings, elevated)?;
            let name = wide(SETTINGS_SECTION);
            for _ in 0..30 {
                match CreateFileMappingW(HANDLE(-1isize as _), Some(&sa.attrs), PAGE_READWRITE, 0, std::mem::size_of::<SharedSettings>() as u32, PCWSTR(name.as_ptr())) {
                    Ok(m) => {
                        g.map = m;
                        break;
                    }
                    Err(_) if GetLastError() == ERROR_ACCESS_DENIED => std::thread::sleep(std::time::Duration::from_millis(100)),
                    Err(e) => {
                        log!("genie: settings section: {e}");
                        return None;
                    }
                }
            }
            if g.map.is_invalid() {
                log!("genie: settings section still held by a previous DockSide");
                return None;
            }
            g.view = MapViewOfFile(g.map, FILE_MAP_WRITE, 0, 0, std::mem::size_of::<SharedSettings>()).Value as *mut SharedSettings;
            if g.view.is_null() {
                log!("genie: mapping the settings section failed");
                return None;
            }

            let class = wide(HOST_WNDCLASS);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(ipc_wndproc),
                hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            g.ipc_hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), w!("DockSide genie"), WINDOW_STYLE(0), 0, 0, 0, 0, HWND_MESSAGE, None, GetModuleHandleW(None).unwrap_or_default(), None)
                .map_err(|e| log!("genie: IPC window: {e}"))
                .ok()?;
            SetWindowLongPtrW(g.ipc_hwnd, GWLP_USERDATA, &*g.inner as *const RefCell<Inner> as isize);
            // The DLLs run in programs of lower integrity while we are elevated: UIPI
            // drops their requests unless let through.
            crate::win::elevation::allow_messages(g.ipc_hwnd, &[WM_COPYDATA]);
        }

        let renderer = render::Renderer::start()?;
        g.inner.borrow_mut().server = Some(ipc::Server { renderer, settings, dock_icon, last_icon: Default::default() });
        g.publish(true);

        let mut injector = injector::Injector::start()?;
        injector.begin_preload();
        g.inner.borrow_mut().injector = Some(injector);
        unsafe { SetTimer(g.ipc_hwnd, PRELOAD_TIMER, 2000, None) };
        log!("genie: ready");
        Some(g)
    }

    pub fn configure(&mut self, cfg: &WindowsCfg) {
        let settings = Settings::from_cfg(cfg);
        if settings == self.settings {
            return;
        }
        self.settings = settings.clone();
        if let Some(s) = &mut self.inner.borrow_mut().server {
            s.settings = settings;
        }
        self.publish(true);
    }

    /// Whether `cfg` asks for any of our animations at all.
    pub fn wanted(cfg: &WindowsCfg) -> bool {
        Settings::from_cfg(cfg).enabled
    }

    /// Writes the settings section under its seqlock (`seq` odd while writing).
    fn publish(&self, enabled: bool) {
        if self.view.is_null() {
            return;
        }
        use std::sync::atomic::{fence, AtomicI32, Ordering};
        let s = &self.settings;
        unsafe {
            let v = &mut *self.view;
            let seq = &*(std::ptr::addr_of_mut!(v.seq) as *const AtomicI32);
            seq.fetch_add(1, Ordering::SeqCst); // odd: readers retry
            fence(Ordering::SeqCst);
            v.magic = SETTINGS_MAGIC;
            v.size = std::mem::size_of::<SharedSettings>() as u32;
            v.host_pid = GetCurrentProcessId();
            v.host_instance = self.instance;
            v.host_ipc_hwnd = self.ipc_hwnd.0 as usize as u64;
            v.enabled = (enabled && s.enabled) as i32;
            v.dev_mode = 0;
            v.genie_duration_ms = s.duration_ms;
            v.genie_restore = s.restore as i32;
            v.genie_launch = (s.open_effect != EFFECT_SYSTEM) as i32;
            v.open_effect = s.open_effect;
            v.close_effect = s.close_effect;
            v.open_duration_ms = s.open_duration_ms;
            v.close_duration_ms = s.close_duration_ms;
            v.genie_multi_monitor = s.multi_monitor as i32;
            // DockSide hides the taskbar for good; the genie goes into the Dock.
            v.genie_unhide_taskbar = 0;
            v.genie_unhide_duration_ms = 450;
            v.genie_mesh_quality = s.mesh_quality;
            v.corner_radius = s.corner_radius;
            v.corner_smoothing = s.corner_smoothing;
            // Windows keep their own (DWM) corners, and their own icons.
            v.window_corners = 0;
            v.app_icons = 0;
            write_list(&mut v.genie_excluded, &[]);
            write_list(&mut v.no_inject, NO_INJECT);
            write_list(&mut v.dev_allowlist, &[]);
            v.generation = v.generation.wrapping_add(1);
            fence(Ordering::SeqCst);
            seq.fetch_add(1, Ordering::SeqCst); // even again
        }
    }
}

impl Drop for Genie {
    fn drop(&mut self) {
        log!("genie: shutting down");
        // 1. No new animations from here on.
        self.publish(false);
        // 2. Every running genie tells its DLL to reveal / finish.
        let server = self.inner.borrow_mut().server.take();
        drop(server);
        // 3. Every DLL removes its hooks and unloads itself; then the hook goes.
        unsafe {
            if !self.shutdown.is_invalid() {
                let _ = SetEvent(self.shutdown);
            }
        }
        let injector = self.inner.borrow_mut().injector.take();
        drop(injector);
        unsafe {
            if !self.ipc_hwnd.is_invalid() {
                let _ = DestroyWindow(self.ipc_hwnd);
            }
            if !self.view.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as _ });
            }
            if !self.map.is_invalid() {
                let _ = CloseHandle(self.map);
            }
            if !self.alive.is_invalid() {
                let _ = ReleaseMutex(self.alive);
                let _ = CloseHandle(self.alive);
            }
            if !self.shutdown.is_invalid() {
                let _ = CloseHandle(self.shutdown);
            }
        }
    }
}
