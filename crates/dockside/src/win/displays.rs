//! The connected displays, with ids that survive reboots and rearranging
//! (for settings that name a display) and the names their monitors report.

use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};

use super::Monitor;

/// `MONITORINFOF_PRIMARY`.
const PRIMARY: u32 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub monitor: Monitor,
    /// Stable: the monitor's device path (its model, serial and port), or the
    /// GDI name (`\\.\DISPLAY2`) when Windows doesn't report one.
    pub id: String,
    /// What the monitor calls itself ("DELL U2720Q"); "Built-in Display" for a
    /// laptop panel; otherwise "Display N".
    pub name: String,
    pub primary: bool,
}

fn wide(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}

/// GDI device name → (device path, friendly name, internal panel), from the display configuration.
fn targets() -> Vec<(String, String, String, bool)> {
    let mut out = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).is_err() {
            return out;
        }
        for p in &paths[..np as usize] {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut tgt.header) != 0 {
                continue;
            }
            let tech = p.targetInfo.outputTechnology;
            let internal = tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
                || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
                || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED;
            out.push((wide(&src.viewGdiDeviceName), wide(&tgt.monitorDevicePath), wide(&tgt.monitorFriendlyDeviceName), internal));
        }
    }
    out
}

unsafe extern "system" fn collect(hmon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let list = &mut *(data.0 as *mut Vec<HMONITOR>);
    list.push(hmon);
    true.into()
}

thread_local! {
    static LAST: std::cell::RefCell<Vec<Display>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The displays as last listed (cheap: for drawing Settings and menus).
pub fn current() -> Vec<Display> {
    let cached = LAST.with(|l| l.borrow().clone());
    if cached.is_empty() {
        list()
    } else {
        cached
    }
}

/// Every display, the primary first, then left to right, top to bottom.
pub fn list() -> Vec<Display> {
    let out = query();
    LAST.with(|l| *l.borrow_mut() = out.clone());
    out
}

fn query() -> Vec<Display> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(HDC::default(), None, Some(collect), LPARAM(&mut handles as *mut _ as isize));
    }
    let targets = targets();
    let mut out: Vec<Display> = handles
        .into_iter()
        .map(|h| {
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            unsafe {
                let _ = GetMonitorInfoW(h, &mut mi.monitorInfo as *mut MONITORINFO);
            }
            let gdi = wide(&mi.szDevice);
            let target = targets.iter().find(|t| t.0 == gdi);
            let id = target.map(|t| t.1.clone()).filter(|p| !p.is_empty()).unwrap_or_else(|| gdi.clone());
            let name = match target {
                Some(t) if t.3 => "Built-in Display".to_string(),
                Some(t) if !t.2.is_empty() => t.2.clone(),
                _ => String::new(),
            };
            Display { monitor: Monitor::from_handle(h), id, name, primary: mi.monitorInfo.dwFlags & PRIMARY != 0 }
        })
        .collect();
    out.sort_by_key(|d| (!d.primary, d.monitor.rect.left, d.monitor.rect.top));
    // Unnamed (or identically named) monitors are told apart by number.
    for i in 0..out.len() {
        let dup = out.iter().filter(|d| d.name == out[i].name).count() > 1;
        if out[i].name.is_empty() || dup {
            out[i].name = if out[i].name.is_empty() { format!("Display {}", i + 1) } else { format!("{} ({})", out[i].name, i + 1) };
        }
    }
    out
}

/// The display with `id`, if it's connected.
pub fn find<'a>(list: &'a [Display], id: &str) -> Option<&'a Display> {
    list.iter().find(|d| d.id.eq_ignore_ascii_case(id))
}

/// The display `id` names, falling back to the primary one (an empty id, or a
/// display that's been disconnected).
pub fn resolve<'a>(list: &'a [Display], id: &str) -> Option<&'a Display> {
    find(list, id).or_else(|| list.first())
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_the_primary_first() {
        let list = super::list();
        for d in &list {
            println!("{:?} {} [{}] {:?} @{}", d.primary, d.name, d.id, d.monitor.rect, d.monitor.scale);
        }
        assert!(!list.is_empty());
        assert!(list[0].primary);
    }
}
