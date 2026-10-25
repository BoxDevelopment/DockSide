//! The shelf's holding folder (`%LOCALAPPDATA%\DockSide\Shelf`). Files dropped
//! on the island are copied in, so they outlive the original (a download you
//! then delete, a file on a drive you unplug), and drag back out from here.
//! Conversions land here too. Copies and conversions run on worker threads,
//! writing to a `.dockside-part` file that's renamed into place when done.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use super::convert::{self, Format};

const PART: &str = ".dockside-part";

#[derive(Clone, Debug)]
pub struct Item {
    pub path: PathBuf,
    /// Where it was dropped from (to skip dropping the same file twice).
    pub source: Option<PathBuf>,
    /// When it arrived; the expiry counts from here.
    pub added: SystemTime,
    /// Still copying or converting.
    pub pending: bool,
}

pub enum Done {
    Ready(PathBuf),
    Failed(PathBuf),
}

/// Finished jobs, for whichever island is current (it may be rebuilt meanwhile).
static DONE: Mutex<Vec<Done>> = Mutex::new(Vec::new());
/// Destinations still being written, and ones removed from the shelf mid-way.
static JOBS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static CANCELLED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static NOTIFY_HWND: AtomicIsize = AtomicIsize::new(0);
static NOTIFY_MSG: AtomicU32 = AtomicU32::new(0);
static LOADED: AtomicBool = AtomicBool::new(false);

pub fn dir() -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    local.join("DockSide").join("Shelf")
}

/// Where finished jobs are announced (the island's signal message).
pub fn set_notify(hwnd: isize, msg: u32) {
    NOTIFY_HWND.store(hwnd, Ordering::Relaxed);
    NOTIFY_MSG.store(msg, Ordering::Relaxed);
}

pub fn drain() -> Vec<Done> {
    DONE.lock().map(|mut d| std::mem::take(&mut *d)).unwrap_or_default()
}

fn finish(done: Done) {
    if let Ok(mut d) = DONE.lock() {
        d.push(done);
    }
    crate::win::post(NOTIFY_HWND.load(Ordering::Relaxed), NOTIFY_MSG.load(Ordering::Relaxed));
}

fn part(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(OsString::from).unwrap_or_default();
    name.push(PART);
    dest.with_file_name(name)
}

fn is_part(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(PART))
}

/// What's held, oldest first. The first call in a process empties the
/// folder when `clear` is set (and always drops a crashed run's partial files);
/// later calls (an island rebuilt after a settings change) keep everything.
pub fn load(clear: bool) -> Vec<Item> {
    let dir = dir();
    let first = !LOADED.swap(true, Ordering::Relaxed);
    let entries: Vec<PathBuf> =
        std::fs::read_dir(&dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    if first {
        let stale: Vec<PathBuf> = entries.iter().filter(|p| clear || is_part(p)).cloned().collect();
        delete_all(&stale);
        if clear {
            return Vec::new();
        }
    }
    let jobs = JOBS.lock().map(|j| j.clone()).unwrap_or_default();
    let mut items: Vec<Item> = entries
        .into_iter()
        .filter(|p| !is_part(p) && p.exists())
        .map(|path| {
            let added = std::fs::metadata(&path).and_then(|m| m.created().or_else(|_| m.modified())).unwrap_or_else(|_| SystemTime::now());
            Item { path, source: None, added, pending: false }
        })
        .chain(jobs.into_iter().map(|path| Item { path, source: None, added: SystemTime::now(), pending: true }))
        .collect();
    items.sort_by_key(|i| i.added);
    items
}

/// Picks a free name in the folder ("a.png", "a 2.png", ...) and claims it
/// with an empty partial file (or folder).
fn reserve(name: &Path, folder: bool) -> Option<PathBuf> {
    let dir = dir();
    std::fs::create_dir_all(&dir).ok()?;
    let stem = name.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Item".into());
    let ext = name.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let jobs = JOBS.lock().ok()?;
    for n in 1..1000 {
        let file = if n == 1 { format!("{stem}{ext}") } else { format!("{stem} {n}{ext}") };
        let dest = dir.join(file);
        if dest.exists() || jobs.contains(&dest) {
            continue;
        }
        let claimed = if folder {
            std::fs::create_dir(part(&dest)).is_ok()
        } else {
            std::fs::OpenOptions::new().write(true).create_new(true).open(part(&dest)).is_ok()
        };
        if claimed {
            return Some(dest);
        }
    }
    None
}

/// Runs `work` (writing to the partial path) on a worker thread, then moves the result into place.
fn spawn(dest: PathBuf, what: String, work: impl FnOnce(&Path) -> anyhow::Result<()> + Send + 'static) -> Item {
    if let Ok(mut j) = JOBS.lock() {
        j.push(dest.clone());
    }
    let d = dest.clone();
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let tmp = part(&d);
        let result = work(&tmp).and_then(|()| std::fs::rename(&tmp, &d).map_err(Into::into));
        if let Ok(mut j) = JOBS.lock() {
            j.retain(|p| *p != d);
        }
        let cancelled = CANCELLED
            .lock()
            .map(|mut c| {
                let was = c.contains(&d);
                c.retain(|p| *p != d);
                was
            })
            .unwrap_or(false);
        match result {
            Ok(()) if cancelled => delete_all(&[d]),
            Ok(()) => finish(Done::Ready(d)),
            Err(e) => {
                log!("shelf: {what} failed: {e:#}");
                delete_all(&[tmp]);
                finish(Done::Failed(d.clone()));
                if !cancelled {
                    crate::win::system::notice(&format!("Couldn't {what}"), &format!("{e:#}"));
                }
            }
        }
    });
    Item { path: dest, source: None, added: SystemTime::now(), pending: true }
}

