//! Calendar accounts: iCloud, Google, Outlook / Microsoft 365, any CalDAV
//! server, and subscribed .ics links, kept in sync on a worker thread for the
//! menu bar calendar and the Dynamic Island.
//!
//! Accounts (with their sign-ins) are saved encrypted to the Windows user
//! (DPAPI) in `calendars.dat` next to config.toml. Adding one goes through a
//! short setup in System Settings > Calendars ([`Setup`]), which explains the
//! choices each provider gives before asking for anything.
//!
//! * Google and Microsoft: a browser sign-in (OAuth with PKCE) through the
//!   user's own app registration ([`oauth`], [`google`], [`graph`]).
//! * iCloud and other CalDAV servers: an app-specific password ([`caldav`]).
//! * Any provider: a private .ics link, read-only.

pub mod caldav;
pub mod google;
pub mod graph;
pub mod oauth;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::config::CalendarCfg;
use crate::island::calendar::CalEvent;

/// How often every account is read again.
const REFRESH: Duration = Duration::from_secs(300);
/// What's kept: from two months back to six months ahead.
const BACK: i64 = 62 * 86_400;
const AHEAD: i64 = 186 * 86_400;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub enum Provider {
    ICloud,
    Google,
    Outlook,
    CalDav,
    Link,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::ICloud => "iCloud",
            Provider::Google => "Google",
            Provider::Outlook => "Outlook",
            Provider::CalDav => "CalDAV Account",
            Provider::Link => "Subscribed Calendar",
        }
    }

    /// The provider's own colour, for its badge.
    pub fn tint(self) -> [u8; 3] {
        match self {
            Provider::ICloud => [64, 156, 255],
            Provider::Google => [66, 133, 244],
            Provider::Outlook => [0, 114, 198],
            Provider::CalDav => [142, 142, 147],
            Provider::Link => [255, 149, 0],
        }
    }
}

/// How an account connects.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Method {
    SignIn,
    Password,
    Link,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: u64,
    pub provider: Provider,
    /// The address or user name it's signed in as (a link's calendar name).
    pub name: String,
    /// OAuth: the app registration it signed in through, and its refresh token.
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub refresh: String,
    /// CalDAV: server, user name and app-specific password. Links: the URL.
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

