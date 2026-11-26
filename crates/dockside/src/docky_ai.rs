//! Docky, chatting: he answers questions through the AI you choose.
//!
//! * **Claude** (`anthropic`): the Messages API with your own Anthropic API
//!   key. (Signing in with a Claude Free/Pro/Max subscription isn't offered:
//!   Anthropic's terms don't allow third-party apps to use subscription
//!   credentials.)
//! * **ChatGPT** (`chatgpt`): "Sign in with ChatGPT", so answers come out of
//!   your ChatGPT plan. DockSide registers itself as an open-source app
//!   through OpenAI's dynamic registration (under its own name), keeps the
//!   tokens only on this PC, and only ever asks when you do.
//! * **OpenAI-style** (`openai`) and **Anthropic-style**
//!   (`anthropic-compatible`): any API that speaks either, at a URL you give
//!   (local models like Ollama or LM Studio, proxies, other providers).
//!
//! Keys and sign-ins are kept encrypted to your Windows user (DPAPI) in
//! `docky.dat` next to config.toml, never in config.toml itself. A question
//! goes off on a worker thread; the UI is woken with `WM_APP_DOCKY` when the
//! answer is in. Only the newest question's answer is kept.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::calsync::oauth::{base64url, decode, random, sha256};
use crate::config::DockyCfg;

/// The model used for Claude unless `[docky] model` says otherwise.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// The providers: (config value, name in Settings).
pub const PROVIDERS: [(&str, &str); 5] = [
    ("anthropic", "Claude, with an Anthropic API key"),
    ("chatgpt", "ChatGPT, with your ChatGPT plan (Plus or Pro)"),
    ("cloudflare", "Cloudflare Workers AI: free, nothing kept or trained on"),
    ("openai", "Custom: an OpenAI-style API"),
    ("anthropic-compatible", "Custom: an Anthropic-style API"),
];

