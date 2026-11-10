//! What Control Center reads and changes, on a worker thread of its own (radio,
//! WMI and scan calls can take a while): Wi-Fi, Bluetooth, Airplane Mode,
//! Energy Saver, Do Not Disturb, Night Light, Mobile Hotspot, Nearby Sharing,
//! brightness, audio outputs and the accessibility switches.
//!
//! Several of these have no public API. Those use what Windows' own Quick
//! Settings uses: the Radio Management service for Airplane Mode, the Quiet
//! Hours COM server for Do Not Disturb, the CloudStore blob for Night Light,
//! the CDP policy values for Nearby Sharing and the audio policy server for the
//! default output. Each falls back to the matching Settings page if it fails.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{w, Interface, BSTR, GUID, HSTRING, IUnknown, PCWSTR, PWSTR, VARIANT};
use windows::Devices::Enumeration::{DeviceInformation, DeviceInformationKind};
use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Devices::WiFi::{WiFiAccessStatus, WiFiAdapter, WiFiAvailableNetwork, WiFiConnectionStatus, WiFiReconnectionKind};
use windows::Foundation::Collections::IIterable;
use windows::Foundation::IReference;
use windows::Networking::Connectivity::{NetworkAuthenticationType, NetworkConnectivityLevel, NetworkInformation};
use windows::Networking::NetworkOperators::{NetworkOperatorTetheringManager, TetheringOperationalState};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Wmi::*;
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

use crate::win::system;

/// The on/off switches Control Center shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Toggle {
    Wifi,
    Bluetooth,
    Airplane,
    EnergySaver,
    Focus,
    NightLight,
    Hotspot,
    Nearby,
    Dark,
    Magnifier,
    Narrator,
    ColorFilters,
    LiveCaptions,
    StickyKeys,
    /// DockSide's own: personal details swapped for demo data.
    ScreenshotMode,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Network {
    pub ssid: String,
    pub bars: u8,
    pub secure: bool,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub name: String,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub id: String,
    pub name: String,
    pub default: bool,
}

/// A snapshot of everything Control Center shows. A switch missing from
/// `toggles` isn't available on this PC.
#[derive(Clone, Default)]
pub struct Status {
    /// The newest change request this snapshot already reflects.
    pub gen: u64,
    pub toggles: HashMap<Toggle, bool>,
    pub ssid: Option<String>,
    /// The built-in display's brightness (`None` without one).
    pub brightness: Option<u8>,
    /// `None` until the first scan finishes.
    pub networks: Option<Vec<Network>>,
    /// The scan couldn't run (no adapter, or Windows denied access).
    pub scan_failed: bool,
    pub devices: Option<Vec<Device>>,
    pub outputs: Option<Vec<Output>>,
    /// The default output's (level 0-1, muted).
    pub volume: Option<(f32, bool)>,
}

/// What the regular refresh reads.
#[derive(Clone, PartialEq)]
struct Reading {
    toggles: HashMap<Toggle, bool>,
    ssid: Option<String>,
    brightness: Option<u8>,
    volume: Option<(f32, bool)>,
}

pub enum Req {
    ScanWifi,
    ListDevices,
    ListOutputs,
    Set(Toggle, bool, u64),
    Brightness(u8, u64),
    Connect(String),
    SetOutput(String, u64),
}

/// The worker: it stops when this is dropped (the panel closed).
pub struct Worker {
    tx: Sender<Req>,
    shared: Arc<Mutex<Status>>,
}

impl Worker {
    /// Starts reading right away; `msg` is posted to `hwnd` after every update.
    pub fn start(hwnd: isize, msg: u32) -> Self {
        let (tx, rx) = channel();
        let shared: Arc<Mutex<Status>> = Arc::default();
        let s = shared.clone();
        let spawned = std::thread::Builder::new().name("control-center".into()).spawn(move || run(rx, s, hwnd, msg));
        if let Err(e) = spawned {
            log!("control center thread failed: {e}");
        }
        Self { tx, shared }
    }

    pub fn send(&self, r: Req) {
        let _ = self.tx.send(r);
    }

