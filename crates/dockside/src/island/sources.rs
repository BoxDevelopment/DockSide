//! Everything the island listens to besides Now Playing.
//!
//! Background sources (folder watchers, WMI, Bluetooth, calendar, notifications)
//! push [`Signal`]s into a shared [`Inbox`] and post one message to the UI
//! thread. Cheap state (mic / camera use, Do Not Disturb, volume) is read on
//! the UI thread directly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{w, BSTR, PCWSTR};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Wmi::*;

use super::calendar::CalEvent;
use super::notifications::Note;
use crate::win::post;

pub enum Signal {
    /// Display brightness changed (0-100).
    Brightness(u8),
    Screenshot(PathBuf),
    /// A browser's partial download file appeared or grew.
    DownloadActive(PathBuf),
    /// A partial download file went away (finished, renamed or cancelled).
    DownloadGone(PathBuf),
    Bluetooth { name: String, connected: bool, battery: Option<u8> },
    Notification(Note),
    Calendar(Vec<CalEvent>),
}

#[derive(Clone)]
pub struct Inbox {
    queue: Arc<Mutex<Vec<Signal>>>,
    hwnd: isize,
    msg: u32,
}

impl Inbox {
    pub fn new(hwnd: isize, msg: u32) -> Self {
        Self { queue: Arc::default(), hwnd, msg }
    }

    pub fn push(&self, s: Signal) {
        // One message per batch: a growing download reports hundreds of writes a second,
        // and an undrained queue already has its message on the way.
        let first = match self.queue.lock() {
            Ok(mut q) => {
                q.push(s);
                q.len() == 1
            }
            Err(_) => true,
        };
        if first {
            post(self.hwnd, self.msg);
        }
    }

    pub fn drain(&self) -> Vec<Signal> {
        self.queue.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }
}

/// Set on drop; background threads that poll check it and exit.
#[derive(Clone, Default)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Screenshots and downloads
// ---------------------------------------------------------------------------

const PARTIAL_EXTS: [&str; 5] = ["crdownload", "part", "partial", "download", "opdownload"];

pub fn is_partial(p: &Path) -> bool {
    p.extension().is_some_and(|e| PARTIAL_EXTS.contains(&e.to_string_lossy().to_lowercase().as_str()))
}

/// The finished file a partial download becomes ("a.zip.crdownload" → "a.zip").
pub fn finished_name(p: &Path) -> PathBuf {
    p.with_extension("")
}

pub fn screenshots_dir() -> Option<PathBuf> {
    // FOLDERID_Screenshots
    let id = windows::core::GUID::from_u128(0xb7bede81_df94_4682_a7d8_57a52620b86f);
    crate::finder::fs::known_folder(&id)
        .or_else(|| crate::finder::fs::known_folder(&windows::Win32::UI::Shell::FOLDERID_Pictures).map(|p| p.join("Screenshots")))
}

pub fn downloads_dir() -> Option<PathBuf> {
    crate::finder::fs::known_folder(&windows::Win32::UI::Shell::FOLDERID_Downloads)
}

/// Watches the Screenshots and Downloads folders.
pub fn watch_folders(inbox: Inbox) -> Option<notify::RecommendedWatcher> {
    use notify::event::{ModifyKind, RenameMode};
    use notify::{EventKind, RecursiveMode, Watcher};

    let shots = screenshots_dir();
    let downloads = downloads_dir();
    let (s2, d2) = (shots.clone(), downloads.clone());
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        for (i, path) in ev.paths.iter().enumerate() {
            let parent = path.parent();
            if parent.is_some_and(|p| Some(p) == s2.as_deref()) {
                let image = path.extension().is_some_and(|e| {
                    matches!(e.to_string_lossy().to_lowercase().as_str(), "png" | "jpg" | "jpeg")
                });
                let created = matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)));
                if image && created {
                    // Let the capture tool finish writing before we decode a thumbnail.
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    inbox.push(Signal::Screenshot(path.clone()));
                }
            } else if parent.is_some_and(|p| Some(p) == d2.as_deref()) && is_partial(path) {
                let gone = match ev.kind {
                    EventKind::Remove(_) => true,
                    EventKind::Modify(ModifyKind::Name(RenameMode::From)) => true,
                    // `Both` lists [from, to].
                    EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => i == 0,
                    _ => false,
                };
                inbox.push(if gone { Signal::DownloadGone(path.clone()) } else { Signal::DownloadActive(path.clone()) });
            }
        }
    })
    .map_err(|e| log!("island folder watcher failed: {e}"))
    .ok()?;
    for dir in [shots, downloads].into_iter().flatten() {
        if dir.is_dir() {
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                log!("island: can't watch {}: {e}", dir.display());
            }
        }
    }
    Some(watcher)
}

