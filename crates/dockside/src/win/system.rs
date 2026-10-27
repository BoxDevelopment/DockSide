//! System state (theme, battery) and system actions (keys, power, URIs).

use std::ffi::c_void;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Power::{GetSystemPowerStatus, SetSuspendState, SYSTEM_POWER_STATUS};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

fn personalize_dword(name: PCWSTR) -> Option<u32> {
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            name,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut _ as *mut c_void),
            Some(&mut size),
        )
    };
    r.is_ok().then_some(value)
}

/// A regional setting from Control Panel\International ("iMeasure", "iFirstDayOfWeek"...).
pub fn intl_string(name: &str) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Control Panel\\International"),
            &windows::core::HSTRING::from(name),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    r.is_ok().then(|| crate::win::from_wide(&buf))
}

pub fn is_dark_mode() -> bool {
    personalize_dword(w!("AppsUseLightTheme")) == Some(0)
}

/// Settings → Personalization → Colors → Transparency effects (on unless turned off).
pub fn transparency_enabled() -> bool {
    personalize_dword(w!("EnableTransparency")) != Some(0)
}

/// Settings > Accessibility > Visual effects > "Animation effects".
pub fn animations_enabled() -> bool {
    let mut on = windows::Win32::Foundation::BOOL(1);
    let ok = unsafe {
        SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some(&mut on as *mut _ as *mut c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0))
    };
    ok.is_err() || on.as_bool()
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

pub fn battery() -> Option<Battery> {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s).ok()? };
    // 128 = no system battery, 255 = unknown
    if s.BatteryFlag & 128 != 0 || s.BatteryFlag == 255 || s.BatteryLifePercent > 100 {
        return None;
    }
    Some(Battery { percent: s.BatteryLifePercent, charging: s.ACLineStatus == 1 })
}

/// A key chord. Mac-style display maps Ctrl→⌘, Alt→⌥, Win→⌃.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shortcut {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    pub vk: u16,
    pub key: &'static str,
}

impl Shortcut {
    /// `mods` is any of "c" (ctrl), "s" (shift), "a" (alt), "w" (win).
    pub const fn new(mods: &str, vk: u16, key: &'static str) -> Self {
        let b = mods.as_bytes();
        let mut s = Shortcut { ctrl: false, shift: false, alt: false, win: false, vk, key };
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'c' => s.ctrl = true,
                b's' => s.shift = true,
                b'a' => s.alt = true,
                b'w' => s.win = true,
                _ => {}
            }
            i += 1;
        }
        s
    }

    pub fn label(&self, mac: bool) -> String {
        let mut out = String::new();
        if mac {
            if self.win {
                out.push('⌃');
            }
            if self.alt {
                out.push('⌥');
            }
            if self.shift {
                out.push('⇧');
            }
            if self.ctrl {
                out.push('⌘');
            }
            out.push_str(self.key);
        } else {
            for (on, name) in [(self.win, "Win+"), (self.ctrl, "Ctrl+"), (self.alt, "Alt+"), (self.shift, "Shift+")] {
                if on {
                    out.push_str(name);
                }
            }
            out.push_str(self.key);
        }
        out
    }

    pub fn send(&self) {
        let mut keys = Vec::new();
        for (on, vk) in [(self.win, VK_LWIN), (self.ctrl, VK_CONTROL), (self.alt, VK_MENU), (self.shift, VK_SHIFT)] {
            if on {
                keys.push(vk);
            }
        }
        keys.push(VIRTUAL_KEY(self.vk));
        send_keys(&keys);
    }
}

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Presses `keys` in order, then releases them in reverse.
pub fn send_keys(keys: &[VIRTUAL_KEY]) {
    let mut inputs: Vec<INPUT> = keys.iter().map(|&k| key_input(k, false)).collect();
    inputs.extend(keys.iter().rev().map(|&k| key_input(k, true)));
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Opens the Start menu, as the Windows key does. (Posting to `Shell_TrayWnd`
/// could reach our own tray window, see `win::tray`, not Explorer's.)
pub fn open_start() {
    send_keys(&[VK_LWIN]);
}

/// Whether the Recycle Bin has anything in it, on any drive.
pub fn trash_full() -> bool {
    use windows::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};
    let mut info = SHQUERYRBINFO { cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info) }.is_ok() && info.i64NumItems > 0
}

