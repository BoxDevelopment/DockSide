//! Battery details for the menu bar's Battery menu: charge, time left, health
//! (from the battery driver itself), the Windows power mode, the charge level
//! over the last day (sampled by DockSide while it runs) and the apps using
//! significant energy (CPU time, sampled while the menu is open).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{CloseHandle, FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::Power::*;
use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// The battery device interface class.
const GUID_DEVICE_BATTERY: GUID = GUID::from_u128(0x72631e54_78a4_11d0_bcf7_00aa00b7b32a);
const BATTERY_CHARGING: u32 = 0x4;
const UNKNOWN_RATE: i32 = i32::MIN;
const UNKNOWN_CAPACITY: u32 = u32::MAX;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Details {
    pub percent: u8,
    /// On the power adapter.
    pub plugged: bool,
    /// The driver says it's charging (plugged in can still be "not charging").
    pub charging: bool,
    /// Seconds of battery left, or until full.
    pub left: Option<u32>,
    pub to_full: Option<u32>,
    /// Watt-hours (the driver reports mWh; some report relative units).
    pub design: Option<u32>,
    pub full: Option<u32>,
    pub cycles: Option<u32>,
    /// Energy Saver is on.
    pub saver: bool,
}

impl Details {
    /// Maximum capacity as a share of the design capacity.
    pub fn health(&self) -> Option<u8> {
        let (d, f) = (self.design?, self.full?);
        (d > 0 && f > 0).then(|| ((f as f32 / d as f32) * 100.0).round().min(100.0) as u8)
    }
}

pub fn details() -> Option<Details> {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s).ok()? };
    if s.BatteryFlag & 128 != 0 || s.BatteryFlag == 255 || s.BatteryLifePercent > 100 {
        return None;
    }
    let mut d = Details {
        percent: s.BatteryLifePercent,
        plugged: s.ACLineStatus == 1,
        charging: s.BatteryFlag & 8 != 0,
        left: (s.ACLineStatus != 1 && s.BatteryLifeTime != u32::MAX && s.BatteryLifeTime > 0).then_some(s.BatteryLifeTime),
        saver: crate::win::controls::low_power_mode(),
        ..Default::default()
    };
    if let Some(b) = driver() {
        d.design = (b.design > 0).then_some(b.design);
        d.full = (b.full > 0).then_some(b.full);
        d.cycles = (b.cycles > 0).then_some(b.cycles);
        if let Some(st) = b.status {
            d.charging = st.PowerState & BATTERY_CHARGING != 0;
            if d.charging && st.Rate != UNKNOWN_RATE && st.Rate > 0 && st.Capacity != UNKNOWN_CAPACITY && b.full > st.Capacity {
                d.to_full = Some(((b.full - st.Capacity) as f64 / st.Rate as f64 * 3600.0) as u32);
            }
        }
    }
    Some(d)
}

struct Driver {
    design: u32,
    full: u32,
    cycles: u32,
    status: Option<BATTERY_STATUS>,
}

