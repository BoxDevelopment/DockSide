//! Claude Code on the island, through Claude Code hooks.
//!
//! "Connect Claude Code" (System Settings) adds hooks to Claude's
//! `settings.json` that run `dockside.exe --claude-hook` on each step: a prompt
//! sent, a tool starting / finishing, a permission request, the turn ending.
//! That short-lived relay reads the hook's JSON from stdin, keeps the few
//! fields the island needs (plus the session's title and what Claude last
//! wrote, from its transcript), and hands them to the running DockSide with
//! WM_COPYDATA. The island turns them into a live activity, a step-by-step
//! Progress page and "finished" / "needs you" alerts.
//!
//! It also sets Claude's status line to `dockside.exe --claude-status` (unless
//! you have your own), which passes the plan's usage on for the Usage page.

use std::io::{Read, Seek, SeekFrom};
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
pub const STATUS_ARG: &str = "--claude-status";
/// A session that stops reporting (Claude closed mid-turn) is dropped after this.
const STALE: Duration = Duration::from_secs(15 * 60);
/// Steps kept per turn (the panel shows the last few).
const MAX_STEPS: usize = 12;
/// How much of the transcript's end is searched for the title: a lot when a
/// turn starts or ends, less on each tool call.
const TITLE_SCAN: u64 = 8 << 20;
const TOOL_SCAN: u64 = 1 << 20;

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

/// What sort of thing a tool works on, for the card's icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Terminal,
    File,
    Search,
    Web,
    Agent,
    Other,
}

impl Kind {
    fn of(tool: &str) -> Kind {
        match tool {
            "Bash" | "PowerShell" => Kind::Terminal,
            "Read" | "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Kind::File,
            "Grep" | "Glob" => Kind::Search,
            "WebSearch" | "WebFetch" => Kind::Web,
            "Task" | "Agent" => Kind::Agent,
            _ => Kind::Other,
        }
    }
}

/// One tool call of the turn, as the Progress page lists it ("Edit app.rs").
pub struct Step {
    pub label: String,
    pub done: bool,
    id: String,
}

/// The latest tool call, in detail.
pub struct Card {
    pub tool: String,
    pub kind: Kind,
    /// What it works on: the command, the file's path, the pattern.
    pub target: String,
    /// Lines written and replaced (edits and writes).
    pub added: Option<u32>,
    pub removed: Option<u32>,
    /// Lines read, once a Read finished.
    pub lines: Option<u32>,
    pub start: Instant,
    pub end: Option<Instant>,
    id: String,
}

impl Card {
    pub fn secs(&self) -> f32 {
        self.end.unwrap_or_else(Instant::now).duration_since(self.start).as_secs_f32()
    }

    /// How far the bar is: creeping toward the end while it runs, full once done.
    pub fn progress(&self) -> f32 {
        if self.end.is_some() {
            1.0
        } else {
            0.9 * (1.0 - (-self.secs() / 1.5).exp())
        }
    }

    /// The line under the target: a label, and a value shown in bold after it.
    pub fn status(&self) -> (String, Option<String>) {
        let done = self.end.is_some();
        match (self.kind, self.tool.as_str()) {
            (Kind::Terminal, _) => (format!("{:.2} seconds", self.secs()), None),
            (_, "Read") if done => match self.lines {
                Some(n) => ("Lines".into(), Some(n.to_string())),
                None => ("Read".into(), None),
            },
            (_, "Read") => ("Reading...".into(), None),
            (Kind::File, _) => ((if done { "Saved" } else { "Saving..." }).into(), None),
            _ if done => ("Done".into(), None),
            (Kind::Search, _) => ("Searching...".into(), None),
            (Kind::Web, _) => ("Fetching...".into(), None),
            (Kind::Agent, _) => ("Working...".into(), None),
            _ => ("Running...".into(), None),
        }
    }
}

pub struct Session {
    pub id: String,
    pub project: String,
    pub state: State,
    /// When the current turn started.
    pub since: Instant,
    pub tools: u32,
    pub prompt: String,
    /// The session's title (Claude names its sessions), once it has one.
    pub title: String,
    /// This turn's tool calls, oldest first.
    pub steps: Vec<Step>,
    pub card: Option<Card>,
    /// The last thing Claude wrote this turn.
    pub said: String,
    /// When the turn finished (`None` while it works).
    pub ended: Option<Instant>,
    last: Instant,
}