impl Account {
    pub fn method(&self) -> Method {
        match self.provider {
            _ if !self.refresh.is_empty() => Method::SignIn,
            Provider::Link => Method::Link,
            _ if self.user.is_empty() && !self.server.is_empty() => Method::Link,
            _ => Method::Password,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub title: String,
    pub location: String,
    /// Unix seconds. All-day events run from local midnight to local midnight.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub calendar: String,
    pub color: [u8; 3],
    /// Opens the event on the web, when the provider gives a link.
    pub link: String,
    pub account: u64,
}

impl Event {
    pub fn from_cal(e: CalEvent, all_day: bool, calendar: &str, color: [u8; 3], account: u64) -> Self {
        Event { title: e.title, location: e.location, start: e.start, end: e.end, all_day, calendar: calendar.to_string(), color, link: String::new(), account }
    }
}

/// One calendar of an account, as last read.
#[derive(Clone, Debug, PartialEq)]
pub struct Calendar {
    pub name: String,
    pub color: [u8; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Syncing,
    Ok { calendars: Vec<Calendar>, at: i64 },
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct AccountView {
    pub id: u64,
    pub provider: Provider,
    pub method: Method,
    pub name: String,
    pub status: Status,
}

// ---------------------------------------------------------------------------
// Setup (System Settings > Calendars)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Step {
    /// Which provider.
    Pick,
    /// The provider's choices, explained.
    Explain(Provider),
    /// Its own app registration, then the browser.
    SignIn(Provider),
    /// User name and app-specific password (and server, for CalDAV).
    Password(Provider),
    /// A private calendar link, and where to find it.
    Link(Provider),
}

/// What's typed during setup (kept out of config.toml).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Field {
    User,
    Password,
    Server,
    Link,
}

#[derive(Clone, Default, Debug)]
pub struct Draft {
    pub user: String,
    pub password: String,
    pub server: String,
    pub link: String,
}

#[derive(Clone, Debug)]
pub struct Setup {
    pub step: Step,
    pub draft: Draft,
    /// Connecting (waiting on the browser or the server).
    pub busy: bool,
    /// What happened last, and whether it's an error.
    pub message: Option<(String, bool)>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub accounts: Vec<AccountView>,
    pub setup: Option<Setup>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    accounts: Vec<Account>,
    events: HashMap<u64, Vec<Event>>,
    status: HashMap<u64, Status>,
    /// Access tokens: account → (token, expires at).
    tokens: HashMap<u64, (String, i64)>,
    /// Events from Windows' own calendar store (Settings > Accounts).
    windows: Vec<Event>,
    setup: Option<Setup>,
    gen: u64,
    loaded: bool,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    if !s.loaded {
        s.loaded = true;
        s.accounts = load();
    }
    f(&mut s)
}

static NOTIFY: AtomicIsize = AtomicIsize::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
static WAKE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// A sign-in in progress should give up.
pub(crate) fn cancelled() -> bool {
    CANCEL.load(Ordering::Relaxed)
}

pub fn now() -> i64 {
    crate::island::calendar::unix_now()
}

/// Tells the UI thread something changed.
fn changed(s: &mut State) {
    s.gen += 1;
    let hwnd = NOTIFY.load(Ordering::Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(HWND(hwnd as _), crate::win::WM_APP_CALENDAR, WPARAM(0), LPARAM(0));
        }
    }
}

/// Starts syncing; changes are announced to `hwnd` as `WM_APP_CALENDAR`.
pub fn start(hwnd: isize) {
    NOTIFY.store(hwnd, Ordering::Relaxed);
    if STARTED.swap(true, Ordering::Relaxed) {
        sync_now();
        return;
    }
    let spawned = std::thread::Builder::new().name("calendar sync".into()).spawn(|| {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        loop {
            sync_all();
            let (lock, cv) = &WAKE;
            let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
            if !*woken {
                woken = cv.wait_timeout(woken, REFRESH).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
            }
            *woken = false;
        }
    });
    if let Err(e) = spawned {
        log!("calendar sync thread failed: {e}");
    }
}

/// Reads every account again now (the worker does it).
pub fn sync_now() {
    let (lock, cv) = &WAKE;
    *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cv.notify_one();
}

fn sync_all() {
    let (from, to) = (now() - BACK, now() + AHEAD);
    let windows = windows_store(from, to);
    let accounts = with(|s| {
        s.windows = windows;
        for a in &s.accounts {
            if !s.status.contains_key(&a.id) {
                s.status.insert(a.id, Status::Syncing);
            }
        }
        changed(s);
        s.accounts.clone()
    });
    for a in accounts {
        let result = sync_account(&a, from, to);
        with(|s| {
            // Removed while it was being read.
            if !s.accounts.iter().any(|x| x.id == a.id) {
                return;
            }
            match result {
                Ok((events, calendars)) => {
                    s.events.insert(a.id, events);
                    s.status.insert(a.id, Status::Ok { calendars, at: now() });
                }
                Err(e) => {
                    log!("calendar: {} ({}) failed: {e}", a.provider.label(), host_of(&a));
                    // Keep the last good events while offline.
                    s.status.insert(a.id, Status::Failed(e));
                }
            }
            changed(s);
        });
    }
}

/// For logs: never the link itself (it's a secret).
fn host_of(a: &Account) -> String {
    match a.provider {
        Provider::Link => a.server.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or("link").to_string(),
        _ => a.provider.label().to_string(),
    }
}

type Synced = (Vec<Event>, Vec<Calendar>);

fn sync_account(a: &Account, from: i64, to: i64) -> Result<Synced, String> {
    match (a.provider, a.method()) {
        (_, Method::Link) => link_events(a, from, to),
        (Provider::Google, _) => {
            let token = access_token(a)?;
            google::events(&token, a.id, from, to).inspect_err(|_| forget_token(a.id))
        }
        (Provider::Outlook, _) => {
            let token = access_token(a)?;
            graph::events(&token, a.id, from, to).inspect_err(|_| forget_token(a.id))
        }
        _ => caldav::events(&a.server, &a.user, &a.password, a.id, from, to),
    }
}

fn link_events(a: &Account, from: i64, to: i64) -> Result<Synced, String> {
    let text = download_link(&a.server)?;
    let name = calendar_name(&text).unwrap_or_else(|| a.name.clone());
    let color = palette(&name);
    let events = crate::island::ics::occurrences(&text, from, to)
        .into_iter()
        .map(|(e, all_day)| Event::from_cal(e, all_day, &name, color, a.id))
        .collect();
    Ok((events, vec![Calendar { name, color }]))
}

fn download_link(url: &str) -> Result<String, String> {
    let url = match url.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    };
    let body = crate::net::get(&url)?;
    let text = String::from_utf8_lossy(&body).into_owned();
    if !text.contains("BEGIN:VCALENDAR") {
        return Err("that link isn't a calendar (.ics)".into());
    }
    Ok(text)
}

