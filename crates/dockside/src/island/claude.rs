//! Claude Code on the island, through Claude Code hooks.
//!
//! "Connect Claude Code" (System Settings) adds hooks to Claude's
//! `settings.json` that run `dockside.exe --claude-hook` on each step: a prompt
//! sent, a tool starting / finishing, a permission request, the turn ending.
//! That short-lived relay reads the hook's JSON from stdin, keeps the few
//! fields the island needs, and hands them to the running DockSide with
//! WM_COPYDATA. The island turns them into a live activity plus
//! "finished" / "needs you" alerts.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, SendMessageTimeoutW, HWND_MESSAGE, SMTO_ABORTIFHUNG, WM_COPYDATA};

/// Marks our WM_COPYDATA payloads.
pub const COPYDATA_TAG: usize = 0xC1A0_DE01;
const HOOK_ARG: &str = "--claude-hook";
/// A session that stops reporting (Claude closed mid-turn) is dropped after this.
const STALE: Duration = Duration::from_secs(15 * 60);

const EVENTS: [(&str, bool); 7] = [
    ("UserPromptSubmit", false),
    ("PreToolUse", true),
    ("PostToolUse", true),
    ("Notification", false),
    ("Stop", false),
    ("SessionStart", false),
    ("SessionEnd", false),
];

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Thinking,
    /// Running a tool, described ("Editing app.rs").
    Tool(String),
    /// Waiting on you (a permission prompt).
    Waiting(String),
}

pub struct Session {
    pub id: String,
    pub project: String,
    pub state: State,
    /// When the current turn started.
    pub since: Instant,
    pub tools: u32,
    pub prompt: String,
    last: Instant,
}

impl Session {
    pub fn working(&self) -> bool {
        self.state != State::Idle
    }

    pub fn elapsed(&self) -> f32 {
        self.since.elapsed().as_secs_f32()
    }

    /// Short status for the compact pill.
    pub fn status(&self) -> String {
        match &self.state {
            State::Idle => "Idle".into(),
            State::Thinking => "Thinking…".into(),
            State::Tool(d) => d.clone(),
            State::Waiting(_) => "Needs you".into(),
        }
    }
}

pub enum Event {
    Finished { project: String, secs: f32, tools: u32 },
    Needs { project: String, message: String },
}

#[derive(Default)]
pub struct Claude {
    sessions: Vec<Session>,
}

impl Claude {
    /// The session to show: the most recently active one that's working.
    pub fn current(&self) -> Option<&Session> {
        self.sessions.iter().filter(|s| s.working()).max_by_key(|s| s.last)
    }

    /// Other sessions working at the same time.
    pub fn others(&self) -> usize {
        self.sessions.iter().filter(|s| s.working()).count().saturating_sub(1)
    }

    /// Drops sessions that went quiet; returns true if anything changed.
    pub fn prune(&mut self) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.last.elapsed() < STALE);
        before != self.sessions.len()
    }

    /// Applies one relayed hook event.
    pub fn handle(&mut self, json: &str) -> Option<Event> {
        let v: Value = serde_json::from_str(json).ok()?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let (event, id) = (s("hook_event_name"), s("session_id"));
        if id.is_empty() {
            return None;
        }
        if event == "SessionEnd" {
            self.sessions.retain(|x| x.id != id);
            return None;
        }
        let project = Path::new(&s("cwd")).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let i = match self.sessions.iter().position(|x| x.id == id) {
            Some(i) => i,
            None => {
                self.sessions.push(Session {
                    id: id.clone(),
                    project: project.clone(),
                    state: State::Idle,
                    since: Instant::now(),
                    tools: 0,
                    prompt: String::new(),
                    last: Instant::now(),
                });
                self.sessions.len() - 1
            }
        };
        let sess = &mut self.sessions[i];
        sess.last = Instant::now();
        if !project.is_empty() {
            sess.project = project;
        }
        let begin = |sess: &mut Session| {
            if !sess.working() {
                sess.since = Instant::now();
                sess.tools = 0;
            }
        };
        match event.as_str() {
            "UserPromptSubmit" => {
                sess.since = Instant::now();
                sess.tools = 0;
                sess.prompt = one_line(&s("prompt"), 140);
                sess.state = State::Thinking;
            }
            "PreToolUse" => {
                begin(sess);
                sess.tools += 1;
                let detail = s("detail");
                sess.state = State::Tool(if detail.is_empty() { s("tool_name") } else { detail });
            }
            "PostToolUse" => {
                begin(sess);
                sess.state = State::Thinking;
            }
            "Notification" => {
                // Only mid-turn requests matter; "waiting for your input" after a finished turn doesn't.
                if sess.working() {
                    let message = one_line(&s("message"), 120);
                    sess.state = State::Waiting(message.clone());
                    return Some(Event::Needs { project: sess.project.clone(), message });
                }
            }
            "Stop" => {
                if sess.working() {
                    sess.state = State::Idle;
                    return Some(Event::Finished { project: sess.project.clone(), secs: sess.elapsed(), tools: sess.tools });
                }
            }
            _ => {}
        }
        None
    }
}