    pub fn snapshot(&self) -> Status {
        self.shared.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// What the worker keeps between requests (made once: several are
/// cross-process COM servers that are slow to start).
#[derive(Default)]
struct Ctx {
    radios: Vec<Radio>,
    adapter: Option<WiFiAdapter>,
    networks: HashMap<String, WiFiAvailableNetwork>,
    wmi: Option<IWbemServices>,
    radio_manager: Option<Raw>,
    /// The tethering manager for the current internet connection, and when it was found.
    tethering: Option<(std::time::Instant, Option<NetworkOperatorTetheringManager>)>,
    /// The last reading sent to the UI.
    last: Option<Reading>,
}

fn run(rx: Receiver<Req>, shared: Arc<Mutex<Status>>, hwnd: isize, msg: u32) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut ctx = Ctx::default();
    ctx.radios = radios();
    let publish = |f: &mut dyn FnMut(&mut Status)| {
        if let Ok(mut s) = shared.lock() {
            f(&mut s);
        }
        crate::win::post(hwnd, msg);
    };
    // Only news reaches the UI: an unchanged reading posts nothing (and draws nothing).
    let refresh = |ctx: &mut Ctx, gen: u64, force: bool| {
        let r = read_all(ctx);
        if ctx.last.as_ref() == Some(&r) && !force {
            return;
        }
        publish(&mut |s| {
            s.toggles = r.toggles.clone();
            s.ssid = r.ssid.clone();
            s.brightness = r.brightness;
            s.volume = r.volume;
            s.gen = s.gen.max(gen);
        });
        ctx.last = Some(r);
    };
    refresh(&mut ctx, 0, false);
    loop {
        // While open, states changed elsewhere (a hotkey, Settings) show up within a couple of seconds.
        let first = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                refresh(&mut ctx, 0, false);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // A dragged slider queues many levels: only the last one matters.
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let last_brightness = batch.iter().rposition(|r| matches!(r, Req::Brightness(..)));
        let mut changed = false;
        let mut gen = 0;
        for (i, req) in batch.into_iter().enumerate() {
            match req {
                Req::ScanWifi => {
                    let found = scan_wifi(&mut ctx);
                    publish(&mut |s| {
                        s.scan_failed = found.is_none();
                        s.networks = Some(found.clone().unwrap_or_default());
                    });
                }
                Req::ListDevices => {
                    let found = bluetooth_devices();
                    publish(&mut |s| s.devices = Some(found.clone()));
                }
                Req::ListOutputs => {
                    let found = outputs();
                    publish(&mut |s| s.outputs = Some(found.clone()));
                }
                Req::Set(t, on, g) => {
                    set(&mut ctx, t, on);
                    gen = gen.max(g);
                    changed = true;
                }
                Req::Brightness(level, g) => {
                    if Some(i) == last_brightness {
                        if let Err(e) = set_brightness(&mut ctx, level) {
                            log!("brightness change failed: {e}");
                        }
                    }
                    gen = gen.max(g);
                    changed = true;
                }
                Req::Connect(ssid) => {
                    connect(&ctx, &ssid);
                    let found = scan_wifi(&mut ctx);
                    publish(&mut |s| s.networks = Some(found.clone().unwrap_or_default()));
                    changed = true;
                }
                Req::SetOutput(id, g) => {
                    set_default_output(&id);
                    let found = outputs();
                    publish(&mut |s| s.outputs = Some(found.clone()));
                    gen = gen.max(g);
                }
            }
        }
        if changed {
            refresh(&mut ctx, gen, true);
        } else if gen > 0 {
            publish(&mut |s| s.gen = s.gen.max(gen));
        }
    }
}

fn read_all(ctx: &mut Ctx) -> Reading {
    let mut t = HashMap::new();
    for r in &ctx.radios {
        let kind = r.Kind().ok();
        let on = r.State().ok() == Some(RadioState::On);
        match kind {
            Some(RadioKind::WiFi) => {
                t.insert(Toggle::Wifi, on);
            }
            Some(RadioKind::Bluetooth) => {
                t.insert(Toggle::Bluetooth, on);
            }
            _ => {}
        }
    }
    if let Some(on) = airplane_mode(ctx) {
        t.insert(Toggle::Airplane, on);
    }
    t.insert(Toggle::EnergySaver, crate::win::controls::low_power_mode());
    if let Some(on) = crate::island::sources::do_not_disturb() {
        t.insert(Toggle::Focus, on);
    }
    if let Some(on) = night_light() {
        t.insert(Toggle::NightLight, on);
    }
    if let Some(on) = hotspot(ctx) {
        t.insert(Toggle::Hotspot, on);
    }
    if let Some(on) = nearby_sharing() {
        t.insert(Toggle::Nearby, on);
    }
    t.insert(Toggle::Dark, system::is_dark_mode());
    let running = running(&["magnify.exe", "narrator.exe", "livecaptions.exe"]);
    t.insert(Toggle::Magnifier, running[0]);
    t.insert(Toggle::Narrator, running[1]);
    t.insert(Toggle::LiveCaptions, running[2]);
    t.insert(Toggle::ColorFilters, reg_dword(HKEY_CURRENT_USER, w!("Software\\Microsoft\\ColorFiltering"), w!("Active")) == Some(1));
    t.insert(Toggle::StickyKeys, sticky_keys());
    let brightness = brightness(ctx);
    Reading { toggles: t, ssid: wifi_ssid(), brightness, volume: crate::island::sources::volume() }
}

fn set(ctx: &mut Ctx, t: Toggle, on: bool) {
    use crate::win::system::Shortcut;
    let ok = match t {
        Toggle::Wifi => set_radio(ctx, RadioKind::WiFi, on),
        Toggle::Bluetooth => set_radio(ctx, RadioKind::Bluetooth, on),
        Toggle::Airplane => set_airplane_mode(on),
        Toggle::EnergySaver => {
            crate::win::controls::set_low_power_mode(on);
            true
        }
        Toggle::Focus => set_do_not_disturb(on),
        Toggle::NightLight => set_night_light(on),
        Toggle::Hotspot => set_hotspot(ctx, on),
        Toggle::Nearby => set_nearby_sharing(on),
        // Applied on the UI thread (it rebuilds every surface).
        Toggle::Dark => true,
        // Applied on the UI thread (it's a DockSide setting).
        Toggle::ScreenshotMode => true,
        Toggle::Magnifier => {
            if on {
                system::open("magnify.exe");
            } else {
                // Win+Esc closes Magnifier.
                Shortcut::new("w", 0x1B, "Esc").send();
            }
            true
        }
        Toggle::Narrator => {
            Shortcut::new("cw", 0x0D, "Enter").send();
            true
        }
        Toggle::LiveCaptions => {
            Shortcut::new("cw", b'L' as u16, "L").send();
            true
        }
        Toggle::ColorFilters => set_color_filters(),
        Toggle::StickyKeys => set_sticky_keys(on),
    };
    if !ok {
        let page = match t {
            Toggle::Wifi => "ms-settings:network-wifi",
            Toggle::Bluetooth => "ms-settings:bluetooth",
            Toggle::Airplane => "ms-settings:network-airplanemode",
            Toggle::Focus => "ms-settings:notifications",
            Toggle::NightLight => "ms-settings:nightlight",
            Toggle::Hotspot => "ms-settings:network-mobilehotspot",
            Toggle::Nearby => "ms-settings:crossdevice",
            Toggle::ColorFilters => "ms-settings:easeofaccess-colorfilter",
            _ => "ms-settings:",
        };
        log!("control center: couldn't turn {t:?} {}; opening {page}", if on { "on" } else { "off" });
        system::open(page);
    }
}

// ---------------------------------------------------------------------------
// Radios (Wi-Fi, Bluetooth)
// ---------------------------------------------------------------------------

fn radios() -> Vec<Radio> {
    let get = || -> windows::core::Result<Vec<Radio>> {
        let _ = Radio::RequestAccessAsync()?.get();
        let list = Radio::GetRadiosAsync()?.get()?;
        (0..list.Size()?).map(|i| list.GetAt(i)).collect()
    };
    get().unwrap_or_else(|e| {
        log!("radios unavailable: {e}");
        Vec::new()
    })
}

fn set_radio(ctx: &mut Ctx, kind: RadioKind, on: bool) -> bool {
    let state = if on { RadioState::On } else { RadioState::Off };
    let mut done = false;
    for r in ctx.radios.iter().filter(|r| r.Kind().ok() == Some(kind)) {
        match r.SetStateAsync(state).and_then(|op| op.get()) {
            Ok(s) if s == windows::Devices::Radios::RadioAccessStatus::Allowed => done = true,
            Ok(s) => log!("radio change refused: {s:?}"),
            Err(e) => log!("radio change failed: {e}"),
        }
    }
    done
}

/// The SSID of the Wi-Fi network this PC is connected to.
fn wifi_ssid() -> Option<String> {
    let profiles = NetworkInformation::GetConnectionProfiles().ok()?;
    for i in 0..profiles.Size().ok()? {
        let Ok(p) = profiles.GetAt(i) else { continue };
        if !p.IsWlanConnectionProfile().unwrap_or(false) {
            continue;
        }
        if p.GetNetworkConnectivityLevel().ok() == Some(NetworkConnectivityLevel::None) {
            continue;
        }
        let ssid = p.WlanConnectionProfileDetails().and_then(|d| d.GetConnectedSsid()).map(|s| s.to_string()).unwrap_or_default();
        if !ssid.is_empty() {
            return Some(ssid);
        }
        // Windows hides the SSID from apps without location access; the profile is named after it.
        if let Ok(name) = p.ProfileName() {
            return Some(name.to_string());
        }
    }
    None
}

fn scan_wifi(ctx: &mut Ctx) -> Option<Vec<Network>> {
    let scan = |ctx: &mut Ctx| -> windows::core::Result<Vec<Network>> {
        if ctx.adapter.is_none() {
            let access = WiFiAdapter::RequestAccessAsync()?.get()?;
            if access != WiFiAccessStatus::Allowed {
                log!("wi-fi scan not allowed: {access:?}");
                return Err(windows::core::Error::empty());
            }
            let adapters = WiFiAdapter::FindAllAdaptersAsync()?.get()?;
            ctx.adapter = Some(adapters.GetAt(0)?);
        }
        let adapter = ctx.adapter.as_ref().unwrap();
        adapter.ScanAsync()?.get()?;
        let list = adapter.NetworkReport()?.AvailableNetworks()?;
        let current = wifi_ssid();
        let mut by_ssid: HashMap<String, (Network, WiFiAvailableNetwork)> = HashMap::new();
        for i in 0..list.Size()? {
            let n = list.GetAt(i)?;
            let ssid = n.Ssid()?.to_string();
            if ssid.trim().is_empty() {
                continue;
            }
            let bars = n.SignalBars().unwrap_or(0);
            let auth = n.SecuritySettings().and_then(|s| s.NetworkAuthenticationType()).unwrap_or(NetworkAuthenticationType::None);
            let secure = !matches!(auth, NetworkAuthenticationType::Open80211 | NetworkAuthenticationType::None);
            let connected = current.as_deref() == Some(ssid.as_str());
            if by_ssid.get(&ssid).is_none_or(|(old, _)| old.bars < bars) {
                by_ssid.insert(ssid.clone(), (Network { ssid, bars, secure, connected }, n));
            }
        }
        ctx.networks = by_ssid.iter().map(|(k, (_, n))| (k.clone(), n.clone())).collect();
        let mut out: Vec<Network> = by_ssid.into_values().map(|(n, _)| n).collect();
        out.sort_by(|a, b| b.connected.cmp(&a.connected).then(b.bars.cmp(&a.bars)).then(a.ssid.to_lowercase().cmp(&b.ssid.to_lowercase())));
        Ok(out)
    };
    match scan(ctx) {
        Ok(v) => Some(v),
        Err(e) => {
            if !e.message().is_empty() {
                log!("wi-fi scan failed: {e}");
            }
            None
        }
    }
}

/// Joins a network with its saved profile; anything that needs a password goes
/// to Windows' own network list, which can ask for one.
fn connect(ctx: &Ctx, ssid: &str) {
    let joined = (|| -> windows::core::Result<bool> {
        let (Some(a), Some(n)) = (&ctx.adapter, ctx.networks.get(ssid)) else { return Ok(false) };
        let r = a.ConnectAsync(n, WiFiReconnectionKind::Automatic)?.get()?;
        Ok(r.ConnectionStatus()? == WiFiConnectionStatus::Success)
    })();
    match joined {
        Ok(true) => {}
        other => {
            if let Err(e) = other {
                log!("wi-fi connect failed: {e}");
            }
            system::open("ms-availablenetworks:");
        }
    }
}

// ---------------------------------------------------------------------------
// Airplane Mode (the Radio Management service's IRadioManager)
// ---------------------------------------------------------------------------

const CLSID_RADIO_MANAGEMENT: GUID = GUID::from_u128(0x581333f6_28db_41be_bc7a_ff201f12f3f6);
const IID_RADIO_MANAGER: GUID = GUID::from_u128(0xdb3afbfb_08e6_46c6_aa70_bf9a34c30ab7);

/// An undocumented interface's method `slot` (counting IUnknown's three).
unsafe fn method<F: Copy>(obj: *mut c_void, slot: usize) -> F {
    let vtable = *(obj as *const *const *const c_void);
    std::mem::transmute_copy(&*vtable.add(slot))
}

/// `iid` of a new `clsid` object, as a raw pointer; released when the guard drops.
struct Raw(*mut c_void);

impl Drop for Raw {
    fn drop(&mut self) {
        unsafe {
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = method(self.0, 2);
            release(self.0);
        }
    }
}

fn create(clsid: &GUID, iid: &GUID, ctx: CLSCTX) -> Option<Raw> {
    unsafe {
        let unk: IUnknown = CoCreateInstance(clsid, None, ctx).map_err(|e| log!("CoCreateInstance {clsid:?}: {e}")).ok()?;
        let mut out = std::ptr::null_mut();
        unk.query(iid, &mut out).ok().map_err(|e| log!("QueryInterface {iid:?}: {e}")).ok()?;
        (!out.is_null()).then_some(Raw(out))
    }
}

fn airplane_mode(ctx: &mut Ctx) -> Option<bool> {
    if ctx.radio_manager.is_none() {
        ctx.radio_manager = create(&CLSID_RADIO_MANAGEMENT, &IID_RADIO_MANAGER, CLSCTX_LOCAL_SERVER);
    }
    let rm = ctx.radio_manager.as_ref()?;
    unsafe {
        let get: unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32, *mut i32) -> windows::core::HRESULT = method(rm.0, 5);
        let (mut enabled, mut b, mut c) = (0, 0, 0);
        if get(rm.0, &mut enabled, &mut b, &mut c).is_err() {
            // The service restarted: connect again next time.
            ctx.radio_manager = None;
            return None;
        }
        // "Radios enabled" is the opposite of Airplane Mode.
        Some(enabled == 0)
    }
}

