//! Background icon / thumbnail loading for Finder.
//!
//! Shell icon extraction can take tens of milliseconds per file (much more
//! for thumbnails), so a worker thread with its own COM apartment does it and
//! pings the UI thread as results arrive. Only items actually drawn are
//! requested, so visible icons load first.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};

use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

use super::fs::Entry;
use crate::gfx::icons::{load_shell_image, IconPixels};

/// Pixel size of loaded Finder icons (big enough for 64pt icons at 2x).
const ICON_SIZE: i32 = 128;
/// Quick Look previews: thumbnails at this size, type icons at the shell's largest (256).
const LARGE_THUMB: i32 = 1024;
const LARGE_ICON: i32 = 256;
/// Large previews kept in the cache (they are several MB each).
const LARGE_KEEP: usize = 6;

/// Extensions whose icon depends on the file itself (not just its type).
const PER_FILE: &[&str] = &["exe", "lnk", "ico", "url", "appref-ms", "msc", "cpl"];
/// Extensions that get content thumbnails.
const THUMBNAIL: &[&str] =
    &["png", "jpg", "jpeg", "gif", "bmp", "webp", "heic", "avif", "tif", "tiff", "mp4", "mov", "mkv", "avi", "wmv", "pdf"];

pub struct IconLoader {
    tx: Sender<(String, PathBuf, bool, i32)>,
    rx: Receiver<(String, Option<IconPixels>)>,
    pending: HashSet<String>,
    cache: HashMap<String, Option<Rc<IconPixels>>>,
    /// Large preview keys, oldest first.
    large: VecDeque<String>,
}

impl IconLoader {
    /// `notify`: window that receives `msg` when results are ready.
    pub fn new(notify: isize, msg: u32) -> Self {
        let (req_tx, req_rx) = channel::<(String, PathBuf, bool, i32)>();
        let (res_tx, res_rx) = channel();
        std::thread::Builder::new()
            .name("finder-icons".into())
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                }
                while let Ok((key, path, thumb, size)) = req_rx.recv() {
                    let px = load_shell_image(&path.to_string_lossy(), size, thumb);
                    if res_tx.send((key, px)).is_err() {
                        break;
                    }
                    crate::win::post(notify, msg);
                }
            })
            .expect("spawn icon loader");
        Self { tx: req_tx, rx: res_rx, pending: HashSet::new(), cache: HashMap::new(), large: VecDeque::new() }
    }

    /// Cache key and whether to ask for a content thumbnail. Ordinary files share
    /// one icon per extension; folders, apps and images are per path.
    pub fn key_for(entry: &Entry) -> (String, bool) {
        if entry.is_dir {
            return (format!("dir:{}", entry.path.display()), false);
        }
        if THUMBNAIL.contains(&entry.ext.as_str()) {
            return (format!("thumb:{}", entry.path.display()), true);
        }
        if PER_FILE.contains(&entry.ext.as_str()) {
            return (format!("file:{}", entry.path.display()), false);
        }
        (format!("ext:{}", entry.ext), false)
    }

    /// Returns the icon if loaded; otherwise queues it (once) and returns None.
    pub fn get(&mut self, key: &str, path: &Path, thumbnail: bool, size: i32) -> Option<Rc<IconPixels>> {
        if let Some(px) = self.cache.get(key) {
            return px.clone();
        }
        if self.pending.insert(key.to_string()) {
            let _ = self.tx.send((key.to_string(), path.to_path_buf(), thumbnail, size));
        }
        None
    }

    pub fn get_entry(&mut self, entry: &Entry) -> Option<Rc<IconPixels>> {
        let (key, thumb) = Self::key_for(entry);
        self.get(&key, &entry.path, thumb, ICON_SIZE)
    }

    /// Key of the Quick Look preview for `entry` (see [`Self::get_large`]).
    pub fn large_key(entry: &Entry) -> String {
        format!("large:{}", entry.path.display())
    }

    /// Big content preview (or type icon) for Quick Look.
    pub fn get_large(&mut self, entry: &Entry) -> Option<Rc<IconPixels>> {
        let (_, thumb) = Self::key_for(entry);
        let key = Self::large_key(entry);
        if !self.cache.contains_key(&key) && !self.pending.contains(&key) {
            self.large.push_back(key.clone());
        }
        self.get(&key, &entry.path, thumb, if thumb { LARGE_THUMB } else { LARGE_ICON })
    }

    /// Moves finished results into the cache; true if anything arrived.
    pub fn drain(&mut self) -> bool {
        let mut any = false;
        while let Ok((key, px)) = self.rx.try_recv() {
            self.pending.remove(&key);
            self.cache.insert(key, px.map(Rc::new));
            any = true;
        }
        while self.large.len() > LARGE_KEEP {
            if let Some(old) = self.large.pop_front() {
                self.cache.remove(&old);
            }
        }
        if self.cache.len() > 4000 {
            // Crude bound: thumbnails are ~64 KB each.
            self.cache.retain(|k, _| k.starts_with("ext:") || k.starts_with("dir:"));
        }
        any
    }
}