const ANTHROPIC_API: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2026-10-21";
/// Server-side fallback when a safety classifier declines (models that support it).
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const FALLBACK_MODELS: [&str; 5] = ["claude-fable-5-2", "claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];

const OPENAI_AUTH: &str = "https://auth.openai.com/api/accounts/authorize";
const OPENAI_TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const OPENAI_API: &str = "https://api.openai.com/v1";
const CHATGPT_SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// OpenAI's first-time registration entrypoint for open-source apps (not a client ID to keep).
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const APP_NAME: &str = "DockSide";

/// Cloudflare Workers AI: free (10,000 neurons a day), with zero data
/// retention and no training on what you send, per Cloudflare's docs.
const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4/accounts";
pub const CLOUDFLARE_SIGN_UP: &str = "https://dash.cloudflare.com/sign-up/workers-and-pages";
/// The dashboard's Workers AI page, where "Use REST API" makes a token and shows the Account ID.
pub const CLOUDFLARE_WORKERS_AI: &str = "https://dash.cloudflare.com/?to=/:account/ai/workers-ai";
/// Models that suit Docky's short chats on the free allowance (no paid-only
/// ones): (model, name in Settings). Daily counts assume a typical answer
/// (~300 tokens in, ~150 out) against Cloudflare's per-model prices (Nov 2026).
pub const CLOUDFLARE_MODELS: [(&str, &str); 4] = [
    ("@cf/google/gemma-4-26b-a4b-it", "Gemma 4 \u{00B7} smart and light (recommended) \u{00B7} ~1,400 answers a day free"),
    ("@cf/meta/llama-3.3-70b-instruct-fp8-fast", "Llama 3.3 70B \u{00B7} the smartest \u{00B7} ~250 answers a day free"),
    ("@cf/meta/llama-3.2-3b-instruct", "Llama 3.2 3B \u{00B7} the quickest \u{00B7} ~1,600 answers a day free"),
    ("@cf/openai/gpt-oss-20b", "gpt-oss 20B \u{00B7} thinks first \u{00B7} fewer answers a day"),
];

const PERSONA: &str = "You are Docky, a small, round, friendly pet who lives on the user's Windows desktop \
inside an app called DockSide. You talk like a cute little pet: lowercase, short and warm, now and then a \
small noise like \"mrrp\" or \"hehe\" (not every time). But you are also genuinely smart and helpful: give \
correct, useful answers. Keep replies to a few short sentences unless the user asks for more detail. Write \
plain text only: no markdown, no headings, no bullet symbols.";

// ---------------------------------------------------------------------------
// Settings, kept where the worker threads can see them
// ---------------------------------------------------------------------------

static CFG: Mutex<Option<DockyCfg>> = Mutex::new(None);

/// The `[docky]` settings questions go out with (set whenever config loads).
pub fn set_config(cfg: &DockyCfg) {
    if let Ok(mut c) = CFG.lock() {
        *c = Some(cfg.clone());
    }
}

fn config() -> Option<DockyCfg> {
    CFG.lock().ok()?.clone()
}

// ---------------------------------------------------------------------------
// Secrets (docky.dat, DPAPI)
// ---------------------------------------------------------------------------

fn secrets_path() -> PathBuf {
    crate::config::dir().join("docky.dat")
}

/// Everything kept secret, as one JSON object: API keys by provider, and the
/// ChatGPT sign-in under "chatgpt".
fn load_secrets() -> Map<String, Value> {
    let Ok(raw) = std::fs::read(secrets_path()) else { return Map::new() };
    let Some(plain) = crate::island::discord::unprotect(&raw) else { return Map::new() };
    match serde_json::from_slice::<Value>(&plain) {
        Ok(Value::Object(m)) => m,
        // Before there were providers, docky.dat held just the Anthropic key.
        _ => match String::from_utf8(plain) {
            Ok(k) if !k.trim().is_empty() => Map::from_iter([("anthropic".to_string(), Value::String(k.trim().into()))]),
            _ => Map::new(),
        },
    }
}

fn save_secrets(m: &Map<String, Value>) -> bool {
    let plain = serde_json::to_vec(&Value::Object(m.clone())).unwrap_or_default();
    match crate::island::discord::protect(&plain) {
        Some(sealed) => std::fs::write(secrets_path(), sealed).is_ok(),
        None => false,
    }
}

/// The API key stored for `slot` ("anthropic" or "custom").
fn key(slot: &str) -> Option<String> {
    load_secrets().get(slot).and_then(Value::as_str).map(str::to_string).filter(|k| !k.is_empty())
}

/// Stores the API key for `slot` encrypted (or, empty, forgets it).
pub fn save_key(slot: &str, value: &str) -> bool {
    let mut m = load_secrets();
    let value = value.trim();
    if value.is_empty() {
        m.remove(slot);
    } else {
        m.insert(slot.into(), Value::String(value.into()));
    }
    save_secrets(&m)
}

/// How a stored key shows in Settings: dots and its last four characters.
pub fn key_hint(slot: &str) -> String {
    match key(slot) {
        Some(k) => {
            let tail: String = k.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
            format!("\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}{tail}")
        }
        None => String::new(),
    }
}

/// Whether Docky can answer with the chosen provider, or (in his words) what's missing.
pub fn ready() -> Result<(), String> {
    let cfg = config().ok_or("not ready")?;
    match cfg.provider.as_str() {
        "chatgpt" if chatgpt().is_none() => Err("sign in with chatgpt first! (settings \u{203A} docky)".into()),
        "cloudflare" if cfg.cloudflare_account.trim().is_empty() || key("cloudflare").is_none() => {
            Err("finish the cloudflare setup first! (settings \u{203A} docky)".into())
        }
        "cloudflare" => Ok(()),
        "openai" | "anthropic-compatible" if cfg.custom_url.trim().is_empty() => Err("i need the api's address first! (settings \u{203A} docky)".into()),
        "openai" | "anthropic-compatible" | "chatgpt" => Ok(()),
        _ if key("anthropic").is_none() => Err("i need an api key first! (settings \u{203A} docky)".into()),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Asking
// ---------------------------------------------------------------------------

static NEXT: AtomicU64 = AtomicU64::new(1);
/// The newest question's answer: its id and the reply (or what went wrong, in his words).
static ANSWER: Mutex<Option<(u64, Result<String, String>)>> = Mutex::new(None);

/// Asks Docky `question` on a worker thread; `WM_APP_DOCKY` is posted to `hwnd`
/// when the answer (see [`take`]) is in. Returns the question's id.
pub fn ask(question: String, hwnd: isize) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::thread::spawn(move || {
        let reply = answer(&[Turn { me: true, text: question }]);
        if let Ok(mut a) = ANSWER.lock() {
            *a = Some((id, reply));
        }
        if hwnd != 0 {
            crate::win::post(hwnd, crate::win::WM_APP_DOCKY);
        }
    });
    id
}

/// One message in a conversation: yours (`me`) or Docky's.
#[derive(Clone, Debug)]
pub struct Turn {
    pub me: bool,
    pub text: String,
}

/// Docky's reply to the conversation so far (its last turn is yours), from the
/// chosen provider; what went wrong comes back in his words.
fn answer(turns: &[Turn]) -> Result<String, String> {
    let reply = match config() {
        Some(cfg) => match cfg.provider.as_str() {
            "chatgpt" => ask_chatgpt(turns, &cfg),
            "openai" => ask_openai_style(turns, &cfg),
            "cloudflare" => ask_cloudflare(turns, &cfg),
            "anthropic-compatible" => ask_anthropic(turns, &cfg, true),
            _ => ask_anthropic(turns, &cfg, false),
        },
        None => Err("no config".into()),
    };
    if let Err(e) = &reply {
        log!("docky: question failed: {e}");
    }
    reply.map_err(|e| pet_error(&e))
}

/// The conversation as alternating user / assistant messages (consecutive
/// ones from the same side joined), starting with yours.
fn alternating(turns: &[Turn]) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    for t in turns.iter().filter(|t| !t.text.trim().is_empty()) {
        match out.last_mut() {
            Some((me, text)) if *me == t.me => {
                text.push_str("\n\n");
                text.push_str(&t.text);
            }
            _ => out.push((t.me, t.text.clone())),
        }
    }
    while out.first().is_some_and(|(me, _)| !me) {
        out.remove(0);
    }
    out
}

// ---------------------------------------------------------------------------
// The conversation (the chat window and the widget's speech box share it)
// ---------------------------------------------------------------------------

/// How many recent messages go with each question.
const CONTEXT: usize = 24;
/// How many messages are kept.
const KEEP: usize = 300;

/// A message in the conversation, with when it was sent (Unix seconds).
#[derive(Clone, Debug)]
pub struct Message {
    pub me: bool,
    pub text: String,
    pub at: i64,
}

struct Chat {
    loaded: bool,
    messages: Vec<Message>,
    /// Docky is thinking about a reply.
    pending: bool,
    /// Bumped on every change, so views know to redraw.
    version: u64,
}

static CHAT: Mutex<Chat> = Mutex::new(Chat { loaded: false, messages: Vec::new(), pending: false, version: 0 });

fn chat_path() -> PathBuf {
    crate::config::dir().join("docky-chat.dat")
}

fn with_chat<R>(f: impl FnOnce(&mut Chat) -> R) -> Option<R> {
    let mut c = CHAT.lock().ok()?;
    if !c.loaded {
        c.loaded = true;
        c.messages = load_chat();
    }
    Some(f(&mut c))
}

/// The conversation, kept encrypted (DPAPI) like the keys: it's private.
fn load_chat() -> Vec<Message> {
    let Ok(raw) = std::fs::read(chat_path()) else { return Vec::new() };
    let Some(plain) = crate::island::discord::unprotect(&raw) else { return Vec::new() };
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&plain) else { return Vec::new() };
    items
        .iter()
        .filter_map(|m| {
            Some(Message {
                me: m.get("me")?.as_bool()?,
                text: m.get("text")?.as_str()?.to_string(),
                at: m.get("at").and_then(Value::as_i64).unwrap_or(0),
            })
        })
        .collect()
}

