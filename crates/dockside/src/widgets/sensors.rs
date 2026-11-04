//! What a System widget can show, and the readings behind it.
//!
//! * Each System widget lists its sensors in `sensors` (config keys like
//!   `"cpu"`, `"disk:D"`, `"gpu:10de-2504"`); none means [`DEFAULTS`].
//! * CPU, memory, drives and network are cheap: read on the UI thread every
//!   other second, and only the drives on screen are asked.
//! * GPUs (usage the way Task Manager adds it up, memory, temperature) come
//!   from a worker thread, started only while a widget shows one: the
//!   `GPU Engine` performance counters can take a while to collect.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::HSTRING;

use crate::config::WidgetItem;
use crate::island::sources::Stop;

/// Samples kept for the graphs.
pub const HISTORY: usize = 60;
/// What a System widget shows until it's told otherwise.
pub const DEFAULTS: [&str; 4] = ["cpu", "memory", "disk", "network"];
const GPU_EVERY: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Sensors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sensor {
    Cpu,
    Memory,
    /// A drive by letter; none = the system drive.
    Disk(Option<char>),
    /// An adapter by name ("Wi-Fi"); none = every adapter together.
    Network(Option<String>),
    /// GPU usage, by id (see [`GpuInfo::id`]); none = the first GPU.
    Gpu(Option<String>),
    GpuMemory(Option<String>),
    GpuTemp(Option<String>),
}

impl Sensor {
    pub fn parse(s: &str) -> Option<Sensor> {
        let (kind, arg) = match s.split_once(':') {
            Some((k, a)) => (k.trim(), Some(a.trim()).filter(|a| !a.is_empty())),
            None => (s.trim(), None),
        };
        let gpu = || arg.map(str::to_ascii_lowercase);
        Some(match kind.to_ascii_lowercase().as_str() {
            "cpu" => Sensor::Cpu,
            "memory" | "mem" | "ram" => Sensor::Memory,
            "disk" | "drive" => Sensor::Disk(arg.and_then(|a| a.chars().next()).filter(char::is_ascii_alphabetic).map(|c| c.to_ascii_uppercase())),
            "network" | "net" => Sensor::Network(arg.map(str::to_string)),
            "gpu" => Sensor::Gpu(gpu()),
            "gpu_memory" | "gpu_mem" | "vram" => Sensor::GpuMemory(gpu()),
            "gpu_temp" | "gpu_temperature" => Sensor::GpuTemp(gpu()),
            _ => return None,
        })
    }

    /// Its config key.
    pub fn key(&self) -> String {
        let with = |k: &str, a: &Option<String>| a.as_ref().map_or_else(|| k.to_string(), |a| format!("{k}:{a}"));
        match self {
            Sensor::Cpu => "cpu".into(),
            Sensor::Memory => "memory".into(),
            Sensor::Disk(d) => d.map_or_else(|| "disk".into(), |d| format!("disk:{d}")),
            Sensor::Network(n) => with("network", n),
            Sensor::Gpu(g) => with("gpu", g),
            Sensor::GpuMemory(g) => with("gpu_memory", g),
            Sensor::GpuTemp(g) => with("gpu_temp", g),
        }
    }

    /// Usage over time: CPU and GPUs get a graph where there's room.
    pub fn graphable(&self) -> bool {
        matches!(self, Sensor::Cpu | Sensor::Gpu(_))
    }

    fn gpu(&self) -> Option<&Option<String>> {
        match self {
            Sensor::Gpu(g) | Sensor::GpuMemory(g) | Sensor::GpuTemp(g) => Some(g),
            _ => None,
        }
    }
}

/// `item`'s sensors as written (defaults when it has none).
pub fn keys(item: &WidgetItem) -> Vec<String> {
    item.sensors.clone().unwrap_or_else(|| DEFAULTS.map(String::from).to_vec())
}

/// `item`'s sensors, in order; unknown keys are skipped.
pub fn of(item: &WidgetItem) -> Vec<Sensor> {
    keys(item).iter().filter_map(|k| Sensor::parse(k)).collect()
}