pub fn open(target: &str) {
    super::elevation::shell_open(target, None, start_dir(target).as_deref());
}

/// The folder a file should run in, like Explorer: its own. Scripts (.bat,
/// .cmd, ...) otherwise resolve relative paths against DockSide's folder.
/// Shortcuts keep their own "Start in", and URLs / shell: paths have none.
pub fn start_dir(target: &str) -> Option<String> {
    let path = std::path::Path::new(target);
    let is_lnk = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"));
    if is_lnk || !path.is_absolute() || !path.is_file() {
        return None;
    }
    path.parent().map(|p| p.to_string_lossy().into_owned())
}

/// Opens a folder in File Explorer (not whatever the default folder handler is).
pub fn open_in_explorer(path: &str) {
    // A quoted drive root ("C:\") would escape its own closing quote; roots never need quotes.
    let trimmed = path.trim_end_matches('\\');
    let arg = if trimmed.len() <= 2 { format!("{trimmed}\\") } else { format!("\"{trimmed}\"") };
    super::elevation::shell_open("explorer.exe", Some(&arg), None);
}

pub fn reveal(path: &str) {
    super::elevation::shell_open("explorer.exe", Some(&format!("/select,\"{path}\"")), None);
}

/// Shows the system file picker for an app (.exe / .lnk) to add to the Dock.
pub fn pick_app(owner: HWND) -> Option<String> {
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    let filters = [
        COMDLG_FILTERSPEC { pszName: w!("Apps and shortcuts"), pszSpec: w!("*.exe;*.lnk") },
        COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
    ];
    pick_file(owner, w!("Add to Dock"), &filters)
}

/// An image for a Dock icon.
pub fn pick_image(owner: HWND) -> Option<String> {
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    let filters = [
        COMDLG_FILTERSPEC { pszName: w!("Images"), pszSpec: w!("*.png;*.ico;*.jpg;*.jpeg;*.bmp") },
        COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
    ];
    pick_file(owner, w!("Choose an Icon"), &filters)
}

fn pick_file(owner: HWND, title: PCWSTR, filters: &[windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC]) -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetFileTypes(filters).ok()?;
        dialog.SetTitle(title).ok()?;
        dialog.Show(owner).ok()?; // Err when cancelled
        let item = dialog.GetResult().ok()?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const c_void));
        s
    }
}

pub fn confirm(title: &str, text: &str) -> bool {
    let r = unsafe {
        MessageBoxW(
            HWND::default(),
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_OKCANCEL | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    r == IDOK
}

pub fn notice(title: &str, text: &str) {
    unsafe {
        MessageBoxW(HWND::default(), &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND);
    }
}

pub fn lock() {
    unsafe {
        let _ = LockWorkStation();
    }
}

pub fn sleep() {
    unsafe {
        let _ = SetSuspendState(false, false, false);
    }
}

fn shutdown_exe(args: &[&str]) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("shutdown").args(args).creation_flags(CREATE_NO_WINDOW).spawn();
}

pub fn restart() {
    shutdown_exe(&["/r", "/t", "0"]);
}

pub fn shut_down() {
    shutdown_exe(&["/s", "/t", "0"]);
}

pub fn log_out() {
    shutdown_exe(&["/l"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_start_in_their_own_folder() {
        let dir = std::env::temp_dir().join("dockside-start-dir-test");
        std::fs::create_dir_all(&dir).unwrap();
        let bat = dir.join("run.bat");
        let lnk = dir.join("app.lnk");
        std::fs::write(&bat, "@echo off").unwrap();
        std::fs::write(&lnk, "").unwrap();
        assert_eq!(start_dir(&bat.to_string_lossy()), Some(dir.to_string_lossy().into_owned()));
        assert_eq!(start_dir(&lnk.to_string_lossy()), None);
        assert_eq!(start_dir(&dir.to_string_lossy()), None);
        assert_eq!(start_dir("shell:RecycleBinFolder"), None);
        assert_eq!(start_dir("https://example.com"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
