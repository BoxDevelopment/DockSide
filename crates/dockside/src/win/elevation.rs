//! "Run as administrator": an elevated DockSide can drive admin apps (Task
//! Manager, OBS run as admin...), which Windows' UIPI otherwise blocks.
//!
//! Elevation comes from an on-demand scheduled task with highest privileges:
//! running it needs no UAC prompt, so a normal launch (the Start menu, the
//! login `Run` value) hands off to the task. Apps DockSide opens are started
//! through Explorer, so they don't inherit admin rights.

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};

use windows::core::{w, Interface, BSTR, HSTRING, PCWSTR, VARIANT};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Com::{CoCreateInstance, IDispatch, IServiceProvider, CLSCTX_LOCAL_SERVER};
use windows::Win32::System::Threading::{GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, ChangeWindowMessageFilterEx, GetWindowThreadProcessId, MSGFLT_ALLOW, SW_SHOWNORMAL,
};

const TASK: &str = "DockSide";
/// Passed by the task, so an instance it starts never hands off again.
pub const FROM_TASK: &str = "--from-task";
/// `dockside --admin-task on|off`: run elevated (via UAC) to change the task.
pub const ADMIN_TASK: &str = "--admin-task";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Lets normal (unelevated) processes send `msgs` to `hwnd` while we're elevated.
/// Harmless when we aren't.
pub fn allow_messages(hwnd: HWND, msgs: &[u32]) {
    for &m in msgs {
        unsafe {
            let _ = ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, None);
        }
    }
}

/// `ShellExecute("open")`, but started by Explorer when we're elevated, so the
/// new process runs as the user rather than as admin.
pub fn shell_open(file: &str, args: Option<&str>, dir: Option<&str>) -> bool {
    if is_elevated() {
        match via_explorer(file, args.unwrap_or(""), dir.unwrap_or("")) {
            Ok(()) => return true,
            Err(e) => log!("unelevated launch failed, launching directly: {e}"),
        }
    }
    let args = args.map(HSTRING::from);
    let dir = dir.map(HSTRING::from);
    let ptr = |s: &Option<HSTRING>| s.as_ref().map(|s| PCWSTR(s.as_ptr())).unwrap_or(PCWSTR::null());
    let r = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(file), ptr(&args), ptr(&dir), SW_SHOWNORMAL) };
    r.0 as isize > 32
}

/// The desktop's `IShellDispatch2` lives in Explorer: its `ShellExecute` runs there.
fn via_explorer(file: &str, args: &str, dir: &str) -> windows::core::Result<()> {
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
        let loc = VARIANT::from(CSIDL_DESKTOP as i32);
        let mut hwnd = 0i32;
        let disp = windows.FindWindowSW(&loc, &VARIANT::default(), SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)?;
        let provider: IServiceProvider = disp.cast()?;
        let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
        let view = browser.QueryActiveShellView()?;
        let background: IDispatch = view.GetItemObject(SVGIO_BACKGROUND)?;
        let folder_view: IShellFolderViewDual = background.cast()?;
        let shell: IShellDispatch2 = folder_view.Application()?.cast()?;
        // Explorer isn't the foreground app (we took the click), so what it
        // starts would open behind everything. Lend it our right to come forward.
        let mut pid = 0;
        GetWindowThreadProcessId(HWND(hwnd as isize as *mut _), Some(&mut pid));
        if pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        shell.ShellExecute(
            &BSTR::from(file),
            &VARIANT::from(BSTR::from(args)),
            &VARIANT::from(BSTR::from(dir)),
            &VARIANT::from(BSTR::from("open")),
            &VARIANT::from(SW_SHOWNORMAL.0),
        )
    }
}

fn schtasks(args: &[&str]) -> Option<std::process::Output> {
    Command::new("schtasks").args(args).creation_flags(CREATE_NO_WINDOW).output().ok()
}

/// The exe the admin task starts, or `None` when there's no task.
fn task_exe() -> Option<String> {
    let out = schtasks(&["/Query", "/TN", TASK, "/XML"]).filter(|o| o.status.success())?;
    let xml = String::from_utf8_lossy(&out.stdout);
    let start = xml.find("<Command>")? + "<Command>".len();
    let end = start + xml[start..].find("</Command>")?;
    Some(unescape(&xml[start..end]))
}

fn current_exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