/// Does any of these widgets show a GPU?
pub fn wants_gpu<'a>(items: impl IntoIterator<Item = &'a WidgetItem>) -> bool {
    items.into_iter().any(|i| of(i).iter().any(|s| s.gpu().is_some()))
}

/// The drives these widgets show.
pub fn wanted_drives<'a>(items: impl IntoIterator<Item = &'a WidgetItem>) -> Vec<char> {
    let mut v = Vec::new();
    for i in items {
        for s in of(i) {
            if let Sensor::Disk(d) = s {
                let d = d.unwrap_or_else(system_drive);
                if !v.contains(&d) {
                    v.push(d);
                }
            }
        }
    }
    v
}

pub fn system_drive() -> char {
    std::env::var("SystemDrive").ok().and_then(|s| s.chars().next()).map_or('C', |c| c.to_ascii_uppercase())
}

/// Fills in what a sensor without an argument stands for (the system drive,
/// the first GPU), so ticking one in a menu matches it.
pub fn canonical(s: Sensor, first_gpu: Option<&str>) -> Sensor {
    let gpu = |g: Option<String>| g.or_else(|| first_gpu.map(str::to_string));
    match s {
        Sensor::Disk(None) => Sensor::Disk(Some(system_drive())),
        Sensor::Gpu(g) => Sensor::Gpu(gpu(g)),
        Sensor::GpuMemory(g) => Sensor::GpuMemory(gpu(g)),
        Sensor::GpuTemp(g) => Sensor::GpuTemp(gpu(g)),
        s => s,
    }
}

/// What to store for `list`: nothing when it's just the defaults.
pub fn stored(list: &[Sensor], first_gpu: Option<&str>) -> Option<Vec<String>> {
    let defaults: Vec<Sensor> = DEFAULTS.iter().filter_map(|k| Sensor::parse(k)).map(|s| canonical(s, first_gpu)).collect();
    let canon: Vec<Sensor> = list.iter().cloned().map(|s| canonical(s, first_gpu)).collect();
    (canon != defaults).then(|| list.iter().map(Sensor::key).collect())
}

// ---------------------------------------------------------------------------
// Hardware
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct GpuInfo {
    /// "vendor-device" in hex ("10de-2504"), plus "#2", "#3"... for twins.
    pub id: String,
    pub name: String,
    pub vendor: u32,
    luid: u64,
    /// Bytes of its own memory (or of shared system memory, for integrated GPUs).
    mem_total: u64,
    dedicated: bool,
}

/// Graphics adapters, as DXGI lists them (the software renderer left out).
pub fn gpus() -> Vec<GpuInfo> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return Vec::new() };
    let mut out: Vec<GpuInfo> = Vec::new();
    let mut seen_luids = Vec::new();
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        let Ok(d) = (unsafe { adapter.GetDesc1() }) else { continue };
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || d.VendorId == 0x1414 {
            continue;
        }
        let luid = ((d.AdapterLuid.HighPart as u32 as u64) << 32) | d.AdapterLuid.LowPart as u64;
        if seen_luids.contains(&luid) {
            continue;
        }
        seen_luids.push(luid);
        let base = format!("{:04x}-{:04x}", d.VendorId, d.DeviceId);
        let twins = out.iter().filter(|g| g.id == base || g.id.starts_with(&format!("{base}#"))).count();
        let id = if twins == 0 { base } else { format!("{base}#{}", twins + 1) };
        let name = String::from_utf16_lossy(&d.Description[..d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len())]);
        let name = name.replace("(R)", "").replace("(TM)", "").replace("(tm)", "").split_whitespace().collect::<Vec<_>>().join(" ");
        // Integrated GPUs set aside a sliver of their own; what they use is shared memory.
        let dedicated = d.DedicatedVideoMemory as u64 >= 512 << 20;
        let mem_total = if dedicated { d.DedicatedVideoMemory as u64 } else { d.SharedSystemMemory as u64 };
        out.push(GpuInfo { id, name, vendor: d.VendorId, luid, mem_total, dedicated });
    }
    out
}

