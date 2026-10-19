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
