//! What the widgets show, gathered off the UI thread where it may block:
//!
//! * weather from Open-Meteo (place from the config, geocoded, or found by IP),
//!   refreshed every 15 minutes,
//! * Bluetooth device batteries, polled every minute,
//!
//! plus cheap readings taken on the UI thread: CPU / memory / disk / network
//! for the System widget, World Clock zones and regional preferences.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDate, Timelike};
use windows::core::HSTRING;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::island::sources::Stop;
use crate::win::{post, system};

const WEATHER_REFRESH: Duration = Duration::from_secs(15 * 60);
const WEATHER_RETRY: Duration = Duration::from_secs(2 * 60);
const DEVICES_REFRESH: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Weather
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Hour {
    /// "Now", "3PM" / "15".
    pub label: String,
    pub code: u8,
    pub is_day: bool,
    pub temp: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Day {
    /// "Today", "Tue".
    pub label: String,
    pub code: u8,
    pub low: f32,
    pub high: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Weather {
    pub place: String,
    /// The place was found automatically (shown with a location arrow).
    pub located: bool,
    pub temp: f32,
    pub code: u8,
    pub is_day: bool,
    pub high: f32,
    pub low: f32,
    pub hours: Vec<Hour>,
    pub days: Vec<Day>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum WeatherState {
    #[default]
    Loading,
    Ready(Weather),
    Failed(String),
}

/// WMO weather interpretation codes, as Open-Meteo reports them.
pub fn condition(code: u8, is_day: bool) -> &'static str {
    match code {
        0 if is_day => "Sunny",
        0 => "Clear",
        1 if is_day => "Mostly Sunny",
        1 => "Mostly Clear",
        2 => "Partly Cloudy",
        3 => "Cloudy",
        45 | 48 => "Foggy",
        51..=55 => "Drizzle",
        56 | 57 => "Freezing Drizzle",
        61 => "Light Rain",
        63 => "Rain",
        65 => "Heavy Rain",
        66 | 67 => "Freezing Rain",
        71 => "Light Snow",
        73 => "Snow",
        75 => "Heavy Snow",
        77 => "Flurries",
        80..=82 => "Showers",
        85 | 86 => "Snow Showers",
        95 => "Thunderstorms",
        96..=99 => "Hail",
        _ => "—",
    }
}

/// A blocking HTTPS GET through WinHTTP. (WinRT's HttpClient sits on WinINet,
/// which fails the TLS handshake with some servers, Open-Meteo's among them.)
fn http_get(url: &str) -> Result<String, String> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Networking::WinHttp::*;

    let rest = url.strip_prefix("https://").ok_or("not an https link")?;
    let (host, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let fail = |what: &str| format!("{what}: {}", windows::core::Error::from_win32());
    unsafe {
        let agent = HSTRING::from(format!("DockSide/{}", env!("CARGO_PKG_VERSION")));
        let session = WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0);
        if session.is_null() {
            return Err(fail("open"));
        }
        let _ = WinHttpSetTimeouts(session, 10_000, 10_000, 15_000, 15_000);
        let connect = WinHttpConnect(session, &HSTRING::from(host), INTERNET_DEFAULT_HTTPS_PORT, 0);
        let request = if connect.is_null() {
            std::ptr::null_mut()
        } else {
            WinHttpOpenRequest(connect, w!("GET"), &HSTRING::from(path), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), WINHTTP_FLAG_SECURE)
        };
        let result = (|| {
            if request.is_null() {
                return Err(fail("connect"));
            }
            WinHttpSendRequest(request, None, None, 0, 0, 0).map_err(|e| format!("send: {e}"))?;
            WinHttpReceiveResponse(request, std::ptr::null_mut()).map_err(|e| format!("receive: {e}"))?;
            let mut status = 0u32;
            let mut size = std::mem::size_of::<u32>() as u32;
            let _ = WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut std::ffi::c_void),
                &mut size,
                std::ptr::null_mut(),
            );
            let mut body = Vec::new();
            loop {
                let mut available = 0u32;
                WinHttpQueryDataAvailable(request, &mut available).map_err(|e| format!("read: {e}"))?;
                if available == 0 {
                    break;
                }
                let start = body.len();
                body.resize(start + available as usize, 0u8);
                let mut read = 0u32;
                WinHttpReadData(request, body[start..].as_mut_ptr() as *mut std::ffi::c_void, available, &mut read)
                    .map_err(|e| format!("read: {e}"))?;
                body.truncate(start + read as usize);
            }
            if status != 200 {
                return Err(format!("HTTP {status}"));
            }
            String::from_utf8(body).map_err(|e| e.to_string())
        })();
        for h in [request, connect, session] {
            if !h.is_null() {
                let _ = WinHttpCloseHandle(h);
            }
        }
        result
    }
}

