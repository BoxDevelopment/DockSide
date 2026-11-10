//! Screenshot Mode: personal details are swapped for believable demo data
//! (like Apple's marketing shots), so the desktop can be captured and shared.
//! Photo and Video widgets are left alone: what's in them is the user's choice.
//!
//! The switch lives in `[general] screenshot_mode`; the App mirrors it here so
//! every surface can ask `on()` while it paints.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use crate::island::calendar::{self, CalEvent};
use crate::island::discord::{Member, Snapshot, Voice};
use crate::island::media::NowPlaying;
use crate::island::notifications::Note;
use crate::widgets::sources::{self, Day, Hour, Weather, WeatherState, Zone};
use crate::widgets::user::UserInfo;

static ON: AtomicBool = AtomicBool::new(false);

/// Whether personal details are hidden right now.
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Turns it on or off; true when that changed something.
pub fn set(on: bool) -> bool {
    ON.swap(on, Ordering::Relaxed) != on
}

pub const NAME: &str = "John Appleseed";
pub const FIRST: &str = "John";
pub const PC: &str = "DESKTOP";
pub const SSID: &str = "Home Network";
pub const PLACE: &str = "Cupertino";

/// The song playing, keeping the real track's progress and controls.
pub fn now_playing(real: &NowPlaying) -> NowPlaying {
    NowPlaying { title: "Midnight City".into(), artist: "M83".into(), art: None, ..real.clone() }
}

/// A mild, partly cloudy day, in the unit the user reads.
pub fn weather(fahrenheit: bool, h24: bool) -> WeatherState {
    let t = |c: f32| if fahrenheit { c * 9.0 / 5.0 + 32.0 } else { c };
    let now = chrono::Local::now().naive_local();
    let hours = (0..6)
        .map(|i| {
            let at = now + chrono::Duration::hours(i);
            let label = if i == 0 {
                "Now".to_string()
            } else if h24 {
                at.format("%H").to_string()
            } else {
                at.format("%-I%p").to_string()
            };
            let hour = chrono::Timelike::hour(&at);
            Hour { label, code: [2, 2, 1, 0, 1, 2][i as usize], is_day: (7..19).contains(&hour), temp: t(18.0 + [0.0, 1.0, 1.0, 0.0, -1.0, -2.0][i as usize]) }
        })
        .collect();
    let today = chrono::Local::now().date_naive();
    let days = (0..6)
        .map(|i| Day {
            label: if i == 0 { "Today".into() } else { (today + chrono::Duration::days(i)).format("%a").to_string() },
            code: [2, 0, 1, 3, 61, 1][i as usize],
            low: t(11.0 + [0.0, 1.0, 2.0, 1.0, 0.0, 1.0][i as usize]),
            high: t(19.0 + [0.0, 2.0, 3.0, 1.0, -2.0, 0.0][i as usize]),
        })
        .collect();
    let hour = chrono::Timelike::hour(&now);
    WeatherState::Ready(Weather {
        place: PLACE.into(),
        located: true,
        temp: t(18.0),
        code: 2,
        is_day: (7..19).contains(&hour),
        high: t(19.0),
        low: t(11.0),
        hours,
        days,
    })
}

/// Meetings at believable hours: later today on the half hour, or (in the
/// evening) tomorrow morning.
pub fn events() -> Vec<CalEvent> {
    use chrono::{Duration as Days, Local, NaiveTime, TimeZone, Timelike};
    let now = Local::now();
    let at = |day: chrono::NaiveDate, h: u32, m: u32| {
        Local.from_local_datetime(&day.and_time(NaiveTime::from_hms_opt(h, m, 0).unwrap())).earliest().map_or(calendar::unix_now(), |t| t.timestamp())
    };
    let today = now.date_naive();
    let (first, second) = if (7..17).contains(&now.hour()) {
        let base = calendar::unix_now();
        let base = base - base.rem_euclid(1800) + 1800;
        (base, base + 2 * 3600)
    } else {
        let day = if now.hour() < 7 { today } else { today + Days::days(1) };
        (at(day, 10, 0), at(day, 12, 30))
    };
    let ev = |title: &str, location: &str, start: i64, mins: i64| CalEvent { title: title.into(), location: location.into(), start, end: start + mins * 60 };
    vec![
        ev("Team Sync", "", first, 30),
        ev("Lunch with Sam", "Caffè Macs", second, 60),
        ev("Design Review", "", at(today + Days::days(2), 14, 0), 45),
    ]
}

