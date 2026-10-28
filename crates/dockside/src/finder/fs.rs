//! Filesystem side of Finder: listing, sorting, formatting, places and file operations.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, FILE_ATTRIBUTE_NORMAL,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Shell::*;

const HIDDEN: u32 = 0x2;
// GetDriveTypeW results.
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_CDROM: u32 = 5;
const SYSTEM: u32 = 0x4;

#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub name: String,
    /// A filesystem path, or a shell parsing name for virtual items (see [`super::shell`]).
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// Lower-case extension without the dot ("" for none / folders).
    pub ext: String,
    /// A real file or folder that `std::fs` can work on (not a phone's photo or a recycled file).
    pub fs: bool,
    pub hidden: bool,
    /// Recycle Bin items: where they were deleted from (search results: their folder).
    pub location: Option<String>,
    pub deleted: Option<SystemTime>,
    /// Drives: "120 GB free of 512 GB".
    pub extra: Option<String>,
    /// The shell's own type name for virtual items ("Local Disk", "Phone").
    pub type_text: Option<String>,
}

impl Entry {
    /// The name as shown: shortcuts never show ".lnk" (like Explorer), other
    /// extensions only when `show_ext`.
    pub fn label(&self, show_ext: bool) -> &str {
        if self.is_dir || self.ext.is_empty() {
            return &self.name;
        }
        let always_hide = matches!(self.ext.as_str(), "lnk" | "url" | "appref-ms");
        if show_ext && !always_hide {
            return &self.name;
        }
        match self.name.rfind('.') {
            Some(i) if i > 0 && self.name[i + 1..].eq_ignore_ascii_case(&self.ext) => &self.name[..i],
            _ => &self.name,
        }
    }

    /// The folder an entry is in, for search results' Location column.
    pub fn folder(&self) -> String {
        self.location.clone().unwrap_or_else(|| self.path.parent().map(|p| p.display().to_string()).unwrap_or_default())
    }
}

/// Hidden/system files and dotfiles are skipped unless `show_hidden`.
pub fn list(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let Ok(md) = item.metadata() else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        if !show_hidden && (md.file_attributes() & (HIDDEN | SYSTEM) != 0 || name.starts_with('.')) {
            continue;
        }
        let path = item.path();
        let is_dir = md.is_dir();
        let ext = if is_dir {
            String::new()
        } else {
            path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
        };
        let attrs = md.file_attributes();
        out.push(Entry {
            name,
            path,
            is_dir,
            size: md.len(),
            modified: md.modified().ok(),
            created: md.created().ok(),
            ext,
            fs: true,
            hidden: attrs & (HIDDEN | SYSTEM) != 0,
            ..Default::default()
        });
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    Name,
    Modified,
    Created,
    Size,
    Kind,
    /// Search results' folder, or where a recycled item came from.
    Location,
    Deleted,
}

impl SortKey {
    /// Name used in config.toml.
    pub fn name(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Modified => "modified",
            SortKey::Created => "created",
            SortKey::Size => "size",
            SortKey::Kind => "kind",
            SortKey::Location => "location",
            SortKey::Deleted => "deleted",
        }
    }
}

/// Case-insensitive comparison where digit runs compare numerically ("file 2" < "file 10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(|c| c.is_ascii_digit()) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (na, nb) = (take(&mut a), take(&mut b));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

pub fn sort(entries: &mut [Entry], key: SortKey, ascending: bool, kinds: &mut KindCache) {
    entries.sort_by(|a, b| {
        let ord = match key {
            SortKey::Name => natural_cmp(&a.name, &b.name),
            SortKey::Modified => a.modified.cmp(&b.modified),
            SortKey::Created => a.created.cmp(&b.created),
            SortKey::Location => natural_cmp(&a.folder(), &b.folder()),
            SortKey::Deleted => a.deleted.cmp(&b.deleted),
            // Folders have no size; keep them together at the top.
            SortKey::Size => (!a.is_dir).cmp(&!b.is_dir).then(a.size.cmp(&b.size)),
            SortKey::Kind => kinds.kind(a).cmp(&kinds.kind(b)),
        };
        let ord = ord.then_with(|| natural_cmp(&a.name, &b.name));
        if ascending {
            ord
        } else {
            ord.reverse()
        }
    });
}

/// macOS uses decimal units.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes == 0 {
        return "Zero bytes".into();
    }
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 && unit > 0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{} {}", v.round() as u64, UNITS[unit])
    }
}

/// "Today at 9:41 AM", "Yesterday at 9:41 AM", or "17 Oct 2026 at 9:41 AM".
pub fn format_date(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    let today = chrono::Local::now().date_naive();
    let time = dt.format("%-I:%M %p");
    match (today - dt.date_naive()).num_days() {
        0 => format!("Today at {time}"),
        1 => format!("Yesterday at {time}"),
        _ => format!("{} at {time}", dt.format("%-d %b %Y")),
    }
}