fn save_chat(messages: &[Message]) {
    let v: Vec<Value> = messages.iter().map(|m| json!({ "me": m.me, "text": m.text, "at": m.at })).collect();
    let plain = serde_json::to_vec(&v).unwrap_or_default();
    if let Some(sealed) = crate::island::discord::protect(&plain) {
        let _ = std::fs::write(chat_path(), sealed);
    }
}

/// The conversation so far, whether he's thinking, and its version.
pub fn chat() -> (Vec<Message>, bool, u64) {
    with_chat(|c| (c.messages.clone(), c.pending, c.version)).unwrap_or_default()
}

/// Whether Docky is thinking about a reply.
pub fn chat_pending() -> bool {
    with_chat(|c| c.pending).unwrap_or(false)
}

/// His newest message, if it's the latest thing said.
pub fn last_reply() -> Option<String> {
    with_chat(|c| c.messages.last().filter(|m| !m.me).map(|m| m.text.clone())).flatten()
}

/// Says `text` to Docky; he answers on a worker thread (`WM_APP_DOCKY` is
/// posted to `hwnd` when he has). False if he's still busy with the last one.
pub fn send(text: &str, hwnd: isize) -> bool {
    let text = text.trim().to_string();
    if text.is_empty() {
        return false;
    }
    let turns = with_chat(|c| {
        if c.pending {
            return None;
        }
        c.messages.push(Message { me: true, text, at: now() });
        c.pending = true;
        c.version += 1;
        let from = c.messages.len().saturating_sub(CONTEXT);
        Some(c.messages[from..].iter().map(|m| Turn { me: m.me, text: m.text.clone() }).collect::<Vec<_>>())
    })
    .flatten();
    let Some(turns) = turns else { return false };
    std::thread::spawn(move || {
        let reply = answer(&turns).unwrap_or_else(|said| said);
        with_chat(|c| {
            c.messages.push(Message { me: false, text: reply, at: now() });
            if c.messages.len() > KEEP {
                let extra = c.messages.len() - KEEP;
                c.messages.drain(..extra);
            }
            c.pending = false;
            c.version += 1;
            save_chat(&c.messages);
        });
        crate::win::post(hwnd, crate::win::WM_APP_DOCKY);
    });
    true
}

/// Replaces the conversation in memory (tests), without touching docky-chat.dat.
#[cfg(test)]
pub fn seed_chat(messages: Vec<Message>, pending: bool) {
    if let Ok(mut c) = CHAT.lock() {
        c.loaded = true;
        c.messages = messages;
        c.pending = pending;
        c.version += 1;
    }
}

/// Forgets the conversation.
pub fn clear_chat() {
    with_chat(|c| {
        c.messages.clear();
        c.version += 1;
    });
    let _ = std::fs::remove_file(chat_path());
}

static TESTING: AtomicBool = AtomicBool::new(false);
static TESTED: Mutex<Option<Result<String, String>>> = Mutex::new(None);

