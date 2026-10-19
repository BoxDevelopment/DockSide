//! Upcoming events from the Windows calendar (the accounts set up in the
//! Calendar / Outlook apps), re-read every few minutes on a worker thread.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::HSTRING;
use windows::ApplicationModel::Appointments::{
    AppointmentManager, AppointmentProperties, AppointmentStoreAccessType, FindAppointmentsOptions,
};
use windows::Foundation::{DateTime, TimeSpan};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::sources::{Inbox, Signal, Stop};

const REFRESH: Duration = Duration::from_secs(180);
/// 100ns ticks between 1601 and 1970.
const EPOCH_DIFF: i64 = 116_444_736_000_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct CalEvent {
    pub title: String,
    pub location: String,
    /// Unix seconds.
    pub start: i64,
    pub end: i64,
}

impl CalEvent {
    /// Seconds until the event starts (negative once it has begun).
    pub fn starts_in(&self) -> i64 {
        self.start - unix_now()
    }

    pub fn ongoing(&self) -> bool {
        let now = unix_now();
        self.start <= now && now < self.end
    }

    /// "in 5 min", "now", "at 3:30 PM".
    pub fn when(&self) -> String {
        let s = self.starts_in();
        if s <= 0 {
            "now".into()
        } else if s < 3600 {
            format!("in {} min", ((s + 59) / 60).max(1))
        } else {
            format!("at {}", self.clock())
        }
    }

    /// Local start time, e.g. "3:30 PM".
    pub fn clock(&self) -> String {
        chrono::DateTime::from_timestamp(self.start, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%-I:%M %p").to_string())
            .unwrap_or_default()
    }

    pub fn span(&self) -> String {
        let end = chrono::DateTime::from_timestamp(self.end, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%-I:%M %p").to_string())
            .unwrap_or_default();
        format!("{} – {end}", self.clock())
    }
}

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn watch(inbox: Inbox, stop: Stop) {
    let spawned = std::thread::Builder::new().name("calendar".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let store = match AppointmentManager::RequestStoreAsync(AppointmentStoreAccessType::AllCalendarsReadOnly)
            .and_then(|op| op.get())
        {
            Ok(s) => s,
            Err(e) => return log!("calendar unavailable: {e}"),
        };
        let mut last: Option<Vec<CalEvent>> = None;
        while !stop.stopped() {
            match fetch(&store) {
                Ok(events) => {
                    if last.as_ref() != Some(&events) {
                        inbox.push(Signal::Calendar(events.clone()));
                        last = Some(events);
                    }
                }
                Err(e) => log!("calendar read failed: {e}"),
            }
            // Sleep in short slices so a rebuild can stop us promptly.
            let until = std::time::Instant::now() + REFRESH;
            while !stop.stopped() && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
    if let Err(e) = spawned {
        log!("calendar thread failed: {e}");
    }
}

fn fetch(store: &windows::ApplicationModel::Appointments::AppointmentStore) -> windows::core::Result<Vec<CalEvent>> {
    let options = FindAppointmentsOptions::new()?;
    let props = options.FetchProperties()?;
    for p in [
        AppointmentProperties::Subject()?,
        AppointmentProperties::Location()?,
        AppointmentProperties::StartTime()?,
        AppointmentProperties::Duration()?,
        AppointmentProperties::AllDay()?,
        AppointmentProperties::IsCanceledMeeting()?,
    ] {
        props.Append(&p)?;
    }
    options.SetMaxCount(40)?;
    // From an hour ago (events in progress) to the end of tomorrow-ish.
    let start = DateTime { UniversalTime: EPOCH_DIFF + (unix_now() - 3600) * 10_000_000 };
    let length = TimeSpan { Duration: 30 * 3600 * 10_000_000 };
    let list = store.FindAppointmentsAsyncWithOptions(start, length, &options)?.get()?;
    let mut out = Vec::new();
    for a in list {
        if a.AllDay().unwrap_or(false) || a.IsCanceledMeeting().unwrap_or(false) {
            continue;
        }
        let s = (a.StartTime()?.UniversalTime - EPOCH_DIFF) / 10_000_000;
        let d = a.Duration()?.Duration / 10_000_000;
        let title = a.Subject().map(|h: HSTRING| h.to_string()).unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        out.push(CalEvent {
            title,
            location: a.Location().map(|h| h.to_string()).unwrap_or_default(),
            start: s,
            end: s + d.max(0),
        });
    }
    out.sort_by_key(|e| e.start);
    out.retain(|e| e.end > unix_now());
    Ok(out)
}
