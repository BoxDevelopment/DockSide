//! Bluetooth connect / disconnect, AirPods-style: a `DeviceWatcher` over
//! Bluetooth (classic + LE) association endpoints reports connection changes,
//! and the battery level comes from the matching PnP device's battery property.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use windows::core::{Interface, HSTRING};
use windows::Devices::Enumeration::{DeviceInformation, DeviceInformationKind, DeviceInformationUpdate, DeviceWatcher};
use windows::Foundation::Collections::{IIterable, IMapView};
use windows::Foundation::{IReference, TypedEventHandler};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::sources::{Inbox, Signal, Stop};

const CONNECTED: &str = "System.Devices.Aep.IsConnected";
/// DEVPKEY_Bluetooth_Battery
const BATTERY: &str = "{104EA319-6EE2-4701-BD47-8DDBF425BBE5} 2";
const AQS: &str = "System.Devices.Aep.ProtocolId:=\"{e0cbf06c-cd8b-4647-bb8a-263b43f0f974}\" OR \
                   System.Devices.Aep.ProtocolId:=\"{bb7bb05e-5972-42b5-94fc-76eaa7084d49}\"";

enum Ev {
    Added { id: String, name: String, connected: bool },
    Updated { id: String, connected: Option<bool> },
    Ready,
}

fn connected(props: windows::core::Result<IMapView<HSTRING, windows::core::IInspectable>>) -> Option<bool> {
    let v = props.ok()?.Lookup(&HSTRING::from(CONNECTED)).ok()?;
    v.cast::<IReference<bool>>().ok()?.Value().ok()
}

pub fn watch(inbox: Inbox, stop: Stop) {
    let spawned = std::thread::Builder::new().name("bluetooth".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (tx, rx) = channel::<Ev>();
        let watcher = match start(&tx) {
            Ok(w) => w,
            Err(e) => return log!("bluetooth watcher unavailable: {e}"),
        };
        let mut devices: HashMap<String, (String, bool)> = HashMap::new();
        let mut ready = false;
        while !stop.stopped() {
            let Ok(ev) = rx.recv_timeout(Duration::from_millis(500)) else { continue };
            match ev {
                Ev::Ready => ready = true,
                Ev::Added { id, name, connected } => {
                    devices.insert(id, (name, connected));
                }
                Ev::Updated { id, connected: Some(now) } => {
                    let Some((name, was)) = devices.get_mut(&id) else { continue };
                    if *was == now {
                        continue;
                    }
                    *was = now;
                    if !ready || name.is_empty() {
                        continue;
                    }
                    let (name, inbox) = (name.clone(), inbox.clone());
                    // Battery levels show up a moment after the connection; don't hold up the watcher.
                    std::thread::spawn(move || {
                        unsafe {
                            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                        }
                        let mut battery = None;
                        if now {
                            for wait in [0, 1500, 2500] {
                                std::thread::sleep(Duration::from_millis(wait));
                                battery = battery_of(&name);
                                if battery.is_some() {
                                    break;
                                }
                            }
                        }
                        inbox.push(Signal::Bluetooth { name, connected: now, battery });
                    });
                }
                Ev::Updated { .. } => {}
            }
        }
        let _ = watcher.Stop();
    });
    if let Err(e) = spawned {
        log!("bluetooth thread failed: {e}");
    }
}

fn start(tx: &Sender<Ev>) -> windows::core::Result<DeviceWatcher> {
    let props: IIterable<HSTRING> = vec![HSTRING::from(CONNECTED)].try_into()?;
    let watcher = DeviceInformation::CreateWatcherWithKindAqsFilterAndAdditionalProperties(
        &HSTRING::from(AQS),
        &props,
        DeviceInformationKind::AssociationEndpoint,
    )?;
    let t = tx.clone();
    watcher.Added(&TypedEventHandler::new(move |_: &Option<DeviceWatcher>, info: &Option<DeviceInformation>| {
        if let Some(info) = info {
            let _ = t.send(Ev::Added {
                id: info.Id()?.to_string(),
                name: info.Name()?.to_string(),
                connected: connected(info.Properties()).unwrap_or(false),
            });
        }
        Ok(())
    }))?;
    let t = tx.clone();
    watcher.Updated(&TypedEventHandler::new(move |_: &Option<DeviceWatcher>, u: &Option<DeviceInformationUpdate>| {
        if let Some(u) = u {
            let _ = t.send(Ev::Updated { id: u.Id()?.to_string(), connected: connected(u.Properties()) });
        }
        Ok(())
    }))?;
    let t = tx.clone();
    watcher.EnumerationCompleted(&TypedEventHandler::new(
        move |_: &Option<DeviceWatcher>, _: &Option<windows::core::IInspectable>| {
            let _ = t.send(Ev::Ready);
            Ok(())
        },
    ))?;
    watcher.Start()?;
    Ok(watcher)
}

