use std::sync::Mutex;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub const REPO_URL: &str = "https://github.com/BoxDevelopment/DockSide";
const LATEST_API: &str = "https://api.github.com/repos/BoxDevelopment/DockSide/releases/latest";

static AVAILABLE: Mutex<Option<Release>> = Mutex::new(None);

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub url: String,
}

type Version = (u64, u64, u64, Option<u64>);

pub fn current() -> String {
    match option_env!("DOCKSIDE_BUILD") {
        Some(b) => format!("{} (build {b})", env!("CARGO_PKG_VERSION")),
        None => format!("{} (local build)", env!("CARGO_PKG_VERSION")),
    }
}

fn current_version() -> Option<Version> {
    let (ma, mi, pa, _) = parse(env!("CARGO_PKG_VERSION"))?;
    Some((ma, mi, pa, option_env!("DOCKSIDE_BUILD").and_then(|b| b.parse().ok())))
}

pub fn available() -> Option<Release> {
    AVAILABLE.lock().ok().and_then(|a| a.clone())
}

fn parse(tag: &str) -> Option<Version> {
    let tag = tag.trim().trim_start_matches(['v', 'V']);
    let (ver, build) = match tag.split_once("-build.") {
        Some((v, b)) => (v, Some(b.parse().ok()?)),
        None => (tag, None),
    };
    let mut parts = ver.split('.').map(|p| p.parse::<u64>().ok());
    let (ma, mi, pa) = (parts.next()??, parts.next().flatten().unwrap_or(0), parts.next().flatten().unwrap_or(0));
    Some((ma, mi, pa, build))
}

fn is_newer(latest: Version, current: Version) -> bool {
    let (l, c) = ((latest.0, latest.1, latest.2), (current.0, current.1, current.2));
    if l != c {
        return l > c;
    }
    matches!((latest.3, current.3), (Some(lb), Some(cb)) if lb > cb)
}

fn fetch_latest() -> windows::core::Result<String> {
    use windows::Foundation::Uri;
    use windows::Web::Http::HttpClient;
    let client = HttpClient::new()?;
    let headers = client.DefaultRequestHeaders()?;
    headers.UserAgent()?.TryParseAdd(&HSTRING::from(format!("DockSide/{}", env!("CARGO_PKG_VERSION"))))?;
    headers.Accept()?.TryParseAdd(&HSTRING::from("application/vnd.github+json"))?;
    let uri = Uri::CreateUri(&HSTRING::from(LATEST_API))?;
    Ok(client.GetStringAsync(&uri)?.get()?.to_string())
}

fn check() -> Option<Release> {
    let body = match fetch_latest() {
        Ok(b) => b,
        Err(e) => {
            log!("update check failed: {e}");
            return None;
        }
    };
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = json.get("tag_name")?.as_str()?.to_string();
    let url = json.get("html_url").and_then(|u| u.as_str()).unwrap_or(REPO_URL).to_string();
    let newer = is_newer(parse(&tag)?, current_version()?);
    log!("update check: running {}, latest release {tag}{}", current(), if newer { " (update available)" } else { "" });
    newer.then_some(Release { tag, url })
}

pub fn watch(hwnd: isize, msg: u32) {
    let spawned = std::thread::Builder::new().name("update".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        std::thread::sleep(Duration::from_secs(10));
        loop {
            if let Some(r) = check() {
                if let Ok(mut a) = AVAILABLE.lock() {
                    *a = Some(r);
                }
                crate::win::post(hwnd, msg);
                return;
            }
            std::thread::sleep(Duration::from_secs(24 * 60 * 60));
        }
    });
    if let Err(e) = spawned {
        log!("update thread failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(parse("v0.1.0-build.21"), Some((0, 1, 0, Some(21))));
        assert_eq!(parse("0.2.0"), Some((0, 2, 0, None)));
        assert_eq!(parse("v1.2"), Some((1, 2, 0, None)));
        assert_eq!(parse("nightly"), None);
    }

    #[test]
    fn newer() {
        let p = |s| parse(s).unwrap();
        assert!(is_newer(p("v0.2.0-build.21"), p("v0.1.0-build.30")));
        assert!(is_newer(p("v0.2.0-build.22"), p("v0.2.0-build.21")));
        assert!(!is_newer(p("v0.2.0-build.21"), p("v0.2.0-build.21")));
        assert!(!is_newer(p("v0.1.0-build.40"), p("v0.2.0-build.1")));
        assert!(!is_newer(p("v0.2.0-build.21"), p("0.2.0")));
        assert!(is_newer(p("v0.3.0-build.1"), p("0.2.0")));
    }

    #[test]
    #[ignore]
    fn live_release() {
        let body = fetch_latest().expect("GitHub reachable");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parse(json["tag_name"].as_str().unwrap()).is_some());
    }
}