/// "NVIDIA", "Intel", "AMD"...
pub fn vendor_name(vendor: u32) -> Option<&'static str> {
    Some(match vendor {
        0x10de => "NVIDIA",
        0x8086 => "Intel",
        0x1002 | 0x1022 => "AMD",
        0x5143 => "Qualcomm",
        0x13b5 => "Arm",
        _ => return None,
    })
}

/// A drive the widget can show.
#[derive(Clone, Debug, PartialEq)]
pub struct DriveInfo {
    pub letter: char,
    /// "Local Disk (C:)", "Data (D:)".
    pub name: String,
}

/// Fixed and removable drives with something in them (network drives are
/// left out: asking them can hang).
pub fn drives() -> Vec<DriveInfo> {
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i) as char;
        let root = HSTRING::from(format!("{letter}:\\"));
        let kind = unsafe { GetDriveTypeW(&root) };
        if kind != DRIVE_FIXED && kind != DRIVE_REMOVABLE {
            continue;
        }
        if disk_space(letter).is_none() {
            continue;
        }
        let mut label = [0u16; 261];
        let ok = unsafe { GetVolumeInformationW(&root, Some(&mut label), None, None, None, None) }.is_ok();
        let label = if ok { String::from_utf16_lossy(&label[..label.iter().position(|&c| c == 0).unwrap_or(0)]) } else { String::new() };
        let label = if label.is_empty() {
            if kind == DRIVE_FIXED { "Local Disk".to_string() } else { "USB Drive".to_string() }
        } else {
            label
        };
        out.push(DriveInfo { letter, name: format!("{label} ({letter}:)") });
    }
    out
}

/// (free, total) bytes on a drive.
fn disk_space(letter: char) -> Option<(u64, u64)> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let (mut free, mut total) = (0u64, 0u64);
    let root = HSTRING::from(format!("{letter}:\\"));
    unsafe { GetDiskFreeSpaceExW(&root, None, Some(&mut total), Some(&mut free)) }.ok()?;
    (total > 0).then_some((free, total))
}

/// Physical network adapters that are up, by name ("Wi-Fi", "Ethernet").
pub fn adapters() -> Vec<String> {
    let mut v: Vec<String> = net_octets().into_iter().map(|(name, _, _)| name).collect();
    v.dedup();
    v
}

/// Bytes received / sent so far by each physical network adapter that's up.
fn net_octets() -> Vec<(String, u64, u64)> {
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    let mut out = Vec::new();
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).is_err() || table.is_null() {
            return out;
        }
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        for r in rows {
            // Bit 0: HardwareInterface (skips loopback, tunnels and virtual adapters).
            let hardware = r.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
            if hardware && r.OperStatus == IfOperStatusUp {
                let alias = String::from_utf16_lossy(&r.Alias[..r.Alias.iter().position(|&c| c == 0).unwrap_or(0)]);
                // An adapter can have several rows (filter drivers): add them up.
                match out.iter_mut().find(|(n, _, _): &&mut (String, u64, u64)| *n == alias) {
                    Some(e) => {
                        e.1 = e.1.max(r.InOctets);
                        e.2 = e.2.max(r.OutOctets);
                    }
                    None => out.push((alias, r.InOctets, r.OutOctets)),
                }
            }
        }
        FreeMibTable(table as *const _);
    }
    out
}

