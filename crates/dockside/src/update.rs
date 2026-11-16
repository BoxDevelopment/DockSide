//! Updates: a daily check for a newer release on GitHub, and (opted in with
//! `[general] updates = "install"`) downloading it, checking it against
//! DockSide's release signing key, and installing it when the user says so.
//!
//! Installing swaps the exe in place, so its path (and the admin task and
//! Start at Login that point at it) stays the same: the running exe is renamed
//! `dockside.old.exe` (Windows allows renaming a running exe) and the new one
//! written in its place. DockSide then quits and relaunches the usual way
//! (through the admin task when elevated). The old exe stays behind as a
//! guard: if the new version isn't running half a minute later, the guard puts
//! the old one back and starts it. Only official builds update; a local build
//! never replaces itself.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::config::Updates;

pub const REPO_URL: &str = "https://github.com/BoxDevelopment/DockSide";
const LATEST_API: &str = "https://api.github.com/repos/BoxDevelopment/DockSide/releases/latest";
/// The key releases are signed with. Its secret half is the release workflow's
/// `DOCKSIDE_SIGNING_KEY`; `dockside --update-keygen <file>` makes a new pair.
const PUBLIC_KEY: &str = "27bc0cc93f6492e424b40ca652ca8cda6b8cd850c194766f696bbbf69945e02b";
/// The release's files: the exe, and its signature (hex).
const EXE_ASSET: &str = "dockside.exe";
const SIG_ASSET: &str = "dockside.exe.sig";
/// Run by the old exe: watch the new one's first seconds, putting the old one back if it dies.
pub const GUARD: &str = "--update-guard";
/// How long a new version must stay up for the update to count as good.
const GUARD_SECS: u64 = 30;
/// What an island notice of a ready update carries, so a click installs it.
pub const INSTALL_NOTE: &str = "dockside:install-update";

static AVAILABLE: Mutex<Option<Release>> = Mutex::new(None);
/// A downloaded, verified update: its tag and file.
static STAGED: Mutex<Option<(String, PathBuf)>> = Mutex::new(None);
static MODE: AtomicU8 = AtomicU8::new(1);
/// Started by an update just now (shown on the island once).
static JUST_UPDATED: AtomicBool = AtomicBool::new(false);
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
/// Where `watch` reports (window, message), for checks started by a setting change.
static NOTIFY: Mutex<Option<(isize, u32)>> = Mutex::new(None);

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub url: String,
    exe_url: Option<String>,
    sig_url: Option<String>,
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

/// The tag of an update that's downloaded, verified and ready to install.
pub fn staged() -> Option<String> {
    STAGED.lock().ok().and_then(|s| s.as_ref().map(|(t, _)| t.clone()))
}

pub fn just_updated() -> bool {
    JUST_UPDATED.load(Ordering::Relaxed)
}

fn mode() -> Updates {
    match MODE.load(Ordering::Relaxed) {
        0 => Updates::Off,
        2 => Updates::Install,
        _ => Updates::Notify,
    }
}

