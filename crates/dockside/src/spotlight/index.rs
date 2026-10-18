//! What Spotlight searches: every app Windows knows about (desktop and Store,
//! via `shell:AppsFolder`) plus files and folders under the user's main
//! folders. Built on a background thread and swapped in when ready.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName, SIGDN, SIGDN_NORMALDISPLAY,
    SIGDN_PARENTRELATIVEPARSING,
};

/// Stop walking after this many files (keeps memory and search time bounded).
const MAX_FILES: usize = 300_000;
const MAX_DEPTH: usize = 10;
/// Folders never worth searching into.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "__pycache__", "venv", ".venv", "bin", "obj", "dist", "build"];
/// Rebuild when Spotlight opens and the index is older than this.
pub const STALE_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    App,
    Folder,
    File,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    /// Lower-case name, precomputed for matching.
    pub lower: String,
    /// Word initials ("Visual Studio Code" -> "vsc").
    pub initials: String,
    /// Launch target: `shell:AppsFolder\<id>` for apps, a path otherwise.
    pub target: String,
    pub kind: Kind,
    /// Folder depth below its search root (files only); shallower ranks higher.
    pub depth: u16,
}

impl Item {
    fn new(name: String, target: String, kind: Kind, depth: u16) -> Self {
        let lower = name.to_lowercase();
        let initials = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter_map(|w| w.chars().next())
            .collect();
        Item { name, lower, initials, target, kind, depth }
    }
}

#[derive(Default)]
pub struct Index {
    pub apps: Vec<Item>,
    pub files: Vec<Item>,
    pub built: Option<Instant>,
}

pub type Shared = Arc<Mutex<Index>>;

/// Rebuilds `shared` on a background thread, then posts `msg` to `notify`.
/// Concurrent rebuild requests are ignored while one is running.
pub fn rebuild(shared: Shared, roots: Vec<PathBuf>, notify: isize, msg: u32) {
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    use std::sync::atomic::Ordering;
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("spotlight-index".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let started = Instant::now();
        let apps = apps();
        let mut files = Vec::new();
        for root in &roots {
            walk(root, 0, &mut files);
        }
        log!("spotlight index: {} apps, {} files in {:?}", apps.len(), files.len(), started.elapsed());
        if let Ok(mut idx) = shared.lock() {
            *idx = Index { apps, files, built: Some(Instant::now()) };
        }
        RUNNING.store(false, Ordering::Release);
        crate::win::post(notify, msg);
    });
    if spawned.is_err() {
        RUNNING.store(false, Ordering::Release);
    }
}

fn display_name(item: &IShellItem, kind: SIGDN) -> Option<String> {
    unsafe {
        let p = item.GetDisplayName(kind).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// All entries of the Start menu's "All apps" list (desktop + Store apps).
fn apps() -> Vec<Item> {
    const NOT_APPS: &[&str] = &[".txt", ".html", ".htm", ".chm", ".pdf", ".url", ".rtf", ".md", ".ini", ".log"];
    let mut out = Vec::new();
    unsafe {
        let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(w!("shell:AppsFolder"), None) else {
            return out;
        };
        let Ok(items) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else { return out };
        loop {
            let mut batch: [Option<IShellItem>; 1] = [None];
            let mut fetched = 0u32;
            let _ = items.Next(&mut batch, Some(&mut fetched));
            let Some(item) = batch[0].take().filter(|_| fetched == 1) else { break };
            let (Some(name), Some(id)) = (display_name(&item, SIGDN_NORMALDISPLAY), display_name(&item, SIGDN_PARENTRELATIVEPARSING))
            else {
                continue;
            };
            let id_lower = id.to_lowercase();
            if name.to_lowercase().contains("uninstall") || NOT_APPS.iter().any(|ext| id_lower.ends_with(ext)) {
                continue;
            }
            out.push(Item::new(name, format!("shell:AppsFolder\\{id}"), Kind::App, 0));
        }
    }
    out.sort_by(|a, b| a.lower.cmp(&b.lower));
    out.dedup_by(|a, b| a.lower == b.lower);
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<Item>) {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(md) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || md.file_attributes() & HIDDEN_OR_SYSTEM != 0 {
            continue;
        }
        let path = entry.path();
        // Don't follow junctions/symlinks (avoids cycles like "My Music" in Documents).
        if md.file_type().is_symlink() || md.file_attributes() & 0x400 != 0 {
            continue;
        }
        if md.is_dir() {
            out.push(Item::new(name.clone(), path.display().to_string(), Kind::Folder, depth as u16));
            if !SKIP_DIRS.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
                walk(&path, depth + 1, out);
            }
        } else {
            out.push(Item::new(name, path.display().to_string(), Kind::File, depth as u16));
        }
        if out.len() >= MAX_FILES {
            return;
        }
    }
}

/// Match quality of `item` for lower-case query `q` (higher is better).
pub fn score(item: &Item, q: &str) -> Option<i32> {
    let name = &item.lower;
    let base = if *name == q {
        1000
    } else if name.starts_with(q) {
        900 - (name.len() as i32 - q.len() as i32).min(100)
    } else if item.initials.starts_with(q) && q.len() >= 2 {
        800
    } else if let Some(pos) = name.find(q) {
        // Prefer matches at the start of a word.
        let at_word = pos == 0 || !name.as_bytes()[pos - 1].is_ascii_alphanumeric();
        if at_word {
            700
        } else {
            500 - (pos as i32).min(100)
        }
    } else {
        fuzzy(name, q)?
    };
    Some(base - item.depth as i32 * 4)
}

/// All query chars appear in order, starting at a word, without big gaps
/// ("clc" -> "Calculator", but not "calc" -> "VLC ... reset ... cache").
fn fuzzy(name: &str, q: &str) -> Option<i32> {
    if q.len() < 2 {
        return None;
    }
    let name: Vec<char> = name.chars().collect();
    let mut qs = q.chars();
    let first = qs.next()?;
    let start = (0..name.len()).find(|&i| name[i] == first && (i == 0 || !name[i - 1].is_alphanumeric()))?;
    let mut pos = start + 1;
    let mut score = 100;
    let mut run = 0;
    let mut total_skipped = 0;
    for qc in qs {
        let next = (pos..name.len()).find(|&i| name[i] == qc)?;
        let skipped = next - pos;
        total_skipped += skipped;
        if skipped == 0 {
            run += 1;
            score += 10 * run;
        } else {
            run = 0;
            score -= skipped.min(10) as i32;
        }
        pos = next + 1;
    }
    (total_skipped <= 3 * q.len()).then_some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> Item {
        Item::new(name.into(), String::new(), Kind::App, 0)
    }

    #[test]
    fn ranking_order() {
        let q = "vis";
        let exact = score(&app("vis"), q).unwrap();
        let prefix = score(&app("Visual Studio Code"), q).unwrap();
        let word = score(&app("Microsoft Visio"), q).unwrap();
        let inside = score(&app("Envision"), q).unwrap();
        assert!(exact > prefix && prefix > word && word > inside);
    }

    #[test]
    fn initials_and_fuzzy() {
        assert!(score(&app("Visual Studio Code"), "vsc").unwrap() >= 800);
        assert!(score(&app("Calculator"), "clc").is_some());
        assert!(score(&app("Calculator"), "xyz").is_none());
        // Scattered letters across unrelated words don't count.
        assert!(score(&app("VLC media player - reset preferences and cache files"), "calc").is_none());
    }
}