fn set_airplane_mode(on: bool) -> bool {
    let Some(rm) = create(&CLSID_RADIO_MANAGEMENT, &IID_RADIO_MANAGER, CLSCTX_LOCAL_SERVER) else { return false };
    unsafe {
        let set: unsafe extern "system" fn(*mut c_void, i32) -> windows::core::HRESULT = method(rm.0, 6);
        set(rm.0, (!on) as i32).is_ok()
    }
}

// ---------------------------------------------------------------------------
// Do Not Disturb (the Quiet Hours settings server)
// ---------------------------------------------------------------------------

const CLSID_QUIET_HOURS: GUID = GUID::from_u128(0xf53321fa_34f8_4b7f_b9a3_361877cb94cf);
const IID_QUIET_HOURS_SETTINGS: GUID = GUID::from_u128(0x6bff4732_81ec_4ffb_ae67_b6c1bc29631f);

fn set_do_not_disturb(on: bool) -> bool {
    let Some(q) = create(&CLSID_QUIET_HOURS, &IID_QUIET_HOURS_SETTINGS, CLSCTX_LOCAL_SERVER) else { return false };
    let profile = if on { w!("Microsoft.QuietHoursProfile.PriorityOnly") } else { w!("Microsoft.QuietHoursProfile.Unrestricted") };
    unsafe {
        let put: unsafe extern "system" fn(*mut c_void, PCWSTR) -> windows::core::HRESULT = method(q.0, 4);
        match put(q.0, profile).ok() {
            Ok(()) => true,
            Err(e) => {
                log!("do not disturb change failed: {e}");
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Night Light (CloudStore state blob)
// ---------------------------------------------------------------------------

const NIGHT_LIGHT_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\CloudStore\\Store\\DefaultAccount\\Current\\default$windows.data.bluelightreduction.bluelightreductionstate\\windows.data.bluelightreduction.bluelightreductionstate");

fn night_light_blob() -> Option<Vec<u8>> {
    let mut size = 0u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, NIGHT_LIGHT_KEY, w!("Data"), RRF_RT_REG_BINARY, None, None, Some(&mut size)).ok().ok()?;
        let mut buf = vec![0u8; size as usize];
        RegGetValueW(HKEY_CURRENT_USER, NIGHT_LIGHT_KEY, w!("Data"), RRF_RT_REG_BINARY, None, Some(buf.as_mut_ptr() as *mut c_void), Some(&mut size))
            .ok()
            .ok()?;
        buf.truncate(size as usize);
        Some(buf)
    }
}

fn night_light() -> Option<bool> {
    let d = night_light_blob()?;
    (d.len() > 24).then(|| d[18] == 0x15)
}

/// The blob is a serialized record: "on" adds a two-byte field after byte 22
/// and changes byte 18 from 0x13 to 0x15. Bumping the timestamp (bytes 10-14)
/// makes Windows pick the change up.
fn set_night_light(on: bool) -> bool {
    let Some(mut d) = night_light_blob() else { return false };
    if d.len() < 26 {
        return false;
    }
    let is_on = d[18] == 0x15;
    if is_on == on {
        return true;
    }
    if on {
        d[18] = 0x15;
        d.splice(23..23, [0x10, 0x00]);
    } else {
        d[18] = 0x13;
        if d[23] != 0x10 {
            return false;
        }
        d.drain(23..25);
    }
    if let Some(b) = d[10..15].iter_mut().find(|b| **b != 0xFF) {
        *b += 1;
    }
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, NIGHT_LIGHT_KEY, w!("Data"), REG_BINARY.0, Some(d.as_ptr() as *const c_void), d.len() as u32).is_ok()
    }
}