impl Session {
    pub fn working(&self) -> bool {
        self.state != State::Idle
    }

    pub fn elapsed(&self) -> f32 {
        self.ended.unwrap_or_else(Instant::now).duration_since(self.since).as_secs_f32()
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

    /// The Progress page's heading: the title, else the project.
    pub fn heading(&self) -> &str {
        if self.title.is_empty() {
            &self.project
        } else {
            &self.title
        }
    }

    /// Starts a fresh turn.
    fn begin(&mut self) {
        self.since = Instant::now();
        self.tools = 0;
        self.steps.clear();
        self.card = None;
        self.said.clear();
        self.ended = None;
    }
}

pub enum Event {
    Finished { project: String, secs: f32, tools: u32 },
    Needs { project: String, message: String },
}

/// A plan's usage limit, as last reported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limit {
    /// Percent used, 0–100.
    pub used: f32,
    /// When the window resets (Unix seconds).
    pub resets_at: Option<i64>,
}

impl Limit {
    /// The share left, 0–1 (all of it once the window has reset).
    pub fn left(&self) -> f32 {
        if self.resets_at.is_some_and(|t| t <= super::calendar::unix_now()) {
            return 1.0;
        }
        (1.0 - self.used / 100.0).clamp(0.0, 1.0)
    }
}

/// A coding agent on the Usage page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
        }
    }
}

#[derive(Default)]
pub struct Claude {
    sessions: Vec<Session>,
    /// Claude's 5-hour limit, from its status line.
    pub usage: Option<Limit>,
}

impl Claude {
    /// Starts with the usage the status line last saved.
    pub fn load() -> Self {
        let mut c = Self::default();
        if let Ok(saved) = std::fs::read_to_string(usage_path()) {
            c.handle(&saved);
        }
        c
    }

    /// The session to show: the most recently active one that's working.
    pub fn current(&self) -> Option<&Session> {
        self.sessions.iter().filter(|s| s.working()).max_by_key(|s| s.last)
    }