/// File type names ("Text Document", "PNG File") from the shell, cached by extension.
#[derive(Default)]
pub struct KindCache {
    cache: HashMap<String, String>,
}

impl KindCache {
    pub fn kind(&mut self, e: &Entry) -> String {
        if let Some(t) = &e.type_text {
            return t.clone();
        }
        if e.is_dir {
            return "Folder".into();
        }
        self.cache
            .entry(e.ext.clone())
            .or_insert_with(|| {
                let probe = HSTRING::from(format!("x.{}", e.ext));
                let mut info = SHFILEINFOW::default();
                let ok = unsafe {
                    SHGetFileInfoW(
                        &probe,
                        FILE_ATTRIBUTE_NORMAL,
                        Some(&mut info),
                        std::mem::size_of::<SHFILEINFOW>() as u32,
                        SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
                    )
                };
                let name = crate::win::from_wide(&info.szTypeName);
                if ok != 0 && !name.is_empty() {
                    name
                } else if e.ext.is_empty() {
                    "Document".into()
                } else {
                    format!("{} File", e.ext.to_uppercase())
                }
            })
            .clone()
    }
}

/// What a sidebar row is, for its glyph and context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceKind {
    Recents,
    Home,
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Movies,
    Cloud,
    /// A folder added with "Add to Sidebar".
    Pinned,
    ThisPc,
    Drive,
    Removable,
    Network,
    Libraries,
    Trash,
}

#[derive(Clone, Debug)]
pub struct Place {
    pub name: String,
    pub path: PathBuf,
    pub kind: PlaceKind,
}

pub fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), HANDLE::default()).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s.map(PathBuf::from)
    }
}

pub fn home() -> PathBuf {
    known_folder(&FOLDERID_Profile).unwrap_or_else(|| PathBuf::from("C:\\"))
}

/// The sidebar's Favorites: Recents, the user's folders, OneDrive, then folders they pinned.
pub fn favorites(pins: &[PathBuf]) -> Vec<Place> {
    let mut out = Vec::new();
    if let Some(path) = known_folder(&FOLDERID_Recent) {
        out.push(Place { name: "Recents".into(), path, kind: PlaceKind::Recents });
    }
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "Home".into());
    for (id, name, kind) in [
        (&FOLDERID_Profile, user.as_str(), PlaceKind::Home),
        (&FOLDERID_Desktop, "Desktop", PlaceKind::Desktop),
        (&FOLDERID_Documents, "Documents", PlaceKind::Documents),
        (&FOLDERID_Downloads, "Downloads", PlaceKind::Downloads),
        (&FOLDERID_Pictures, "Pictures", PlaceKind::Pictures),
        (&FOLDERID_Music, "Music", PlaceKind::Music),
        (&FOLDERID_Videos, "Movies", PlaceKind::Movies),
        (&FOLDERID_SkyDrive, "OneDrive", PlaceKind::Cloud),
    ] {
        if let Some(path) = known_folder(id).filter(|p| p.exists()) {
            out.push(Place { name: name.to_string(), path, kind });
        }
    }
    for p in pins {
        if !out.iter().any(|o| o.path == *p) {
            out.push(Place { name: super::shell::title(p), path: p.clone(), kind: PlaceKind::Pinned });
        }
    }
    out
}

/// The sidebar's Locations: This PC, each drive, Network, Libraries and the Recycle Bin.
pub fn locations() -> Vec<Place> {
    use super::shell::{LIBRARIES, NETWORK, RECYCLE_BIN, THIS_PC};
    let mut out = vec![Place { name: "This PC".into(), path: THIS_PC.into(), kind: PlaceKind::ThisPc }];
    out.extend(drives());
    out.push(Place { name: "Network".into(), path: NETWORK.into(), kind: PlaceKind::Network });
    out.push(Place { name: "Libraries".into(), path: LIBRARIES.into(), kind: PlaceKind::Libraries });
    out.push(Place { name: "Recycle Bin".into(), path: RECYCLE_BIN.into(), kind: PlaceKind::Trash });
    out
}

pub fn drives() -> Vec<Place> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let letter = (b'A' + i) as char;
            let root = format!("{letter}:\\");
            let mut label = [0u16; 128];
            let ok = unsafe {
                GetVolumeInformationW(&HSTRING::from(root.as_str()), Some(&mut label), None, None, None, None).is_ok()
            };
            let label = if ok { crate::win::from_wide(&label) } else { String::new() };
            let kind = match unsafe { GetDriveTypeW(&HSTRING::from(root.as_str())) } {
                DRIVE_REMOVABLE | DRIVE_CDROM => PlaceKind::Removable,
                _ => PlaceKind::Drive,
            };
            let name = match (label.is_empty(), kind) {
                (false, _) => format!("{label} ({letter}:)"),
                (true, PlaceKind::Removable) => format!("USB Drive ({letter}:)"),
                _ => format!("Local Disk ({letter}:)"),
            };
            Place { name, path: PathBuf::from(root), kind }
        })
        .collect()
}