fn one_line(s: &str, max: usize) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>().trim_end())
    } else {
        line
    }
}

fn file_name(input: &Value, key: &str) -> String {
    input
        .get(key)
        .and_then(Value::as_str)
        .and_then(|p| Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// What a tool call is doing, in a few words.
pub fn describe(tool: &str, input: &Value) -> String {
    let str_of = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    match tool {
        "Bash" | "PowerShell" => {
            let d = str_of("description");
            if !d.is_empty() {
                one_line(d, 48)
            } else {
                format!("Running {}", one_line(str_of("command").lines().next().unwrap_or_default(), 40))
            }
        }
        "Edit" | "MultiEdit" => format!("Editing {}", file_name(input, "file_path")),
        "Write" => format!("Writing {}", file_name(input, "file_path")),
        "Read" => format!("Reading {}", file_name(input, "file_path")),
        "NotebookEdit" => format!("Editing {}", file_name(input, "notebook_path")),
        "Grep" => format!("Searching for “{}”", one_line(str_of("pattern"), 30)),
        "Glob" => "Finding files".into(),
        "WebSearch" => "Searching the web".into(),
        "WebFetch" => {
            let host = str_of("url").split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or("the web");
            format!("Reading {host}")
        }
        "Task" | "Agent" => "Running an agent".into(),
        "TodoWrite" => "Planning".into(),
        "Skill" => "Using a skill".into(),
        t if t.starts_with("mcp__") => {
            let server = t.trim_start_matches("mcp__").split("__").next().unwrap_or_default();
            format!("Using {}", server.replace('_', " "))
        }
        t => t.to_string(),
    }
}

/// Keeps what the island needs from a hook's JSON (tool inputs and outputs can be huge).
fn slim(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
    let tool = s("tool_name");
    let detail = if tool.is_empty() { String::new() } else { describe(tool, v.get("tool_input").unwrap_or(&Value::Null)) };
    Some(
        json!({
            "hook_event_name": s("hook_event_name"),
            "session_id": s("session_id"),
            "cwd": s("cwd"),
            "tool_name": tool,
            "detail": detail,
            "prompt": one_line(s("prompt"), 300),
            "message": s("message"),
        })
        .to_string(),
    )
}

/// `dockside --claude-hook`: forwards one hook event to the running DockSide and exits.
/// Never fails loudly: a hook must not get in Claude's way.
pub fn relay() {
    let mut raw = String::new();
    if std::io::stdin().take(4 << 20).read_to_string(&mut raw).is_err() {
        return;
    }
    // Tolerate a byte-order mark from whatever wrote our stdin.
    let Some(payload) = slim(raw.trim_start_matches('\u{feff}')) else { return };
    unsafe {
        let found = FindWindowExW(HWND_MESSAGE, None, crate::win::CLASS_NAME, None);
        let Ok(target) = found else { return };
        let cds = COPYDATASTRUCT {
            dwData: COPYDATA_TAG,
            cbData: payload.len() as u32,
            lpData: payload.as_ptr() as *mut _,
        };
        let _ = SendMessageTimeoutW(
            target,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&cds as *const _ as isize),
            SMTO_ABORTIFHUNG,
            500,
            None,
        );
    }
}

/// Reads a WM_COPYDATA payload sent by [`relay`].
pub fn receive(lp: LPARAM) -> Option<String> {
    let cds = unsafe { (lp.0 as *const COPYDATASTRUCT).as_ref()? };
    if cds.dwData != COPYDATA_TAG || cds.lpData.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
    String::from_utf8(bytes.to_vec()).ok()
}

// ---------------------------------------------------------------------------
// Connecting: hooks in Claude Code's user settings
// ---------------------------------------------------------------------------

pub fn settings_path() -> PathBuf {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| {
        std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join(".claude")
    });
    dir.join("settings.json")
}

fn is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hs| hs.iter().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(HOOK_ARG))))
}

fn read_settings(path: &Path) -> Result<Map<String, Value>> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(&text).context("Claude settings.json isn't valid JSON")? {
        Value::Object(m) => Ok(m),
        _ => anyhow::bail!("Claude settings.json isn't a JSON object"),
    }
}

fn write_settings(path: &Path, settings: &Map<String, Value>) -> Result<()> {
    if path.exists() {
        // One backup of the file as it was before DockSide first touched it.
        let backup = path.with_extension("json.before-dockside");
        if !backup.exists() {
            std::fs::copy(path, &backup)?;
        }
    } else if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(settings)? + "\n")?;
    Ok(())
}