/// Asks a quick hello with the current settings (Settings' Test button);
/// `WM_APP_DOCKY` is posted to `hwnd` when it's done.
pub fn test(hwnd: isize) {
    if TESTING.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Ok(mut t) = TESTED.lock() {
        *t = None;
    }
    let id = ask("Say hi to me in one short sentence.".into(), 0);
    std::thread::spawn(move || {
        let started = Instant::now();
        let reply = loop {
            if let Some(r) = take(id) {
                break r;
            }
            if started.elapsed() > Duration::from_secs(130) {
                break Err("*sad mrrp* no answer\u{2026}".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if let Ok(mut t) = TESTED.lock() {
            *t = Some(reply);
        }
        TESTING.store(false, Ordering::Relaxed);
        crate::win::post(hwnd, crate::win::WM_APP_DOCKY);
    });
}

/// The Test button's state: testing, and the last result (his reply, or what went wrong).
pub fn test_status() -> (bool, Option<Result<String, String>>) {
    (TESTING.load(Ordering::Relaxed), TESTED.lock().ok().and_then(|t| t.clone()))
}

/// The answer to question `id`, once it's in.
pub fn take(id: u64) -> Option<Result<String, String>> {
    let mut a = ANSWER.lock().ok()?;
    match a.as_ref() {
        Some((got, _)) if *got == id => a.take().map(|(_, r)| r),
        _ => None,
    }
}

/// `base` (an API's address) with `path` on the end, unless it's already there.
fn endpoint(base: &str, path: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.ends_with(path) {
        base.to_string()
    } else {
        format!("{base}{path}")
    }
}

/// Claude, or (custom) any API that speaks Anthropic's Messages format.
fn ask_anthropic(turns: &[Turn], cfg: &DockyCfg, custom: bool) -> Result<String, String> {
    let key = key(if custom { "custom" } else { "anthropic" });
    if !custom && key.is_none() {
        return Err("no key".into());
    }
    let model = if cfg.model.trim().is_empty() { DEFAULT_MODEL } else { cfg.model.trim() };
    let mut body = json!({
        "model": model,
        "max_tokens": 16000,
        "system": PERSONA,
        "messages": alternating(turns)
            .into_iter()
            .map(|(me, text)| json!({ "role": if me { "user" } else { "assistant" }, "content": text }))
            .collect::<Vec<_>>(),
    });
    // Claude itself: quick answers at low effort, with server-side fallback.
    let fallback = !custom && FALLBACK_MODELS.contains(&model);
    if !custom {
        body["output_config"] = json!({ "effort": "low" });
    }
    if fallback {
        body["fallbacks"] = json!("default");
    }
    let url = if custom { endpoint(&cfg.custom_url, "/v1/messages") } else { ANTHROPIC_API.to_string() };
    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let bearer = key.as_ref().map(|k| format!("Bearer {k}"));
    let mut headers = vec![("content-type", "application/json"), ("anthropic-version", ANTHROPIC_VERSION)];
    if let Some(k) = &key {
        headers.push(("x-api-key", k.as_str()));
    }
    // Some Anthropic-style services want the key as a bearer token instead.
    if let (true, Some(b)) = (custom, &bearer) {
        headers.push(("authorization", b.as_str()));
    }
    if fallback {
        headers.push(("anthropic-beta", FALLBACK_BETA));
    }
    let (status, raw) = crate::net::request_with("POST", &url, &headers, Some(&body), 120_000)?;
    let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("bad reply (HTTP {status}): {e}"))?;
    if !(200..300).contains(&status) {
        return Err(api_error(status, &v));
    }
    if v.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        return Ok("*tilts head* mm\u{2026} i can't help with that one.".into());
    }
    let text: String = v
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    non_empty(text)
}

/// Any API that speaks OpenAI's Chat Completions format.
fn ask_openai_style(turns: &[Turn], cfg: &DockyCfg) -> Result<String, String> {
    chat_completions(&endpoint(&cfg.custom_url, "/chat/completions"), key("custom"), cfg.custom_model.trim(), turns)
}

/// Cloudflare Workers AI, through its OpenAI-compatible endpoint.
fn ask_cloudflare(turns: &[Turn], cfg: &DockyCfg) -> Result<String, String> {
    let account = cfg.cloudflare_account.trim();
    if account.is_empty() {
        return Err("no account".into());
    }
    let token = key("cloudflare").ok_or("no key")?;
    let model = match cfg.cloudflare_model.trim() {
        "" => CLOUDFLARE_MODELS[0].0,
        m => m,
    };
    let url = format!("{CLOUDFLARE_API}/{}/ai/v1/chat/completions", crate::net::encode(account));
    chat_completions(&url, Some(token), model, turns)
}

/// A Chat Completions request (OpenAI's format, which many services speak).
fn chat_completions(url: &str, key: Option<String>, model: &str, turns: &[Turn]) -> Result<String, String> {
    let mut messages = vec![json!({ "role": "system", "content": PERSONA })];
    messages.extend(
        alternating(turns).into_iter().map(|(me, text)| json!({ "role": if me { "user" } else { "assistant" }, "content": text })),
    );
    let body = json!({ "model": model, "messages": messages });
    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let bearer = key.map(|k| format!("Bearer {k}"));
    let mut headers = vec![("content-type", "application/json")];
    if let Some(b) = &bearer {
        headers.push(("authorization", b.as_str()));
    }
    let (status, raw) = crate::net::request_with("POST", url, &headers, Some(&body), 120_000)?;
    let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("bad reply (HTTP {status}): {e}"))?;
    if !(200..300).contains(&status) {
        return Err(api_error(status, &v));
    }
    let text = v.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or_default().to_string();
    non_empty(text)
}