// ---------------------------------------------------------------------------
// Mobile Hotspot, Nearby Sharing
// ---------------------------------------------------------------------------

/// The tethering manager, looked up again every half minute (the internet connection can change).
fn tethering(ctx: &mut Ctx) -> Option<NetworkOperatorTetheringManager> {
    let fresh = ctx.tethering.as_ref().is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(30));
    if !fresh {
        let m = NetworkInformation::GetInternetConnectionProfile()
            .ok()
            .and_then(|p| NetworkOperatorTetheringManager::CreateFromConnectionProfile(&p).ok());
        ctx.tethering = Some((std::time::Instant::now(), m));
    }
    ctx.tethering.as_ref().and_then(|(_, m)| m.clone())
}

fn hotspot(ctx: &mut Ctx) -> Option<bool> {
    let state = tethering(ctx)?.TetheringOperationalState().ok()?;
    match state {
        TetheringOperationalState::On | TetheringOperationalState::InTransition => Some(true),
        TetheringOperationalState::Off => Some(false),
        _ => None,
    }
}

fn set_hotspot(ctx: &mut Ctx, on: bool) -> bool {
    let Some(m) = tethering(ctx) else { return false };
    let r = if on { m.StartTetheringAsync() } else { m.StopTetheringAsync() }.and_then(|op| op.get());
    match r.and_then(|r| r.Status()) {
        Ok(s) if s == windows::Networking::NetworkOperators::TetheringOperationStatus::Success => true,
        Ok(s) => {
            log!("hotspot change failed: {s:?}");
            false
        }
        Err(e) => {
            log!("hotspot change failed: {e}");
            false
        }
    }
}

