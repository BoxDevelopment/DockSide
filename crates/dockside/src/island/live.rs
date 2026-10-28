//! Live Activities from other apps and scripts.
//!
//! Windows has no Live Activities of its own, so DockSide takes them from
//! anything that asks: `dockside activity start build --title "Build"
//! --progress 40%`, or a WM_COPYDATA message (tag [`COPYDATA_TAG`]) to the
//! window of class `DockSideSurface` carrying the same thing as JSON:
//!
//! ```json
//! {"op": "start", "id": "build", "title": "Build", "subtitle": "Compiling dockside",
//!  "progress": 0.4, "value": "4/10", "timer": 300, "stopwatch": false,
//!  "color": "#34C759", "icon": "C:\\path\\to\\icon.png", "open": "https://ci.example.com"}
//! ```
//!
//! `op` is `start`, `update` or `end`; only `id` is required. `update` keeps
//! the fields it doesn't mention, and `end` (optionally with a `subtitle`, such
//! as "Finished") removes the activity and shows that message for a moment.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, SendMessageTimeoutW, HWND_MESSAGE, SMTO_ABORTIFHUNG, WM_COPYDATA};

use crate::geom::Color;

/// Marks Live Activity WM_COPYDATA payloads.
pub const COPYDATA_TAG: usize = 0xAC71_0001;
/// Most activities kept at once; starting another drops the oldest.
const MAX: usize = 8;
/// An activity nobody has updated for this long is dropped (its app likely quit).
const STALE: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Clock {
    /// Counts down to `ends`.
    Timer { ends: Instant },
    /// Counts up from `since`.
    Stopwatch { since: Instant },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    /// Stable for the activity's life: the island's handle on it.
    pub key: u32,
    /// The sender's name for it.
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// 0..1.
    pub progress: Option<f32>,
    /// Short text for the right of the compact pill ("4/10", "ETA 2m").
    pub value: String,
    pub clock: Option<Clock>,
    pub color: Option<Color>,
    pub icon: Option<PathBuf>,
    /// Opened on click: a URL, file, app path or AppUserModelID.
    pub open: Option<String>,
    updated: Instant,
}

impl Activity {
    /// The time a clock shows, in seconds (counting down for a timer).
    pub fn clock_secs(&self) -> Option<f32> {
        Some(match self.clock? {
            Clock::Timer { ends } => ends.saturating_duration_since(Instant::now()).as_secs_f32(),
            Clock::Stopwatch { since } => since.elapsed().as_secs_f32(),
        })
    }

    /// What the compact pill shows on the right.
    pub fn trailing(&self) -> String {
        if !self.value.is_empty() {
            return self.value.clone();
        }
        if let Some(s) = self.clock_secs() {
            return super::fmt_time(s);
        }
        self.progress.map(|p| format!("{:.0}%", p * 100.0)).unwrap_or_default()
    }
}

/// What a message did.
#[derive(Debug, PartialEq)]
pub enum Change {
    Started(u32),
    Updated(u32),
    /// Removed; with the closing message to show, if it had one.
    Ended { title: String, message: String, color: Option<Color> },
    None,
}

#[derive(Default)]
pub struct Live {
    pub list: Vec<Activity>,
    next_key: u32,
}

fn text(m: &Map<String, Value>, key: &str) -> Option<String> {
    m.get(key).and_then(Value::as_str).map(|s| s.trim().to_string())
}