/// ChatGPT, on the signed-in user's plan: the Responses API, streamed.
fn ask_chatgpt(turns: &[Turn], cfg: &DockyCfg) -> Result<String, String> {
    let token = chatgpt_token()?;
    let model = match cfg.chatgpt_model.trim() {
        "" => chatgpt_default_model(&token)?,
        m => m.to_string(),
    };
    // The plan-usage docs only show user messages, so his persona rides along in it.
    let body = json!({
        "model": model,
        "input": [{ "role": "user", "content": chatgpt_prompt(turns) }],
        "store": false,
        "stream": true,
    });
    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let bearer = format!("Bearer {token}");
    let headers = [("content-type", "application/json"), ("authorization", bearer.as_str())];
    let (status, raw) = crate::net::request_with("POST", &format!("{OPENAI_API}/responses"), &headers, Some(&body), 120_000)?;
    let raw = String::from_utf8_lossy(&raw);
    if !(200..300).contains(&status) {
        let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        return Err(api_error(status, &v));
    }
    // Server-sent events: text arrives as deltas; it only counts once completed.
    let mut text = String::new();
    let mut completed = false;
    for data in raw.lines().filter_map(|l| l.strip_prefix("data:")) {
        let Ok(ev) = serde_json::from_str::<Value>(data.trim()) else { continue };
        match ev.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => text.push_str(ev.get("delta").and_then(Value::as_str).unwrap_or_default()),
            Some("response.completed") => completed = true,
            Some("response.failed") | Some("response.incomplete") => {
                let code = ev.pointer("/response/error/code").and_then(Value::as_str).unwrap_or("unknown_error");
                return Err(format!("chatgpt: {code}"));
            }
            _ => {}
        }
    }
    if !completed {
        return Err("chatgpt: the answer didn't finish".into());
    }
    non_empty(text)
}

/// The plan-usage docs only show user messages, so his persona and the
/// conversation so far ride along in one.
fn chatgpt_prompt(turns: &[Turn]) -> String {
    let turns = alternating(turns);
    let mut s = format!("{PERSONA}\n\n");
    if turns.len() > 1 {
        s.push_str("The conversation so far:\n");
        for (me, text) in &turns[..turns.len() - 1] {
            s.push_str(if *me { "User: " } else { "Docky: " });
            s.push_str(text);
            s.push('\n');
        }
        s.push('\n');
    }
    if let Some((_, last)) = turns.last() {
        s.push_str("The user now says: ");
        s.push_str(last);
    }
    s
}

/// The first model the signed-in account's plan offers.
fn chatgpt_default_model(token: &str) -> Result<String, String> {
    let bearer = format!("Bearer {token}");
    let (status, raw) = crate::net::request_with("GET", &format!("{OPENAI_API}/models"), &[("authorization", bearer.as_str())], None, 30_000)?;
    let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("bad model list (HTTP {status}): {e}"))?;
    if !(200..300).contains(&status) {
        return Err(api_error(status, &v));
    }
    v.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("visibility").and_then(Value::as_str).is_none_or(|vis| vis == "list"))
        .find_map(|m| m.get("slug").or_else(|| m.get("id")).and_then(Value::as_str).map(str::to_string))
        .ok_or_else(|| "chatgpt: no models offered".to_string())
}

fn non_empty(text: String) -> Result<String, String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        Err("empty reply".into())
    } else {
        Ok(text)
    }
}