/// "Run as administrator" is on (the task exists).
pub fn task_enabled() -> bool {
    task_exe().is_some()
}

/// A normal launch should hand off to the elevated task: it exists, starts this
/// exe (a moved DockSide would otherwise start a stale path), and we're not
/// already elevated or started by it.
pub fn should_hand_off() -> bool {
    if is_elevated() || std::env::args().any(|a| a == FROM_TASK) {
        return false;
    }
    match task_exe() {
        Some(exe) if exe.eq_ignore_ascii_case(&current_exe()) => true,
        Some(exe) => {
            log!("admin task starts {exe}, not this exe: starting without admin rights");
            false
        }
        None => false,
    }
}

/// Starts the elevated task (no UAC prompt).
pub fn run_task() -> bool {
    schtasks(&["/Run", "/TN", TASK]).is_some_and(|o| o.status.success())
}

/// Creates (or repoints) the task. Needs admin rights.
pub fn install_task() -> std::io::Result<()> {
    let exe = current_exe();
    let dir = std::path::Path::new(&exe).parent().map(|p| p.display().to_string()).unwrap_or_default();
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default()
    );
    // Task Scheduler's defaults would stop DockSide on battery and after 3 days,
    // and run it at below-normal priority.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts DockSide with administrator rights, without a UAC prompt.</Description></RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>false</AllowHardTerminate>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
      <Arguments>{FROM_TASK}</Arguments>
      <WorkingDirectory>{}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        escape(&exe),
        escape(&dir),
        user = escape(&user),
    );
    // schtasks wants the file in UTF-16.
    let path = std::env::temp_dir().join("dockside-task.xml");
    let bytes: Vec<u8> = std::iter::once(0xFEFFu16).chain(xml.encode_utf16()).flat_map(|c| c.to_le_bytes()).collect();
    std::fs::write(&path, bytes)?;
    let out = schtasks(&["/Create", "/TN", TASK, "/XML", &path.display().to_string(), "/F"]);
    let _ = std::fs::remove_file(&path);
    match out {
        Some(o) if o.status.success() => Ok(()),
        Some(o) => Err(std::io::Error::other(String::from_utf8_lossy(&o.stderr).trim().to_string())),
        None => Err(std::io::Error::other("schtasks didn't run")),
    }
}

/// Deletes the task. Needs admin rights.
pub fn remove_task() -> std::io::Result<()> {
    match schtasks(&["/Delete", "/TN", TASK, "/F"]) {
        Some(o) if o.status.success() => Ok(()),
        Some(o) => Err(std::io::Error::other(String::from_utf8_lossy(&o.stderr).trim().to_string())),
        None => Err(std::io::Error::other("schtasks didn't run")),
    }
}

/// `dockside --admin-task on|off`, run elevated: exit code 0 on success.
pub fn admin_task_command(on: bool) -> i32 {
    let r = if on { install_task() } else { remove_task() };
    match r {
        Ok(()) => 0,
        Err(e) => {
            log!("admin task {}: {e}", if on { "install" } else { "removal" });
            1
        }
    }
}

/// Runs `dockside --admin-task on|off` elevated (shows the UAC prompt) and
/// waits for it. Blocks: call from a background thread.
pub fn change_task_elevated(on: bool) -> bool {
    let exe = HSTRING::from(current_exe());
    let args = HSTRING::from(format!("{ADMIN_TASK} {}", if on { "on" } else { "off" }));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: 0,
        ..Default::default()
    };
    unsafe {
        // Fails when the UAC prompt is declined.
        if ShellExecuteExW(&mut info).is_err() || info.hProcess.is_invalid() {
            return false;
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        code == 0
    }
}

/// 0 = none, 1 = unelevated, 2 = elevated.
static RELAUNCH: AtomicU8 = AtomicU8::new(0);

/// Restart DockSide (elevated or not) after this instance quits.
pub fn request_relaunch(elevated: bool) {
    RELAUNCH.store(if elevated { 2 } else { 1 }, Ordering::SeqCst);
}

pub fn take_relaunch() -> Option<bool> {
    match RELAUNCH.swap(0, Ordering::SeqCst) {
        0 => None,
        n => Some(n == 2),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escaping_round_trips() {
        let path = r#"C:\Apps & Tools\<dock>\"DockSide".exe"#;
        assert_eq!(unescape(&escape(path)), path);
    }
}