/// The first battery, asked directly (works without admin rights).
fn driver() -> Option<Driver> {
    unsafe {
        let set = SetupDiGetClassDevsW(Some(&GUID_DEVICE_BATTERY), PCWSTR::null(), HWND::default(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE).ok()?;
        let result = (|| {
            let mut iface = SP_DEVICE_INTERFACE_DATA { cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
            SetupDiEnumDeviceInterfaces(set, None, &GUID_DEVICE_BATTERY, 0, &mut iface).ok()?;
            let mut need = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &iface, None, 0, Some(&mut need), None);
            if need == 0 {
                return None;
            }
            // A u64 buffer keeps the struct aligned.
            let mut buf = vec![0u64; (need as usize).div_ceil(8)];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            SetupDiGetDeviceInterfaceDetailW(set, &iface, Some(detail), need, None, None).ok()?;
            let path = PCWSTR((*detail).DevicePath.as_ptr());
            let h = CreateFileW(path, (GENERIC_READ | GENERIC_WRITE).0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, HANDLE::default()).ok()?;
            let out = query(h);
            let _ = CloseHandle(h);
            out
        })();
        let _ = SetupDiDestroyDeviceInfoList(set);
        result
    }
}

unsafe fn ioctl<I, O: Default>(h: HANDLE, code: u32, input: &I) -> Option<O> {
    let mut out = O::default();
    let mut got = 0u32;
    DeviceIoControl(
        h,
        code,
        Some(input as *const I as *const _),
        std::mem::size_of::<I>() as u32,
        Some(&mut out as *mut O as *mut _),
        std::mem::size_of::<O>() as u32,
        Some(&mut got),
        None,
    )
    .ok()?;
    Some(out)
}

unsafe fn query(h: HANDLE) -> Option<Driver> {
    let wait = 0u32;
    let tag: u32 = ioctl(h, IOCTL_BATTERY_QUERY_TAG, &wait)?;
    if tag == 0 {
        return None;
    }
    let q = BATTERY_QUERY_INFORMATION { BatteryTag: tag, InformationLevel: BatteryInformation, AtRate: 0 };
    let info: BATTERY_INFORMATION = ioctl(h, IOCTL_BATTERY_QUERY_INFORMATION, &q)?;
    let ws = BATTERY_WAIT_STATUS { BatteryTag: tag, ..Default::default() };
    let status: Option<BATTERY_STATUS> = ioctl(h, IOCTL_BATTERY_QUERY_STATUS, &ws);
    Some(Driver { design: info.DesignedCapacity, full: info.FullChargedCapacity, cycles: info.CycleCount, status })
}

// ---------------------------------------------------------------------------
// Power mode
// ---------------------------------------------------------------------------

pub use crate::spotlight::commands::PowerMode;

pub fn power_mode() -> PowerMode {
    crate::win::controls::power_mode()
}

// ---------------------------------------------------------------------------
// Charge history: a sample every few minutes while DockSide runs.
// ---------------------------------------------------------------------------

const SAMPLE_EVERY: Duration = Duration::from_secs(300);
/// Kept a little longer than the graph shows.
const KEEP: i64 = 26 * 3600;

fn history_path() -> std::path::PathBuf {
    crate::config::dir().join("battery_history.csv")
}

/// (Unix seconds, percent, plugged in), oldest first.
pub fn history() -> Vec<(i64, u8, bool)> {
    let text = std::fs::read_to_string(history_path()).unwrap_or_default();
    let cutoff = crate::island::calendar::unix_now() - KEEP;
    text.lines()
        .filter_map(|l| {
            let mut it = l.split(',');
            let t: i64 = it.next()?.trim().parse().ok()?;
            let p: u8 = it.next()?.trim().parse().ok()?;
            let ac = it.next().map(|s| s.trim() == "1").unwrap_or(false);
            (t >= cutoff && p <= 100).then_some((t, p, ac))
        })
        .collect()
}

fn record() {
    let Some(b) = crate::win::system::battery() else { return };
    let now = crate::island::calendar::unix_now();
    let mut rows = history();
    // Back-to-back runs: keep one sample per minute.
    rows.retain(|(t, ..)| now - t >= 60);
    let plugged = {
        let mut s = SYSTEM_POWER_STATUS::default();
        unsafe { GetSystemPowerStatus(&mut s).is_ok() && s.ACLineStatus == 1 }
    };
    rows.push((now, b.percent, plugged));
    let text: String = rows.iter().map(|(t, p, ac)| format!("{t},{p},{}\n", *ac as u8)).collect();
    let _ = std::fs::write(history_path(), text);
}

/// Starts sampling the charge level (only on machines with a battery).
pub fn start_history() {
    if crate::win::system::battery().is_none() {
        return;
    }
    let spawned = std::thread::Builder::new().name("battery history".into()).spawn(|| loop {
        record();
        std::thread::sleep(SAMPLE_EVERY);
    });
    if let Err(e) = spawned {
        log!("battery history thread failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// Apps using significant energy
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Hog {
    pub exe: String,
    pub name: String,
    /// Share of one CPU core, 0–100+.
    pub cpu: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub details: Option<Details>,
    pub mode: Option<PowerMode>,
    /// `None` until the first measurement is in.
    pub hogs: Option<Vec<Hog>>,
}

/// Measures while the Battery menu is open; dropped (stopped) when it closes.
pub struct Watcher {
    pub live: Arc<Mutex<Live>>,
    stop: Arc<AtomicBool>,
}

/// A core's worth of CPU at this share or more counts as significant energy.
const SIGNIFICANT: f32 = 8.0;

impl Watcher {
    pub fn start(notify: isize, msg: u32) -> Watcher {
        let live = Arc::new(Mutex::new(Live { details: details(), mode: Some(power_mode()), hogs: None }));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, st) = (live.clone(), stop.clone());
        let spawned = std::thread::Builder::new().name("battery menu".into()).spawn(move || {
            let mut names: HashMap<String, String> = HashMap::new();
            let mut last = cpu_times();
            let mut at = Instant::now();
            let mut first = true;
            while !st.load(Ordering::Relaxed) {
                // The first reading comes quickly; later ones every few seconds.
                let wait = if first { 1200 } else { 3000 };
                let until = Instant::now() + Duration::from_millis(wait);
                while Instant::now() < until && !st.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                if st.load(Ordering::Relaxed) {
                    break;
                }
                first = false;
                let now = cpu_times();
                let secs = at.elapsed().as_secs_f32().max(0.1);
                at = Instant::now();
                let apps: HashSet<String> = crate::win::apps::list_windows().into_iter().map(|w| w.exe.to_lowercase()).collect();
                let mut by_exe: HashMap<String, f32> = HashMap::new();
                for (pid, (exe, t)) in &now {
                    let Some((_, before)) = last.get(pid) else { continue };
                    let used = t.saturating_sub(*before) as f32 / 10_000_000.0;
                    *by_exe.entry(exe.clone()).or_default() += used / secs * 100.0;
                }
                last = now;
                let me = std::env::current_exe().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
                let mut hogs: Vec<Hog> = by_exe
                    .into_iter()
                    .filter(|(exe, cpu)| *cpu >= SIGNIFICANT && apps.contains(exe) && *exe != me)
                    .map(|(exe, cpu)| {
                        let name = names
                            .entry(exe.clone())
                            .or_insert_with(|| {
                                crate::win::apps::file_description(&exe)
                                    .unwrap_or_else(|| crate::win::apps::exe_name(&exe).trim_end_matches(".exe").to_string())
                            })
                            .clone();
                        Hog { exe, name, cpu }
                    })
                    .collect();
                hogs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
                hogs.truncate(5);
                let fresh = Live { details: details(), mode: Some(power_mode()), hogs: Some(hogs) };
                let changed = {
                    let mut g = l.lock().unwrap_or_else(|e| e.into_inner());
                    let c = *g != fresh;
                    *g = fresh;
                    c
                };
                if changed {
                    unsafe {
                        let _ = PostMessageW(HWND(notify as _), msg, WPARAM(0), LPARAM(0));
                    }
                }
            }
        });
        if let Err(e) = spawned {
            log!("battery menu thread failed: {e}");
        }
        Watcher { live, stop }
    }

    pub fn snapshot(&self) -> Live {
        self.live.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Reads the battery again now (after a change made from the menu).
    pub fn refresh(&self) {
        if let Ok(mut g) = self.live.lock() {
            g.details = details();
            g.mode = Some(power_mode());
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Kernel + user CPU time (100 ns) of every process we can read, with its exe (lowercase).
fn cpu_times() -> HashMap<u32, (String, u64)> {
    let mut out = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let pid = e.th32ProcessID;
            if pid != 0 {
                if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                    let (mut c, mut x, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
                    if GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u).is_ok() {
                        let ft = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
                        if let Some(path) = crate::win::apps::process_path(pid) {
                            out.insert(pid, (path.to_lowercase(), ft(k) + ft(u)));
                        }
                    }
                    let _ = CloseHandle(h);
                }
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_battery() {
        // Prints what this machine reports (no battery is fine too).
        println!("battery: {:?}", super::details());
        println!("mode: {:?}", super::power_mode());
        let d = super::details();
        if let Some(d) = d {
            assert!(d.percent <= 100);
        }
    }
}