/// Starts holding `src`: a copy for files from elsewhere, the file itself if it's already here.
pub fn hold(src: &Path) -> Option<Item> {
    if src.parent() == Some(dir().as_path()) {
        return Some(Item { path: src.to_path_buf(), source: None, added: SystemTime::now(), pending: false });
    }
    let folder = src.is_dir();
    let dest = reserve(Path::new(src.file_name()?), folder)?;
    let from = src.to_path_buf();
    let name = from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut item = spawn(dest, format!("hold “{name}”"), move |tmp| {
        if folder {
            copy_tree(&from, tmp)
        } else {
            std::fs::copy(&from, tmp).map(|_| ()).map_err(Into::into)
        }
    });
    item.source = Some(src.to_path_buf());
    Some(item)
}

/// Starts converting `src` to `to`, next to it in the folder.
pub fn convert(src: &Path, to: Format) -> Option<Item> {
    let stem = src.file_stem()?.to_string_lossy().into_owned();
    let dest = reserve(Path::new(&format!("{stem}.{}", to.ext())), false)?;
    let from = src.to_path_buf();
    let name = from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Some(spawn(dest, format!("convert “{name}” to {}", to.label()), move |tmp| convert::convert(&from, to, tmp)))
}

fn copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

/// Deletes held files (in the background; folders can be large). Only ever
/// touches the holding folder. One still being written is deleted when it finishes.
pub fn discard(paths: Vec<PathBuf>) {
    let dir = dir();
    let mut ours: Vec<PathBuf> = paths.into_iter().filter(|p| p.parent() == Some(dir.as_path())).collect();
    if let (Ok(jobs), Ok(mut cancelled)) = (JOBS.lock(), CANCELLED.lock()) {
        for p in ours.iter().filter(|p| jobs.contains(p)) {
            cancelled.push(p.clone());
        }
        ours.retain(|p| !jobs.contains(p));
    }
    if !ours.is_empty() {
        std::thread::spawn(move || delete_all(&ours));
    }
}

fn delete_all(paths: &[PathBuf]) {
    for p in paths {
        let r = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
        if let Err(e) = r {
            if p.exists() {
                log!("shelf: couldn't delete {}: {e}", p.display());
            }
        }
    }
}

/// "42 min", "3 h 20 min", "2 days": how long until an item expires.
pub fn remaining_label(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs.div_ceil(60)),
        3600..=86399 => {
            let (h, m) = (secs / 3600, (secs % 3600) / 60);
            if m == 0 { format!("{h} h") } else { format!("{h} h {m} min") }
        }
        _ => {
            let d = (secs + 43200) / 86400;
            if d == 1 { "1 day".into() } else { format!("{d} days") }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(remaining_label(5), "5 s");
        assert_eq!(remaining_label(61), "2 min");
        assert_eq!(remaining_label(3600), "1 h");
        assert_eq!(remaining_label(3600 + 20 * 60 + 5), "1 h 20 min");
        assert_eq!(remaining_label(86400 + 3600), "1 day");
        assert_eq!(remaining_label(6 * 86400), "6 days");
    }

    #[test]
    fn partial_names() {
        let p = part(Path::new(r"C:\x\a.png"));
        assert_eq!(p, Path::new(r"C:\x\a.png.dockside-part"));
        assert!(is_part(&p));
        assert!(!is_part(Path::new(r"C:\x\a.png")));
    }
}