    /// What the open panel shows: the working session, else the one that finished last.
    pub fn shown(&self) -> Option<&Session> {
        self.current().or_else(|| self.sessions.iter().filter(|s| s.ended.is_some()).max_by_key(|s| s.last))
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
        let n = |k: &str| v.get(k).and_then(Value::as_u64).map(|n| n as u32);
        let (event, id) = (s("hook_event_name"), s("session_id"));
        if event == "Usage" {
            let used = v.get("used").and_then(Value::as_f64)? as f32;
            self.usage = Some(Limit { used, resets_at: v.get("resets_at").and_then(Value::as_f64).map(|t| t as i64) });
            return None;
        }
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
                    title: String::new(),
                    steps: Vec::new(),
                    card: None,
                    said: String::new(),
                    ended: None,
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
        let title = s("title");
        if !title.is_empty() {
            sess.title = title;
        }
        let said = s("said");
        match event.as_str() {
            "UserPromptSubmit" => {
                sess.begin();
                sess.prompt = one_line(&s("prompt"), 140);
                sess.state = State::Thinking;
            }
            "PreToolUse" => {
                if !sess.working() {
                    sess.begin();
                }
                sess.tools += 1;
                if !said.is_empty() {
                    sess.said = said;
                }
                let (tool, use_id, step, detail) = (s("tool_name"), s("tool_use_id"), s("step"), s("detail"));
                sess.steps.push(Step { label: if step.is_empty() { tool.clone() } else { step }, done: false, id: use_id.clone() });
                if sess.steps.len() > MAX_STEPS {
                    sess.steps.remove(0);
                }
                sess.card = Some(Card {
                    kind: Kind::of(&tool),
                    target: s("target"),
                    added: n("added"),
                    removed: n("removed"),
                    lines: None,
                    start: Instant::now(),
                    end: None,
                    id: use_id,
                    tool: tool.clone(),
                });
                sess.state = State::Tool(if detail.is_empty() { tool } else { detail });
            }
            "PostToolUse" => {
                if !sess.working() {
                    sess.begin();
                }
                let use_id = s("tool_use_id");
                let step = match sess.steps.iter().position(|x| !x.done && x.id == use_id) {
                    Some(i) => Some(i),
                    None => sess.steps.iter().position(|x| !x.done),
                };
                if let Some(i) = step {
                    sess.steps[i].done = true;
                }
                if let Some(card) = sess.card.as_mut().filter(|c| c.end.is_none() && (use_id.is_empty() || c.id == use_id)) {
                    card.end = Some(Instant::now());
                    card.lines = n("lines");
                }
                // Parallel tools: still busy while another runs.
                if !sess.steps.iter().any(|x| !x.done) {
                    sess.state = State::Thinking;
                }
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
                    if !said.is_empty() {
                        sess.said = said;
                    }
                    for step in &mut sess.steps {
                        step.done = true;
                    }
                    if let Some(card) = sess.card.as_mut().filter(|c| c.end.is_none()) {
                        card.end = Some(Instant::now());
                    }
                    sess.ended = Some(Instant::now());
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

fn host(url: &str) -> Option<&str> {
    url.split("://").nth(1).and_then(|r| r.split('/').next())
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
        "WebFetch" => format!("Reading {}", host(str_of("url")).unwrap_or("the web")),
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

/// A tool call as a step of the turn ("Edit app.rs", "Run cargo build").
pub fn step(tool: &str, input: &Value) -> String {
    let str_of = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    match tool {
        "Bash" | "PowerShell" => {
            let d = str_of("description");
            if !d.is_empty() {
                one_line(d, 48)
            } else {
                format!("Run {}", one_line(str_of("command").lines().next().unwrap_or_default(), 40))
            }
        }
        "Edit" | "MultiEdit" => format!("Edit {}", file_name(input, "file_path")),
        "Write" => format!("Write {}", file_name(input, "file_path")),
        "Read" => format!("Read {}", file_name(input, "file_path")),
        "NotebookEdit" => format!("Edit {}", file_name(input, "notebook_path")),
        "Grep" => format!("Search “{}”", one_line(str_of("pattern"), 30)),
        "Glob" => format!("Find {}", one_line(str_of("pattern"), 30)),
        "WebSearch" => format!("Search “{}”", one_line(str_of("query"), 30)),
        "WebFetch" => format!("Fetch {}", host(str_of("url")).unwrap_or("the web")),
        "Task" | "Agent" => match str_of("description") {
            "" => "Run an agent".into(),
            d => one_line(d, 40),
        },
        "TodoWrite" => "Update the plan".into(),
        "Skill" => format!("Use {}", str_of("skill")).trim_end().to_string(),
        t if t.starts_with("mcp__") => {
            let server = t.trim_start_matches("mcp__").split("__").next().unwrap_or_default();
            format!("Use {}", server.replace('_', " "))
        }
        t => t.to_string(),
    }
}

/// What the card names: the command, the file (relative to the project), the pattern or host.
fn target(tool: &str, input: &Value, cwd: &str) -> String {
    let str_of = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    let path = |k: &str| {
        let p = str_of(k);
        let rel = Path::new(p).strip_prefix(cwd).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| p.to_string());
        rel.replace('\\', "/")
    };
    match tool {
        "Bash" | "PowerShell" => one_line(str_of("command").lines().next().unwrap_or_default(), 80),
        "Edit" | "MultiEdit" | "Write" | "Read" => path("file_path"),
        "NotebookEdit" => path("notebook_path"),
        "Grep" | "Glob" => one_line(str_of("pattern"), 60),
        "WebSearch" => one_line(str_of("query"), 60),
        "WebFetch" => host(str_of("url")).unwrap_or_default().to_string(),
        "Task" | "Agent" => one_line(str_of("subagent_type"), 40),
        "Skill" => str_of("skill").to_string(),
        _ => String::new(),
    }
}

fn line_count(s: &str) -> u32 {
    s.lines().count() as u32
}

/// Lines an edit or write puts in, and takes out.
fn changes(tool: &str, input: &Value) -> (Option<u32>, Option<u32>) {
    let lines = |v: &Value, k: &str| line_count(v.get(k).and_then(Value::as_str).unwrap_or_default());
    match tool {
        "Edit" => (Some(lines(input, "new_string")), Some(lines(input, "old_string"))),
        "MultiEdit" => {
            let edits = input.get("edits").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            (Some(edits.iter().map(|e| lines(e, "new_string")).sum()), Some(edits.iter().map(|e| lines(e, "old_string")).sum()))
        }
        "Write" => (Some(lines(input, "content")), None),
        "NotebookEdit" => (Some(lines(input, "new_source")), None),
        _ => (None, None),
    }
}

/// Lines a finished Read returned.
fn lines_read(response: &Value) -> Option<u32> {
    let file = response.get("file")?;
    file.get("numLines")
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .or_else(|| file.get("content").and_then(Value::as_str).map(line_count))
}

/// The end of a file (at most `max` bytes), from the start of a line.
fn tail(path: &Path, max: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let start = f.metadata().ok()?.len().saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Some(if start > 0 { text.split_once('\n').map(|(_, r)| r.to_string()).unwrap_or_default() } else { text })
}

/// The session's newest title in its transcript.
fn transcript_title(text: &str) -> String {
    text.lines()
        .rev()
        .filter(|l| l.contains("\"customTitle\""))
        .find_map(|l| serde_json::from_str::<Value>(l).ok()?.get("customTitle")?.as_str().map(|t| one_line(t, 80)))
        .unwrap_or_default()
}

/// What Claude last wrote this turn, as one line (empty if nothing since the prompt).
fn transcript_said(text: &str) -> String {
    for l in text.lines().rev() {
        if !l.contains("\"type\":\"user\"") && !l.contains("\"role\":\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let content = v.pointer("/message/content");
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                // A tool's result is part of the turn; a typed prompt starts it.
                let result = content.and_then(Value::as_array).is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
                if !result && v.get("isMeta") != Some(&Value::Bool(true)) {
                    return String::new();
                }
            }
            Some("assistant") => {
                let text = content.and_then(Value::as_array).and_then(|c| {
                    c.iter().rev().filter(|b| b["type"] == "text").find_map(|b| b["text"].as_str().filter(|t| !t.trim().is_empty()))
                });
                if let Some(t) = text {
                    // Its first line, without markdown marks.
                    let first = t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
                    let plain: String =
                        first.trim_start_matches(['#', '>', '-', ' ']).chars().filter(|c| !matches!(c, '*' | '`')).collect();
                    return one_line(&plain, 160);
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// Keeps what the island needs from a hook's JSON (tool inputs and outputs can be huge).
fn slim(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
    let (event, tool) = (s("hook_event_name"), s("tool_name"));
    let input = v.get("tool_input").unwrap_or(&Value::Null);
    let pre = event == "PreToolUse" && !tool.is_empty();
    let (detail, step, target) =
        if pre { (describe(tool, input), self::step(tool, input), target(tool, input, s("cwd"))) } else { Default::default() };
    let (added, removed) = if pre { changes(tool, input) } else { (None, None) };
    let lines = if event == "PostToolUse" && tool == "Read" { v.get("tool_response").and_then(lines_read) } else { None };
    // The title and Claude's latest words come from the transcript.
    let scan = match event {
        "UserPromptSubmit" | "Stop" | "SessionStart" => TITLE_SCAN,
        "PreToolUse" => TOOL_SCAN,
        _ => 0,
    };
    let text = if scan > 0 { tail(Path::new(s("transcript_path")), scan).unwrap_or_default() } else { String::new() };
    let said = if matches!(event, "PreToolUse" | "Stop") { transcript_said(&text) } else { String::new() };
    Some(
        json!({
            "hook_event_name": event,
            "session_id": s("session_id"),
            "cwd": s("cwd"),
            "tool_name": tool,
            "tool_use_id": s("tool_use_id"),
            "detail": detail,
            "step": step,
            "target": target,
            "added": added,
            "removed": removed,
            "lines": lines,
            "title": transcript_title(&text),
            "said": said,
            "prompt": one_line(s("prompt"), 300),
            "message": s("message"),
        })
        .to_string(),
    )
}

/// Hands one payload to the running DockSide.
fn send(payload: &str) {
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

fn read_stdin() -> Option<String> {
    let mut raw = String::new();
    std::io::stdin().take(4 << 20).read_to_string(&mut raw).ok()?;
    // Tolerate a byte-order mark from whatever wrote our stdin.
    Some(raw.trim_start_matches('\u{feff}').to_string())
}

/// `dockside --claude-hook`: forwards one hook event to the running DockSide and exits.
/// Never fails loudly: a hook must not get in Claude's way.
pub fn relay() {
    if let Some(payload) = read_stdin().and_then(|raw| slim(&raw)) {
        send(&payload);
    }
}

/// `dockside --claude-status`: Claude Code's status line. Passes the plan's
/// 5-hour usage to the island and prints a short line for Claude's footer.
pub fn status_line() {
    let v: Value = read_stdin().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default();
    let five = v.pointer("/rate_limits/five_hour");
    let used = five.and_then(|f| f.get("used_percentage")).and_then(Value::as_f64);
    if let Some(used) = used {
        let resets_at = five.and_then(|f| f.get("resets_at")).and_then(Value::as_f64);
        let payload = json!({ "hook_event_name": "Usage", "used": used, "resets_at": resets_at }).to_string();
        // Kept on disk too, so the island has it after a restart (or if it wasn't running).
        let _ = std::fs::write(usage_path(), &payload);
        send(&payload);
    }
    let model = v.pointer("/model/display_name").and_then(Value::as_str).unwrap_or("Claude");
    match used {
        Some(u) => println!("{model} · {:.0}% left", (100.0 - u).max(0.0)),
        None => println!("{model}"),
    }
}

/// Where the status line keeps Claude's latest usage.
fn usage_path() -> PathBuf {
    crate::config::dir().join("claude-usage.json")
}

/// The Claude desktop app's 5-hour usage, from the samples it saves each time
/// it checks the plan's limits, with the sample's time (Unix seconds).
pub fn desktop_limit() -> Option<(Limit, i64)> {
    const NAME: &str = "plan-usage-history.json";
    // The Store app keeps its data in Claude-Data\<package>; the installer's in Roaming\Claude.
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        if let Ok(rd) = std::fs::read_dir(PathBuf::from(local).join("Claude-Data")) {
            files.extend(rd.flatten().map(|e| e.path().join(NAME)));
        }
    }
    if let Some(roaming) = std::env::var_os("APPDATA") {
        files.push(PathBuf::from(roaming).join("Claude").join(NAME));
    }
    files.iter().filter_map(|f| desktop_sample(&std::fs::read_to_string(f).ok()?)).max_by_key(|(_, t)| *t)
}

/// The newest sample in a `plan-usage-history.json`.
fn desktop_sample(text: &str) -> Option<(Limit, i64)> {
    let v: Value = serde_json::from_str(text).ok()?;
    let last = v.get("samples")?.as_array()?.iter().filter(|s| s.pointer("/u/fh").is_some()).max_by_key(|s| s.get("t").and_then(Value::as_i64).unwrap_or(0))?;
    let at = last.get("t")?.as_i64()? / 1000;
    let used = last.pointer("/u/fh")?.as_f64()? as f32;
    // The sample has no reset time, but the window resets within 5 hours of it.
    Some((Limit { used, resets_at: Some(at + 5 * 3600) }, at))
}

/// Claude's limit: the desktop app's sample, or the status line's when that's newer.
pub fn claude_limit(status: Option<Limit>) -> Option<Limit> {
    let Some((app, at)) = desktop_limit() else { return status };
    let status_at = std::fs::metadata(usage_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    match status {
        Some(s) if status_at.is_some_and(|t| t > at) => Some(s),
        _ => Some(app),
    }
}

/// Codex's 5-hour limit, from the newest of its session logs that reported one.
pub fn codex_limit() -> Option<Limit> {
    let home = match std::env::var_os("CODEX_HOME") {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(std::env::var_os("USERPROFILE")?).join(".codex"),
    };
    let mut logs = Vec::new();
    newest_logs(&home.join("sessions"), 3, &mut logs);
    logs.iter().find_map(|log| {
        let text = tail(log, 1 << 20)?;
        let at = text.rfind("\"rate_limits\":{")? + "\"rate_limits\":".len();
        let limits = serde_json::Deserializer::from_str(&text[at..]).into_iter::<Value>().next()?.ok()?;
        let primary = limits.get("primary")?;
        Some(Limit {
            used: primary.get("used_percent")?.as_f64()? as f32,
            resets_at: primary.get("resets_at").and_then(Value::as_f64).map(|t| t as i64),
        })
    })
}

/// The newest few `.jsonl` files under `dir`'s year / month / day folders.
fn newest_logs(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    const MAX: usize = 6;
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    // Names are dates and timestamps, so the newest sorts last.
    entries.sort();
    for e in entries.into_iter().rev() {
        if out.len() >= MAX {
            return;
        }
        if depth > 0 {
            if e.is_dir() {
                newest_logs(&e, depth - 1, out);
            }
        } else if e.extension().is_some_and(|x| x == "jsonl") {
            out.push(e);
        }
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

fn status_is_ours(settings: &Map<String, Value>) -> bool {
    settings.get("statusLine").and_then(|s| s.get("command")).and_then(Value::as_str).is_some_and(|c| c.contains(STATUS_ARG))
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

/// Removes our hooks (and status line) from `settings`; empty event lists (and an empty `hooks`) go too.
fn strip(settings: &mut Map<String, Value>) {
    if status_is_ours(settings) {
        settings.remove("statusLine");
    }
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

/// Adds (or refreshes) our hooks and status line, pointing at `exe`.
fn merge(settings: &mut Map<String, Value>, exe: &Path) -> Result<bool> {
    strip(settings);
    let exe = exe.display().to_string().replace('\\', "/");
    let command = format!("\"{exe}\" {HOOK_ARG}");
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
    // A status line you set yourself is kept (the Usage page then has no Claude ring).
    let status = !settings.contains_key("statusLine");
    if status {
        settings.insert("statusLine".into(), json!({ "type": "command", "command": format!("\"{exe}\" {STATUS_ARG}") }));
    }
    Ok(status)
}

/// Adds (or refreshes) the island's hooks, pointing at `exe`. Returns whether
/// the status line (for the Usage page) is ours.
pub fn connect(exe: &Path) -> Result<bool> {
    let path = settings_path();
    let mut settings = read_settings(&path)?;
    let status = merge(&mut settings, exe)?;
    write_settings(&path, &settings)?;
    Ok(status)
}

/// Brings an older connection up to date: hooks added before the status line
/// existed get it now (unless you have your own), so the Usage page fills in.
/// Returns whether settings.json was changed.
pub fn upgrade(exe: &Path) -> Result<bool> {
    let path = settings_path();
    if !path.exists() {
        return Ok(false);
    }
    let mut settings = read_settings(&path)?;
    let hooked = settings.get("hooks").and_then(Value::as_object).is_some_and(|h| h.values().filter_map(Value::as_array).flatten().any(is_ours));
    if !hooked || settings.contains_key("statusLine") {
        return Ok(false);
    }
    merge(&mut settings, exe)?;
    write_settings(&path, &settings)?;
    Ok(true)
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
        let edit = json!({ "file_path": "C:\\code\\DockSide\\src\\app.rs", "old_string": "a", "new_string": "a\nb\nc" });
        c.handle(&ev("PreToolUse", json!({ "tool_name": "Edit", "tool_use_id": "t1", "tool_input": edit })));
        let s = c.current().unwrap();
        assert_eq!(s.status(), "Editing app.rs");
        assert_eq!((s.steps[0].label.as_str(), s.steps[0].done), ("Edit app.rs", false));
        let card = s.card.as_ref().unwrap();
        assert_eq!((card.kind, card.target.as_str(), card.added, card.removed), (Kind::File, "src/app.rs", Some(3), Some(1)));
        c.handle(&ev("PostToolUse", json!({ "tool_name": "Edit", "tool_use_id": "t1", "tool_response": "x".repeat(10_000) })));
        let s = c.current().unwrap();
        assert_eq!(s.state, State::Thinking);
        assert!(s.steps[0].done && s.card.as_ref().unwrap().end.is_some());
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
        // The finished turn stays on the open panel.
        assert!(c.shown().is_some_and(|s| s.ended.is_some()));
        // The idle "waiting for input" reminder after a turn is ignored.
        assert!(c.handle(&ev("Notification", json!({ "message": "Claude is waiting for your input" }))).is_none());
        c.handle(&ev("SessionEnd", json!({})));
        assert!(c.sessions.is_empty());
    }

    #[test]
    fn reads_finish_with_their_line_count() {
        let mut c = Claude::default();
        c.handle(&ev("PreToolUse", json!({ "tool_name": "Read", "tool_input": { "file_path": "C:\\x\\notes.md" } })));
        let resp = json!({ "type": "text", "file": { "filePath": "C:\\x\\notes.md", "numLines": 184 } });
        c.handle(&ev("PostToolUse", json!({ "tool_name": "Read", "tool_response": resp })));
        let card = c.current().unwrap().card.as_ref().unwrap();
        assert_eq!(card.status(), ("Lines".to_string(), Some("184".to_string())));
        assert_eq!(card.target, "C:/x/notes.md");
    }

    #[test]
    fn usage_from_the_desktop_app() {
        let text = r#"{"version":2,"samples":[{"t":1794547771841,"org":"a","u":{"fh":5,"sd":22}},{"t":1794546263366,"org":"a","u":{"fh":3,"sd":22}}]}"#;
        let (l, at) = desktop_sample(text).unwrap();
        assert_eq!(at, 1794547771);
        assert_eq!(l.used, 5.0);
        assert_eq!(l.resets_at, Some(1794547771 + 5 * 3600));
    }

    #[test]
    fn usage_from_the_status_line() {
        let mut c = Claude::default();
        c.handle(r#"{"hook_event_name":"Usage","used":58.0,"resets_at":4102444800}"#);
        assert!((c.usage.unwrap().left() - 0.42).abs() < 1e-4);
        // A window that has reset is all left.
        assert_eq!(Limit { used: 90.0, resets_at: Some(1) }.left(), 1.0);
    }

    #[test]
    fn transcript_title_and_words() {
        let text = [
            r#"{"type":"custom-title","customTitle":"Old name"}"#,
            r#"{"type":"user","message":{"role":"user","content":"make it"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"**Planning** the release notes page.\n\nMore."}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"custom-title","customTitle":"Release notes page"}"#,
        ]
        .join("\n");
        assert_eq!(transcript_title(&text), "Release notes page");
        assert_eq!(transcript_said(&text), "Planning the release notes page.");
        // Nothing said since the latest prompt.
        let fresh = format!("{text}\n{}", r#"{"type":"user","message":{"role":"user","content":"next"}}"#);
        assert_eq!(transcript_said(&fresh), "");
    }

    #[test]
    fn descriptions() {
        assert_eq!(describe("Bash", &json!({ "command": "cargo build --release\nmore", "description": "" })), "Running cargo build --release");
        assert_eq!(describe("Bash", &json!({ "command": "ls", "description": "List files" })), "List files");
        assert_eq!(describe("WebFetch", &json!({ "url": "https://docs.rs/x" })), "Reading docs.rs");
        assert_eq!(describe("mcp__Claude_Browser__navigate", &json!({})), "Using Claude Browser");
        assert_eq!(describe("Grep", &json!({ "pattern": "fn main" })), "Searching for “fn main”");
        assert_eq!(step("Bash", &json!({ "command": "swift build" })), "Run swift build");
        assert_eq!(step("Write", &json!({ "file_path": "C:\\a\\Page.swift" })), "Write Page.swift");
    }

    #[test]
    fn hooks_merge_and_strip() {
        let mut s: Map<String, Value> = serde_json::from_str(
            r#"{"model":"x","hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#,
        )
        .unwrap();
        assert!(merge(&mut s, Path::new("C:\\d\\dockside.exe")).unwrap());
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert!(status_is_ours(&s));
        // Merging again refreshes rather than duplicating.
        assert!(merge(&mut s, Path::new("C:\\d\\dockside.exe")).unwrap());
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 2);
        strip(&mut s);
        // The user's own hook survives; ours (and our status line) are gone.
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(!s.contains_key("statusLine"));
        assert_eq!(s["model"], "x");
        // Your own status line is kept.
        s.insert("statusLine".into(), json!({ "type": "command", "command": "mine" }));
        assert!(!merge(&mut s, Path::new("C:\\d\\dockside.exe")).unwrap());
        assert_eq!(s["statusLine"]["command"], "mine");
    }
}