const CDP_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\CDP");

fn reg_dword(root: HKEY, key: PCWSTR, name: PCWSTR) -> Option<u32> {
    let (mut v, mut size) = (0u32, 4u32);
    unsafe { RegGetValueW(root, key, name, RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut c_void), Some(&mut size)).ok().ok()? };
    Some(v)
}

fn set_reg_dword(root: HKEY, key: PCWSTR, name: PCWSTR, v: u32) -> bool {
    unsafe { RegSetKeyValueW(root, key, name, REG_DWORD.0, Some(&v as *const u32 as *const c_void), 4).is_ok() }
}

/// 0 is off; 1 "My devices only"; 2 "Everyone nearby".
fn nearby_sharing() -> Option<bool> {
    reg_dword(HKEY_CURRENT_USER, CDP_KEY, w!("NearShareChannelUserAuthzPolicy")).map(|v| v != 0)
}

fn set_nearby_sharing(on: bool) -> bool {
    // Quick Settings turns it on for your own devices.
    let v = if on { 1 } else { 0 };
    set_reg_dword(HKEY_CURRENT_USER, CDP_KEY, w!("NearShareChannelUserAuthzPolicy"), v)
        && set_reg_dword(HKEY_CURRENT_USER, CDP_KEY, w!("CdpSessionUserAuthzPolicy"), v)
}