// ---------------------------------------------------------------------------
// Readings
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Disk {
    pub letter: char,
    /// 0..1 full.
    pub used: f32,
    pub free: u64,
    pub total: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Net {
    pub name: String,
    /// Bytes per second.
    pub down: f64,
    pub up: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gpu {
    pub id: String,
    pub name: String,
    pub vendor: u32,
    /// 0..1, the busiest engine.
    pub usage: f32,
    pub history: VecDeque<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub dedicated: bool,
    /// °C, when the driver reports it.
    pub temp: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct SysStats {
    /// 0..1
    pub cpu: f32,
    pub cpu_history: VecDeque<f32>,
    pub mem: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disks: Vec<Disk>,
    /// Every adapter's rates; `down` / `up` are their total.
    pub nets: Vec<Net>,
    pub down: f64,
    pub up: f64,
    pub gpus: Vec<Gpu>,
}

pub type SharedGpus = Arc<Mutex<Vec<Gpu>>>;

#[derive(Default)]
pub struct Sampler {
    cpu: Option<(u64, u64)>,
    net: HashMap<String, (u64, u64)>,
    net_at: Option<Instant>,
    pub stats: SysStats,
}

fn ft(f: windows::Win32::Foundation::FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

fn push(history: &mut VecDeque<f32>, v: f32) {
    history.push_back(v);
    while history.len() > HISTORY {
        history.pop_front();
    }
}

impl Sampler {
    /// Takes a reading of everything cheap, and the drives in `drives`; GPUs
    /// are copied from their worker.
    pub fn sample(&mut self, drives: &[char], gpus: Option<&SharedGpus>) {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        use windows::Win32::System::Threading::GetSystemTimes;

        let s = &mut self.stats;
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        if unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_ok() {
            // Kernel time includes idle time.
            let (busy_total, idle) = (ft(kernel) + ft(user), ft(idle));
            if let Some((t0, i0)) = self.cpu {
                let (dt, di) = (busy_total.saturating_sub(t0), idle.saturating_sub(i0));
                if dt > 0 {
                    s.cpu = (1.0 - di as f32 / dt as f32).clamp(0.0, 1.0);
                    push(&mut s.cpu_history, s.cpu);
                }
            }
            self.cpu = Some((busy_total, idle));
        }

        let mut mem = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        if unsafe { GlobalMemoryStatusEx(&mut mem) }.is_ok() {
            s.mem_total = mem.ullTotalPhys;
            s.mem_used = mem.ullTotalPhys.saturating_sub(mem.ullAvailPhys);
            s.mem = s.mem_used as f32 / s.mem_total.max(1) as f32;
        }

        s.disks = drives
            .iter()
            .filter_map(|&letter| {
                let (free, total) = disk_space(letter)?;
                Some(Disk { letter, used: 1.0 - free as f32 / total as f32, free, total })
            })
            .collect();

        let now = Instant::now();
        let secs = self.net_at.map(|t| (now - t).as_secs_f64().max(0.1));
        let mut nets = Vec::new();
        for (name, rx, tx) in net_octets() {
            let (down, up) = match (self.net.get(&name), secs) {
                (Some(&(rx0, tx0)), Some(secs)) => (rx.saturating_sub(rx0) as f64 / secs, tx.saturating_sub(tx0) as f64 / secs),
                _ => (0.0, 0.0),
            };
            self.net.insert(name.clone(), (rx, tx));
            nets.push(Net { name, down, up });
        }
        self.net.retain(|n, _| nets.iter().any(|m| m.name == *n));
        self.net_at = Some(now);
        s.down = nets.iter().map(|n| n.down).sum();
        s.up = nets.iter().map(|n| n.up).sum();
        s.nets = nets;

        s.gpus = gpus.and_then(|g| g.lock().ok().map(|g| g.clone())).unwrap_or_default();
    }
}

// ---------------------------------------------------------------------------
// GPU worker
// ---------------------------------------------------------------------------

/// Performance counters for every GPU at once.
struct Counters {
    query: isize,
    engines: isize,
    dedicated: isize,
    shared: isize,
}

impl Counters {
    fn open() -> Option<Counters> {
        use windows::Win32::System::Performance::{PdhAddEnglishCounterW, PdhCloseQuery, PdhOpenQueryW};
        unsafe {
            let mut query = 0isize;
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            let add = |path: &str| {
                let mut c = 0isize;
                (PdhAddEnglishCounterW(query, &HSTRING::from(path), 0, &mut c) == 0).then_some(c)
            };
            let (Some(engines), Some(dedicated), Some(shared)) = (
                add("\\GPU Engine(*)\\Utilization Percentage"),
                add("\\GPU Adapter Memory(*)\\Dedicated Usage"),
                add("\\GPU Adapter Memory(*)\\Shared Usage"),
            ) else {
                PdhCloseQuery(query);
                return None;
            };
            Some(Counters { query, engines, dedicated, shared })
        }
    }

    fn collect(&self) -> bool {
        unsafe { windows::Win32::System::Performance::PdhCollectQueryData(self.query) == 0 }
    }

    /// Every instance of `counter`: (name, value).
    fn values(counter: isize) -> Vec<(String, f64)> {
        use windows::Win32::System::Performance::{
            PdhGetFormattedCounterArrayW, PDH_CSTATUS_VALID_DATA, PDH_FMT, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA,
        };
        const NOCAP100: u32 = 0x8000;
        let fmt = PDH_FMT(PDH_FMT_DOUBLE.0 | NOCAP100);
        let (mut size, mut count) = (0u32, 0u32);
        unsafe {
            if PdhGetFormattedCounterArrayW(counter, fmt, &mut size, &mut count, None) != PDH_MORE_DATA || size == 0 {
                return Vec::new();
            }
            // Item structs first, then the names they point at.
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(counter, fmt, &mut size, &mut count, Some(items)) != 0 {
                return Vec::new();
            }
            std::slice::from_raw_parts(items, count as usize)
                .iter()
                .filter(|i| i.FmtValue.CStatus == PDH_CSTATUS_VALID_DATA)
                .map(|i| (i.szName.to_string().unwrap_or_default(), i.FmtValue.Anonymous.doubleValue))
                .collect()
        }
    }
}

impl Drop for Counters {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::Performance::PdhCloseQuery(self.query);
        }
    }
}

/// The adapter LUID in a counter instance name ("pid_42_luid_0x00000000_0x0000D1B4_phys_0_eng_3_engtype_3D").
fn instance_luid(name: &str) -> Option<u64> {
    let rest = &name[name.find("luid_0x")? + 7..];
    let (hi, rest) = rest.split_once("_0x")?;
    let end = rest.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(rest.len());
    Some(((u32::from_str_radix(hi, 16).ok()? as u64) << 32) | u32::from_str_radix(&rest[..end], 16).ok()? as u64)
}

/// Which engine of which adapter an instance is ("luid_..._phys_0_eng_3"), whatever the process.
fn instance_engine(name: &str) -> Option<&str> {
    let start = name.find("luid_")?;
    let end = name.find("_engtype").unwrap_or(name.len());
    name.get(start..end)
}

/// Task Manager's GPU usage: each engine's use summed over processes, and the busiest engine.
fn usage_by_adapter(instances: &[(String, f64)]) -> HashMap<u64, f64> {
    let mut engines: HashMap<&str, f64> = HashMap::new();
    for (name, v) in instances {
        if let Some(e) = instance_engine(name) {
            *engines.entry(e).or_default() += v;
        }
    }
    let mut out: HashMap<u64, f64> = HashMap::new();
    for (e, v) in engines {
        if let Some(luid) = instance_luid(e) {
            let slot = out.entry(luid).or_default();
            *slot = slot.max(v);
        }
    }
    out
}

fn sum_by_adapter(instances: &[(String, f64)]) -> HashMap<u64, f64> {
    let mut out: HashMap<u64, f64> = HashMap::new();
    for (name, v) in instances {
        if let Some(luid) = instance_luid(name) {
            *out.entry(luid).or_default() += v;
        }
    }
    out
}

/// The GPU's temperature from its display driver, in °C (None where it doesn't say).
fn gpu_temp(luid: u64) -> Option<f32> {
    use windows::Wdk::Graphics::Direct3D::{
        D3DKMTCloseAdapter, D3DKMTOpenAdapterFromLuid, D3DKMTQueryAdapterInfo, D3DKMT_ADAPTER_PERFDATA, D3DKMT_CLOSEADAPTER,
        D3DKMT_OPENADAPTERFROMLUID, D3DKMT_QUERYADAPTERINFO, KMTQAITYPE_ADAPTERPERFDATA,
    };
    use windows::Win32::Foundation::LUID;
    unsafe {
        let mut open = D3DKMT_OPENADAPTERFROMLUID {
            AdapterLuid: LUID { LowPart: luid as u32, HighPart: (luid >> 32) as i32 },
            hAdapter: 0,
        };
        if D3DKMTOpenAdapterFromLuid(&mut open).is_err() {
            return None;
        }
        let mut perf = D3DKMT_ADAPTER_PERFDATA::default();
        let mut q = D3DKMT_QUERYADAPTERINFO {
            hAdapter: open.hAdapter,
            Type: KMTQAITYPE_ADAPTERPERFDATA,
            pPrivateDriverData: &mut perf as *mut _ as *mut _,
            PrivateDriverDataSize: std::mem::size_of::<D3DKMT_ADAPTER_PERFDATA>() as u32,
        };
        let ok = D3DKMTQueryAdapterInfo(&mut q).is_ok();
        let _ = D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: open.hAdapter });
        // Deci-degrees; 0 means the driver didn't fill it in.
        (ok && perf.Temperature > 0 && perf.Temperature < 1500).then(|| perf.Temperature as f32 / 10.0)
    }
}

