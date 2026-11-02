//! Discord voice controls, through Discord's local RPC: the voice channel
//! you're in, who's in it and who's speaking, and your mute / deafen state;
//! the island can mute, deafen and leave.
//!
//! RPC's voice scopes are only given to apps Discord approves, or to your own
//! app: so you create one (free) in the Discord Developer Portal, with
//! `http://localhost` as an OAuth2 redirect, and paste its Client ID and Secret
//! into Settings. "Connect Discord" then asks Discord once for permission; the
//! tokens it hands back are kept in `discord.dat`, encrypted to your Windows
//! account (DPAPI), and refreshed as they expire.
//!
//! One worker thread owns the connection for the life of the process (so a
//! rebuilt island doesn't ask again). It talks over the named pipe
//! `\\.\pipe\discord-ipc-N`: frames of (opcode, length, JSON), polled so
//! commands from the island can go out between messages.

use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB};
use windows::Win32::System::Pipes::PeekNamedPipe;

/// The redirect your Discord app must list (the token exchange names it).
pub const REDIRECT: &str = "http://localhost";
const SCOPES: [&str; 3] = ["rpc", "rpc.voice.read", "rpc.voice.write"];

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub id: String,
    pub name: String,
    /// Avatar hash, for `avatar_url`.
    pub avatar: Option<String>,
    pub speaking: bool,
    /// Muted or deafened (by themselves or a moderator).
    pub muted: bool,
    pub deafened: bool,
}

impl Member {
    pub fn avatar_url(&self) -> Option<String> {
        self.avatar.as_ref().map(|a| format!("https://cdn.discordapp.com/avatars/{}/{a}.png?size=64", self.id))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Voice {
    pub channel_id: String,
    pub channel: String,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Switched off, or no Client ID yet.
    Off,
    /// Discord isn't running (or isn't reachable).
    NotRunning,
    /// Connected, but DockSide hasn't been allowed in yet.
    NeedsAuth,
    /// Waiting for you to click Authorize in Discord.
    Authorizing,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub status: Status,
    pub voice: Option<Voice>,
    pub mute: bool,
    pub deaf: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self { status: Status::Off, voice: None, mute: false, deaf: false }
    }
}

pub enum Cmd {
    ToggleMute,
    ToggleDeaf,
    Leave,
    /// Ask Discord for permission (Settings' "Connect Discord").
    Authorize,
    /// Settings changed (or "Disconnect" removed the tokens): reconnect.
    Reconfigure,
}

#[derive(Clone, Default, PartialEq)]
struct Setup {
    enabled: bool,
    client_id: String,
    secret: String,
}

/// A square BGRA picture: (side, pixels).
pub type Pixels = (u32, Vec<u8>);

static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);
/// Members' pictures by user id (`None` while downloading, or if it failed).
static AVATARS: Mutex<Option<std::collections::HashMap<String, Option<Pixels>>>> = Mutex::new(None);
static SETUP: Mutex<Option<Setup>> = Mutex::new(None);
static SENDER: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);

/// What the island and Settings show.
pub fn snapshot() -> Snapshot {
    SNAPSHOT.lock().ok().and_then(|s| s.clone()).unwrap_or_default()
}

/// Applies the settings, starting the worker the first time it's wanted.
pub fn configure(enabled: bool, client_id: &str, secret: &str) {
    let setup = Setup { enabled, client_id: client_id.trim().to_string(), secret: secret.trim().to_string() };
    let changed = SETUP.lock().map(|mut s| s.replace(setup.clone()) != Some(setup.clone())).unwrap_or(false);
    let mut sender = match SENDER.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if sender.is_none() {
        if !setup.enabled {
            return;
        }
        let (tx, rx) = channel();
        *sender = Some(tx);
        let _ = std::thread::Builder::new().name("discord".into()).spawn(move || run(rx));
    } else if changed {
        if let Some(tx) = sender.as_ref() {
            let _ = tx.send(Cmd::Reconfigure);
        }
    }
}

pub fn send(cmd: Cmd) {
    if let Some(tx) = SENDER.lock().ok().and_then(|s| s.clone()) {
        let _ = tx.send(cmd);
    }
}