/// An API's error reply, as one line for the log.
fn api_error(status: u32, v: &Value) -> String {
    let msg = v
        .pointer("/error/message")
        .or_else(|| v.pointer("/errors/0/message"))
        .or_else(|| v.pointer("/error/code"))
        .or_else(|| v.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("");
    format!("HTTP {status}: {msg}")
}

/// What went wrong, as Docky would put it.
fn pet_error(e: &str) -> String {
    let lower = e.to_lowercase();
    if lower.contains("neuron") || (lower.contains("daily") && lower.contains("limit")) {
        return "i've used up today's free brain\u{2026} back tomorrow!".into();
    }
    if e == "no account" {
        return "i need your cloudflare account id first! (settings \u{203A} docky)".into();
    }
    if e == "no key" {
        "i need an api key first! (settings \u{203A} docky)".into()
    } else if e.contains("not signed in") {
        "sign in with chatgpt first! (settings \u{203A} docky)".into()
    } else if e.contains("subscription_sharing_usage_limit_exceeded") || e.contains("subscription_sharing_usage_unavailable") {
        "your chatgpt plan's all used up for now\u{2026}".into()
    } else if e.contains("subscription_sharing_user_not_eligible") {
        "this needs chatgpt plus or pro\u{2026} (a free local model works too!)".into()
    } else if e.contains("HTTP 401") || e.contains("HTTP 403") {
        "hmm, i'm not allowed in\u{2026} check settings?".into()
    } else if e.contains("HTTP 429") {
        "too many questions! give me a breather\u{2026}".into()
    } else if e.contains("HTTP 529") || e.contains("overloaded") {
        "the clouds are busy\u{2026} try again in a bit?".into()
    } else if e.contains("HTTP 400") || e.contains("HTTP 404") {
        "i got confused by that one\u{2026} (check the model in settings?)".into()
    } else {
        "*sad mrrp* couldn't reach my brain\u{2026}".into()
    }
}

// ---------------------------------------------------------------------------
// Sign in with ChatGPT
// ---------------------------------------------------------------------------

/// The ChatGPT sign-in, as kept in docky.dat.
struct ChatGpt {
    client_id: String,
    access: String,
    refresh: String,
    /// Unix seconds.
    expires_at: i64,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn chatgpt() -> Option<ChatGpt> {
    let m = load_secrets();
    let c = m.get("chatgpt")?;
    let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let g = ChatGpt { client_id: s("client_id"), access: s("access"), refresh: s("refresh"), expires_at: c.get("expires_at").and_then(Value::as_i64).unwrap_or(0) };
    (!g.access.is_empty()).then_some(g)
}

fn store_chatgpt(g: &ChatGpt) -> bool {
    let mut m = load_secrets();
    let entry = m.entry("chatgpt").or_insert_with(|| json!({}));
    entry["client_id"] = json!(g.client_id);
    entry["access"] = json!(g.access);
    entry["refresh"] = json!(g.refresh);
    entry["expires_at"] = json!(g.expires_at);
    save_secrets(&m)
}

/// This PC's stable `ext_agent_host_id` for OpenAI's registration: a random
/// UUIDv4 as `urn:uuid:…` (one of the formats OpenAI accepts), made once
/// before the first sign-in and kept in docky.dat. (An early build kept a bare
/// random string, which OpenAI rejects: that's replaced.)
fn host_id() -> String {
    let mut m = load_secrets();
    if let Some(id) = m.get("host_id").and_then(Value::as_str).filter(|id| id.starts_with("urn:uuid:")) {
        return id.to_string();
    }
    let id = format!("urn:uuid:{}", uuid_v4(&random(16)));
    m.insert("host_id".into(), json!(id));
    save_secrets(&m);
    id
}

/// A UUIDv4 in its hyphenated form, from 16 random bytes.
fn uuid_v4(bytes: &[u8]) -> String {
    let mut b = [0u8; 16];
    b.copy_from_slice(&bytes[..16]);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// The client ID OpenAI issued this install on first sign-in (kept across sign-outs).
fn issued_client_id() -> Option<String> {
    load_secrets().get("chatgpt").and_then(|c| c.get("client_id")).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty())
}

/// Whether ChatGPT is signed in.
pub fn chatgpt_signed_in() -> bool {
    chatgpt().is_some()
}

static SIGNING_IN: AtomicBool = AtomicBool::new(false);
static SIGN_IN_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Whether a ChatGPT sign-in is under way, and what went wrong with the last one.
pub fn chatgpt_status() -> (bool, Option<String>) {
    (SIGNING_IN.load(Ordering::Relaxed), SIGN_IN_ERROR.lock().ok().and_then(|e| e.clone()))
}

/// Signs in with ChatGPT in the browser (on a worker thread); `WM_APP_DOCKY` is
/// posted to `hwnd` when it's done, either way.
pub fn chatgpt_sign_in(hwnd: isize) {
    if SIGNING_IN.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Ok(mut e) = SIGN_IN_ERROR.lock() {
        *e = None;
    }
    std::thread::spawn(move || {
        let r = chatgpt_flow();
        if let Err(e) = &r {
            log!("docky: chatgpt sign-in failed: {e}");
            if let Ok(mut slot) = SIGN_IN_ERROR.lock() {
                *slot = Some(e.clone());
            }
        }
        SIGNING_IN.store(false, Ordering::Relaxed);
        crate::win::post(hwnd, crate::win::WM_APP_DOCKY);
    });
}

/// Forgets the ChatGPT sign-in (the issued client ID and this PC's id stay, for
/// next time), and revokes it with OpenAI in the background.
pub fn chatgpt_sign_out() {
    if let Some(g) = chatgpt().filter(|g| !g.refresh.is_empty()) {
        std::thread::spawn(move || {
            if let Err(e) = revoke(&g) {
                log!("docky: couldn't revoke the chatgpt sign-in: {e}");
            }
        });
    }
    let mut m = load_secrets();
    if let Some(Value::Object(c)) = m.get_mut("chatgpt") {
        c.retain(|k, _| k == "client_id");
    }
    save_secrets(&m);
}

/// Sends `ext_agent_host_id`: OpenAI's reference kit only does so for
/// "a compatible authorization deployment" (off by default), so neither do we.
const SEND_HOST_ID: bool = false;

/// The browser sign-in, as OpenAI's reference kit does it. A code that's
/// refused (used or expired, as can happen right after first registering)
/// starts one fresh sign-in as the now-registered app, never a new registration.
fn chatgpt_flow() -> Result<(), String> {
    match chatgpt_attempt() {
        Err(e) if e.contains("invalid_grant") => {
            log!("docky: chatgpt code refused ({e}); signing in again as the registered app");
            chatgpt_attempt()
        }
        r => r,
    }
}

/// One sign-in: PKCE with a loopback redirect on 127.0.0.1, registering
/// DockSide (as an open-source app, under its own name) the first time.
fn chatgpt_attempt() -> Result<(), String> {
    // OpenAI's example port, else any free one (only the port may change between sign-ins).
    let listener = TcpListener::bind("127.0.0.1:1455")
        .or_else(|_| TcpListener::bind("127.0.0.1:0"))
        .map_err(|e| format!("couldn't listen for the sign-in: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}/auth/callback");
    // 32 random bytes each, base64url, fresh every attempt.
    let verifier = base64url(&random(32));
    let challenge = base64url(&sha256(verifier.as_bytes()));
    let (state, nonce) = (base64url(&random(32)), base64url(&random(32)));
    let known = issued_client_id();
    let enc = crate::net::encode;
    let mut url = format!(
        "{OPENAI_AUTH}?client_id={}&response_type=code&redirect_uri={}&scope={}&resource={}&state={state}&nonce={nonce}&code_challenge_method=S256&code_challenge={challenge}",
        enc(known.as_deref().unwrap_or(DYNAMIC_CLIENT)),
        enc(&redirect),
        enc(CHATGPT_SCOPE),
        enc(OPENAI_API),
    );
    if SEND_HOST_ID {
        url.push_str(&format!("&ext_agent_host_id={}", enc(&host_id())));
    }
    if known.is_none() {
        url.push_str(&format!("&agent_name_hint={}", enc(APP_NAME)));
    }
    crate::win::system::open(&url);

    let (code, issued) = wait_for_callback(&listener, &state)?;
    if let (Some(k), Some(i)) = (&known, &issued) {
        if k != i {
            return Err("chatgpt sent back a different app registration; try signing in again".into());
        }
    }
    let client_id = issued.or(known).ok_or("the sign-in didn't register DockSide (no client id came back)")?;
    // Keep the registration before trading the code, so a retry reuses it.
    remember_client_id(&client_id);
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id.as_str()),
        ("code", code.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("resource", OPENAI_API),
    ];
    let v = token_request(&form)?;
    let granted = v.get("scope").and_then(Value::as_str).unwrap_or(CHATGPT_SCOPE);
    if !granted.split_whitespace().any(|s| s == "chatgpt.tokens.use.direct") {
        return Err("plan usage wasn't allowed during sign-in".into());
    }
    let g = tokens_from(&v, client_id, None)?;
    if !store_chatgpt(&g) {
        return Err("couldn't store the sign-in".into());
    }
    Ok(())
}

/// Saves the client ID OpenAI issued DockSide (kept across sign-outs).
fn remember_client_id(id: &str) {
    let mut m = load_secrets();
    let entry = m.entry("chatgpt").or_insert_with(|| json!({}));
    entry["client_id"] = json!(id);
    save_secrets(&m);
}

/// A current access token (refreshed when it's about to expire).
fn chatgpt_token() -> Result<String, String> {
    let g = chatgpt().ok_or("not signed in")?;
    if g.expires_at > now() + 60 {
        return Ok(g.access);
    }
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", g.client_id.as_str()),
        ("refresh_token", g.refresh.as_str()),
        ("resource", OPENAI_API),
    ];
    let v = token_request(&form).map_err(|e| format!("not signed in (refresh failed: {e})"))?;
    let fresh = tokens_from(&v, g.client_id.clone(), Some(&g.refresh))?;
    store_chatgpt(&fresh);
    Ok(fresh.access)
}

/// Ends the renewable session: the revocation endpoint from OpenAI's discovery
/// document, with the refresh token (an empty 200 is success).
fn revoke(g: &ChatGpt) -> Result<(), String> {
    let (_, raw) = crate::net::request_with("GET", "https://auth.openai.com/.well-known/openid-configuration", &[], None, 15_000)?;
    let v: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
    let url = v.get("revocation_endpoint").and_then(Value::as_str).ok_or("no revocation endpoint")?;
    let enc = crate::net::encode;
    let body = format!("token={}&token_type_hint=refresh_token&client_id={}", enc(&g.refresh), enc(&g.client_id));
    let headers = [("content-type", "application/x-www-form-urlencoded")];
    match crate::net::request_with("POST", url, &headers, Some(body.as_bytes()), 15_000)? {
        (200, _) => Ok(()),
        (status, _) => Err(format!("HTTP {status}")),
    }
}

fn token_request(form: &[(&str, &str)]) -> Result<Value, String> {
    let body: Vec<String> = form.iter().map(|(k, v)| format!("{k}={}", crate::net::encode(v))).collect();
    let headers = [("content-type", "application/x-www-form-urlencoded"), ("accept", "application/json")];
    let (status, raw) = crate::net::request_with("POST", OPENAI_TOKEN, &headers, Some(body.join("&").as_bytes()), 30_000)?;
    let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("bad token reply (HTTP {status}): {e}"))?;
    if !(200..300).contains(&status) {
        let code = v.get("error").and_then(Value::as_str).unwrap_or("");
        let why = v.get("error_description").and_then(Value::as_str).unwrap_or("");
        return Err(if why.is_empty() { format!("HTTP {status} {code}") } else { format!("HTTP {status} {code}: {why}") });
    }
    Ok(v)
}