/// Battery percentage reported by the device's PnP node ("AirPods Hands-Free AG", ...).
fn battery_of(name: &str) -> Option<u8> {
    let props: IIterable<HSTRING> = vec![HSTRING::from(BATTERY)].try_into().ok()?;
    let list = DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(
        &HSTRING::from("System.Devices.Present:=System.StructuredQueryType.Boolean#True"),
        &props,
        DeviceInformationKind::Device,
    )
    .ok()?
    .get()
    .ok()?;
    list.into_iter().find_map(|d| {
        let dn = d.Name().ok()?.to_string();
        if !dn.starts_with(name) {
            return None;
        }
        let v = d.Properties().ok()?.Lookup(&HSTRING::from(BATTERY)).ok()?;
        v.cast::<IReference<u8>>().ok()?.Value().ok()
    })
}

/// Battery levels of the Bluetooth devices connected right now, by device name
/// (the Batteries widget). Blocking: call from a worker thread.
pub fn batteries() -> Vec<(String, u8)> {
    // Paired devices keep their PnP nodes (and last battery level) while away:
    // only list the ones connected now.
    let connected: Vec<String> = IIterable::<HSTRING>::try_from(vec![HSTRING::from(CONNECTED)])
        .and_then(|props| {
            let aqs = format!("({AQS}) AND {CONNECTED}:=System.StructuredQueryType.Boolean#True");
            DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(
                &HSTRING::from(aqs),
                &props,
                DeviceInformationKind::AssociationEndpoint,
            )?
            .get()
        })
        .map(|list| list.into_iter().filter_map(|d| d.Name().ok()).map(|n| n.to_string()).filter(|n| !n.is_empty()).collect())
        .unwrap_or_default();
    if connected.is_empty() {
        return Vec::new();
    }
    let Ok(props) = IIterable::<HSTRING>::try_from(vec![HSTRING::from(BATTERY)]) else { return Vec::new() };
    let list = DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(
        &HSTRING::from("System.Devices.Present:=System.StructuredQueryType.Boolean#True"),
        &props,
        DeviceInformationKind::Device,
    )
    .and_then(|op| op.get());
    let Ok(list) = list else { return Vec::new() };
    let mut out: Vec<(String, u8)> = Vec::new();
    for d in list {
        let level = d
            .Properties()
            .and_then(|p| p.Lookup(&HSTRING::from(BATTERY)))
            .ok()
            .and_then(|v| v.cast::<IReference<u8>>().ok())
            .and_then(|v| v.Value().ok());
        let (Some(level), Ok(name)) = (level, d.Name()) else { continue };
        // One device shows up as several nodes ("AirPods Hands-Free AG", "AirPods Stereo"...).
        let name = device_name(&name.to_string());
        let live = connected.iter().any(|c| name.starts_with(c.as_str()) || c.starts_with(name.as_str()));
        if live && !name.is_empty() && !out.iter().any(|(n, _)| *n == name) {
            out.push((name, level.min(100)));
        }
    }
    out
}

fn device_name(node: &str) -> String {
    const SUFFIXES: [&str; 5] = [" Hands-Free AG", " Hands-Free", " Stereo", " Avrcp Transport", " LE"];
    let mut name = node.trim();
    for s in SUFFIXES {
        name = name.strip_suffix(s).unwrap_or(name);
    }
    name.trim().to_string()
}