/// Removes our hooks from `settings`; empty event lists (and an empty `hooks`) go too.
fn strip(settings: &mut Map<String, Value>) {
    let Some(Value::Object(hooks)) = settings.get_mut("hooks") else { return };
    for (_, list) in hooks.iter_mut() {
        if let Value::Array(groups) = list {
            groups.retain(|g| !is_ours(g));
        }
    }
    hooks.retain(|_, list| !list.as_array().is_some_and(|a| a.is_empty()));
    if hooks.is_empty() {
        settings.remove("hooks");
    }
}

/// Adds (or refreshes) the island's hooks, pointing at `exe`.
pub fn connect(exe: &Path) -> Result<PathBuf> {
    let path = settings_path();
    let mut settings = read_settings(&path)?;
    strip(&mut settings);
    let command = format!("\"{}\" {HOOK_ARG}", exe.display().to_string().replace('\\', "/"));
    let hooks = settings.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else { anyhow::bail!("\"hooks\" in Claude settings.json isn't an object") };
    for (event, tool) in EVENTS {
        let mut group = Map::new();
        if tool {
            group.insert("matcher".into(), json!("*"));
        }
        group.insert("hooks".into(), json!([{ "type": "command", "command": command, "timeout": 5 }]));
        let list = hooks.entry(event).or_insert_with(|| json!([]));
        match list {
            Value::Array(a) => a.push(Value::Object(group)),
            _ => anyhow::bail!("\"hooks.{event}\" in Claude settings.json isn't a list"),
        }
    }
    write_settings(&path, &settings)?;
    Ok(path)
}

pub fn disconnect() -> Result<()> {
    let path = settings_path();
    if !path.exists() {
        return Ok(());
    }
    let mut settings = read_settings(&path)?;
    strip(&mut settings);
    write_settings(&path, &settings)
}

pub fn connected() -> bool {
    read_settings(&settings_path()).is_ok_and(|s| {
        s.get("hooks").and_then(Value::as_object).is_some_and(|h| {
            h.values().filter_map(Value::as_array).flatten().any(is_ours)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str, extra: Value) -> String {
        let mut v = json!({ "hook_event_name": name, "session_id": "s1", "cwd": "C:\\code\\DockSide" });
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        slim(&v.to_string()).unwrap()
    }

    #[test]
    fn a_turn_from_prompt_to_finish() {
        let mut c = Claude::default();
        assert!(c.handle(&ev("UserPromptSubmit", json!({ "prompt": "Fix the\nbug" }))).is_none());
        assert_eq!(c.current().unwrap().state, State::Thinking);
        assert_eq!(c.current().unwrap().prompt, "Fix the bug");
        c.handle(&ev("PreToolUse", json!({ "tool_name": "Edit", "tool_input": { "file_path": "C:\\code\\app.rs" } })));
        assert_eq!(c.current().unwrap().status(), "Editing app.rs");
        c.handle(&ev("PostToolUse", json!({ "tool_name": "Edit", "tool_response": "x".repeat(10_000) })));
        assert_eq!(c.current().unwrap().state, State::Thinking);
        match c.handle(&ev("Notification", json!({ "message": "Claude needs your permission to use Bash" }))) {
            Some(Event::Needs { project, message }) => {
                assert_eq!(project, "DockSide");
                assert!(message.contains("permission"));
            }
            _ => panic!("expected Needs"),
        }
        match c.handle(&ev("Stop", json!({}))) {
            Some(Event::Finished { tools, project, .. }) => assert_eq!((tools, project.as_str()), (1, "DockSide")),
            _ => panic!("expected Finished"),
        }
        assert!(c.current().is_none());
        // The idle "waiting for input" reminder after a turn is ignored.
        assert!(c.handle(&ev("Notification", json!({ "message": "Claude is waiting for your input" }))).is_none());
        c.handle(&ev("SessionEnd", json!({})));
        assert!(c.sessions.is_empty());
    }

    #[test]
    fn descriptions() {
        assert_eq!(describe("Bash", &json!({ "command": "cargo build --release\nmore", "description": "" })), "Running cargo build --release");
        assert_eq!(describe("Bash", &json!({ "command": "ls", "description": "List files" })), "List files");
        assert_eq!(describe("WebFetch", &json!({ "url": "https://docs.rs/x" })), "Reading docs.rs");
        assert_eq!(describe("mcp__Claude_Browser__navigate", &json!({})), "Using Claude Browser");
        assert_eq!(describe("Grep", &json!({ "pattern": "fn main" })), "Searching for “fn main”");
    }

    #[test]
    fn hooks_merge_and_strip() {
        let mut s: Map<String, Value> = serde_json::from_str(
            r#"{"model":"x","hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#,
        )
        .unwrap();
        // Simulate connect's merge without touching the real file.
        strip(&mut s);
        let hooks = s.get_mut("hooks").unwrap().as_object_mut().unwrap();
        hooks.get_mut("Stop").unwrap().as_array_mut().unwrap().push(json!({ "hooks": [{ "type": "command", "command": "\"C:/d/dockside.exe\" --claude-hook" }] }));
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 2);
        strip(&mut s);
        // The user's own hook survives; ours is gone.
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(s["model"], "x");
    }
}
