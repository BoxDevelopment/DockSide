//! Where the picker's icons come from.
//!
//! * [Dashboard Icons](https://github.com/homarr-labs/dashboard-icons)
//!   (Apache-2.0): ~3,400 app and service logos. Its index (`metadata.json`,
//!   every icon's name and aliases) is downloaded once a week and searched
//!   locally, so typing costs no requests. Icons come from jsDelivr.
//! * [macOSicons.com](https://macosicons.com/developers): 30,000+ Mac-style
//!   icons, searched with the user's own API key. Each search is one request
//!   from their monthly allowance, so it only searches when asked.
//!
//! Network and decoding run on a few worker threads; results queue up in
//! `Shared` and the UI thread is told with a posted message.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::gfx::icons::IconPixels;

const DASHBOARD_CDN: &str = "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons";
const MACOS_SEARCH: &str = "https://api.macosicons.com/api/v1/search";
/// Re-download the index when the cached copy is older than this.
const INDEX_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Most results kept per search.
const MAX_RESULTS: usize = 240;
/// Thumbnail size in pixels.
pub const THUMB_PX: u32 = 96;
const WORKERS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Dashboard,
    MacOs,
}

/// One icon in the Dashboard Icons index.
#[derive(Clone, Debug)]
pub struct Entry {
    pub slug: String,
    /// `slug` and aliases, lower-cased with only letters and digits.
    keys: Vec<String>,
}

/// A search result.
#[derive(Clone, Debug)]
pub struct Hit {
    pub label: String,
    /// Full-size PNG.
    pub url: String,
    pub source: Source,
    /// Who made it (macOSicons designers), shown under the grid.
    pub credit: Option<String>,
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// "visual-studio-code" → "Visual Studio Code".
fn title(slug: &str) -> String {
    slug.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn cache_dir() -> PathBuf {
    crate::config::dir().join("icon-library")
}

/// Where chosen icons are kept (the config points at files here).
pub fn chosen_dir() -> PathBuf {
    crate::config::dir().join("icons")
}

/// Parses Dashboard Icons' `metadata.json`: `{ "slug": { "aliases": [...], ... }, ... }`.
pub fn parse_index(json: &str) -> Result<Vec<Entry>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let map = v.as_object().ok_or("index isn't an object")?;
    let mut out: Vec<Entry> = map
        .iter()
        .map(|(slug, meta)| {
            let aliases: Vec<String> = meta
                .get("aliases")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let keys = std::iter::once(slug.as_str()).chain(aliases.iter().map(String::as_str)).map(squash).collect();
            Entry { slug: slug.clone(), keys }
        })
        .collect();
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(out)
}

/// The index, from the cache when it's fresh enough (or the download fails).
fn load_index() -> Result<Vec<Entry>, String> {
    let path = cache_dir().join("dashboard-index.json");
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < INDEX_MAX_AGE);
    if !fresh {
        match crate::net::get(&format!("{DASHBOARD_CDN}/metadata.json")) {
            Ok(body) => {
                let _ = std::fs::create_dir_all(cache_dir());
                let _ = std::fs::write(&path, &body);
                return parse_index(&String::from_utf8_lossy(&body));
            }
            Err(e) => log!("icon library index download failed: {e}"),
        }
    }
    let text = std::fs::read_to_string(&path).map_err(|_| "Couldn't reach the icon library".to_string())?;
    parse_index(&text)
}

/// Dashboard Icons matching `query`, best first: exact names, then names that
/// start with it, then ones containing every word of it (aliases count too).
pub fn search(index: &[Entry], query: &str) -> Vec<Hit> {
    let q = squash(query);
    let words: Vec<String> = query.split_whitespace().map(squash).filter(|w| !w.is_empty()).collect();
    if q.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u8, &Entry)> = index
        .iter()
        .filter_map(|e| {
            let score = e.keys.iter().enumerate().map(|(i, k)| {
                let alias = (i > 0) as u8;
                if *k == q {
                    alias
                } else if k.starts_with(&q) {
                    2 + alias
                } else if k.contains(&q) || words.iter().all(|w| k.contains(w.as_str())) {
                    4 + alias
                } else {
                    u8::MAX
                }
            });
            score.min().filter(|&s| s != u8::MAX).map(|s| (s, e))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.slug.len().cmp(&b.1.slug.len())).then(a.1.slug.cmp(&b.1.slug)));
    scored
        .into_iter()
        .take(MAX_RESULTS)
        .map(|(_, e)| Hit {
            label: title(&e.slug),
            url: format!("{DASHBOARD_CDN}/png/{}.png", e.slug),
            source: Source::Dashboard,
            credit: None,
        })
        .collect()
}