// ---------------------------------------------------------------------------
// Accessibility
// ---------------------------------------------------------------------------

fn sticky_keys() -> bool {
    use windows::Win32::UI::Accessibility::{SKF_STICKYKEYSON, STICKYKEYS};
    use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETSTICKYKEYS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};
    let mut sk = STICKYKEYS { cbSize: std::mem::size_of::<STICKYKEYS>() as u32, ..Default::default() };
    unsafe {
        SystemParametersInfoW(SPI_GETSTICKYKEYS, sk.cbSize, Some(&mut sk as *mut _ as *mut c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).is_ok()
            && sk.dwFlags.0 & SKF_STICKYKEYSON.0 != 0
    }
}

fn set_sticky_keys(on: bool) -> bool {
    use windows::Win32::UI::Accessibility::{SKF_STICKYKEYSON, STICKYKEYS};
    use windows::Win32::UI::WindowsAndMessaging::*;
    let mut sk = STICKYKEYS { cbSize: std::mem::size_of::<STICKYKEYS>() as u32, ..Default::default() };
    unsafe {
        if SystemParametersInfoW(SPI_GETSTICKYKEYS, sk.cbSize, Some(&mut sk as *mut _ as *mut c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).is_err() {
            return false;
        }
        if on {
            sk.dwFlags.0 |= SKF_STICKYKEYSON.0;
        } else {
            sk.dwFlags.0 &= !SKF_STICKYKEYSON.0;
        }
        SystemParametersInfoW(SPI_SETSTICKYKEYS, sk.cbSize, Some(&mut sk as *mut _ as *mut c_void), SPIF_UPDATEINIFILE | SPIF_SENDCHANGE).is_ok()
    }
}

/// Color filters only switch from their shortcut (Win+Ctrl+C), which works
/// while the shortcut setting is on: turn that on for the moment it takes.
fn set_color_filters() -> bool {
    let key = w!("Software\\Microsoft\\ColorFiltering");
    let was = reg_dword(HKEY_CURRENT_USER, key, w!("HotkeyEnabled"));
    if was != Some(1) && !set_reg_dword(HKEY_CURRENT_USER, key, w!("HotkeyEnabled"), 1) {
        return false;
    }
    std::thread::sleep(Duration::from_millis(150));
    crate::win::system::Shortcut::new("cw", b'C' as u16, "C").send();
    if was != Some(1) {
        std::thread::sleep(Duration::from_millis(600));
        set_reg_dword(HKEY_CURRENT_USER, key, w!("HotkeyEnabled"), was.unwrap_or(0));
    }
    true
}

/// Whether each of `exes` (lowercase file names) is running.
fn running(exes: &[&str]) -> Vec<bool> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let mut out = vec![false; exes.len()];
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let name = crate::win::from_wide(&e.szExeFile).to_lowercase();
            if let Some(i) = exes.iter().position(|x| *x == name) {
                out[i] = true;
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

// ---------------------------------------------------------------------------
// Brightness (WMI, connected once per panel)
// ---------------------------------------------------------------------------

fn wmi(ctx: &mut Ctx) -> Option<&IWbemServices> {
    if ctx.wmi.is_none() {
        ctx.wmi = unsafe {
            (|| -> windows::core::Result<IWbemServices> {
                let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
                let services =
                    locator.ConnectServer(&BSTR::from("ROOT\\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)?;
                CoSetProxyBlanket(&services, 10, 0, PCWSTR::null(), RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE)?;
                Ok(services)
            })()
            .ok()
        };
    }
    ctx.wmi.as_ref()
}

unsafe fn first(services: &IWbemServices, query: &str) -> Option<IWbemClassObject> {
    let rows = services
        .ExecQuery(&BSTR::from("WQL"), &BSTR::from(query), WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY, None)
        .ok()?;
    let mut objs = [None];
    let mut n = 0u32;
    let _ = rows.Next(WBEM_INFINITE, &mut objs, &mut n);
    if n == 0 {
        None
    } else {
        objs[0].take()
    }
}

fn brightness(ctx: &mut Ctx) -> Option<u8> {
    let services = wmi(ctx)?;
    unsafe {
        let o = first(services, "SELECT CurrentBrightness FROM WmiMonitorBrightness")?;
        let mut v = VARIANT::default();
        o.Get(w!("CurrentBrightness"), 0, &mut v, None, None).ok()?;
        let level = u32::try_from(&v).ok().or_else(|| i32::try_from(&v).ok().map(|x| x as u32))?.min(100) as u8;
        Some(level.min(100))
    }
}

fn set_brightness(ctx: &mut Ctx, level: u8) -> windows::core::Result<()> {
    let Some(services) = wmi(ctx) else { return Ok(()) };
    unsafe {
        let Some(instance) = first(services, "SELECT * FROM WmiMonitorBrightnessMethods") else { return Ok(()) };
        let mut path = VARIANT::default();
        instance.Get(w!("__PATH"), 0, &mut path, None, None)?;
        let path = BSTR::try_from(&path)?;
        let mut class = None;
        services.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_GENERIC_FLAG_TYPE(0), None, Some(&mut class), None)?;
        let mut in_def = None;
        class.unwrap().GetMethod(w!("WmiSetBrightness"), 0, &mut in_def, std::ptr::null_mut())?;
        let params = in_def.unwrap().SpawnInstance(0)?;
        params.Put(w!("Timeout"), 0, &VARIANT::from(1i32), 0)?;
        params.Put(w!("Brightness"), 0, &VARIANT::from(level.min(100)), 0)?;
        services.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_GENERIC_FLAG_TYPE(0), None, &params, None, None)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bluetooth devices
// ---------------------------------------------------------------------------

fn bluetooth_devices() -> Vec<Device> {
    use windows::Devices::Enumeration::{DeviceInformationUpdate, DeviceWatcher};
    use windows::Foundation::TypedEventHandler;
    const AQS: &str = "(System.Devices.Aep.ProtocolId:=\"{e0cbf06c-cd8b-4647-bb8a-263b43f0f974}\" OR                        System.Devices.Aep.ProtocolId:=\"{bb7bb05e-5972-42b5-94fc-76eaa7084d49}\") AND                        System.Devices.Aep.IsPaired:=System.StructuredQueryType.Boolean#True";
    const CONNECTED: &str = "System.Devices.Aep.IsConnected";
    enum Ev {
        Added(String, String, bool),
        Done,
    }
    // FindAllAsync waits out a 30-second radio inquiry; a watcher reports the
    // paired devices Windows already knows about at once.
    let (tx, rx) = channel::<Ev>();
    let watcher = (|| -> windows::core::Result<DeviceWatcher> {
        let props: IIterable<HSTRING> = vec![HSTRING::from(CONNECTED)].try_into()?;
        let w = DeviceInformation::CreateWatcherWithKindAqsFilterAndAdditionalProperties(
            &HSTRING::from(AQS),
            &props,
            DeviceInformationKind::AssociationEndpoint,
        )?;
        let t = tx.clone();
        w.Added(&TypedEventHandler::new(move |_: &Option<DeviceWatcher>, d: &Option<DeviceInformation>| {
            if let Some(d) = d {
                let connected = d
                    .Properties()
                    .and_then(|p| p.Lookup(&HSTRING::from(CONNECTED)))
                    .and_then(|v| v.cast::<IReference<bool>>()?.Value())
                    .unwrap_or(false);
                let _ = t.send(Ev::Added(d.Id()?.to_string(), d.Name()?.to_string(), connected));
            }
            Ok(())
        }))?;
        w.Updated(&TypedEventHandler::new(|_: &Option<DeviceWatcher>, _: &Option<DeviceInformationUpdate>| Ok(())))?;
        let t = tx.clone();
        w.EnumerationCompleted(&TypedEventHandler::new(move |_: &Option<DeviceWatcher>, _| {
            let _ = t.send(Ev::Done);
            Ok(())
        }))?;
        w.Start()?;
        Ok(w)
    })();
    let watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            log!("bluetooth devices unavailable: {e}");
            return Vec::new();
        }
    };
    let mut out: Vec<Device> = Vec::new();
    let mut ids = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    // Cached devices come in a burst: stop once it's over (or at the deadline).
    let mut quiet = Duration::from_millis(1500);
    while let Ok(ev) = rx.recv_timeout(quiet.min(deadline.saturating_duration_since(std::time::Instant::now()))) {
        match ev {
            Ev::Done => break,
            Ev::Added(id, name, connected) => {
                quiet = Duration::from_millis(300);
                if name.trim().is_empty() || !ids.insert(id) {
                    continue;
                }
                // Classic and LE endpoints of one device: list it once.
                match out.iter_mut().find(|o| o.name == name) {
                    Some(o) => o.connected |= connected,
                    None => out.push(Device { name, connected }),
                }
            }
        }
    }
    let _ = watcher.Stop();
    out.sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out
}

// ---------------------------------------------------------------------------
// Audio outputs
// ---------------------------------------------------------------------------

const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY =
    PROPERTYKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 14 };

fn outputs() -> Vec<Output> {
    unsafe {
        let list = (|| -> windows::core::Result<Vec<Output>> {
            let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let default = devices.GetDefaultAudioEndpoint(eRender, eConsole).and_then(|d| take_id(d.GetId()?)).ok();
            let all = devices.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut out = Vec::new();
            for i in 0..all.GetCount()? {
                let d = all.Item(i)?;
                let id = take_id(d.GetId()?)?;
                let store = d.OpenPropertyStore(STGM_READ)?;
                let value = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME)?;
                let name = take_id(PropVariantToStringAlloc(&value)?)?;
                out.push(Output { default: default.as_deref() == Some(id.as_str()), id, name });
            }
            Ok(out)
        })();
        list.unwrap_or_else(|e| {
            log!("audio outputs unavailable: {e}");
            Vec::new()
        })
    }
}