/// A member's picture once it has downloaded; starts the download the first
/// time it's asked for.
pub fn avatar(m: &Member) -> Option<Pixels> {
    let url = m.avatar_url()?;
    let mut avatars = AVATARS.lock().ok()?;
    let map = avatars.get_or_insert_with(std::collections::HashMap::new);
    if let Some(found) = map.get(&m.id) {
        return found.clone();
    }
    map.insert(m.id.clone(), None);
    let id = m.id.clone();
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let px = crate::net::get(&url).ok().and_then(|bytes| super::media::decode_bytes(&bytes, 96));
        if px.is_some() {
            if let Ok(mut a) = AVATARS.lock() {
                a.get_or_insert_with(std::collections::HashMap::new).insert(id, px);
            }
            super::holding::signal();
        }
    });
    None
}

/// Forgets the tokens ("Disconnect" in Settings).
pub fn forget() {
    let _ = std::fs::remove_file(token_path());
    send(Cmd::Reconfigure);
}

fn setup() -> Setup {
    SETUP.lock().ok().and_then(|s| s.clone()).unwrap_or_default()
}

fn publish(f: impl FnOnce(&mut Snapshot)) {
    let changed = SNAPSHOT
        .lock()
        .map(|mut s| {
            let snap = s.get_or_insert_with(Snapshot::default);
            let before = snap.clone();
            f(snap);
            *snap != before
        })
        .unwrap_or(false);
    if changed {
        super::holding::signal();
    }
}

fn set_status(status: Status) {
    publish(|s| {
        if status != Status::Ready {
            s.voice = None;
        }
        s.status = status;
    });
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

fn run(rx: Receiver<Cmd>) {
    loop {
        let s = setup();
        if !s.enabled || s.client_id.is_empty() {
            set_status(Status::Off);
            if wait(&rx, Duration::from_secs(3600)).is_err() {
                return;
            }
            continue;
        }
        let Some(pipe) = Pipe::open() else {
            set_status(Status::NotRunning);
            if wait(&rx, Duration::from_secs(5)).is_err() {
                return;
            }
            continue;
        };
        let mut session = Session { pipe, rx: &rx, setup: s, nonce: 0, mute: false, deaf: false, voice: None, auth_asked: false };
        match session.run() {
            Ok(End::Disconnected) => set_status(Status::NotRunning),
            Ok(End::Reconfigure) => {}
            Ok(End::Quit) => return,
            Err(e) => {
                log!("discord: {e}");
                set_status(Status::Failed(e));
                if wait(&rx, Duration::from_secs(10)).is_err() {
                    return;
                }
            }
        }
    }
}

/// Waits up to `d` for a command, dropping it: only a reconfigure (or a
/// connect) matters while there's no session. Err once the island is gone.
fn wait(rx: &Receiver<Cmd>, d: Duration) -> Result<(), ()> {
    match rx.recv_timeout(d) {
        Ok(_) | Err(RecvTimeoutError::Timeout) => Ok(()),
        Err(RecvTimeoutError::Disconnected) => Err(()),
    }
}

enum End {
    Disconnected,
    Reconfigure,
    Quit,
}

struct Session<'a> {
    pipe: Pipe,
    rx: &'a Receiver<Cmd>,
    setup: Setup,
    nonce: u64,
    mute: bool,
    deaf: bool,
    voice: Option<Voice>,
    /// An Authorize is out (only one at a time).
    auth_asked: bool,
}