/// Bluetooth devices and their batteries.
pub fn devices() -> Vec<(String, u8)> {
    vec![("AirPods Pro".into(), 80), ("Magic Mouse".into(), 45), ("Magic Keyboard".into(), 90)]
}

/// One connected device's name.
pub fn device_name(i: usize) -> String {
    devices().get(i).map_or_else(|| format!("Bluetooth Device {}", i + 1), |d| d.0.clone())
}

/// A Wi-Fi network's name in the list (the connected one is always "Home Network").
pub fn network_name(i: usize, connected: bool) -> String {
    const NAMES: [&str; 6] = ["Coffee Shop", "Guest Wi-Fi", "Office", "Neighbor 5G", "Printer-Direct", "Library"];
    if connected { SSID.into() } else { NAMES.get(i).map_or_else(|| format!("Network {}", i + 1), |n| n.to_string()) }
}

/// Notifications, newest first (no app icons: those come from the real apps).
pub fn notes() -> Vec<Note> {
    let note = |id: u32, mins: u64, app: &str, title: &str, body: &str| Note {
        id,
        at: SystemTime::now() - Duration::from_secs(mins * 60),
        app: app.into(),
        app_id: String::new(),
        title: title.into(),
        body: body.into(),
        icon: None,
    };
    vec![
        note(1, 2, "Messages", "Sam", "Are we still on for lunch tomorrow?"),
        note(2, 14, "Calendar", "Team Sync", "Starts in 15 minutes"),
        note(3, 95, "Mail", "Weekly update", "Here's what the team shipped this week."),
    ]
}

/// Real notifications with their words swapped: same apps, count and order,
/// so rows still stack and dismiss the real ones.
pub fn mask_notes(real: &[Note]) -> Vec<Note> {
    let demo = notes();
    real.iter()
        .enumerate()
        .map(|(i, n)| {
            let d = &demo[i % demo.len()];
            Note { title: d.title.clone(), body: d.body.clone(), ..n.clone() }
        })
        .collect()
}

/// One notification's words swapped.
pub fn mask_note(real: &Note) -> Note {
    let d = &notes()[(real.id as usize) % 3];
    Note { title: d.title.clone(), body: d.body.clone(), ..real.clone() }
}

/// Real calendar events (their times, so the island still shows them when
/// they're due) with made-up titles and no places.
pub fn mask_events(real: &[CalEvent]) -> Vec<CalEvent> {
    const TITLES: [&str; 5] = ["Team Sync", "Lunch with Sam", "Design Review", "Focus Time", "Weekly Planning"];
    real.iter().enumerate().map(|(i, e)| CalEvent { title: TITLES[i % TITLES.len()].into(), location: String::new(), ..e.clone() }).collect()
}

/// The World Clock cities DockSide ships with.
pub fn zones() -> Vec<Zone> {
    ["America/Los_Angeles", "America/New_York", "Europe/London", "Asia/Tokyo"].iter().filter_map(|z| sources::parse_zone(z)).collect()
}

/// The signed-in account, without its picture (a silhouette is drawn instead).
pub fn user() -> UserInfo {
    UserInfo { name: NAME.into(), first: FIRST.into(), picture: None, pc: PC.into() }
}

/// Notes widget text.
pub const NOTE: &str = "Groceries\nMilk, eggs, coffee beans\nPick up the dry cleaning before 6pm.";

/// Reminders list.
pub const REMINDERS: [&str; 3] = ["Call the dentist", "Water the plants", "Book train tickets"];

/// A Discord voice call: same people count and states, made-up names, no pictures.
pub fn discord(real: &Snapshot) -> Snapshot {
    const NAMES: [&str; 6] = ["Alex", "Sam", "Jordan", "Riley", "Taylor", "Casey"];
    let voice = real.voice.as_ref().map(|v| Voice {
        channel_id: v.channel_id.clone(),
        channel: "General".into(),
        members: v
            .members
            .iter()
            .enumerate()
            .map(|(i, m)| Member { name: NAMES.get(i).map_or_else(|| format!("Friend {}", i + 1), |n| n.to_string()), avatar: None, ..m.clone() })
            .collect(),
    });
    Snapshot { voice, ..real.clone() }
}
