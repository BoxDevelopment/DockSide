//! Finder search: walks the folder being searched (and everything under it) on
//! a worker thread, streaming matches back as it goes.
//!
//! The query is words to find in names, plus Explorer-style filters:
//! `kind:image`, `ext:pdf` (or `type:.pdf`), `date:today` / `modified:thisweek`,
//! and `size:large`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use super::fs::{self, Entry};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Folder,
    Document,
    Image,
    Music,
    Movie,
    Archive,
    App,
}

impl Kind {
    pub const ALL: [(Kind, &'static str, &'static str); 7] = [
        (Kind::Folder, "folder", "Folder"),
        (Kind::Document, "document", "Document"),
        (Kind::Image, "image", "Image"),
        (Kind::Music, "music", "Music"),
        (Kind::Movie, "movie", "Movie"),
        (Kind::Archive, "archive", "Archive"),
        (Kind::App, "app", "Application"),
    ];

    fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "folder" | "folders" | "directory" => Kind::Folder,
            "document" | "documents" | "doc" | "text" => Kind::Document,
            "image" | "images" | "picture" | "pictures" | "photo" | "photos" => Kind::Image,
            "music" | "audio" | "song" => Kind::Music,
            "movie" | "movies" | "video" | "videos" => Kind::Movie,
            "archive" | "zip" | "compressed" => Kind::Archive,
            "app" | "apps" | "application" | "program" | "programs" => Kind::App,
            _ => return None,
        })
    }

    fn matches(self, e: &Entry) -> bool {
        let x = e.ext.as_str();
        match self {
            Kind::Folder => e.is_dir,
            _ if e.is_dir => false,
            Kind::Document => matches!(
                x,
                "txt" | "md" | "rtf" | "pdf" | "doc" | "docx" | "odt" | "xls" | "xlsx" | "ods" | "csv" | "ppt" | "pptx" | "odp" | "pages" | "numbers" | "key" | "epub"
            ),
            Kind::Image => matches!(x, "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "heic" | "avif" | "tif" | "tiff" | "svg" | "ico" | "raw" | "psd"),
            Kind::Music => matches!(x, "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "wma" | "opus" | "aiff"),
            Kind::Movie => matches!(x, "mp4" | "mov" | "mkv" | "avi" | "wmv" | "webm" | "m4v" | "mpg" | "mpeg"),
            Kind::Archive => matches!(x, "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "cab" | "iso"),
            Kind::App => matches!(x, "exe" | "msi" | "lnk" | "bat" | "cmd" | "ps1" | "appref-ms" | "msix" | "appx"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum When {
    Today,
    Yesterday,
    Week,
    Month,
    Year,
}

impl When {
    pub const ALL: [(When, &'static str, &'static str); 5] = [
        (When::Today, "today", "Today"),
        (When::Yesterday, "yesterday", "Yesterday"),
        (When::Week, "thisweek", "This Week"),
        (When::Month, "thismonth", "This Month"),
        (When::Year, "thisyear", "This Year"),
    ];

    fn parse(s: &str) -> Option<When> {
        Self::ALL.iter().find(|(_, k, _)| *k == s).map(|w| w.0).or(match s {
            "week" | "lastweek" => Some(When::Week),
            "month" | "lastmonth" => Some(When::Month),
            "year" | "lastyear" => Some(When::Year),
            _ => None,
        })
    }

    fn matches(self, t: Option<SystemTime>) -> bool {
        let Some(t) = t else { return false };
        let dt: chrono::DateTime<chrono::Local> = t.into();
        let days = (chrono::Local::now().date_naive() - dt.date_naive()).num_days();
        match self {
            When::Today => days == 0,
            When::Yesterday => days == 1,
            When::Week => days < 7,
            When::Month => days < 31,
            When::Year => days < 366,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Size {
    Empty,
    Tiny,
    Small,
    Medium,
    Large,
    Huge,
}

impl Size {
    /// Explorer's size buckets.
    pub const ALL: [(Size, &'static str, &'static str); 6] = [
        (Size::Empty, "empty", "Empty (0 KB)"),
        (Size::Tiny, "tiny", "Tiny (0 – 16 KB)"),
        (Size::Small, "small", "Small (16 KB – 1 MB)"),
        (Size::Medium, "medium", "Medium (1 – 128 MB)"),
        (Size::Large, "large", "Large (128 MB – 1 GB)"),
        (Size::Huge, "huge", "Huge (> 1 GB)"),
    ];

    fn parse(s: &str) -> Option<Size> {
        Self::ALL.iter().find(|(_, k, _)| *k == s).map(|w| w.0).or((s == "gigantic").then_some(Size::Huge))
    }

    pub fn of(bytes: u64) -> Size {
        const KB: u64 = 1024;
        match bytes {
            0 => Size::Empty,
            b if b <= 16 * KB => Size::Tiny,
            b if b <= KB * KB => Size::Small,
            b if b <= 128 * KB * KB => Size::Medium,
            b if b <= KB * KB * KB => Size::Large,
            _ => Size::Huge,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    /// Lower-case words that must all appear in the name.
    pub words: Vec<String>,
    pub kind: Option<Kind>,
    pub ext: Option<String>,
    pub when: Option<When>,
    pub size: Option<Size>,
}

impl Query {
    pub fn parse(text: &str) -> Query {
        let mut q = Query::default();
        for token in text.split_whitespace() {
            let lower = token.to_lowercase();
            let (key, value) = match lower.split_once(':') {
                Some((k, v)) => (k, v.trim_start_matches('=').trim_matches('"')),
                None => ("", lower.as_str()),
            };
            match key {
                "kind" => q.kind = Kind::parse(value),
                "ext" | "type" | "extension" => q.ext = Some(value.trim_start_matches('*').trim_start_matches('.').to_string()).filter(|e| !e.is_empty()),
                "date" | "modified" | "datemodified" => q.when = When::parse(value),
                "size" => q.size = Size::parse(value),
                _ => q.words.push(lower.clone()),
            }
        }
        q
    }

    pub fn is_empty(&self) -> bool {
        *self == Query::default()
    }

    pub fn matches(&self, e: &Entry) -> bool {
        let name = e.name.to_lowercase();
        self.words.iter().all(|w| name.contains(w.as_str()))
            && self.kind.map_or(true, |k| k.matches(e))
            && self.ext.as_ref().map_or(true, |x| !e.is_dir && e.ext == *x)
            && self.when.map_or(true, |w| w.matches(e.modified))
            && self.size.map_or(true, |s| !e.is_dir && Size::of(e.size) == s)
    }
}

/// A search running on its own thread; dropping it stops the walk.
pub struct Search {
    rx: Receiver<Vec<Entry>>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Search {
    /// Starts searching `roots` (posting `msg` with `id` to `notify` as batches arrive).
    pub fn start(roots: Vec<PathBuf>, query: Query, show_hidden: bool, notify: isize, msg: u32, id: u32) -> Search {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (c, d, q) = (cancel.clone(), done.clone(), query);
        let _ = std::thread::Builder::new().name("finder-search".into()).spawn(move || {
            let mut queue: VecDeque<PathBuf> = roots.into();
            let mut batch = Vec::new();
            let mut sent = Instant::now();
            while let Some(dir) = queue.pop_front() {
                if c.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(items) = fs::list(&dir, show_hidden) else { continue };
                for e in items {
                    // Junctions (like "Application Data") would loop or repeat whole trees.
                    if e.is_dir && !is_link(&e.path) {
                        queue.push_back(e.path.clone());
                    }
                    if q.matches(&e) {
                        batch.push(e);
                    }
                }
                if !batch.is_empty() && (sent.elapsed() > Duration::from_millis(120) || batch.len() > 400) {
                    if tx.send(std::mem::take(&mut batch)).is_err() {
                        return;
                    }
                    crate::win::post_wp(notify, msg, id as usize);
                    sent = Instant::now();
                }
            }
            let _ = tx.send(batch);
            d.store(true, Ordering::Relaxed);
            crate::win::post_wp(notify, msg, id as usize);
        });
        Search { rx, cancel, done }
    }

    /// Matches found since the last call.
    pub fn take(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        while let Ok(mut b) = self.rx.try_recv() {
            out.append(&mut b);
        }
        out
    }

    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_split_words_and_filters() {
        let q = Query::parse("Tax Report kind:document date:thisyear size:=small ext:.PDF");
        assert_eq!(q.words, vec!["tax", "report"]);
        assert_eq!(q.kind, Some(Kind::Document));
        assert_eq!(q.when, Some(When::Year));
        assert_eq!(q.size, Some(Size::Small));
        assert_eq!(q.ext.as_deref(), Some("pdf"));
    }

    #[test]
    fn matches_names_and_kinds() {
        let e = Entry { name: "Holiday Photo.JPG".into(), ext: "jpg".into(), size: 2_000_000, ..Default::default() };
        assert!(Query::parse("photo").matches(&e));
        assert!(Query::parse("kind:image holiday").matches(&e));
        assert!(!Query::parse("kind:music").matches(&e));
        assert!(Query::parse("size:medium").matches(&e));
    }
}