/// Parses a macOSicons search response.
pub fn parse_macos(json: &str) -> Result<Vec<Hit>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    if let Some(msg) = v.get("message").or_else(|| v.get("error")).and_then(|m| m.as_str()) {
        if v.get("hits").is_none() {
            return Err(msg.to_string());
        }
    }
    let hits = v.get("hits").and_then(|h| h.as_array()).ok_or("unexpected response")?;
    Ok(hits
        .iter()
        .filter_map(|h| {
            let url = h.get("lowResPngUrl").and_then(|u| u.as_str()).filter(|u| u.starts_with("https://"))?;
            Some(Hit {
                label: h.get("appName").and_then(|n| n.as_str()).unwrap_or("Icon").to_string(),
                url: url.to_string(),
                source: Source::MacOs,
                credit: h.get("credit").and_then(|c| c.as_str()).filter(|c| !c.is_empty()).map(str::to_string),
            })
        })
        .take(MAX_RESULTS)
        .collect())
}

fn search_macos(key: &str, query: &str) -> Result<Vec<Hit>, String> {
    let body = serde_json::json!({ "query": query }).to_string();
    let headers = [("Content-Type", "application/json"), ("x-api-key", key)];
    let reply = crate::net::request("POST", MACOS_SEARCH, &headers, Some(body.as_bytes())).map_err(|e| match e.as_str() {
        "HTTP 401" | "HTTP 403" => "macOSicons rejected the API key".to_string(),
        "HTTP 429" => "This month's macOSicons searches are used up".to_string(),
        _ => format!("macOSicons search failed ({e})"),
    })?;
    parse_macos(&String::from_utf8_lossy(&reply))
}

/// A stable file name for an icon's URL (FNV-1a).
fn file_name(url: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}.png")
}

/// The cached full-size file for `url`, if it's been downloaded.
pub fn cached(url: &str) -> Option<PathBuf> {
    let p = cache_dir().join("png").join(file_name(url));
    p.exists().then_some(p)
}