impl Session<'_> {
    fn run(&mut self) -> Result<End, String> {
        self.pipe.write(0, &json!({ "v": 1, "client_id": self.setup.client_id }))?;
        // The first message is READY (or a close for a bad Client ID).
        match self.pipe.read_timeout(Duration::from_secs(5))? {
            Some((2, msg)) => return Err(format!("Discord refused the Client ID: {}", text(&msg, "message"))),
            Some(_) => {}
            None => return Err("Discord didn't answer".into()),
        }
        match load_token().filter(|t| t.client_id == self.setup.client_id) {
            Some(t) => {
                let t = if t.expires_at <= now() + 60 { refresh(&self.setup, &t).unwrap_or(t) } else { t };
                self.authenticate(&t.access_token)?;
            }
            None => set_status(Status::NeedsAuth),
        }
        loop {
            // Commands from the island and Settings.
            loop {
                match self.rx.try_recv() {
                    Ok(Cmd::Reconfigure) => return Ok(End::Reconfigure),
                    Ok(Cmd::Authorize) => self.authorize()?,
                    Ok(Cmd::ToggleMute) => self.command("SET_VOICE_SETTINGS", json!({ "mute": !self.mute }))?,
                    Ok(Cmd::ToggleDeaf) => self.command("SET_VOICE_SETTINGS", json!({ "deaf": !self.deaf }))?,
                    Ok(Cmd::Leave) => self.command("SELECT_VOICE_CHANNEL", json!({ "channel_id": null, "force": true }))?,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(End::Quit),
                }
            }
            match self.pipe.poll()? {
                Some((2, _)) => return Ok(End::Disconnected),
                Some((3, payload)) => self.pipe.write(4, &payload)?,
                Some((_, msg)) => self.handle(msg)?,
                None => std::thread::sleep(Duration::from_millis(30)),
            }
        }
    }

    fn command(&mut self, cmd: &str, args: Value) -> Result<(), String> {
        self.nonce += 1;
        self.pipe.write(1, &json!({ "cmd": cmd, "args": args, "nonce": self.nonce.to_string() }))
    }

    fn subscribe(&mut self, evt: &str, args: Value) -> Result<(), String> {
        self.nonce += 1;
        self.pipe.write(1, &json!({ "cmd": "SUBSCRIBE", "evt": evt, "args": args, "nonce": self.nonce.to_string() }))
    }

    fn unsubscribe(&mut self, evt: &str, args: Value) -> Result<(), String> {
        self.nonce += 1;
        self.pipe.write(1, &json!({ "cmd": "UNSUBSCRIBE", "evt": evt, "args": args, "nonce": self.nonce.to_string() }))
    }

    fn authorize(&mut self) -> Result<(), String> {
        if self.auth_asked {
            return Ok(());
        }
        self.auth_asked = true;
        set_status(Status::Authorizing);
        let args = json!({ "client_id": self.setup.client_id, "scopes": SCOPES });
        self.command("AUTHORIZE", args)
    }

    fn authenticate(&mut self, token: &str) -> Result<(), String> {
        self.command("AUTHENTICATE", json!({ "access_token": token }))
    }

    fn handle(&mut self, msg: Value) -> Result<(), String> {
        let cmd = msg.get("cmd").and_then(Value::as_str).unwrap_or("");
        let evt = msg.get("evt").and_then(Value::as_str).unwrap_or("");
        let data = msg.get("data").cloned().unwrap_or(Value::Null);
        if evt == "ERROR" {
            return self.error(cmd, &data);
        }
        match (cmd, evt) {
            ("AUTHORIZE", _) => {
                self.auth_asked = false;
                let code = text(&data, "code");
                let token = exchange(&self.setup, &code)?;
                save_token(&token);
                self.authenticate(&token.access_token)?;
            }
            ("AUTHENTICATE", _) => {
                set_status(Status::Ready);
                self.subscribe("VOICE_CHANNEL_SELECT", json!({}))?;
                self.subscribe("VOICE_SETTINGS_UPDATE", json!({}))?;
                self.command("GET_VOICE_SETTINGS", json!({}))?;
                self.command("GET_SELECTED_VOICE_CHANNEL", json!({}))?;
            }
            ("GET_VOICE_SETTINGS", _) | ("DISPATCH", "VOICE_SETTINGS_UPDATE") => {
                self.mute = data.get("mute").and_then(Value::as_bool).unwrap_or(self.mute);
                self.deaf = data.get("deaf").and_then(Value::as_bool).unwrap_or(self.deaf);
                self.publish_voice();
            }
            ("DISPATCH", "VOICE_CHANNEL_SELECT") => self.command("GET_SELECTED_VOICE_CHANNEL", json!({}))?,
            ("GET_SELECTED_VOICE_CHANNEL", _) => self.joined(&data)?,
            ("DISPATCH", "SPEAKING_START" | "SPEAKING_STOP") => {
                let id = text(&data, "user_id");
                if let Some(m) = self.voice.as_mut().and_then(|v| v.members.iter_mut().find(|m| m.id == id)) {
                    m.speaking = evt == "SPEAKING_START";
                }
                self.publish_voice();
            }
            ("DISPATCH", "VOICE_STATE_CREATE" | "VOICE_STATE_UPDATE") => {
                if let (Some(v), Some(m)) = (self.voice.as_mut(), member(&data)) {
                    match v.members.iter_mut().find(|x| x.id == m.id) {
                        Some(x) => *x = Member { speaking: x.speaking, ..m },
                        None => v.members.push(m),
                    }
                }
                self.publish_voice();
            }
            ("DISPATCH", "VOICE_STATE_DELETE") => {
                if let (Some(v), Some(m)) = (self.voice.as_mut(), member(&data)) {
                    v.members.retain(|x| x.id != m.id);
                }
                self.publish_voice();
            }
            _ => {}
        }
        Ok(())
    }

    /// An error answer. A rejected token asks for permission again; anything
    /// else is logged.
    fn error(&mut self, cmd: &str, data: &Value) -> Result<(), String> {
        let message = text(data, "message");
        match cmd {
            "AUTHENTICATE" => {
                // Expired or revoked: one refresh, else ask again.
                let refreshed = load_token().and_then(|t| refresh(&self.setup, &t).ok());
                match refreshed {
                    Some(t) if !self.auth_asked => {
                        self.auth_asked = true;
                        self.authenticate(&t.access_token)?;
                    }
                    _ => {
                        self.auth_asked = false;
                        set_status(Status::NeedsAuth);
                    }
                }
            }
            "AUTHORIZE" => {
                self.auth_asked = false;
                set_status(Status::Failed(format!("Discord said no: {message}")));
            }
            _ => log!("discord: {cmd}: {message}"),
        }
        Ok(())
    }

    /// The selected voice channel arrived (null: not in one).
    fn joined(&mut self, data: &Value) -> Result<(), String> {
        let old = self.voice.as_ref().map(|v| v.channel_id.clone());
        let new = data.get("id").and_then(Value::as_str).map(str::to_string);
        if old != new {
            if let Some(id) = &old {
                for evt in CHANNEL_EVENTS {
                    self.unsubscribe(evt, json!({ "channel_id": id }))?;
                }
            }
            if let Some(id) = &new {
                for evt in CHANNEL_EVENTS {
                    self.subscribe(evt, json!({ "channel_id": id }))?;
                }
            }
        }
        self.voice = new.map(|channel_id| {
            let name = text(data, "name");
            let members = data.get("voice_states").and_then(Value::as_array).map(|a| a.iter().filter_map(member).collect()).unwrap_or_default();
            Voice { channel_id, channel: if name.is_empty() { "Call".into() } else { name }, members }
        });
        self.publish_voice();
        Ok(())
    }

    fn publish_voice(&self) {
        let (voice, mute, deaf) = (self.voice.clone(), self.mute, self.deaf);
        publish(|s| {
            s.voice = voice;
            s.mute = mute;
            s.deaf = deaf;
        });
    }
}