fn tokens_from(v: &Value, client_id: String, old_refresh: Option<&str>) -> Result<ChatGpt, String> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let access = s("access_token");
    if access.is_empty() {
        return Err("no access token came back".into());
    }
    let refresh = match s("refresh_token") {
        r if r.is_empty() => old_refresh.unwrap_or_default().to_string(),
        r => r,
    };
    Ok(ChatGpt { client_id, access, refresh, expires_at: now() + v.get("expires_in").and_then(Value::as_i64).unwrap_or(3600) })
}

/// Waits for the browser to come back to `/auth/callback`: the code and (first
/// time) the client ID OpenAI issued DockSide.
fn wait_for_callback(listener: &TcpListener, state: &str) -> Result<(String, Option<String>), String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let until = Instant::now() + Duration::from_secs(300);
    while Instant::now() < until {
        let (mut stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(150));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        let Some(target) = head.lines().next().and_then(|l| l.split_whitespace().nth(1)) else { continue };
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let param = |name: &str| query.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| decode(v)));
        if path != "/auth/callback" || (param("code").is_none() && param("error").is_none()) {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        let result = match (param("code"), param("error"), param("state")) {
            (_, Some(err), _) => Err(match err.as_str() {
                "access_denied" => "you declined the sign-in".to_string(),
                _ => format!("the sign-in failed: {}", param("error_description").unwrap_or(err)),
            }),
            (Some(_), _, s) if s.as_deref() != Some(state) => Err("the sign-in came back for a different request".into()),
            (Some(code), _, _) => Ok((
                code,
                param("client_id").filter(|c| {
                    c != DYNAMIC_CLIENT && (1..=200).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                }),
            )),
            _ => Err("the sign-in came back without a code".into()),
        };
        let (title, line) = match &result {
            Ok(_) => ("Docky's connected!", "You can close this tab and go back to DockSide."),
            Err(_) => ("Sign-in didn't finish", "Go back to DockSide to try again."),
        };
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>DockSide</title>\
             <body style=\"font-family:-apple-system,'Segoe UI',sans-serif;display:grid;place-items:center;height:90vh;color:#222\">\
             <div style=\"text-align:center\"><h2>{title}</h2><p style=\"color:#666\">{line}</p></div>"
        );
        let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
        let _ = stream.write_all(reply.as_bytes());
        return result;
    }
    Err("the browser didn't come back in time".into())
}

