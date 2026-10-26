//! Bluetooth connect / disconnect, AirPods-style: a `DeviceWatcher` over
//! Bluetooth (classic + LE) association endpoints reports connection changes,
//! and the battery level comes from the matching PnP device's battery property.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use windows::core::{Interface, GUID, HSTRING};
use windows::Devices::Enumeration::{DeviceInformation, DeviceInformationKind, DeviceInformationUpdate, DeviceWatcher};
use windows::Foundation::Collections::{IIterable, IMapView};
use windows::Foundation::{IReference, TypedEventHandler};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::sources::{Inbox, Signal, Stop};

const CONNECTED: &str = "System.Devices.Aep.IsConnected";
const AEP_CONTAINER: &str = "System.Devices.Aep.ContainerId";
const CONTAINER: &str = "System.Devices.ContainerId";
/// DEVPKEY_Bluetooth_Battery
const BATTERY: &str = "{104EA319-6EE2-4701-BD47-8DDBF425BBE5} 2";
const AQS: &str = "System.Devices.Aep.ProtocolId:=\"{e0cbf06c-cd8b-4647-bb8a-263b43f0f974}\" OR \
                   System.Devices.Aep.ProtocolId:=\"{bb7bb05e-5972-42b5-94fc-76eaa7084d49}\"";

enum Ev {
    Added { id: String, name: String, connected: bool, container: Option<String> },
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
        // Endpoint id -> (name, connected, container).
        let mut devices: HashMap<String, (String, bool, Option<String>)> = HashMap::new();
        let mut ready = false;
        while !stop.stopped() {
            let Ok(ev) = rx.recv_timeout(Duration::from_millis(500)) else { continue };
            match ev {
                Ev::Ready => ready = true,
                Ev::Added { id, name, connected, container } => {
                    devices.insert(id, (name, connected, container));
                }
                Ev::Updated { id, connected: Some(now) } => {
                    let Some((name, was, container)) = devices.get_mut(&id) else { continue };
                    if *was == now {
                        continue;
                    }
                    *was = now;
                    if !ready || name.is_empty() {
                        continue;
                    }
                    let (name, container, inbox) = (name.clone(), container.clone(), inbox.clone());
                    // Battery levels show up a moment after the connection; don't hold up the watcher.
                    std::thread::spawn(move || {
                        unsafe {
                            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                        }
                        let mut battery = None;
                        if let Some(container) = container.filter(|_| now) {
                            for wait in [0, 1500, 2500] {
                                std::thread::sleep(Duration::from_millis(wait));
                                battery = battery_of(&container);
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
    let props: IIterable<HSTRING> = vec![HSTRING::from(CONNECTED), HSTRING::from(AEP_CONTAINER)].try_into()?;
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
                container: container_of(info, AEP_CONTAINER),
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

/// Battery percentage of the device in `container`.
fn battery_of(container: &str) -> Option<u8> {
    levels(&[container.to_string()]).into_iter().next().map(|(_, level)| level)
}

/// `d`'s container (the physical device its endpoints and PnP nodes belong to), as AQS writes it.
fn container_of(d: &DeviceInformation, key: &str) -> Option<String> {
    let v = d.Properties().ok()?.Lookup(&HSTRING::from(key)).ok()?;
    let id = v.cast::<IReference<GUID>>().ok()?.Value().ok()?;
    Some(format!("{{{id:?}}}"))
}

fn find_all(aqs: &str, props: &[&str], kind: DeviceInformationKind) -> Vec<DeviceInformation> {
    let props: Vec<HSTRING> = props.iter().map(|p| HSTRING::from(*p)).collect();
    IIterable::<HSTRING>::try_from(props)
        .and_then(|props| DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(&HSTRING::from(aqs), &props, kind)?.get())
        .map(|list| list.into_iter().collect())
        .unwrap_or_default()
}

/// Battery levels of the devices in `containers`, by device name, read from just
/// their PnP nodes ("AirPods Hands-Free AG", "AirPods Stereo"...): one per device.
fn levels(containers: &[String]) -> Vec<(String, u8)> {
    let filter: Vec<String> = containers.iter().map(|c| format!("{CONTAINER}:=\"{c}\"")).collect();
    let aqs = format!("({}) AND System.Devices.Present:=System.StructuredQueryType.Boolean#True", filter.join(" OR "));
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<(String, u8)> = Vec::new();
    for d in find_all(&aqs, &[BATTERY, CONTAINER], DeviceInformationKind::Device) {
        let level = d
            .Properties()
            .and_then(|p| p.Lookup(&HSTRING::from(BATTERY)))
            .ok()
            .and_then(|v| v.cast::<IReference<u8>>().ok())
            .and_then(|v| v.Value().ok());
        let (Some(level), Some(container), Ok(name)) = (level, container_of(&d, CONTAINER), d.Name()) else { continue };
        let name = device_name(&name.to_string());
        if name.is_empty() || seen.iter().any(|c| c.eq_ignore_ascii_case(&container)) {
            continue;
        }
        seen.push(container);
        out.push((name, level.min(100)));
    }
    out
}

/// Battery levels of the Bluetooth devices connected right now, by device name
/// (the Batteries widget). Blocking: call from a worker thread.
pub fn batteries() -> Vec<(String, u8)> {
    // Paired devices keep their PnP nodes (and last battery level) while away: only
    // the ones connected now. Asked of device containers, not Bluetooth endpoints:
    // listing endpoints waits out a 30 second discovery scan.
    let aqs = "System.Devices.Paired:=System.StructuredQueryType.Boolean#True AND \
               System.Devices.Connected:=System.StructuredQueryType.Boolean#True";
    let containers: Vec<String> =
        find_all(aqs, &[], DeviceInformationKind::DeviceContainer).iter().filter_map(|d| d.Id().ok()).map(|id| id.to_string()).collect();
    if containers.is_empty() {
        return Vec::new();
    }
    levels(&containers)
}

fn device_name(node: &str) -> String {
    const SUFFIXES: [&str; 5] = [" Hands-Free AG", " Hands-Free", " Stereo", " Avrcp Transport", " LE"];
    let mut name = node.trim();
    for s in SUFFIXES {
        name = name.strip_suffix(s).unwrap_or(name);
    }
    name.trim().to_string()
}