/// `X-WR-CALNAME`, the name calendar apps show for a subscribed calendar.
fn calendar_name(ics: &str) -> Option<String> {
    ics.lines().find_map(|l| l.strip_prefix("X-WR-CALNAME:")).map(|n| n.trim().replace("\\,", ",")).filter(|n| !n.is_empty())
}

/// Calendar's colours, for calendars that don't bring their own.
pub fn palette(name: &str) -> [u8; 3] {
    const COLORS: [[u8; 3]; 7] =
        [[255, 59, 48], [255, 149, 0], [255, 204, 0], [52, 199, 89], [0, 122, 255], [175, 82, 222], [162, 132, 94]];
    let h = name.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    COLORS[h as usize % COLORS.len()]
}

/// "#RRGGBB" (or "#RRGGBBAA").
pub fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().trim_start_matches('#');
    if s.len() < 6 {
        return None;
    }
    let c = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok();
    Some([c(0)?, c(2)?, c(4)?])
}

fn windows_store(from: i64, to: i64) -> Vec<Event> {
    match crate::island::calendar::windows_events(from, to) {
        Ok(list) => list.into_iter().map(|(e, all_day)| Event::from_cal(e, all_day, "Windows Calendar", [142, 142, 147], 0)).collect(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// OAuth clients and tokens
// ---------------------------------------------------------------------------

pub const GOOGLE_SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
pub const MICROSOFT_SCOPE: &str = "offline_access https://graph.microsoft.com/Calendars.Read https://graph.microsoft.com/User.Read";

fn client<'a>(provider: Provider, id: &'a str, secret: &'a str) -> oauth::Client<'a> {
    match provider {
        Provider::Google => oauth::Client {
            authorize: "https://accounts.google.com/o/oauth2/v2/auth",
            token: "https://oauth2.googleapis.com/token",
            client_id: id,
            client_secret: Some(secret).filter(|s| !s.is_empty()),
            scope: GOOGLE_SCOPE,
            redirect_host: "http://127.0.0.1",
            extra: &[("access_type", "offline"), ("prompt", "consent")],
        },
        _ => oauth::Client {
            authorize: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            token: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            client_id: id,
            client_secret: None,
            scope: MICROSOFT_SCOPE,
            redirect_host: "http://localhost",
            extra: &[("prompt", "select_account")],
        },
    }
}

fn access_token(a: &Account) -> Result<String, String> {
    if let Some(t) = with(|s| s.tokens.get(&a.id).filter(|(_, exp)| *exp > now()).map(|(t, _)| t.clone())) {
        return Ok(t);
    }
    let c = client(a.provider, &a.client_id, &a.client_secret);
    let fresh = oauth::refresh(&c, &a.refresh).map_err(|e| {
        if e.contains("HTTP 400") || e.contains("refused") {
            "the sign-in expired: remove the account and sign in again".to_string()
        } else {
            e
        }
    })?;
    with(|s| {
        s.tokens.insert(a.id, (fresh.access.clone(), fresh.expires_at));
        // Microsoft hands out a new refresh token each time.
        if let Some(acc) = s.accounts.iter_mut().find(|x| x.id == a.id) {
            if acc.refresh != fresh.refresh && !fresh.refresh.is_empty() {
                acc.refresh = fresh.refresh.clone();
                save(&s.accounts);
            }
        }
    });
    Ok(fresh.access)
}

fn forget_token(id: u64) {
    with(|s| s.tokens.remove(&id));
}

// ---------------------------------------------------------------------------
// Reading (UI thread)
// ---------------------------------------------------------------------------

pub fn snapshot() -> Snapshot {
    with(|s| Snapshot {
        accounts: s
            .accounts
            .iter()
            .map(|a| AccountView {
                id: a.id,
                provider: a.provider,
                method: a.method(),
                name: a.name.clone(),
                status: s.status.get(&a.id).cloned().unwrap_or(Status::Syncing),
            })
            .collect(),
        setup: s.setup.clone(),
    })
}

pub fn gen() -> u64 {
    with(|s| s.gen)
}

/// Every event overlapping `[from, to)`, sorted (all-day first on each day).
pub fn events_between(from: i64, to: i64) -> Vec<Event> {
    let mut out: Vec<Event> = with(|s| {
        s.events.values().flatten().chain(&s.windows).filter(|e| e.start < to && e.end.max(e.start + 1) > from).cloned().collect()
    });
    out.sort_by(|a, b| (a.start, !a.all_day, &a.title).cmp(&(b.start, !b.all_day, &b.title)));
    // The same event can come from two places (Windows' store and the account itself).
    out.dedup_by(|a, b| a.start == b.start && a.title == b.title && a.all_day == b.all_day);
    out
}

/// Timed events for the Dynamic Island.
pub fn timed_between(from: i64, to: i64) -> Vec<CalEvent> {
    with(|s| {
        s.events
            .values()
            .flatten()
            .filter(|e| !e.all_day && e.start < to && e.end > from)
            .map(|e| CalEvent { title: e.title.clone(), location: e.location.clone(), start: e.start, end: e.end })
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Changing accounts
// ---------------------------------------------------------------------------

pub fn remove(id: u64) {
    with(|s| {
        s.accounts.retain(|a| a.id != id);
        s.events.remove(&id);
        s.status.remove(&id);
        s.tokens.remove(&id);
        save(&s.accounts);
        changed(s);
    });
}

fn add(mut a: Account) {
    with(|s| {
        a.id = s.accounts.iter().map(|x| x.id).max().unwrap_or(0) + 1;
        // Signing in again as the same account replaces it.
        s.accounts.retain(|x| !(x.provider == a.provider && x.name.eq_ignore_ascii_case(&a.name) && x.method() == a.method()));
        s.status.insert(a.id, Status::Syncing);
        s.accounts.push(a);
        save(&s.accounts);
        changed(s);
    });
    sync_now();
}

/// Opens setup at `step` (`None` closes it).
pub fn setup(step: Option<Step>) {
    CANCEL.store(true, Ordering::Relaxed);
    with(|s| {
        s.setup = step.map(|step| {
            let draft = s.setup.take().map(|x| x.draft).unwrap_or_default();
            let mut draft = Draft { password: String::new(), ..draft };
            if let Step::Password(Provider::ICloud) = step {
                draft.server = String::new();
            }
            Setup { step, draft, busy: false, message: None }
        });
        changed(s);
    });
}

/// The step before this one.
pub fn setup_back() {
    let back = with(|s| s.setup.as_ref().map(|x| x.step)).map(|step| match step {
        Step::Pick => None,
        Step::Explain(_) => Some(Step::Pick),
        Step::SignIn(p) | Step::Password(p) | Step::Link(p) => {
            Some(if matches!(p, Provider::CalDav | Provider::Link) { Step::Pick } else { Step::Explain(p) })
        }
    });
    setup(back.flatten());
}

pub fn draft(field: Field) -> String {
    with(|s| {
        let d = s.setup.as_ref().map(|x| &x.draft);
        d.map(|d| match field {
            Field::User => d.user.clone(),
            Field::Password => d.password.clone(),
            Field::Server => d.server.clone(),
            Field::Link => d.link.clone(),
        })
        .unwrap_or_default()
    })
}

pub fn set_draft(field: Field, value: String) {
    with(|s| {
        if let Some(x) = &mut s.setup {
            match field {
                Field::User => x.draft.user = value,
                Field::Password => x.draft.password = value,
                Field::Server => x.draft.server = value,
                Field::Link => x.draft.link = value,
            }
            x.message = None;
            changed(s);
        }
    });
}

fn report(message: &str, error: bool, busy: bool) {
    with(|s| {
        if let Some(x) = &mut s.setup {
            x.message = Some((message.to_string(), error));
            x.busy = busy;
        }
        changed(s);
    });
}

/// Finishes the current setup step: signs in, checks the password, or reads the link.
pub fn connect(cfg: &CalendarCfg) {
    let Some(setup) = with(|s| s.setup.clone()) else { return };
    if setup.busy {
        return;
    }
    let step = setup.step;
    let d = setup.draft;
    // Problems that don't need the network.
    let missing = match step {
        Step::SignIn(Provider::Google) if cfg.google_client_id.trim().is_empty() || cfg.google_client_secret.trim().is_empty() => {
            Some("Paste your Google client ID and secret first.")
        }
        Step::SignIn(_) if step == Step::SignIn(Provider::Outlook) && cfg.microsoft_client_id.trim().is_empty() => {
            Some("Paste your Microsoft application (client) ID first.")
        }
        Step::Password(p) if d.user.trim().is_empty() || d.password.trim().is_empty() => Some(if p == Provider::ICloud {
            "Enter your Apple Account email and an app-specific password."
        } else {
            "Enter your user name and password."
        }),
        Step::Password(Provider::CalDav) if !d.server.trim().starts_with("https://") => Some("Enter the server's address, starting https://"),
        Step::Link(_) if !valid_link(&d.link) => Some("Paste a calendar link starting https:// or webcal://"),
        Step::Pick | Step::Explain(_) => return,
        _ => None,
    };
    if let Some(m) = missing {
        report(m, true, false);
        return;
    }
    CANCEL.store(false, Ordering::Relaxed);
    let cfg = cfg.clone();
    report(
        match step {
            Step::SignIn(_) => "Finish signing in in your browser\u{2026}",
            Step::Password(_) => "Checking your account\u{2026}",
            _ => "Reading the calendar\u{2026}",
        },
        false,
        true,
    );
    let spawned = std::thread::Builder::new().name("calendar setup".into()).spawn(move || {
        let result = match step {
            Step::SignIn(p) => sign_in(p, &cfg),
            Step::Password(p) => password_account(p, &d),
            Step::Link(p) => link_account(p, &d),
            _ => return,
        };
        match result {
            Ok(a) => {
                log!("calendar: added a {} account", a.provider.label());
                add(a);
                with(|s| {
                    s.setup = None;
                    changed(s);
                });
            }
            Err(e) if cancelled() => log!("calendar setup cancelled: {e}"),
            Err(e) => report(&capitalize(&e), true, false),
        }
    });
    if let Err(e) = spawned {
        report(&format!("Couldn't start: {e}"), true, false);
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect::<String>() + if s.ends_with('.') { "" } else { "." },
        None => String::new(),
    }
}

pub fn valid_link(s: &str) -> bool {
    let s = s.trim();
    (s.starts_with("https://") || s.starts_with("webcal://")) && s.len() > 12 && !s.contains(char::is_whitespace)
}

fn sign_in(p: Provider, cfg: &CalendarCfg) -> Result<Account, String> {
    let (id, secret) = match p {
        Provider::Google => (cfg.google_client_id.trim(), cfg.google_client_secret.trim()),
        _ => (cfg.microsoft_client_id.trim(), ""),
    };
    let tokens = oauth::sign_in(&client(p, id, secret))?;
    if tokens.refresh.is_empty() {
        return Err("the provider didn't allow offline access; try signing in again".into());
    }
    let name = match p {
        Provider::Google => google::account_name(&tokens.access)?,
        _ => graph::account_name(&tokens.access)?,
    };
    let a = Account {
        id: 0,
        provider: p,
        name,
        client_id: id.to_string(),
        client_secret: secret.to_string(),
        refresh: tokens.refresh,
        server: String::new(),
        user: String::new(),
        password: String::new(),
    };
    Ok(a)
}

fn password_account(p: Provider, d: &Draft) -> Result<Account, String> {
    let server = match p {
        Provider::ICloud => caldav::ICLOUD.to_string(),
        _ => d.server.trim().to_string(),
    };
    let (user, password) = (d.user.trim().to_string(), d.password.trim().to_string());
    let calendars = caldav::discover(&server, &user, &password).map_err(|e| {
        if e.contains("HTTP 401") || e.contains("HTTP 403") {
            if p == Provider::ICloud {
                "iCloud didn't accept that. Use an app-specific password (not your Apple Account password)".to_string()
            } else {
                "the server didn't accept that user name and password".to_string()
            }
        } else {
            e
        }
    })?;
    if calendars.is_empty() {
        return Err("signed in, but the account has no calendars".into());
    }
    Ok(Account { id: 0, provider: p, name: user.clone(), client_id: String::new(), client_secret: String::new(), refresh: String::new(), server, user, password })
}

fn link_account(p: Provider, d: &Draft) -> Result<Account, String> {
    let url = d.link.trim().to_string();
    let text = download_link(&url)?;
    let name = calendar_name(&text).unwrap_or_else(|| format!("{} Calendar", p.label()));
    Ok(Account {
        id: 0,
        provider: p,
        name,
        client_id: String::new(),
        client_secret: String::new(),
        refresh: String::new(),
        server: url,
        user: String::new(),
        password: String::new(),
    })
}

// ---------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------

fn path() -> std::path::PathBuf {
    crate::config::dir().join("calendars.dat")
}

fn load() -> Vec<Account> {
    let Ok(sealed) = std::fs::read(path()) else { return Vec::new() };
    let Some(plain) = crate::island::discord::unprotect(&sealed) else {
        log!("calendar: couldn't decrypt calendars.dat (made by another Windows user?)");
        return Vec::new();
    };
    serde_json::from_slice(&plain).unwrap_or_else(|e| {
        log!("calendar: calendars.dat unreadable: {e}");
        Vec::new()
    })
}

fn save(accounts: &[Account]) {
    let Ok(plain) = serde_json::to_vec(accounts) else { return };
    match crate::island::discord::protect(&plain) {
        Some(sealed) => {
            if let Err(e) = std::fs::write(path(), sealed) {
                log!("calendar: couldn't save accounts: {e}");
            }
        }
        None => log!("calendar: couldn't encrypt accounts"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex("#0b8043"), Some([11, 128, 67]));
        assert_eq!(parse_hex("#FF2968FF"), Some([255, 41, 104]));
        assert_eq!(parse_hex("nope"), None);
    }

    #[test]
    fn links() {
        assert!(valid_link("webcal://p01-caldav.icloud.com/published/2/abc"));
        assert!(!valid_link("http://example.com/cal.ics"));
        assert!(!valid_link("https://"));
    }

    #[test]
    fn calname() {
        assert_eq!(calendar_name("BEGIN:VCALENDAR\r\nX-WR-CALNAME:Work\\, Team\r\n").as_deref(), Some("Work, Team"));
    }
}