/// Keeps `shared` up to date with every GPU's readings until `stop`.
pub fn watch_gpus(shared: SharedGpus, stop: Stop) {
    let spawned = std::thread::Builder::new().name("gpu stats".into()).spawn(move || {
        let counters = Counters::open();
        if counters.is_none() {
            log!("GPU performance counters unavailable");
        }
        let mut infos = gpus();
        let mut listed = Instant::now();
        let mut history: HashMap<String, VecDeque<f32>> = HashMap::new();
        let mut first = true;
        while !stop.stopped() {
            let started = Instant::now();
            // GPUs come and go (eGPUs, driver updates): list them again now and then.
            if listed.elapsed() > Duration::from_secs(60) {
                infos = gpus();
                listed = Instant::now();
            }
            let (usage, dedicated, shared_mem) = match &counters {
                Some(c) if c.collect() => (
                    usage_by_adapter(&Counters::values(c.engines)),
                    sum_by_adapter(&Counters::values(c.dedicated)),
                    sum_by_adapter(&Counters::values(c.shared)),
                ),
                _ => Default::default(),
            };
            let readings: Vec<Gpu> = infos
                .iter()
                .map(|g| {
                    let u = (usage.get(&g.luid).copied().unwrap_or(0.0) / 100.0).clamp(0.0, 1.0) as f32;
                    let h = history.entry(g.id.clone()).or_default();
                    // The first collection has no rates yet.
                    if !first {
                        push(h, u);
                    }
                    let used = if g.dedicated { &dedicated } else { &shared_mem };
                    Gpu {
                        id: g.id.clone(),
                        name: g.name.clone(),
                        vendor: g.vendor,
                        usage: u,
                        history: h.clone(),
                        mem_used: used.get(&g.luid).copied().unwrap_or(0.0) as u64,
                        mem_total: g.mem_total,
                        dedicated: g.dedicated,
                        temp: gpu_temp(g.luid),
                    }
                })
                .collect();
            first = false;
            if started.elapsed() > Duration::from_millis(250) {
                log!("GPU stats took {:?}", started.elapsed());
            }
            if let Ok(mut s) = shared.lock() {
                *s = readings;
            }
            let until = Instant::now() + GPU_EVERY;
            while !stop.stopped() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    });
    if let Err(e) = spawned {
        log!("GPU stats thread failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// What's drawn
// ---------------------------------------------------------------------------

/// The colour a reading is drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tint {
    Accent,
    Purple,
    Orange,
    Green,
    Teal,
    Pink,
}

/// One sensor, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// Short caps label under a gauge ("CPU", "NVIDIA MEM", "DISK D:").
    pub label: String,
    /// 0..1 for the ring.
    pub frac: f32,
    /// In the ring ("34%", "61°", "–").
    pub value: String,
    /// A line under the label, where there's room.
    pub detail: Option<String>,
    pub tint: Tint,
    /// Graphable readings' history.
    pub history: Option<Vec<f32>>,
    /// Network: (down, up) bytes per second.
    pub net: Option<(f64, f64)>,
}

impl Reading {
    pub fn is_net(&self) -> bool {
        self.net.is_some()
    }
}

fn pct(f: f32) -> String {
    format!("{:.0}%", f * 100.0)
}

/// What to draw for `sensors`: hardware that isn't there (yet) is left out.
pub fn readings(sensors: &[Sensor], s: &SysStats) -> Vec<Reading> {
    let first_gpu = s.gpus.first().map(|g| g.id.as_str());
    let sensors: Vec<Sensor> = sensors.iter().cloned().map(|x| canonical(x, first_gpu)).collect();
    let disk_count = sensors.iter().filter(|x| matches!(x, Sensor::Disk(_))).count();
    // GPUs are named by vendor when there's more than one.
    let gpu_tag = |g: &Gpu| {
        if s.gpus.len() <= 1 {
            return "GPU".to_string();
        }
        let vendor = vendor_name(g.vendor).map(str::to_uppercase);
        let twins = s.gpus.iter().filter(|o| o.vendor == g.vendor).count();
        match vendor {
            Some(v) if twins == 1 => v,
            _ => format!("GPU {}", s.gpus.iter().position(|o| o.id == g.id).unwrap_or(0) + 1),
        }
    };
    let gauge = |label: String, frac: f32, detail: Option<String>, tint: Tint| Reading {
        label,
        frac: frac.clamp(0.0, 1.0),
        value: pct(frac),
        detail,
        tint,
        history: None,
        net: None,
    };
    let mut out = Vec::new();
    for sensor in &sensors {
        let gpu = sensor.gpu().and_then(|id| s.gpus.iter().find(|g| Some(&g.id) == id.as_ref()));
        let r = match sensor {
            Sensor::Cpu => Reading { history: Some(s.cpu_history.iter().copied().collect()), ..gauge("CPU".into(), s.cpu, None, Tint::Accent) },
            Sensor::Memory => gauge(
                "MEMORY".into(),
                s.mem,
                Some(format!("{} of {}", super::sources::gigabytes(s.mem_used), super::sources::gigabytes(s.mem_total))),
                Tint::Purple,
            ),
            Sensor::Disk(d) => {
                let Some(disk) = s.disks.iter().find(|x| Some(x.letter) == *d) else { continue };
                let label = if disk_count > 1 { format!("DISK {}:", disk.letter) } else { "DISK".into() };
                gauge(label, disk.used, Some(format!("{} free", super::sources::gigabytes(disk.free))), Tint::Orange)
            }
            Sensor::Network(name) => {
                let (label, rates) = match name {
                    None => ("Network".to_string(), (s.down, s.up)),
                    Some(n) => match s.nets.iter().find(|x| x.name.eq_ignore_ascii_case(n)) {
                        Some(x) => (x.name.clone(), (x.down, x.up)),
                        // Disconnected: nothing moving.
                        None => (n.clone(), (0.0, 0.0)),
                    },
                };
                Reading { label, frac: 0.0, value: String::new(), detail: None, tint: Tint::Green, history: None, net: Some(rates) }
            }
            Sensor::Gpu(_) => {
                let Some(g) = gpu else { continue };
                Reading { history: Some(g.history.iter().copied().collect()), ..gauge(gpu_tag(g), g.usage, Some(g.name.clone()), Tint::Green) }
            }
            Sensor::GpuMemory(_) => {
                let Some(g) = gpu else { continue };
                let frac = g.mem_used as f32 / g.mem_total.max(1) as f32;
                let detail = format!("{} of {}", super::sources::gigabytes(g.mem_used), super::sources::gigabytes(g.mem_total));
                gauge(format!("{} MEM", gpu_tag(g)), frac, Some(detail), Tint::Teal)
            }
            Sensor::GpuTemp(_) => {
                let Some(g) = gpu else { continue };
                let label = format!("{} TEMP", gpu_tag(g));
                match g.temp {
                    Some(t) => Reading { value: format!("{t:.0}°"), ..gauge(label, t / 100.0, None, Tint::Pink) },
                    None => Reading { value: "–".into(), ..gauge(label, 0.0, Some("Not reported".into()), Tint::Pink) },
                }
            }
        };
        out.push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        for k in ["cpu", "memory", "disk", "disk:D", "network", "network:Wi-Fi", "gpu", "gpu:10de-2504", "gpu_memory:8086-9a49#2", "gpu_temp:10de-2504"] {
            assert_eq!(Sensor::parse(k).unwrap().key(), k);
        }
        assert_eq!(Sensor::parse("disk:d"), Some(Sensor::Disk(Some('D'))));
        assert_eq!(Sensor::parse(" RAM "), Some(Sensor::Memory));
        assert_eq!(Sensor::parse("vram:10DE-2504"), Some(Sensor::GpuMemory(Some("10de-2504".into()))));
        assert!(Sensor::parse("fan").is_none());
    }

    #[test]
    fn defaults_are_not_stored() {
        let list: Vec<Sensor> = DEFAULTS.iter().filter_map(|k| Sensor::parse(k)).collect();
        assert_eq!(stored(&list, Some("8086-9a49")), None);
        let mut more = list.clone();
        more.push(Sensor::Gpu(Some("8086-9a49".into())));
        assert_eq!(stored(&more, Some("8086-9a49")).unwrap().last().unwrap(), "gpu:8086-9a49");
    }

    #[test]
    fn counter_instances() {
        let n = "pid_1234_luid_0x00000000_0x0000D1B4_phys_0_eng_3_engtype_VideoDecode";
        assert_eq!(instance_luid(n), Some(0xD1B4));
        assert_eq!(instance_engine(n), Some("luid_0x00000000_0x0000D1B4_phys_0_eng_3"));
        let usage = usage_by_adapter(&[
            ("pid_1_luid_0x00000000_0x0000D1B4_phys_0_eng_0_engtype_3D".into(), 20.0),
            ("pid_2_luid_0x00000000_0x0000D1B4_phys_0_eng_0_engtype_3D".into(), 15.0),
            ("pid_2_luid_0x00000000_0x0000D1B4_phys_0_eng_3_engtype_VideoDecode".into(), 30.0),
            ("pid_2_luid_0x00000001_0x00000010_phys_0_eng_0_engtype_3D".into(), 5.0),
        ]);
        assert_eq!(usage[&0xD1B4], 35.0);
        assert_eq!(usage[&((1 << 32) | 0x10)], 5.0);
    }

    /// Prints what this machine has, to check the sources by eye.
    #[test]
    #[ignore]
    fn live_hardware() {
        println!("{:#?}", gpus());
        println!("{:#?}", drives());
        println!("{:#?}", adapters());
        if let Some(c) = Counters::open() {
            for _ in 0..3 {
                let t = Instant::now();
                c.collect();
                let n = Counters::values(c.engines).len();
                println!("GPU counters: {n} engine instances in {:?}", t.elapsed());
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        let shared = SharedGpus::default();
        let stop = Stop::default();
        watch_gpus(shared.clone(), stop.clone());
        std::thread::sleep(Duration::from_millis(4500));
        stop.stop();
        let mut s = Sampler::default();
        s.sample(&['C'], Some(&shared));
        std::thread::sleep(Duration::from_millis(500));
        s.sample(&['C'], Some(&shared));
        println!("{:#?}", s.stats);
    }
}