unsafe fn take_id(p: PWSTR) -> windows::core::Result<String> {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const c_void));
    Ok(s)
}

const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

/// Makes `id` the default output for every role, as the Sound settings do.
fn set_default_output(id: &str) {
    let Some(pc) = create(&CLSID_POLICY_CONFIG, &IID_POLICY_CONFIG, CLSCTX_ALL) else {
        system::open("ms-settings:sound");
        return;
    };
    let wide = HSTRING::from(id);
    unsafe {
        let set: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> windows::core::HRESULT = method(pc.0, 13);
        for role in [eConsole, eMultimedia, eCommunications] {
            if let Err(e) = set(pc.0, PCWSTR(wide.as_ptr()), role.0).ok() {
                log!("default output change failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads (never changes) every state on this PC: `cargo test probe -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn probe() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let mut ctx = Ctx { radios: radios(), ..Default::default() };
        let r = read_all(&mut ctx);
        let (ssid, brightness) = (r.ssid, r.brightness);
        let mut t: Vec<_> = r.toggles.into_iter().collect();
        t.sort_by_key(|(k, _)| format!("{k:?}"));
        println!("toggles: {t:?}\nssid: {ssid:?}\nbrightness: {brightness:?}");
        let q = create(&CLSID_QUIET_HOURS, &IID_QUIET_HOURS_SETTINGS, CLSCTX_LOCAL_SERVER).expect("quiet hours");
        unsafe {
            let get: unsafe extern "system" fn(*mut c_void, *mut PWSTR) -> windows::core::HRESULT = method(q.0, 3);
            let mut s = PWSTR::null();
            println!("quiet hours profile: {:?} {:?}", get(q.0, &mut s), take_id(s).ok());
        }
        println!("outputs: {:?}", outputs());
        println!("bluetooth: {:?}", bluetooth_devices());
        println!("networks: {:?}", scan_wifi(&mut ctx).map(|v| v.len()));
    }

}