/// Follows the setting; turning on Install fetches an update already found.
pub fn set_mode(m: Updates) {
    let before = mode();
    MODE.store(
        match m {
            Updates::Off => 0,
            Updates::Notify => 1,
            Updates::Install => 2,
        },
        Ordering::Relaxed,
    );
    // Turned on (or up to Install): look now rather than at the next daily check.
    let target = NOTIFY.lock().ok().and_then(|n| *n);
    if m != before && m != Updates::Off {
        if let Some((hwnd, msg)) = target {
            std::thread::spawn(move || {
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let Some(r) = available().or_else(check) else { return };
                if let Ok(mut a) = AVAILABLE.lock() {
                    *a = Some(r.clone());
                }
                if mode() == Updates::Install {
                    stage(&r);
                }
                crate::win::post_wp(hwnd, msg, 0);
            });
        }
    }
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

fn client() -> windows::core::Result<windows::Web::Http::HttpClient> {
    let client = windows::Web::Http::HttpClient::new()?;
    let headers = client.DefaultRequestHeaders()?;
    headers.UserAgent()?.TryParseAdd(&HSTRING::from(format!("DockSide/{}", env!("CARGO_PKG_VERSION"))))?;
    Ok(client)
}

fn fetch_latest() -> windows::core::Result<String> {
    use windows::Foundation::Uri;
    let client = client()?;
    client.DefaultRequestHeaders()?.Accept()?.TryParseAdd(&HSTRING::from("application/vnd.github+json"))?;
    let uri = Uri::CreateUri(&HSTRING::from(LATEST_API))?;
    Ok(client.GetStringAsync(&uri)?.get()?.to_string())
}

fn download(url: &str) -> windows::core::Result<Vec<u8>> {
    use windows::Foundation::Uri;
    use windows::Storage::Streams::DataReader;
    let client = client()?;
    let resp = client.GetAsync(&Uri::CreateUri(&HSTRING::from(url))?)?.get()?;
    resp.EnsureSuccessStatusCode()?;
    let buf = resp.Content()?.ReadAsBufferAsync()?.get()?;
    let mut bytes = vec![0u8; buf.Length()? as usize];
    DataReader::FromBuffer(&buf)?.ReadBytes(&mut bytes)?;
    Ok(bytes)
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
    let asset = |name: &str| {
        json.get("assets")?
            .as_array()?
            .iter()
            .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name))?
            .get("browser_download_url")?
            .as_str()
            .map(str::to_string)
    };
    let newer = is_newer(parse(&tag)?, current_version()?);
    log!("update check: running {}, latest release {tag}{}", current(), if newer { " (update available)" } else { "" });
    newer.then(|| Release { exe_url: asset(EXE_ASSET), sig_url: asset(SIG_ASSET), tag, url })
}

// ---------------------------------------------------------------------------
// Signatures
// ---------------------------------------------------------------------------