/// A progress value: 0.4, "40%", "0.4" or 40 (anything above 1 is a percentage).
fn progress(v: &Value) -> Option<f32> {
    let n = match v {
        Value::Number(n) => n.as_f64()? as f32,
        Value::String(s) if s.trim().ends_with('%') => s.trim().trim_end_matches('%').trim().parse::<f32>().ok()? / 100.0,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    let n = if n > 1.0 { n / 100.0 } else { n };
    n.is_finite().then(|| n.clamp(0.0, 1.0))
}

impl Live {
    pub fn get(&self, key: u32) -> Option<&Activity> {
        self.list.iter().find(|a| a.key == key)
    }

    /// Applies one JSON message.
    pub fn apply(&mut self, json: &str) -> Change {
        let Ok(Value::Object(m)) = serde_json::from_str::<Value>(json) else { return Change::None };
        let Some(id) = text(&m, "id").filter(|s| !s.is_empty()) else { return Change::None };
        let op = text(&m, "op").unwrap_or_else(|| "update".into()).to_lowercase();
        let found = self.list.iter().position(|a| a.id == id);
        if op == "end" {
            let Some(i) = found else { return Change::None };
            let a = self.list.remove(i);
            return Change::Ended { title: a.title, message: text(&m, "subtitle").unwrap_or_default(), color: a.color };
        }
        let (i, change) = match found {
            Some(i) => (i, Change::Updated(self.list[i].key)),
            None => {
                if self.list.len() >= MAX {
                    self.list.remove(0);
                }
                self.next_key += 1;
                self.list.push(Activity {
                    key: self.next_key,
                    id: id.clone(),
                    title: id.clone(),
                    subtitle: String::new(),
                    progress: None,
                    value: String::new(),
                    clock: None,
                    color: None,
                    icon: None,
                    open: None,
                    updated: Instant::now(),
                });
                (self.list.len() - 1, Change::Started(self.next_key))
            }
        };
        let a = &mut self.list[i];
        a.updated = Instant::now();
        if let Some(t) = text(&m, "title").filter(|t| !t.is_empty()) {
            a.title = t;
        }
        if let Some(s) = text(&m, "subtitle") {
            a.subtitle = s;
        }
        if let Some(s) = text(&m, "value") {
            a.value = s;
        }
        match m.get("progress") {
            Some(Value::Null) => a.progress = None,
            Some(v) => a.progress = progress(v).or(a.progress),
            None => {}
        }
        if let Some(secs) = m.get("timer").and_then(Value::as_f64).filter(|s| *s >= 0.0) {
            a.clock = Some(Clock::Timer { ends: Instant::now() + Duration::from_secs_f64(secs) });
        } else if m.get("stopwatch").and_then(Value::as_bool) == Some(true) && !matches!(a.clock, Some(Clock::Stopwatch { .. })) {
            a.clock = Some(Clock::Stopwatch { since: Instant::now() });
        } else if m.get("timer") == Some(&Value::Null) || m.get("stopwatch").and_then(Value::as_bool) == Some(false) {
            a.clock = None;
        }
        if let Some(c) = text(&m, "color") {
            a.color = Color::parse(&c);
        }
        if let Some(p) = text(&m, "icon") {
            a.icon = (!p.is_empty()).then(|| PathBuf::from(p));
        }
        if let Some(o) = text(&m, "open") {
            a.open = (!o.is_empty()).then_some(o);
        }
        change
    }

    /// Drops activities nobody updates any more; true when one went.
    pub fn prune(&mut self) -> bool {
        let before = self.list.len();
        self.list.retain(|a| a.updated.elapsed() < STALE);
        self.list.len() != before
    }
}

/// Reads a WM_COPYDATA payload sent by [`send`] (or any app following the format).
pub fn receive(lp: LPARAM) -> Option<String> {
    let cds = unsafe { (lp.0 as *const COPYDATASTRUCT).as_ref()? };
    if cds.dwData != COPYDATA_TAG || cds.lpData.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
    String::from_utf8(bytes.to_vec()).ok()
}

/// Hands `json` to the running DockSide; false when it isn't running.
fn send(json: &str) -> bool {
    unsafe {
        let Ok(target) = FindWindowExW(HWND_MESSAGE, None, crate::win::CLASS_NAME, None) else { return false };
        let cds = COPYDATASTRUCT { dwData: COPYDATA_TAG, cbData: json.len() as u32, lpData: json.as_ptr() as *mut _ };
        let mut result = 0usize;
        let sent = SendMessageTimeoutW(target, WM_COPYDATA, WPARAM(0), LPARAM(&cds as *const _ as isize), SMTO_ABORTIFHUNG, 1000, Some(&mut result));
        sent.0 != 0
    }
}

pub const USAGE: &str = "\
Usage: dockside activity <start|update|end> <id> [options]
       dockside activity send '<json>'

Options:
  --title TEXT       Name shown in the island
  --subtitle TEXT    Second line (for end: a closing message to show)
  --progress N       0..1, or a percentage such as 40%
  --value TEXT       Short text at the right of the pill (\"4/10\")
  --timer SECONDS    Count down from SECONDS
  --stopwatch        Count up from now
  --color #RRGGBB    Accent color
  --icon PATH        Image, .ico or app whose icon to show
  --open TARGET      URL, file or app to open when clicked";

/// Turns `activity …` arguments into the JSON message.
pub fn parse_args(args: &[String]) -> Result<String, String> {
    let mut it = args.iter();
    let op = it.next().ok_or("missing command")?.to_lowercase();
    if op == "send" {
        let raw = it.next().ok_or("missing JSON")?;
        return match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(_)) => Ok(raw.clone()),
            _ => Err("that isn't a JSON object".into()),
        };
    }
    if !matches!(op.as_str(), "start" | "update" | "end") {
        return Err(format!("unknown command '{op}'"));
    }
    let id = it.next().ok_or("missing activity id")?;
    let mut m = Map::new();
    m.insert("op".into(), json!(op));
    m.insert("id".into(), json!(id));
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--title" | "--subtitle" | "--value" | "--color" | "--open" | "--progress" => {
                m.insert(flag[2..].into(), json!(value()?));
            }
            "--icon" => {
                let v = value()?;
                // Relative to where the command ran, since DockSide's folder differs.
                let path = std::path::absolute(&v).unwrap_or_else(|_| PathBuf::from(&v));
                m.insert("icon".into(), json!(path.display().to_string()));
            }
            "--timer" => {
                let v = value()?;
                let secs: f64 = v.parse().map_err(|_| format!("--timer wants seconds, not '{v}'"))?;
                m.insert("timer".into(), json!(secs));
            }
            "--stopwatch" => {
                m.insert("stopwatch".into(), json!(true));
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    Ok(Value::Object(m).to_string())
}

