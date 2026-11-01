//! Per-app volume: the default output device's audio sessions, grouped by app,
//! each with its own level (WASAPI's session manager, as in Windows' Volume
//! Mixer). Levels are relative to the master volume.

use std::path::PathBuf;

use windows::core::Interface;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    eConsole, eRender, AudioSessionStateActive, AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use crate::win::apps;

/// One app's sound: all its sessions together, across its processes (Discord
/// and browsers play from several).
#[derive(Clone, Debug, PartialEq)]
pub struct App {
    pub pids: Vec<u32>,
    /// Windows' own sounds (notifications and the like).
    pub system: bool,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub level: f32,
    pub muted: bool,
    /// Making sound right now (listed first).
    pub active: bool,
}

/// Every session of the default output device that hasn't ended, with its volume.
fn each_session(mut f: impl FnMut(&IAudioSessionControl2, &ISimpleAudioVolume)) -> Option<()> {
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = devices.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let manager = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None).ok()?;
        let list = manager.GetSessionEnumerator().ok()?;
        for i in 0..list.GetCount().ok()? {
            let Ok(control) = list.GetSession(i) else { continue };
            if control.GetState().ok() == Some(AudioSessionStateExpired) {
                continue;
            }
            let (Ok(c2), Ok(volume)) = (control.cast::<IAudioSessionControl2>(), control.cast::<ISimpleAudioVolume>()) else { continue };
            f(&c2, &volume);
        }
    }
    Some(())
}

/// The apps with sound, the ones playing first, then by name.
pub fn apps() -> Vec<App> {
    let mut out: Vec<App> = Vec::new();
    each_session(|c, v| unsafe {
        let system = c.IsSystemSoundsSession() == S_OK;
        let pid = if system { 0 } else { c.GetProcessId().unwrap_or(0) };
        let exe = (!system).then(|| apps::process_path(pid)).flatten().map(PathBuf::from);
        let active = c.GetState().ok() == Some(AudioSessionStateActive);
        let (level, muted) = (v.GetMasterVolume().unwrap_or(1.0), v.GetMute().map(|m| m.as_bool()).unwrap_or(false));
        // The same app: the same exe (or, without one, the same process).
        let same = |a: &App| a.system == system && if exe.is_some() { a.exe == exe } else { a.pids.contains(&pid) };
        if let Some(a) = out.iter_mut().find(|a| same(a)) {
            // Shows its loudest session, and counts as playing if any is.
            if !a.pids.contains(&pid) {
                a.pids.push(pid);
            }
            a.active |= active;
            a.level = a.level.max(level);
            a.muted &= muted;
            return;
        }
        let name = if system {
            "System Sounds".to_string()
        } else {
            let path = exe.as_ref().map(|e| e.to_string_lossy().into_owned());
            path.as_deref()
                .and_then(apps::file_description)
                .or_else(|| exe.as_ref().and_then(|e| e.file_stem()).map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| format!("App {pid}"))
        };
        out.push(App { pids: vec![pid], system, name, exe, level, muted, active });
    });
    out.sort_by(|a, b| b.active.cmp(&a.active).then(a.system.cmp(&b.system)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out
}

/// Sets every session of `app` to `level` (0-1); a level above zero unmutes it.
pub fn set_level(app: &App, level: f32) {
    let level = level.clamp(0.0, 1.0);
    each_session(|c, v| unsafe {
        let system = c.IsSystemSoundsSession() == S_OK;
        let matches = if app.system { system } else { !system && c.GetProcessId().is_ok_and(|p| app.pids.contains(&p)) };
        if matches {
            let _ = v.SetMasterVolume(level, std::ptr::null());
            if level > 0.0 {
                let _ = v.SetMute(false, std::ptr::null());
            }
        }
    });
}