/// What a release's signature covers: its tag and its exe, so an older
/// signed exe can't be passed off as a newer release.
fn signed_message(tag: &str, exe: &[u8]) -> Vec<u8> {
    let mut m = format!("dockside-update\n{tag}\n").into_bytes();
    m.extend_from_slice(exe);
    m
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    s.len().is_multiple_of(2).then_some(())?;
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Whether `sig` (hex) is the release key's signature of `exe` as release `tag`.
fn verify_with(key: &str, tag: &str, exe: &[u8], sig: &str) -> bool {
    use ed25519_compact::{PublicKey, Signature};
    let (Some(pk), Some(sig)) = (unhex(key), unhex(sig)) else { return false };
    let (Ok(pk), Ok(sig)) = (PublicKey::from_slice(&pk), Signature::from_slice(&sig)) else { return false };
    pk.verify(signed_message(tag, exe), &sig).is_ok()
}

fn verify(tag: &str, exe: &[u8], sig: &str) -> bool {
    verify_with(PUBLIC_KEY, tag, exe, sig)
}

fn sign_with(seed_hex: &str, tag: &str, exe: &[u8]) -> Option<String> {
    use ed25519_compact::{KeyPair, Seed};
    let seed: [u8; 32] = unhex(seed_hex)?.try_into().ok()?;
    let kp = KeyPair::from_seed(Seed::new(seed));
    Some(hex(&kp.sk.sign(signed_message(tag, exe), None)[..]))
}

/// `dockside --update-sign <exe> <tag>`: writes `<exe>.sig`, signed with the
/// key in `DOCKSIDE_SIGNING_KEY` (the release workflow runs this).
pub fn sign_cli(args: &[String]) -> i32 {
    let (Some(exe), Some(tag)) = (args.first(), args.get(1)) else { return 2 };
    let Ok(seed) = std::env::var("DOCKSIDE_SIGNING_KEY") else { return 3 };
    let Ok(bytes) = std::fs::read(exe) else { return 4 };
    let Some(sig) = sign_with(&seed, tag, &bytes) else { return 5 };
    match std::fs::write(format!("{exe}.sig"), sig) {
        Ok(()) => 0,
        Err(_) => 6,
    }
}

/// `dockside --update-keygen <file>`: writes a new signing key pair to `file`.
pub fn keygen_cli(args: &[String]) -> i32 {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let Some(out) = args.first() else { return 2 };
    let mut seed = [0u8; 32];
    if unsafe { BCryptGenRandom(BCRYPT_ALG_HANDLE::default(), &mut seed, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.is_err() {
        return 3;
    }
    let kp = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new(seed));
    let text = format!("DOCKSIDE_SIGNING_KEY (GitHub secret):\n{}\n\nPUBLIC_KEY (update.rs):\n{}\n", hex(&seed), hex(&kp.pk[..]));
    if std::fs::write(out, text).is_err() {
        return 4;
    }
    0
}

// ---------------------------------------------------------------------------
// Checking, downloading, installing
// ---------------------------------------------------------------------------

fn staging_dir() -> PathBuf {
    crate::config::dir().join("update")
}

/// Left by an install for the version it starts (holds the tag).
fn installed_marker() -> PathBuf {
    crate::config::dir().join("update-installed")
}

/// A release build from the workflow (a local build has no build number).
fn official() -> bool {
    option_env!("DOCKSIDE_BUILD").is_some()
}

/// Downloads `r` and keeps it if its signature checks out.
fn stage(r: &Release) -> bool {
    if !official() {
        log!("update {}: this is a local build, so it isn't replaced", r.tag);
        return false;
    }
    if staged().as_deref() == Some(r.tag.as_str()) || DOWNLOADING.swap(true, Ordering::Relaxed) {
        return false;
    }
    let done = (|| {
        let (Some(exe_url), Some(sig_url)) = (&r.exe_url, &r.sig_url) else {
            log!("update {}: not signed, so it can't be installed from DockSide", r.tag);
            return false;
        };
        let t = Instant::now();
        let (exe, sig) = match (download(exe_url), download(sig_url)) {
            (Ok(e), Ok(s)) => (e, String::from_utf8_lossy(&s).to_string()),
            (Err(e), _) | (_, Err(e)) => {
                log!("update {}: download failed: {e}", r.tag);
                return false;
            }
        };
        if !verify(&r.tag, &exe, &sig) {
            log!("update {}: signature doesn't match, discarded", r.tag);
            return false;
        }
        let dir = staging_dir();
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(format!("dockside-{}.exe", r.tag));
        let ok = std::fs::create_dir_all(&dir).is_ok()
            && std::fs::write(&path, &exe).is_ok()
            && std::fs::write(path.with_extension("exe.sig"), &sig).is_ok();
        if ok {
            log!("update {}: downloaded and verified in {:.1} s ({} KB)", r.tag, t.elapsed().as_secs_f32(), exe.len() / 1024);
            if let Ok(mut s) = STAGED.lock() {
                *s = Some((r.tag.clone(), path));
            }
        }
        ok
    })();
    DOWNLOADING.store(false, Ordering::Relaxed);
    done
}

/// Checks for updates 10 s after launch and then daily, telling `hwnd`
/// (`msg`, wParam 0) when one is found or ready, or (wParam 1) that this
/// launch installed one.
pub fn watch(hwnd: isize, msg: u32) {
    if let Ok(mut n) = NOTIFY.lock() {
        *n = Some((hwnd, msg));
    }
    let spawned = std::thread::Builder::new().name("update".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        if std::fs::remove_file(installed_marker()).is_ok() {
            JUST_UPDATED.store(true, Ordering::Relaxed);
            crate::win::post_wp(hwnd, msg, 1);
        }
        std::thread::sleep(Duration::from_secs(10));
        // What a previous update left behind (the guard has let go of it by now).
        if !just_updated() {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::fs::remove_file(exe.with_file_name("dockside.old.exe"));
                let _ = std::fs::remove_file(exe.with_file_name("dockside.failed.exe"));
            }
        }
        let mut told: Option<String> = None;
        loop {
            if mode() != Updates::Off {
                if let Some(r) = check() {
                    if let Ok(mut a) = AVAILABLE.lock() {
                        *a = Some(r.clone());
                    }
                    let ready = mode() == Updates::Install && stage(&r);
                    let key = format!("{}{}", r.tag, if ready || staged().is_some() { "+ready" } else { "" });
                    if told.as_deref() != Some(key.as_str()) {
                        told = Some(key);
                        crate::win::post_wp(hwnd, msg, 0);
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(24 * 60 * 60));
        }
    });
    if let Err(e) = spawned {
        log!("update thread failed: {e}");
    }
}

/// Installs the staged update: swaps the exe and asks for the usual relaunch
/// when this instance quits (the caller then quits).
pub fn install() -> Result<String, String> {
    if !official() {
        return Err("this is a local build; updates only replace official builds".into());
    }
    let (tag, staged) = STAGED.lock().ok().and_then(|s| s.clone()).ok_or("no update is ready")?;
    // Checked again: the file sat in a folder others can write to.
    let exe = std::fs::read(&staged).map_err(|e| format!("can't read the update: {e}"))?;
    let sig = std::fs::read_to_string(staged.with_extension("exe.sig")).unwrap_or_default();
    if !verify(&tag, &exe, &sig) {
        return Err("the update's signature doesn't match".into());
    }
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = swap_in(&current, &exe)?;
    let _ = std::fs::write(installed_marker(), &tag);
    let guard = std::process::Command::new(&old)
        .args([GUARD, &std::process::id().to_string(), &current.display().to_string()])
        .spawn();
    if let Err(e) = guard {
        log!("update {tag}: no guard ({e}); installing anyway");
    }
    let _ = std::fs::remove_dir_all(staging_dir());
    if let Ok(mut s) = STAGED.lock() {
        *s = None;
    }
    crate::win::elevation::request_relaunch(crate::win::elevation::is_elevated());
    log!("update {tag}: installed, restarting");
    Ok(tag)
}

/// Moves the exe at `current` aside to `dockside.old.exe` and writes `exe`
/// in its place; gives the old one's path. Puts everything back on failure.
fn swap_in(current: &Path, exe: &[u8]) -> Result<PathBuf, String> {
    let old = current.with_file_name("dockside.old.exe");
    if old.exists() && std::fs::remove_file(&old).is_err() {
        return Err("a previous update is still finishing; try again in a minute".into());
    }
    std::fs::rename(current, &old).map_err(|e| format!("can't replace {}: {e}", current.display()))?;
    if let Err(e) = std::fs::write(current, exe) {
        let _ = std::fs::remove_file(current);
        let _ = std::fs::rename(&old, current);
        return Err(format!("can't write {}: {e}", current.display()));
    }
    Ok(old)
}

/// Puts the old exe (`old`, running this guard) back at `exe`, keeping the
/// new one as `dockside.failed.exe`.
fn roll_back(exe: &Path, old: &Path) -> bool {
    let failed = exe.with_file_name("dockside.failed.exe");
    let _ = std::fs::remove_file(&failed);
    if std::fs::rename(exe, &failed).is_err() {
        return false;
    }
    if std::fs::rename(old, exe).is_err() {
        let _ = std::fs::rename(&failed, exe);
        return false;
    }
    true
}

/// Whether a process is running the exe at `path`.
fn running(path: &Path) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    let me = std::process::id();
    let want = path.display().to_string();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = false;
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more && !found {
            found = e.th32ProcessID != me
                && crate::win::apps::process_path(e.th32ProcessID).is_some_and(|p| p.eq_ignore_ascii_case(&want));
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// `--update-guard <pid> <exe>` (run from `dockside.old.exe`): once the old
/// version (`pid`) has quit, gives the new one at `exe` half a minute; if it
/// isn't running by then, puts this old one back in its place and starts it.
pub fn guard(args: &[String]) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    let (Some(pid), Some(exe)) = (args.first().and_then(|p| p.parse::<u32>().ok()), args.get(1)) else { return };
    let exe = Path::new(exe);
    if let Ok(h) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        unsafe {
            WaitForSingleObject(h, 60_000);
            let _ = CloseHandle(h);
        }
    }
    // Up at any point and still up at the end: good.
    let until = Instant::now() + Duration::from_secs(GUARD_SECS);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_secs(1));
    }
    if running(exe) {
        return;
    }
    log!("update: the new version isn't running {GUARD_SECS} s after the update; going back");
    let Ok(me) = std::env::current_exe() else { return };
    crate::win::taskbar::restore();
    if roll_back(exe, &me) {
        let path = exe.display().to_string();
        let started = crate::win::elevation::is_elevated() && crate::win::elevation::run_task();
        if !started {
            crate::win::elevation::shell_open(&path, None, None);
        }
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
    fn signatures_bind_the_exe_and_the_tag() {
        // A throwaway key pair (seed 1, 2, 3, …).
        let seed: String = (1..=32u8).map(|b| format!("{b:02x}")).collect();
        let kp = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new(unhex(&seed).unwrap().try_into().unwrap()));
        let pk = hex(&kp.pk[..]);
        let exe = b"MZ pretend exe";
        let sig = sign_with(&seed, "v1.0.0-build.5", exe).unwrap();
        assert!(verify_with(&pk, "v1.0.0-build.5", exe, &sig));
        assert!(!verify_with(&pk, "v1.0.0-build.6", exe, &sig), "another tag");
        assert!(!verify_with(&pk, "v1.0.0-build.5", b"MZ other exe", &sig), "another exe");
        assert!(!verify_with(PUBLIC_KEY, "v1.0.0-build.5", exe, &sig), "another key");
        assert!(!verify_with(&pk, "v1.0.0-build.5", exe, "zz"));
    }

    #[test]
    fn swapping_in_and_rolling_back_move_the_right_files() {
        let dir = std::env::temp_dir().join(format!("dockside-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("dockside.exe");
        std::fs::write(&exe, b"old").unwrap();
        let old = swap_in(&exe, b"new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(&old).unwrap(), b"old");
        assert!(roll_back(&exe, &old));
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        assert_eq!(std::fs::read(dir.join("dockside.failed.exe")).unwrap(), b"new");
        assert!(!old.exists());
        // A missing exe can't be swapped, and nothing is left half done.
        let none = dir.join("missing").join("dockside.exe");
        assert!(swap_in(&none, b"new").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `DOCKSIDE_TEST_SIG=<exe>|<tag>`: checks `<exe>.sig` (from `--update-sign`)
    /// against the key built into DockSide.
    #[test]
    #[ignore]
    fn signed_file_verifies() {
        let v = std::env::var("DOCKSIDE_TEST_SIG").expect("DOCKSIDE_TEST_SIG=<exe>|<tag>");
        let (exe, tag) = v.split_once('|').unwrap();
        let bytes = std::fs::read(exe).unwrap();
        let sig = std::fs::read_to_string(format!("{exe}.sig")).unwrap();
        assert!(verify(tag, &bytes, &sig), "signature doesn't match the built-in key");
        assert!(!verify("v0.0.1-build.1", &bytes, &sig), "a signature is only good for its own tag");
    }

    #[test]
    #[ignore]
    fn live_release() {
        let body = fetch_latest().expect("GitHub reachable");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parse(json["tag_name"].as_str().unwrap()).is_some());
    }
}