fn download(url: &str) -> Result<PathBuf, String> {
    if let Some(p) = cached(url) {
        return Ok(p);
    }
    let body = crate::net::get(url)?;
    let png = body.starts_with(&[0x89, b'P', b'N', b'G']);
    let jpeg = body.starts_with(&[0xFF, 0xD8]);
    if !png && !jpeg {
        return Err("not an image".into());
    }
    let dir = cache_dir().join("png");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(file_name(url));
    std::fs::write(&path, &body).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Decodes an image file scaled to fit `px` square (premultiplied BGRA).
fn decode(path: &Path, px: u32) -> Option<IconPixels> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    unsafe {
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let decoder = wic.CreateDecoderFromFilename(&HSTRING::from(path.as_os_str()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        let converter = wic.CreateFormatConverter().ok()?;
        converter
            .Initialize(&decoder.GetFrame(0).ok()?, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        converter.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 {
            return None;
        }
        let s = (px as f32 / w.max(h) as f32).min(1.0);
        let (tw, th) = (((w as f32 * s).round() as u32).max(1), ((h as f32 * s).round() as u32).max(1));
        let scaler = wic.CreateBitmapScaler().ok()?;
        scaler.Initialize(&converter, tw, th, WICBitmapInterpolationModeHighQualityCubic).ok()?;
        let mut data = vec![0u8; (tw * th * 4) as usize];
        scaler.CopyPixels(std::ptr::null(), tw * 4, &mut data).ok()?;
        Some(IconPixels { width: tw, height: th, data })
    }
}

enum Job {
    Index,
    Thumb(String),
    MacSearch { key: String, query: String, generation: u32 },
}

pub enum Done {
    Index(Result<Vec<Entry>, String>),
    /// A thumbnail for a URL (its full-size file is cached by now).
    Thumb(String, Option<IconPixels>),
    MacSearch(u32, Result<Vec<Hit>, String>),
}

/// The picker's background workers.
pub struct Library {
    jobs: Sender<Job>,
    done: Arc<Mutex<Vec<Done>>>,
}

impl Library {
    /// Starts the workers; each finished job posts `msg` to `hwnd`. They exit
    /// once the `Library` is dropped and the queue has drained.
    pub fn start(hwnd: isize, msg: u32) -> Self {
        let (jobs, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let done = Arc::new(Mutex::new(Vec::new()));
        for i in 0..WORKERS {
            let (rx, done) = (rx.clone(), done.clone());
            let spawned = std::thread::Builder::new().name(format!("icon-library-{i}")).spawn(move || worker(rx, done, hwnd, msg));
            if let Err(e) = spawned {
                log!("icon library worker failed: {e}");
            }
        }
        let lib = Self { jobs, done };
        let _ = lib.jobs.send(Job::Index);
        lib
    }

    pub fn thumb(&self, url: &str) {
        let _ = self.jobs.send(Job::Thumb(url.to_string()));
    }

    pub fn search_macos(&self, key: &str, query: &str, generation: u32) {
        let _ = self.jobs.send(Job::MacSearch { key: key.to_string(), query: query.to_string(), generation });
    }

    pub fn take(&self) -> Vec<Done> {
        self.done.lock().map(|mut d| std::mem::take(&mut *d)).unwrap_or_default()
    }
}

fn worker(rx: Arc<Mutex<Receiver<Job>>>, done: Arc<Mutex<Vec<Done>>>, hwnd: isize, msg: u32) {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    loop {
        let job = match rx.lock() {
            Ok(rx) => rx.recv(),
            Err(_) => return,
        };
        let Ok(job) = job else { return };
        let result = match job {
            Job::Index => Done::Index(load_index()),
            Job::Thumb(url) => {
                let px = download(&url).map_err(|e| log!("icon download failed ({url}): {e}")).ok().and_then(|p| decode(&p, THUMB_PX));
                Done::Thumb(url, px)
            }
            Job::MacSearch { key, query, generation } => Done::MacSearch(generation, search_macos(&key, &query)),
        };
        if let Ok(mut d) = done.lock() {
            d.push(result);
        }
        crate::win::post(hwnd, msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = r#"{
        "visual-studio-code": { "base": "svg", "aliases": ["vscode"], "categories": [] },
        "code-server": { "base": "svg", "aliases": [], "categories": [] },
        "spotify": { "base": "svg", "aliases": [], "categories": [] },
        "spotify-connect": { "base": "png", "aliases": [], "categories": [] }
    }"#;

    #[test]
    fn searches_names_and_aliases() {
        let index = parse_index(INDEX).unwrap();
        let labels = |q: &str| search(&index, q).into_iter().map(|h| h.label).collect::<Vec<_>>();
        assert_eq!(labels("spotify"), ["Spotify", "Spotify Connect"]);
        assert_eq!(labels("Visual Studio Code")[0], "Visual Studio Code");
        assert_eq!(labels("vscode"), ["Visual Studio Code"]);
        assert_eq!(labels("studio code"), ["Visual Studio Code"]);
        assert!(labels("").is_empty());
        assert!(search(&index, "spotify")[0].url.ends_with("/png/spotify.png"));
    }

    #[test]
    fn parses_macos_hits() {
        let hits = parse_macos(r#"{"hits":[{"appName":"Safari","lowResPngUrl":"https://x/safari.png","credit":"Ann"},{"appName":"Bad"}],"totalHits":2}"#).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].label, "Safari");
        assert_eq!(hits[0].credit.as_deref(), Some("Ann"));
        assert!(parse_macos(r#"{"message":"Invalid API key"}"#).is_err());
    }

    /// Needs the network: `cargo test icon_library -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn icon_library_live() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let index = load_index().unwrap();
        let hits = search(&index, "spotify");
        println!("{} icons in the index; 'spotify' → {:?}", index.len(), hits.iter().take(3).map(|h| &h.label).collect::<Vec<_>>());
        assert!(index.len() > 1000);
        let path = download(&hits[0].url).unwrap();
        assert!(decode(&path, THUMB_PX).is_some());
    }
}