fn get_json(url: &str) -> Result<serde_json::Value, String> {
    let body = http_get(url)?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Percent-encodes a query value.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// "51.5, -0.12" as coordinates.
fn parse_coords(s: &str) -> Option<(f64, f64)> {
    let (a, b) = s.split_once(',')?;
    let (lat, lon) = (a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// (latitude, longitude, place name, found automatically).
fn locate(setting: &str) -> Result<(f64, f64, String, bool), String> {
    let setting = setting.trim();
    if let Some((lat, lon)) = parse_coords(setting) {
        return Ok((lat, lon, String::new(), false));
    }
    if setting.is_empty() {
        let j = get_json("https://ipapi.co/json/")?;
        let lat = j["latitude"].as_f64().ok_or("no location")?;
        let lon = j["longitude"].as_f64().ok_or("no location")?;
        return Ok((lat, lon, j["city"].as_str().unwrap_or_default().to_string(), true));
    }
    // "Paris, France": search the name, prefer a result matching the rest.
    let (name, hint) = match setting.split_once(',') {
        Some((n, h)) => (n.trim(), h.trim().to_lowercase()),
        None => (setting, String::new()),
    };
    let url = format!("https://geocoding-api.open-meteo.com/v1/search?count=10&language=en&name={}", encode(name));
    let j = get_json(&url)?;
    let results = j["results"].as_array().ok_or_else(|| format!("couldn't find \"{setting}\""))?;
    let matches = |r: &serde_json::Value| {
        hint.is_empty()
            || ["country", "country_code", "admin1"]
                .iter()
                .any(|k| r[*k].as_str().is_some_and(|v| v.to_lowercase().starts_with(&hint)))
    };
    let r = results.iter().find(|r| matches(r)).or(results.first()).ok_or("no results")?;
    let lat = r["latitude"].as_f64().ok_or("no location")?;
    let lon = r["longitude"].as_f64().ok_or("no location")?;
    Ok((lat, lon, r["name"].as_str().unwrap_or(name).to_string(), false))
}

fn hour_label(t: &chrono::NaiveDateTime, h24: bool) -> String {
    if h24 {
        format!("{:02}", t.hour())
    } else {
        t.format("%-I%p").to_string()
    }
}

fn forecast(lat: f64, lon: f64, fahrenheit: bool, h24: bool) -> Result<Weather, String> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
         &current=temperature_2m,weather_code,is_day\
         &hourly=temperature_2m,weather_code,is_day\
         &daily=weather_code,temperature_2m_max,temperature_2m_min\
         &timezone=auto&forecast_days=7&temperature_unit={}",
        if fahrenheit { "fahrenheit" } else { "celsius" }
    );
    let j = get_json(&url)?;
    parse_forecast(&j, h24).ok_or_else(|| "unexpected forecast".to_string())
}

fn parse_forecast(j: &serde_json::Value, h24: bool) -> Option<Weather> {
    let num = |v: &serde_json::Value| v.as_f64().map(|f| f as f32);
    let cur = &j["current"];
    let now = chrono::NaiveDateTime::parse_from_str(cur["time"].as_str()?, "%Y-%m-%dT%H:%M").ok()?;
    let this_hour = now.with_minute(0)?;

    let h = &j["hourly"];
    let times = h["time"].as_array()?;
    let start = times.iter().position(|t| {
        t.as_str().and_then(|t| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M").ok()) == Some(this_hour)
    })?;
    let mut hours = Vec::new();
    for i in start..(start + 6).min(times.len()) {
        let t = chrono::NaiveDateTime::parse_from_str(times[i].as_str()?, "%Y-%m-%dT%H:%M").ok()?;
        hours.push(Hour {
            label: if i == start { "Now".into() } else { hour_label(&t, h24) },
            code: h["weather_code"][i].as_u64()? as u8,
            is_day: h["is_day"][i].as_u64()? == 1,
            temp: num(&h["temperature_2m"][i])?,
        });
    }

    let d = &j["daily"];
    let mut days = Vec::new();
    for (i, t) in d["time"].as_array()?.iter().enumerate().take(6) {
        let date = NaiveDate::parse_from_str(t.as_str()?, "%Y-%m-%d").ok()?;
        days.push(Day {
            label: if i == 0 { "Today".into() } else { date.format("%a").to_string() },
            code: d["weather_code"][i].as_u64()? as u8,
            low: num(&d["temperature_2m_min"][i])?,
            high: num(&d["temperature_2m_max"][i])?,
        });
    }
    let today = days.first()?;
    Some(Weather {
        place: String::new(),
        located: false,
        temp: num(&cur["temperature_2m"])?,
        code: cur["weather_code"].as_u64()? as u8,
        is_day: cur["is_day"].as_u64()? == 1,
        high: today.high,
        low: today.low,
        hours,
        days,
    })
}

pub type SharedWeather = Arc<Mutex<WeatherState>>;

/// Keeps `shared` up to date until `stop`, posting `msg` after every change.
pub fn watch_weather(shared: SharedWeather, stop: Stop, location: String, fahrenheit: bool, h24: bool, hwnd: isize, msg: u32) {
    let spawned = std::thread::Builder::new().name("weather".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let mut place: Option<(f64, f64, String, bool)> = None;
        while !stop.stopped() {
            if place.is_none() {
                match locate(&location) {
                    Ok(p) => place = Some(p),
                    Err(e) => log!("weather location failed: {e}"),
                }
            }
            let result = match &place {
                Some((lat, lon, name, located)) => forecast(*lat, *lon, fahrenheit, h24).map(|mut w| {
                    w.place = if name.is_empty() { "My Location".into() } else { name.clone() };
                    w.located = *located;
                    w
                }),
                None => Err("Location unavailable".into()),
            };
            let ok = result.is_ok();
            if let Ok(mut s) = shared.lock() {
                match result {
                    Ok(w) => *s = WeatherState::Ready(w),
                    Err(e) => {
                        log!("weather update failed: {e}");
                        // Keep showing the last forecast when a refresh fails.
                        if !matches!(*s, WeatherState::Ready(_)) {
                            *s = WeatherState::Failed(if place.is_none() { "Location unavailable".into() } else { "Weather unavailable".into() });
                        }
                    }
                }
            }
            post(hwnd, msg);
            let until = Instant::now() + if ok { WEATHER_REFRESH } else { WEATHER_RETRY };
            while !stop.stopped() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
    if let Err(e) = spawned {
        log!("weather thread failed: {e}");
    }
}

/// Fahrenheit for "f", or for "auto" when Windows measures in US units.
pub fn fahrenheit(setting: &str) -> bool {
    match setting.trim().to_lowercase().as_str() {
        "f" | "fahrenheit" => true,
        "c" | "celsius" => false,
        _ => system::intl_string("iMeasure").as_deref() == Some("1"),
    }
}

/// First day of the week from the Windows region (0 = Monday ... 6 = Sunday).
pub fn first_weekday() -> chrono::Weekday {
    match system::intl_string("iFirstDayOfWeek").and_then(|s| s.parse::<u8>().ok()) {
        Some(n @ 0..=6) => chrono::Weekday::try_from(n).unwrap_or(chrono::Weekday::Sun),
        _ => chrono::Weekday::Sun,
    }
}

// ---------------------------------------------------------------------------
// Bluetooth batteries
// ---------------------------------------------------------------------------

pub type SharedDevices = Arc<Mutex<Vec<(String, u8)>>>;

pub fn watch_devices(shared: SharedDevices, stop: Stop, hwnd: isize, msg: u32) {
    let spawned = std::thread::Builder::new().name("device batteries".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let mut last = None;
        while !stop.stopped() {
            let now = crate::island::bluetooth::batteries();
            if last.as_ref() != Some(&now) {
                if let Ok(mut s) = shared.lock() {
                    *s = now.clone();
                }
                last = Some(now);
                post(hwnd, msg);
            }
            let until = Instant::now() + DEVICES_REFRESH;
            while !stop.stopped() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
    if let Err(e) = spawned {
        log!("device battery thread failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// World clocks
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    pub label: String,
    pub tz: chrono_tz::Tz,
}

/// "Label=Area/City" or "Area/City" (labelled by the city).
pub fn parse_zone(s: &str) -> Option<Zone> {
    let (label, name) = match s.split_once('=') {
        Some((l, n)) => (Some(l.trim()), n.trim()),
        None => (None, s.trim()),
    };
    let tz: chrono_tz::Tz = name.parse().ok()?;
    let label = match label {
        Some(l) if !l.is_empty() => l.to_string(),
        _ => name.rsplit('/').next().unwrap_or(name).replace('_', " "),
    };
    Some(Zone { label, tz })
}

/// Time in a zone and how it relates to here: (h, m, s, "Today, +3HRS").
pub fn zone_time(z: &Zone) -> (u32, u32, u32, String) {
    let utc = chrono::Utc::now();
    let there = utc.with_timezone(&z.tz).naive_local();
    let here = utc.with_timezone(&chrono::Local).naive_local();
    let mins = (there - here).num_minutes();
    let day = match (there.date() - here.date()).num_days() {
        0 => "Today",
        1 => "Tomorrow",
        -1 => "Yesterday",
        _ => "",
    };
    let offset = if mins % 60 == 0 {
        format!("{:+}HRS", mins / 60)
    } else {
        format!("{:+}:{:02}", mins / 60, (mins % 60).abs())
    };
    (there.hour(), there.minute(), there.second(), format!("{day}, {offset}"))
}

// ---------------------------------------------------------------------------
// System stats
// ---------------------------------------------------------------------------

/// Samples kept for the CPU graph.
pub const HISTORY: usize = 60;

#[derive(Clone, Debug, Default)]
pub struct SysStats {
    /// 0..1
    pub cpu: f32,
    pub cpu_history: VecDeque<f32>,
    pub mem: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk: f32,
    pub disk_free: u64,
    /// Bytes per second.
    pub down: f64,
    pub up: f64,
}

#[derive(Default)]
pub struct Sampler {
    cpu: Option<(u64, u64)>,
    net: Option<(u64, u64, Instant)>,
    pub stats: SysStats,
}

fn ft(f: windows::Win32::Foundation::FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

impl Sampler {
    pub fn sample(&mut self) {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
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
                    s.cpu_history.push_back(s.cpu);
                    while s.cpu_history.len() > HISTORY {
                        s.cpu_history.pop_front();
                    }
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

        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()) + "\\";
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(&HSTRING::from(drive), None, Some(&mut total), Some(&mut free)) }.is_ok() && total > 0 {
            s.disk_free = free;
            s.disk = 1.0 - free as f32 / total as f32;
        }

        if let Some((rx, tx)) = net_octets() {
            let now = Instant::now();
            if let Some((rx0, tx0, t0)) = self.net {
                let secs = (now - t0).as_secs_f64().max(0.1);
                s.down = rx.saturating_sub(rx0) as f64 / secs;
                s.up = tx.saturating_sub(tx0) as f64 / secs;
            }
            self.net = Some((rx, tx, now));
        }
    }
}

/// Bytes received / sent so far over physical network adapters that are up.
fn net_octets() -> Option<(u64, u64)> {
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).is_err() || table.is_null() {
            return None;
        }
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        let (mut rx, mut tx) = (0u64, 0u64);
        for r in rows {
            // Bit 0: HardwareInterface (skips loopback, tunnels and virtual adapters).
            let hardware = r.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
            if hardware && r.OperStatus == IfOperStatusUp {
                rx += r.InOctets;
                tx += r.OutOctets;
            }
        }
        FreeMibTable(table as *const _);
        Some((rx, tx))
    }
}

/// "1.2 MB/s", "340 KB/s".
pub fn rate(bytes_per_sec: f64) -> String {
    let kb = bytes_per_sec / 1024.0;
    if kb < 1.0 {
        "0 KB/s".into()
    } else if kb < 1000.0 {
        format!("{kb:.0} KB/s")
    } else {
        format!("{:.1} MB/s", kb / 1024.0)
    }
}

/// "16 GB", "9.4 GB".
pub fn gigabytes(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 100.0 {
        format!("{gb:.0} GB")
    } else {
        format!("{gb:.1} GB")
    }
}

/// Days of `month` laid out in weeks starting on `first`: None for blanks.
pub fn month_weeks(day: NaiveDate, first: chrono::Weekday) -> Vec<[Option<u32>; 7]> {
    let start = day.with_day(1).unwrap_or(day);
    let lead = start.weekday().days_since(first) as usize;
    let days = {
        let next = if start.month() == 12 {
            NaiveDate::from_ymd_opt(start.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(start.year(), start.month() + 1, 1)
        };
        next.map_or(30, |n| (n - start).num_days() as usize)
    };
    let mut weeks = Vec::new();
    let mut week = [None; 7];
    for i in 0..lead + days {
        if i >= lead {
            week[i % 7] = Some((i - lead + 1) as u32);
        }
        if i % 7 == 6 {
            weeks.push(week);
            week = [None; 7];
        }
    }
    if week.iter().any(|d| d.is_some()) {
        weeks.push(week);
    }
    weeks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_parse() {
        let z = parse_zone("America/New_York").unwrap();
        assert_eq!(z.label, "New York");
        let z = parse_zone("Home = Europe/London").unwrap();
        assert_eq!(z.label, "Home");
        assert!(parse_zone("Mars/Olympus").is_none());
    }

    #[test]
    fn coords() {
        assert_eq!(parse_coords("51.5, -0.12"), Some((51.5, -0.12)));
        assert_eq!(parse_coords("London, UK"), None);
        assert_eq!(parse_coords("95,0"), None);
    }

    #[test]
    fn weeks_of_october_2026() {
        // October 1st 2026 is a Thursday.
        let d = NaiveDate::from_ymd_opt(2026, 10, 21).unwrap();
        let sun = month_weeks(d, chrono::Weekday::Sun);
        assert_eq!(sun[0], [None, None, None, None, Some(1), Some(2), Some(3)]);
        assert_eq!(sun.len(), 5);
        let mon = month_weeks(d, chrono::Weekday::Mon);
        assert_eq!(mon[0][3], Some(1));
        assert_eq!(mon.last().unwrap()[5], Some(31));
    }

    #[test]
    fn forecast_parses() {
        let j: serde_json::Value = serde_json::from_str(
            r#"{"current":{"time":"2026-10-21T14:15","temperature_2m":18.4,"weather_code":2,"is_day":1},
                "hourly":{"time":["2026-10-21T13:00","2026-10-21T14:00","2026-10-21T15:00"],
                          "temperature_2m":[17.0,18.0,19.0],"weather_code":[1,2,3],"is_day":[1,1,1]},
                "daily":{"time":["2026-10-21","2026-10-22"],"weather_code":[2,61],
                         "temperature_2m_max":[20.0,15.5],"temperature_2m_min":[10.0,9.0]}}"#,
        )
        .unwrap();
        let w = parse_forecast(&j, false).unwrap();
        assert_eq!(w.hours.len(), 2);
        assert_eq!(w.hours[0].label, "Now");
        assert_eq!(w.hours[1].label, "3PM");
        assert_eq!(w.days[1].label, "Thu");
        assert_eq!((w.high, w.low), (20.0, 10.0));
    }

    #[test]
    #[ignore]
    fn live_weather() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (lat, lon, name, _) = locate("").expect("located");
        println!("{name} {lat} {lon}");
        let w = forecast(lat, lon, false, true).expect("forecast");
        println!("{w:?}");
    }

    #[test]
    fn encodes_queries() {
        assert_eq!(encode("São Paulo"), "S%C3%A3o%20Paulo");
    }
}