const CHANNEL_EVENTS: [&str; 5] = ["SPEAKING_START", "SPEAKING_STOP", "VOICE_STATE_CREATE", "VOICE_STATE_UPDATE", "VOICE_STATE_DELETE"];

/// A voice state as a member: their server nickname, else display name, else username.
fn member(state: &Value) -> Option<Member> {
    let user = state.get("user")?;
    let id = text(user, "id");
    if id.is_empty() {
        return None;
    }
    let name = [text(state, "nick"), text(user, "global_name"), text(user, "username")].into_iter().find(|n| !n.is_empty()).unwrap_or_default();
    let vs = state.get("voice_state").unwrap_or(&Value::Null);
    let flag = |k: &str| vs.get(k).and_then(Value::as_bool).unwrap_or(false);
    Some(Member {
        id,
        name,
        avatar: user.get("avatar").and_then(Value::as_str).map(str::to_string),
        speaking: false,
        muted: flag("mute") || flag("self_mute") || state.get("mute").and_then(Value::as_bool).unwrap_or(false),
        deafened: flag("deaf") || flag("self_deaf"),
    })
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------
// The pipe
// ---------------------------------------------------------------------------

struct Pipe(std::fs::File);

impl Pipe {
    /// Discord listens on the first free `discord-ipc-N` (0-9).
    fn open() -> Option<Self> {
        (0..10).find_map(|n| {
            std::fs::OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\discord-ipc-{n}")).ok().map(Pipe)
        })
    }

    fn write(&mut self, op: u32, msg: &Value) -> Result<(), String> {
        let body = msg.to_string();
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(body.as_bytes());
        self.0.write_all(&frame).map_err(|e| format!("write: {e}"))
    }

    /// The next frame if one has (started to) arrive, without blocking otherwise.
    fn poll(&mut self) -> Result<Option<(u32, Value)>, String> {
        let mut avail = 0u32;
        let handle = HANDLE(self.0.as_raw_handle());
        unsafe { PeekNamedPipe(handle, None, 0, None, Some(&mut avail), None) }.map_err(|e| format!("pipe closed: {e}"))?;
        if avail < 8 {
            return Ok(None);
        }
        let mut head = [0u8; 8];
        self.0.read_exact(&mut head).map_err(|e| format!("read: {e}"))?;
        let op = u32::from_le_bytes(head[..4].try_into().unwrap_or_default());
        let len = u32::from_le_bytes(head[4..].try_into().unwrap_or_default()) as usize;
        let mut body = vec![0u8; len];
        self.0.read_exact(&mut body).map_err(|e| format!("read: {e}"))?;
        Ok(Some((op, serde_json::from_slice(&body).unwrap_or(Value::Null))))
    }

    fn read_timeout(&mut self, d: Duration) -> Result<Option<(u32, Value)>, String> {
        let until = Instant::now() + d;
        while Instant::now() < until {
            if let Some(f) = self.poll()? {
                return Ok(Some(f));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// OAuth tokens
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct Token {
    client_id: String,
    access_token: String,
    refresh_token: String,
    /// Seconds since 1970.
    expires_at: u64,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Trades the code Discord gave for tokens (needs the app's Secret).
fn exchange(setup: &Setup, code: &str) -> Result<Token, String> {
    token_request(setup, &[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", REDIRECT)])
}

fn refresh(setup: &Setup, t: &Token) -> Result<Token, String> {
    let fresh = token_request(setup, &[("grant_type", "refresh_token"), ("refresh_token", &t.refresh_token)])?;
    save_token(&fresh);
    Ok(fresh)
}

fn token_request(setup: &Setup, fields: &[(&str, &str)]) -> Result<Token, String> {
    let mut form = format!("client_id={}&client_secret={}", crate::net::encode(&setup.client_id), crate::net::encode(&setup.secret));
    for (k, v) in fields {
        form.push_str(&format!("&{k}={}", crate::net::encode(v)));
    }
    let headers = [("Content-Type", "application/x-www-form-urlencoded")];
    let body = crate::net::request("POST", "https://discord.com/api/oauth2/token", &headers, Some(form.as_bytes())).map_err(|e| {
        if e.starts_with("HTTP 4") {
            format!("Discord refused the sign-in ({e}): check the Client Secret, and that {REDIRECT} is listed under OAuth2 > Redirects")
        } else {
            e
        }
    })?;
    let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let access_token = text(&v, "access_token");
    if access_token.is_empty() {
        return Err("Discord sent no token".into());
    }
    Ok(Token {
        client_id: setup.client_id.clone(),
        access_token,
        refresh_token: text(&v, "refresh_token"),
        expires_at: now() + v.get("expires_in").and_then(Value::as_u64).unwrap_or(0),
    })
}

fn token_path() -> PathBuf {
    crate::config::dir().join("discord.dat")
}

fn save_token(t: &Token) {
    let plain = json!({ "client_id": t.client_id, "access_token": t.access_token, "refresh_token": t.refresh_token, "expires_at": t.expires_at })
        .to_string();
    match protect(plain.as_bytes()) {
        Some(sealed) => {
            if let Err(e) = std::fs::write(token_path(), sealed) {
                log!("discord: couldn't save the sign-in: {e}");
            }
        }
        None => log!("discord: couldn't encrypt the sign-in"),
    }
}

fn load_token() -> Option<Token> {
    let plain = unprotect(&std::fs::read(token_path()).ok()?)?;
    let v: Value = serde_json::from_slice(&plain).ok()?;
    Some(Token {
        client_id: text(&v, "client_id"),
        access_token: text(&v, "access_token"),
        refresh_token: text(&v, "refresh_token"),
        expires_at: v.get("expires_at").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Encrypts to the current Windows user (DPAPI).
fn protect(data: &[u8]) -> Option<Vec<u8>> {
    dpapi(data, true)
}

fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    dpapi(data, false)
}

fn dpapi(data: &[u8], seal: bool) -> Option<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = if seal {
            CryptProtectData(&input, None, None, None, None, 0, &mut out)
        } else {
            CryptUnprotectData(&input, None, None, None, None, 0, &mut out)
        };
        ok.ok()?;
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as _));
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trip() {
        let sealed = protect(b"secret token").unwrap();
        assert_ne!(sealed, b"secret token");
        assert_eq!(unprotect(&sealed).unwrap(), b"secret token");
    }

    #[test]
    fn members() {
        let state: Value = serde_json::from_str(
            r#"{"nick": "", "mute": false, "voice_state": {"mute": false, "deaf": false, "self_mute": true, "self_deaf": false},
                "user": {"id": "42", "username": "box", "global_name": "Box", "avatar": "abc"}}"#,
        )
        .unwrap();
        let m = member(&state).unwrap();
        assert_eq!((m.name.as_str(), m.muted, m.deafened), ("Box", true, false));
        assert_eq!(m.avatar_url().unwrap(), "https://cdn.discordapp.com/avatars/42/abc.png?size=64");
    }
}

