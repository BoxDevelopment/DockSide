//! Clipboard history for Spotlight's Clipboard view (Ctrl+4), like macOS
//! Tahoe's: what you copy is kept for eight hours, newest first. Text and
//! files are kept; content that password managers mark as private (the
//! formats Windows' own clipboard history honours) never is.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use windows::core::w;
use windows::Win32::Foundation::{HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

/// How long copies are kept (macOS's default).
const KEEP: Duration = Duration::from_secs(8 * 60 * 60);
const MAX: usize = 100;
/// Longer text is kept cut short.
const MAX_CHARS: usize = 20_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Clip {
    Text(String),
    Files(Vec<PathBuf>),
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub clip: Clip,
    pub at: SystemTime,
}

thread_local! {
    static HISTORY: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
}

/// Starts listening for clipboard changes: `hwnd` gets WM_CLIPBOARDUPDATE.
pub fn start(hwnd: HWND) {
    unsafe {
        if let Err(e) = AddClipboardFormatListener(hwnd) {
            log!("clipboard history unavailable: {e}");
        }
    }
}

/// Whether the clipboard's owner asked to be left out of history.
unsafe fn private() -> bool {
    let exclude = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
    let ignore = RegisterClipboardFormatW(w!("Clipboard Viewer Ignore"));
    if IsClipboardFormatAvailable(exclude).is_ok() || IsClipboardFormatAvailable(ignore).is_ok() {
        return true;
    }
    // A DWORD 0 means "don't put this in history".
    let allow = RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory"));
    if IsClipboardFormatAvailable(allow).is_ok() {
        if let Ok(h) = GetClipboardData(allow) {
            let mem = HGLOBAL(h.0);
            let p = GlobalLock(mem) as *const u32;
            let no = !p.is_null() && GlobalSize(mem) >= 4 && *p == 0;
            let _ = GlobalUnlock(mem);
            return no;
        }
    }
    false
}

unsafe fn read() -> Option<Clip> {
    if private() {
        return None;
    }
    if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
        if let Ok(h) = GetClipboardData(CF_HDROP.0 as u32) {
            let drop = HDROP(h.0);
            let n = DragQueryFileW(drop, u32::MAX, None);
            let mut files = Vec::new();
            for i in 0..n {
                let len = DragQueryFileW(drop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                DragQueryFileW(drop, i, Some(&mut buf));
                files.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
            }
            if !files.is_empty() {
                return Some(Clip::Files(files));
            }
        }
    }
    if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
        let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        let mem = HGLOBAL(h.0);
        let p = GlobalLock(mem) as *const u16;
        if p.is_null() {
            return None;
        }
        let max = GlobalSize(mem) / 2;
        let mut len = 0;
        while len < max && *p.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        let _ = GlobalUnlock(mem);
        if text.trim().is_empty() {
            return None;
        }
        let text = if text.chars().count() > MAX_CHARS { text.chars().take(MAX_CHARS).collect() } else { text };
        return Some(Clip::Text(text));
    }
    None
}

/// The clipboard changed (WM_CLIPBOARDUPDATE): remember what's on it.
pub fn on_update() {
    let clip = unsafe {
        if OpenClipboard(None).is_err() {
            return;
        }
        let clip = read();
        let _ = CloseClipboard();
        clip
    };
    let Some(clip) = clip else { return };
    HISTORY.with(|h| {
        let mut h = h.borrow_mut();
        // Copying something again moves it to the top.
        h.retain(|e| e.clip != clip);
        h.insert(0, Entry { clip, at: SystemTime::now() });
        h.truncate(MAX);
    });
}

/// Everything copied in the last eight hours, newest first.
pub fn entries() -> Vec<Entry> {
    HISTORY.with(|h| {
        let mut h = h.borrow_mut();
        let now = SystemTime::now();
        h.retain(|e| now.duration_since(e.at).map_or(true, |d| d < KEEP));
        h.clone()
    })
}

pub fn clear() {
    HISTORY.with(|h| h.borrow_mut().clear());
}

/// Puts a history entry back on the clipboard.
pub fn put(clip: &Clip) {
    match clip {
        Clip::Text(t) => crate::finder::fs::copy_text(t),
        Clip::Files(paths) => {
            crate::finder::shell::set_clipboard(paths, false);
        }
    }
}

/// One line describing a clip, for its row.
pub fn title(clip: &Clip) -> String {
    match clip {
        Clip::Text(t) => {
            let line = t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
            let mut s: String = line.chars().take(120).collect();
            if line.chars().count() > 120 || t.lines().filter(|l| !l.trim().is_empty()).count() > 1 {
                s.push('\u{2026}');
            }
            s
        }
        Clip::Files(f) if f.len() == 1 => f[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        Clip::Files(f) => format!("{} items", f.len()),
    }
}

/// "Just now", "5 min ago", "2 hr ago".
pub fn ago(at: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(at).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => "Just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        _ => format!("{} hr ago", secs / 3600),
    }
}