// ---------------------------------------------------------------------------
// Brightness (WMI events; laptops / built-in panels only)
// ---------------------------------------------------------------------------

pub fn watch_brightness(inbox: Inbox, stop: Stop) {
    let spawned = std::thread::Builder::new().name("brightness".into()).spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let run = || -> windows::core::Result<()> {
            let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
            let services =
                locator.ConnectServer(&BSTR::from("ROOT\\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)?;
            // RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE
            CoSetProxyBlanket(
                &services,
                10,
                0,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )?;
            let events = services.ExecNotificationQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT * FROM WmiMonitorBrightnessEvent"),
                WBEM_FLAG_RETURN_IMMEDIATELY | WBEM_FLAG_FORWARD_ONLY,
                None,
            )?;
            while !stop.stopped() {
                let mut objs = [None];
                let mut n = 0u32;
                let _ = events.Next(1000, &mut objs, &mut n);
                if n == 0 {
                    continue;
                }
                if let Some(obj) = &objs[0] {
                    let mut v = windows::core::VARIANT::default();
                    if obj.Get(w!("Brightness"), 0, &mut v, None, None).is_ok() {
                        // WMI hands the uint8 back as VT_UI1; read it through u32.
                        let level = u32::try_from(&v).ok().or_else(|| i32::try_from(&v).ok().map(|x| x as u32));
                        if let Some(level) = level {
                            inbox.push(Signal::Brightness(level.min(100) as u8));
                        }
                    }
                }
            }
            Ok(())
        };
        if let Err(e) = run() {
            log!("brightness events unavailable: {e}");
        }
    });
    if let Err(e) = spawned {
        log!("brightness thread failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// Volume
// ---------------------------------------------------------------------------

fn endpoint() -> Option<IAudioEndpointVolume> {
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = devices.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

/// (level 0-1, muted) of the default output device.
pub fn volume() -> Option<(f32, bool)> {
    let ep = endpoint()?;
    unsafe { Some((ep.GetMasterVolumeLevelScalar().ok()?, ep.GetMute().ok()?.as_bool())) }
}

pub enum VolumeKey {
    Up,
    Down,
    Mute,
}

/// Applies a volume key the way Windows would (2% steps; changing the level unmutes).
pub fn press_volume(key: VolumeKey) -> Option<(f32, bool)> {
    let ep = endpoint()?;
    unsafe {
        let muted = ep.GetMute().ok()?.as_bool();
        match key {
            VolumeKey::Up | VolumeKey::Down => {
                if muted {
                    let _ = ep.SetMute(false, std::ptr::null());
                }
                let _ = if matches!(key, VolumeKey::Up) {
                    ep.VolumeStepUp(std::ptr::null())
                } else {
                    ep.VolumeStepDown(std::ptr::null())
                };
            }
            VolumeKey::Mute => {
                let _ = ep.SetMute(!muted, std::ptr::null());
            }
        }
    }
    volume()
}

pub fn set_volume(level: f32) -> Option<(f32, bool)> {
    let ep = endpoint()?;
    unsafe {
        let _ = ep.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null());
        if level > 0.0 {
            let _ = ep.SetMute(false, std::ptr::null());
        }
    }
    volume()
}

// ---------------------------------------------------------------------------
// Microphone / camera in use
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Privacy {
    /// Apps using the camera / microphone right now, by display name.
    pub camera: Vec<String>,
    pub mic: Vec<String>,
}

/// Reads Windows' capability consent store: an app is using a device while its
/// `LastUsedTimeStop` is 0.
pub fn privacy() -> Privacy {
    let base = "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore";
    Privacy { camera: in_use(&format!("{base}\\webcam")), mic: in_use(&format!("{base}\\microphone")) }
}

fn in_use(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(key) = open_key(HKEY_CURRENT_USER, path) else { return out };
    for name in subkeys(key) {
        if name == "NonPackaged" {
            if let Some(np) = open_key(key, &name) {
                for exe in subkeys(np) {
                    if active(np, &exe) {
                        // "C:#Program Files#Discord#Discord.exe" → "Discord"
                        let stem = exe.rsplit('#').next().unwrap_or(&exe);
                        let stem = stem.rsplit_once('.').map(|(s, _)| s).unwrap_or(stem);
                        out.push(stem.to_string());
                    }
                }
                unsafe {
                    let _ = RegCloseKey(np);
                }
            }
        } else if active(key, &name) {
            // "Microsoft.WindowsCamera_8wekyb3d8bbwe" → "WindowsCamera"
            let pkg = name.split('_').next().unwrap_or(&name);
            out.push(pkg.rsplit('.').next().unwrap_or(pkg).to_string());
        }
    }
    unsafe {
        let _ = RegCloseKey(key);
    }
    out.retain(|n| !n.eq_ignore_ascii_case("dockside"));
    out.sort();
    out.dedup();
    out
}

fn open_key(parent: HKEY, path: &str) -> Option<HKEY> {
    let mut key = HKEY::default();
    let wide = windows::core::HSTRING::from(path);
    unsafe { RegOpenKeyExW(parent, &wide, 0, KEY_READ, &mut key).ok().ok()? };
    Some(key)
}

fn subkeys(key: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = [0u16; 512];
    for i in 0.. {
        let mut len = buf.len() as u32;
        let r = unsafe {
            RegEnumKeyExW(key, i, windows::core::PWSTR(buf.as_mut_ptr()), &mut len, None, windows::core::PWSTR::null(), None, None)
        };
        if r.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    out
}

fn active(parent: HKEY, sub: &str) -> bool {
    let read = |name: PCWSTR| -> Option<u64> {
        let mut v = 0u64;
        let mut size = 8u32;
        let wide = windows::core::HSTRING::from(sub);
        unsafe {
            RegGetValueW(parent, &wide, name, RRF_RT_QWORD, None, Some(&mut v as *mut u64 as *mut _), Some(&mut size))
                .ok()
                .ok()?
        };
        Some(v)
    };
    matches!((read(w!("LastUsedTimeStart")), read(w!("LastUsedTimeStop"))), (Some(start), Some(0)) if start > 0)
}

// ---------------------------------------------------------------------------
// Do Not Disturb
// ---------------------------------------------------------------------------

/// Whether Do Not Disturb / Focus is on. Windows keeps this in a WNF state
/// ("quiet hours profile"): 0 is off, anything else is a Focus profile.
pub fn do_not_disturb() -> Option<bool> {
    type QueryWnf = unsafe extern "system" fn(
        state: *const u64,
        type_id: *const std::ffi::c_void,
        scope: *const std::ffi::c_void,
        stamp: *mut u32,
        buf: *mut std::ffi::c_void,
        size: *mut u32,
    ) -> i32;
    const WNF_SHEL_QUIETHOURS_ACTIVE_PROFILE_CHANGED: u64 = 0x0D83_063E_A3BF_1C75;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let f = GetProcAddress(ntdll, windows::core::s!("NtQueryWnfStateData"))?;
        let query: QueryWnf = std::mem::transmute(f);
        let state = WNF_SHEL_QUIETHOURS_ACTIVE_PROFILE_CHANGED;
        let (mut stamp, mut value, mut size) = (0u32, 0u32, 4u32);
        let status = query(
            &state,
            std::ptr::null(),
            std::ptr::null(),
            &mut stamp,
            &mut value as *mut u32 as *mut _,
            &mut size,
        );
        (status >= 0).then_some(size >= 4 && value != 0)
    }
}

/// Caps Lock's toggle state.
pub fn caps_lock() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL};
    unsafe { GetKeyState(VK_CAPITAL.0 as i32) & 1 != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_downloads() {
        assert!(is_partial(Path::new("C:\\d\\a.zip.crdownload")));
        assert!(is_partial(Path::new("C:\\d\\a.zip.part")));
        assert!(!is_partial(Path::new("C:\\d\\a.zip")));
        assert_eq!(finished_name(Path::new("C:\\d\\a.zip.crdownload")), PathBuf::from("C:\\d\\a.zip"));
    }
}