// ---------------------------------------------------------------------------
// Showing answers
// ---------------------------------------------------------------------------

/// Splits an answer into lines of about `width` characters, for Spotlight's rows.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    #[test]
    fn wraps_by_words() {
        let lines = super::wrap("hello there friend\n\nhow are you", 11);
        assert_eq!(lines, vec!["hello there", "friend", "how are you"]);
    }

    #[test]
    fn conversations_alternate() {
        use super::Turn;
        let t = |me: bool, s: &str| Turn { me, text: s.into() };
        let a = super::alternating(&[t(false, "hi"), t(true, "a"), t(true, "b"), t(false, "c"), t(true, "")]);
        assert_eq!(a, vec![(true, "a\n\nb".to_string()), (false, "c".to_string())]);
    }

    #[test]
    fn host_ids_are_uuid_v4() {
        let id = super::uuid_v4(&[0xff; 16]);
        assert_eq!(id, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(id.len(), 36);
    }

    #[test]
    fn endpoints_join_once() {
        assert_eq!(super::endpoint("http://localhost:11434/v1/", "/chat/completions"), "http://localhost:11434/v1/chat/completions");
        assert_eq!(super::endpoint("https://x.test/v1/chat/completions", "/chat/completions"), "https://x.test/v1/chat/completions");
        assert_eq!(super::endpoint("https://proxy.test", "/v1/messages"), "https://proxy.test/v1/messages");
    }
}