pub fn free_space(path: &Path) -> Option<u64> {
    let mut free = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(&HSTRING::from(path.as_os_str()), Some(&mut free), None, None).ok()?;
    }
    Some(free)
}

/// Creates "untitled folder" (or "untitled folder 2", ...) like macOS.
pub fn new_folder(dir: &Path) -> std::io::Result<PathBuf> {
    for n in 1.. {
        let name = if n == 1 { "untitled folder".to_string() } else { format!("untitled folder {n}") };
        let path = dir.join(name);
        if !path.exists() {
            std::fs::create_dir(&path)?;
            return Ok(path);
        }
    }
    unreachable!()
}

pub fn rename(path: &Path, new_name: &str) -> std::io::Result<PathBuf> {
    let name = new_name.trim();
    if name.is_empty() || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid name"));
    }
    let target = path.with_file_name(name);
    if target != path {
        if target.exists() && !target.to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy()) {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "name already taken"));
        }
        std::fs::rename(path, &target)?;
    }
    Ok(target)
}

pub fn copy_text(text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(None).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let dst = GlobalLock(mem) as *mut u16;
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
                let _ = GlobalUnlock(mem);
                let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(mem.0));
            }
        }
        let _ = CloseClipboard();
    }
}

pub fn paste_text() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let text = GetClipboardData(CF_UNICODETEXT.0 as u32).ok().and_then(|h| {
            let mem = HGLOBAL(h.0);
            let src = GlobalLock(mem) as *const u16;
            if src.is_null() {
                return None;
            }
            let s = PCWSTR(src).to_string().ok();
            let _ = GlobalUnlock(mem);
            s
        });
        let _ = CloseClipboard();
        text
    }
}

/// True when both paths are on the same drive (so a drag moves instead of copies).
pub fn same_volume(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| p.components().next().map(|c| c.as_os_str().to_string_lossy().to_lowercase());
    root(a).is_some() && root(a) == root(b)
}

/// Extensions Quick Look shows as plain text.
const TEXT_EXT: &[&str] = &[
    "txt", "md", "markdown", "log", "csv", "tsv", "json", "jsonc", "toml", "yaml", "yml", "ini", "cfg", "conf", "xml",
    "html", "htm", "css", "scss", "js", "mjs", "cjs", "ts", "tsx", "jsx", "rs", "py", "rb", "go", "java", "kt", "swift",
    "c", "h", "cc", "cpp", "hpp", "cs", "lua", "sh", "bash", "zsh", "ps1", "psm1", "bat", "cmd", "sql", "gitignore",
    "gitattributes", "editorconfig", "env", "lock", "properties", "gradle", "cmake", "mk", "srt", "reg", "nfo",
];

/// The start of a text file for Quick Look (None for folders and binary / unknown types).
pub fn text_preview(e: &Entry) -> Option<String> {
    use std::io::Read;
    const MAX: u64 = 32 * 1024;
    let named = matches!(e.name.to_lowercase().as_str(), "makefile" | "dockerfile" | "license" | "readme");
    if e.is_dir || !(TEXT_EXT.contains(&e.ext.as_str()) || named) {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(&e.path).ok()?.take(MAX).read_to_end(&mut buf).ok()?;
    if buf.contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&buf);
    Some(text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\t', "    "))
}

pub fn open_terminal(dir: &Path) {
    use std::os::windows::process::CommandExt;
    // Elevated, a terminal would inherit admin rights: Explorer starts it instead.
    if crate::win::elevation::is_elevated() {
        let d = dir.display().to_string();
        if !crate::win::elevation::shell_open("wt.exe", Some(&format!("-d \"{d}\"")), Some(&d)) {
            crate::win::elevation::shell_open("cmd.exe", None, Some(&d));
        }
        return;
    }
    let ok = std::process::Command::new("wt").arg("-d").arg(dir).spawn().is_ok();
    if !ok {
        const CREATE_NEW_CONSOLE: u32 = 0x10;
        let _ = std::process::Command::new("cmd").current_dir(dir).creation_flags(CREATE_NEW_CONSOLE).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["file 10", "File 2", "file 1", "alpha"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["alpha", "file 1", "File 2", "file 10"]);
    }

    #[test]
    fn volumes_compare_drive_letters() {
        assert!(same_volume(Path::new("C:\\Users\\a"), Path::new("c:\\temp")));
        assert!(!same_volume(Path::new("C:\\Users"), Path::new("D:\\Users")));
    }

    #[test]
    fn sizes_use_decimal_units() {
        assert_eq!(format_size(0), "Zero bytes");
        assert_eq!(format_size(999), "999 bytes");
        assert_eq!(format_size(1_500), "2 KB"); // Finder rounds KB to whole numbers
        assert_eq!(format_size(1_500_000), "1.5 MB");
        assert_eq!(format_size(12_000), "12 KB");
        assert_eq!(format_size(3_400_000_000), "3.4 GB");
    }
}
