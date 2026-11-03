//! What Spotlight searches: every app Windows knows about (desktop and Store,
//! via `shell:AppsFolder`) plus files and folders under the user's main
//! folders. Built on a background thread and swapped in when ready.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
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
    /// Launch target: `shell:AppsFolder\<id>` for apps, a path otherwise.
    pub target: String,
    pub kind: Kind,
    /// Folder depth below its search root (files only); shallower ranks higher.
    pub depth: u16,
}

impl Item {
    fn new(name: String, target: String, kind: Kind, depth: u16) -> Self {
        let lower = name.to_lowercase();
        Item { name, lower, target, kind, depth }
    }
}

/// The files and folders under the search roots, packed: there can be a few
/// hundred thousand, and as `Item`s (four strings each, every one with its
/// full path) they took ~285 bytes apiece. Here each is 16 bytes plus its
/// name twice in one shared buffer, and points at its folder's entry rather
/// than spelling out its path; `item` builds an `Item` for the few results shown.
#[derive(Default)]
pub struct Files {
    roots: Vec<String>,
    /// Every name followed by its lower-case form.
    text: String,
    entries: Vec<Entry>,
}

#[derive(Clone, Copy)]
struct Entry {
    /// Name at `text[at..at + name_len]`, lower case right after it.
    at: u32,
    name_len: u16,
    lower_len: u16,
    /// Index of its folder's entry, or `ROOT - i` for search root `i`.
    parent: u32,
    depth: u16,
    kind: Kind,
}

const ROOT: u32 = u32::MAX;

impl Files {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn push(&mut self, name: &str, parent: u32, kind: Kind, depth: usize) -> u32 {
        let lower = name.to_lowercase();
        // Names are at most 255 UTF-16 units, so the lengths fit.
        let e = Entry {
            at: self.text.len() as u32,
            name_len: name.len() as u16,
            lower_len: lower.len() as u16,
            parent,
            depth: depth as u16,
            kind,
        };
        self.text.push_str(name);
        self.text.push_str(&lower);
        self.entries.push(e);
        self.entries.len() as u32 - 1
    }

    fn name(&self, e: &Entry) -> &str {
        &self.text[e.at as usize..e.at as usize + e.name_len as usize]
    }

    fn lower(&self, e: &Entry) -> &str {
        let at = e.at as usize + e.name_len as usize;
        &self.text[at..at + e.lower_len as usize]
    }

    fn path(&self, i: u32) -> String {
        let mut parts = Vec::new();
        let mut at = i;
        while at <= ROOT - self.roots.len() as u32 {
            let e = &self.entries[at as usize];
            parts.push(self.name(e));
            at = e.parent;
        }
        let mut path = self.roots.get((ROOT - at) as usize).cloned().unwrap_or_default();
        for p in parts.iter().rev() {
            if !path.ends_with('\\') {
                path.push('\\');
            }
            path.push_str(p);
        }
        path
    }

    fn item(&self, i: u32) -> Item {
        let e = &self.entries[i as usize];
        Item::new(self.name(e).to_string(), self.path(i), e.kind, e.depth)
    }

    /// The `n` best matches for lower-case query `q`, best first (shorter names first among equals).
    pub fn top(&self, q: &str, n: usize) -> Vec<(i32, Item)> {
        let mut scored: Vec<(i32, u32)> = (0..self.entries.len() as u32)
            .filter_map(|i| {
                let e = &self.entries[i as usize];
                score_name(self.lower(e), e.depth, q).map(|s| (s, i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(self.entries[a.1 as usize].name_len.cmp(&self.entries[b.1 as usize].name_len)));
        scored.into_iter().take(n).map(|(s, i)| (s, self.item(i))).collect()
    }
}

#[derive(Default)]
pub struct Index {
    pub apps: Vec<Item>,
    pub files: Files,
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
            // Low CPU, I/O and memory priority: walking the user's folders takes a
            // second or two of a core and lots of disk, and nothing waits on it.
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
        let started = Instant::now();
        let apps = apps();
        let mut files = Files { roots: roots.iter().map(|r| r.display().to_string()).collect(), ..Default::default() };
        for (i, root) in roots.iter().enumerate() {
            walk(root, ROOT - i as u32, 0, &mut files);
        }
        files.text.shrink_to_fit();
        files.entries.shrink_to_fit();
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

fn walk(dir: &Path, parent: u32, depth: usize, out: &mut Files) {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
    if depth > MAX_DEPTH || out.entries.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(md) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || md.file_attributes() & HIDDEN_OR_SYSTEM != 0 {
            continue;
        }
        // Don't follow junctions/symlinks (avoids cycles like "My Music" in Documents).
        if md.file_type().is_symlink() || md.file_attributes() & 0x400 != 0 {
            continue;
        }
        if md.is_dir() {
            let at = out.push(&name, parent, Kind::Folder, depth);
            if !SKIP_DIRS.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
                walk(&entry.path(), at, depth + 1, out);
            }
        } else {
            out.push(&name, parent, Kind::File, depth);
        }
        if out.entries.len() >= MAX_FILES {
            return;
        }
    }
}

/// Match quality of `item` for lower-case query `q` (higher is better).
pub fn score(item: &Item, q: &str) -> Option<i32> {
    score_name(&item.lower, item.depth, q)
}

/// Whether the words of `lower` start with the letters of `q`, in order
/// ("visual studio code" for "vsc").
fn initials_start_with(lower: &str, q: &str) -> bool {
    let mut want = q.chars();
    let mut next = want.next();
    let mut word_start = true;
    for c in lower.chars() {
        let Some(w) = next else { return true };
        if !c.is_alphanumeric() {
            word_start = true;
        } else if word_start {
            if c != w {
                return false;
            }
            next = want.next();
            word_start = false;
        }
    }
    next.is_none()
}

fn score_name(name: &str, depth: u16, q: &str) -> Option<i32> {
    let base = if name == q {
        1000
    } else if name.starts_with(q) {
        900 - (name.len() as i32 - q.len() as i32).min(100)
    } else if q.len() >= 2 && initials_start_with(name, q) {
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
    Some(base - depth as i32 * 4)
}

/// Match quality of plain text (lower case) for query `q`, scored like an item name.
pub fn score_text(text: &str, q: &str) -> Option<i32> {
    score(&Item::new(text.to_string(), String::new(), Kind::App, 0), q)
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
    fn packed_files_give_back_paths() {
        let mut f = Files { roots: vec![r"C:\Users\me\Documents".into(), r"D:\".into()], ..Default::default() };
        let dir = f.push("Projects", ROOT, Kind::Folder, 0);
        let file = f.push("Visual Notes.TXT", dir, Kind::File, 1);
        let other = f.push("a.txt", ROOT - 1, Kind::File, 0);
        assert_eq!(f.path(file), r"C:\Users\me\Documents\Projects\Visual Notes.TXT");
        assert_eq!(f.path(other), r"D:\a.txt");
        let item = f.item(file);
        assert_eq!((item.name.as_str(), item.lower.as_str(), item.kind), ("Visual Notes.TXT", "visual notes.txt", Kind::File));
        let top = f.top("vn", 5);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].1.target, item.target);
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
