//! "Start DockSide at login".
//!
//! A per-user scheduled task with a logon trigger: it fires as soon as the user
//! signs in. The `Run` registry value Windows offers for this waits for
//! Explorer to finish starting and then holds startup apps back a further
//! several seconds, so DockSide used to appear well after the desktop. The
//! `Run` value is kept only as a fallback for when the task can't be created.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

use super::elevation;

const TASK: &str = "DockSide Login";
/// Passed by the login task: wait for Explorer's taskbar before starting.
pub const LOGIN: &str = "--login";

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("DockSide");
/// The value's name from before the rename to DockSide.
const LEGACY_VALUE: PCWSTR = w!("MacFinder");

/// 0 = not checked yet, 1 = off, 2 = on. Checking means running schtasks, so
/// Settings reads this rather than asking again each time it opens.
static ENABLED: AtomicU8 = AtomicU8::new(0);

fn run_value_exists() -> bool {
    let mut size = 0u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, RRF_RT_REG_SZ, None, None, Some(&mut size)).is_ok() }
}

fn delete_run_value() {
    if !run_value_exists() {
        return;
    }
    let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE) };
    log!("login: removed the Run value ({r:?}, still there: {})", run_value_exists());
}

fn set_run_value(exe: &str) -> std::io::Result<()> {
    let cmd: Vec<u16> = format!("\"{exe}\"").encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            REG_SZ.0,
            Some(cmd.as_ptr() as *const c_void),
            (cmd.len() * 2) as u32,
        )
        .ok()
        .map_err(std::io::Error::other)
    }
}

fn current_exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

pub fn is_enabled() -> bool {
    match ENABLED.load(Ordering::SeqCst) {
        0 => {
            let on = elevation::task_command(TASK).is_some() || run_value_exists();
            ENABLED.store(if on { 2 } else { 1 }, Ordering::SeqCst);
            on
        }
        n => n == 2,
    }
}

pub fn set_enabled(enabled: bool) -> std::io::Result<()> {
    let r = if enabled { enable() } else { disable() };
    ENABLED.store(0, Ordering::SeqCst);
    r
}

fn enable() -> std::io::Result<()> {
    let exe = current_exe();
    match install_task(&exe) {
        Ok(()) => {
            delete_run_value();
            Ok(())
        }
        Err(e) => {
            log!("login task: {e}; using the Run key instead");
            set_run_value(&exe)
        }
    }
}

fn disable() -> std::io::Result<()> {
    delete_run_value();
    match elevation::task_command(TASK) {
        Some(_) => elevation::delete_task(TASK),
        None => Ok(()),
    }
}

fn install_task(exe: &str) -> std::io::Result<()> {
    elevation::create_task(TASK, &task_xml(exe))
}

fn task_xml(exe: &str) -> String {
    let dir = std::path::Path::new(exe).parent().map(|p| p.display().to_string()).unwrap_or_default();
    let user = elevation::escape(&elevation::user_id());
    // Task Scheduler's defaults would skip the task on battery, stop DockSide
    // after 3 days, and run it at below-normal priority.
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts DockSide when you sign in.</Description></RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
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
      <Arguments>{LOGIN}</Arguments>
      <WorkingDirectory>{}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        elevation::escape(exe),
        elevation::escape(&dir),
    )
}

/// Moves a `Run` value (older versions, or the rename from MacFinder) over to
/// the login task, and keeps the task pointing at this exe if it was moved.
/// Runs schtasks: call off the startup path.
pub fn refresh() {
    let legacy = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, LEGACY_VALUE).is_ok() };
    let exe = current_exe();
    let stale = match elevation::task_command(TASK) {
        Some(task) => {
            // An older build may have written it back: with both, a second
            // launch at login would open Settings.
            delete_run_value();
            !task.eq_ignore_ascii_case(&exe)
        }
        None => legacy || run_value_exists(),
    };
    if stale {
        if let Err(e) = enable() {
            log!("could not update start at login: {e}");
        }
        ENABLED.store(0, Ordering::SeqCst);
    }
}

/// Started by the login task, which fires as the user signs in: hold off until
/// Explorer's taskbar exists, since DockSide hides it and hosts tray icons
/// alongside it. Gives up after a while rather than never starting.
pub fn wait_for_shell() {
    if !std::env::args().any(|a| a == LOGIN) {
        return;
    }
    let start = std::time::Instant::now();
    while super::tray::explorer_tray().is_none() {
        if start.elapsed() > std::time::Duration::from_secs(60) {
            log!("login: no taskbar after 60s, starting anyway");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    log!("login: taskbar up after {:?}", start.elapsed());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registers the real task XML under a throwaway name: Task Scheduler
    /// accepts it unelevated, and it reads back the exe it starts.
    #[test]
    #[ignore = "creates and deletes a scheduled task"]
    fn task_registers_unelevated() {
        let name = "DockSide Login Test";
        let exe = r"C:\Apps & Tools\DockSide\dockside.exe";
        elevation::create_task(name, &task_xml(exe)).unwrap();
        let read = elevation::task_command(name);
        elevation::delete_task(name).unwrap();
        assert_eq!(read.as_deref(), Some(exe));
    }
}