/// `dockside activity …`: sends the message and returns the exit code.
pub fn cli(args: &[String]) -> i32 {
    // A GUI-subsystem exe has no console: borrow the one it was run from, for messages.
    unsafe {
        use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    let json = match parse_args(args) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("dockside activity: {e}\n\n{USAGE}");
            return 2;
        }
    };
    if send(&json) {
        0
    } else {
        eprintln!("dockside activity: DockSide isn't running");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn lifecycle() {
        let mut live = Live::default();
        let k = match live.apply(r#"{"op":"start","id":"build","title":"Build","progress":"40%"}"#) {
            Change::Started(k) => k,
            c => panic!("{c:?}"),
        };
        assert_eq!(live.get(k).unwrap().progress, Some(0.4));
        assert_eq!(live.get(k).unwrap().trailing(), "40%");
        assert_eq!(live.apply(r#"{"id":"build","progress":75,"value":"3/4"}"#), Change::Updated(k));
        let a = live.get(k).unwrap();
        assert_eq!((a.title.as_str(), a.progress, a.trailing().as_str()), ("Build", Some(0.75), "3/4"));
        assert_eq!(
            live.apply(r#"{"op":"end","id":"build","subtitle":"Finished"}"#),
            Change::Ended { title: "Build".into(), message: "Finished".into(), color: None }
        );
        assert!(live.list.is_empty());
        assert_eq!(live.apply(r#"{"op":"end","id":"build"}"#), Change::None);
        assert_eq!(live.apply("not json"), Change::None);
        assert_eq!(live.apply(r#"{"title":"no id"}"#), Change::None);
    }

    #[test]
    fn keeps_a_few() {
        let mut live = Live::default();
        for i in 0..MAX + 2 {
            live.apply(&format!(r#"{{"op":"start","id":"a{i}"}}"#));
        }
        assert_eq!(live.list.len(), MAX);
        assert_eq!(live.list[0].id, "a2");
    }

    #[test]
    fn command_line() {
        let j = parse_args(&args(&["start", "dl", "--title", "Download", "--progress", "0.5", "--stopwatch"])).unwrap();
        let v: Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["op"], "start");
        assert_eq!(v["title"], "Download");
        assert_eq!(v["stopwatch"], true);
        let mut live = Live::default();
        live.apply(&j);
        assert!(matches!(live.list[0].clock, Some(Clock::Stopwatch { .. })));
        assert!(parse_args(&args(&["start"])).is_err());
        assert!(parse_args(&args(&["start", "x", "--timer", "soon"])).is_err());
        assert!(parse_args(&args(&["start", "x", "--bogus"])).is_err());
        assert!(parse_args(&args(&["frobnicate", "x"])).is_err());
        assert!(parse_args(&args(&["send", "{\"id\":\"x\"}"])).is_ok());
        assert!(parse_args(&args(&["send", "[1]"])).is_err());
    }
}
