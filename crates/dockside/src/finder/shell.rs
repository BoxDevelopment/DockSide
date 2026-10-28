//! Shell namespace side of Finder: places that aren't plain folders (This PC,
//! Network, the Recycle Bin, phones, zip contents, libraries) listed through
//! `IShellFolder`, the shell's own context menus, Open With handlers, item
//! properties, shortcuts, "New" file types and the file clipboard.
//!
//! Locations are kept as parsing names (`PathBuf`s holding "C:\..." or
//! "::{GUID}\..."), which `SHCreateItemFromParsingName` turns back into items.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::{Interface, HSTRING, PCSTR, PCWSTR, PSTR, PWSTR};
use windows::Win32::Foundation::{FILETIME, HGLOBAL, HWND, LPARAM, POINT, S_OK, WPARAM};
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_Capacity, PKEY_DateCreated, PKEY_DateModified, PKEY_FreeSpace, PKEY_ItemTypeText, PKEY_PropList_PreviewDetails, PKEY_Size,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IDataObject, IPersistFile, CLSCTX_INPROC_SERVER, DVASPECT_CONTENT, FORMATETC, STGMEDIUM,
    STGMEDIUM_0, STGM_READ, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::{IsClipboardFormatAvailable, RegisterClipboardFormatW};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{OleFlushClipboard, OleGetClipboard, OleSetClipboard, CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, RRF_RT_ANY, REG_VALUE_TYPE};
use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM, SFGAO_FLAGS, SFGAO_FOLDER, SFGAO_HIDDEN, SFGAO_LINK, SFGAO_STREAM};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IPropertyDescription, IPropertyDescriptionList, IPropertyStore, GPS_BESTEFFORT, PDFF_DEFAULT, PROPERTYKEY,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemInfoW, HMENU, MENUITEMINFOW, MFS_DISABLED, MFT_SEPARATOR,
    MIIM_BITMAP, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, SW_SHOWNORMAL, WM_INITMENUPOPUP,
};

use super::fs::Entry;
use crate::gfx::icons::IconPixels;
use crate::menu::{Action, Entry as MenuEntry};

pub const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
pub const NETWORK: &str = "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}";
pub const RECYCLE_BIN: &str = crate::gfx::icons::RECYCLE_BIN;
pub const LIBRARIES: &str = "::{031E4825-7B94-4DC3-B131-E946B44C8DD5}";

/// Recycle Bin columns: where an item was deleted from, and when.
const PKEY_DISPLACED_FROM: PROPERTYKEY =
    PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0x9b174b33_40ff_11d2_a27e_00c04fc30871), pid: 2 };
const PKEY_DISPLACED_DATE: PROPERTYKEY =
    PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0x9b174b33_40ff_11d2_a27e_00c04fc30871), pid: 3 };

const CMIC_MASK_UNICODE: u32 = 0x4000;
/// Command ids the shell's context menus are given.
const FIRST_ID: u32 = 1;
const LAST_ID: u32 = 0x7FFF;

thread_local! {
    /// The context menu behind the Finder menu that's open (its "More Options" items invoke it).
    static MENU: RefCell<Option<ShellMenu>> = const { RefCell::new(None) };
    /// The Open With handlers listed in the open menu.
    static HANDLERS: RefCell<Vec<IAssocHandler>> = const { RefCell::new(Vec::new()) };
    static TITLES: RefCell<HashMap<PathBuf, String>> = RefCell::new(HashMap::new());
    static NEW_TYPES: RefCell<Option<Vec<NewType>>> = const { RefCell::new(None) };
    static FOLDER_EXT: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
    /// Items cut to the clipboard (shown faded until pasted or replaced).
    static CUT: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

pub fn is_cut(path: &Path) -> bool {
    CUT.with(|c| {
        let c = c.borrow();
        !c.is_empty() && c.iter().any(|p| p == path)
    })
}

/// A drive ("C:\...") or UNC ("\\server\share\...") path, rather than a shell parsing name.
pub fn is_fs_path(p: &Path) -> bool {
    let s = p.as_os_str().to_string_lossy();
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\') || (s.starts_with("\\\\") && !s.starts_with("\\\\?\\"))
}

/// The root places, which have no parent in Finder's path bar.
pub fn is_root(p: &Path) -> bool {
    let s = p.as_os_str().to_string_lossy();
    [THIS_PC, NETWORK, RECYCLE_BIN, LIBRARIES].iter().any(|r| s.eq_ignore_ascii_case(r))
}

unsafe fn take(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

pub fn item(path: &Path) -> Option<IShellItem> {
    unsafe { SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None).ok() }
}

pub fn parsing_name(item: &IShellItem) -> Option<String> {
    unsafe { item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING).ok().map(|p| take(p)) }
}

fn filetime(ft: FILETIME) -> Option<SystemTime> {
    let t = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    // 100 ns ticks since 1601.
    let unix = (t / 10).checked_sub(11_644_473_600_000_000)?;
    (t != 0).then(|| UNIX_EPOCH + Duration::from_micros(unix))
}

/// A Finder entry for a shell item.
pub fn entry(item: &IShellItem2) -> Option<Entry> {
    unsafe {
        let base: IShellItem = item.cast().ok()?;
        let path = parsing_name(&base)?;
        let mask = SFGAO_FOLDER | SFGAO_FILESYSTEM | SFGAO_HIDDEN | SFGAO_STREAM | SFGAO_LINK;
        let attrs = base.GetAttributes(mask).unwrap_or(SFGAO_FLAGS(0));
        let has = |f: SFGAO_FLAGS| attrs.0 & f.0 != 0;
        // Zip files are folders and streams: they list as files (and open into Finder).
        let is_dir = has(SFGAO_FOLDER) && !has(SFGAO_STREAM);
        let editing = base.GetDisplayName(SIGDN_PARENTRELATIVEEDITING).ok().map(|p| take(p)).filter(|s| !s.is_empty());
        let normal = base.GetDisplayName(SIGDN_NORMALDISPLAY).ok().map(|p| take(p)).unwrap_or_default();
        let name = if is_dir { normal } else { editing.unwrap_or(normal) };
        let ext = if is_dir {
            String::new()
        } else {
            Path::new(&name).extension().or(Path::new(&path).extension()).map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
        };
        let location = item.GetString(&PKEY_DISPLACED_FROM).ok().map(|p| take(p));
        let mut e = Entry {
            name,
            path: PathBuf::from(&path),
            is_dir,
            size: item.GetUInt64(&PKEY_Size).unwrap_or(0),
            modified: item.GetFileTime(&PKEY_DateModified).ok().and_then(filetime),
            created: item.GetFileTime(&PKEY_DateCreated).ok().and_then(filetime),
            ext,
            // Recycled files have real paths, but only the Recycle Bin may touch them.
            fs: has(SFGAO_FILESYSTEM) && is_fs_path(Path::new(&path)) && location.is_none(),
            hidden: has(SFGAO_HIDDEN),
            deleted: item.GetFileTime(&PKEY_DISPLACED_DATE).ok().and_then(filetime),
            location,
            extra: None,
            // Plain files and folders keep Finder's own kinds (cached by extension).
            type_text: if has(SFGAO_FILESYSTEM) && !is_dir { None } else { item.GetString(&PKEY_ItemTypeText).ok().map(|p| take(p)).filter(|t| !t.is_empty() && t != "File folder") },
        };
        if let (Ok(cap), Ok(free)) = (item.GetUInt64(&PKEY_Capacity), item.GetUInt64(&PKEY_FreeSpace)) {
            if cap > 0 {
                e.size = cap;
                e.extra = Some(format!("{} free of {}", super::fs::format_size(free), super::fs::format_size(cap)));
            }
        }
        Some(e)
    }
}

/// Lists a shell folder (anything `std::fs` can't: This PC, Network, the Recycle Bin, zip files, phones, ...).
pub fn list(dir: &Path, show_hidden: bool) -> Result<Vec<Entry>, String> {
    unsafe {
        let item = item(dir).ok_or_else(|| "This location can't be found.".to_string())?;
        let folder: IShellFolder =
            item.BindToHandler(None, &BHID_SFObject).map_err(|_| "This item can't be opened as a folder.".to_string())?;
        let mut flags = SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0;
        if show_hidden {
            flags |= SHCONTF_INCLUDEHIDDEN.0 | SHCONTF_INCLUDESUPERHIDDEN.0;
        }
        let mut en: Option<IEnumIDList> = None;
        let hr = folder.EnumObjects(HWND::default(), flags as u32, &mut en);
        if hr.is_err() {
            return Err(format!("The folder can't be opened: {}", hr.message()));
        }
        let Some(en) = en else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        loop {
            let mut pidl = [std::ptr::null_mut::<ITEMIDLIST>(); 1];
            let mut got = 0;
            if en.Next(&mut pidl, Some(&mut got)) != S_OK || got == 0 {
                break;
            }
            let child: windows::core::Result<IShellItem2> = SHCreateItemWithParent(None, &folder, pidl[0]);
            CoTaskMemFree(Some(pidl[0] as *const _));
            if let Some(e) = child.ok().and_then(|c| entry(&c)) {
                if show_hidden || !e.hidden {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }
}

/// A shell listing running on its own thread (Network discovery or a slow
/// phone can take seconds). Results post `msg` with `id` to `notify`.
pub struct Pending {
    pub dir: PathBuf,
    rx: std::sync::mpsc::Receiver<Result<Vec<Entry>, String>>,
}

impl Pending {
    pub fn start(dir: PathBuf, show_hidden: bool, notify: isize, msg: u32, id: u32) -> Pending {
        let (tx, rx) = std::sync::mpsc::channel();
        let d = dir.clone();
        let _ = std::thread::Builder::new().name("finder-list".into()).spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            }
            let _ = tx.send(list(&d, show_hidden));
            crate::win::post_wp(notify, msg, id as usize);
        });
        Pending { dir, rx }
    }

    pub fn take(&self) -> Option<Result<Vec<Entry>, String>> {
        self.rx.try_recv().ok()
    }
}

/// The folder above `p`: drive roots go up to This PC, shares to their server and on to Network.
pub fn parent(p: &Path) -> Option<PathBuf> {
    if is_root(p) {
        return None;
    }
    let s = p.as_os_str().to_string_lossy();
    if is_fs_path(p) {
        if s.starts_with("\\\\") {
            let parts: Vec<&str> = s.trim_matches('\\').split('\\').filter(|x| !x.is_empty()).collect();
            return match parts.len() {
                0 | 1 => Some(NETWORK.into()),
                2 => Some(format!("\\\\{}", parts[0]).into()),
                _ => p.parent().map(Path::to_path_buf),
            };
        }
        return Some(p.parent().map(Path::to_path_buf).unwrap_or_else(|| THIS_PC.into()));
    }
    unsafe {
        let parent = item(p)?.GetParent().ok()?;
        // The namespace root (the desktop) has no parent of its own: stop below it.
        parent.GetParent().ok()?;
        parsing_name(&parent).map(PathBuf::from)
    }
}

/// `dir` and its ancestors, root first.
pub fn crumbs(dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![dir.to_path_buf()];
    while let Some(p) = parent(v.last().unwrap()) {
        if v.len() > 64 || v.contains(&p) {
            break;
        }
        v.push(p);
    }
    v.reverse();
    v
}

/// What the shell calls a location ("This PC", "Recycle Bin", a phone's name), cached.
pub fn title(p: &Path) -> String {
    if is_fs_path(p) {
        if let Some(n) = p.file_name() {
            return n.to_string_lossy().into_owned();
        }
    }
    if let Some(t) = TITLES.with(|t| t.borrow().get(p).cloned()) {
        return t;
    }
    let name = unsafe { item(p).and_then(|i| i.GetDisplayName(SIGDN_NORMALDISPLAY).ok()).map(|s| take(s)) };
    let name = name.filter(|n| !n.is_empty()).unwrap_or_else(|| p.display().to_string());
    TITLES.with(|t| t.borrow_mut().insert(p.to_path_buf(), name.clone()));
    name
}

/// Opens an item with its default verb (works for virtual items, like a phone's photos).
pub fn open(path: &Path) {
    if is_fs_path(path) {
        return crate::win::system::open(&path.to_string_lossy());
    }
    unsafe {
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        if SHParseDisplayName(&HSTRING::from(path.as_os_str()), None, &mut pidl, 0, None).is_err() {
            return;
        }
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST,
            lpIDList: pidl as *mut _,
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        if let Err(e) = ShellExecuteExW(&mut info) {
            log!("open {} failed: {e}", path.display());
        }
        CoTaskMemFree(Some(pidl as *const _));
    }
}

/// "Run as administrator" (the UAC prompt comes up).
pub fn run_as_admin(hwnd: HWND, path: &Path) {
    let file = HSTRING::from(path.as_os_str());
    let dir = path.parent().map(|d| HSTRING::from(d.as_os_str()));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        hwnd,
        lpVerb: windows::core::w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpDirectory: dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let _ = ShellExecuteExW(&mut info);
    }
}

/// Where a shortcut to a folder leads (Finder follows those rather than opening Explorer).
pub fn folder_link_target(path: &Path) -> Option<PathBuf> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        link.cast::<IPersistFile>().ok()?.Load(&HSTRING::from(path.as_os_str()), STGM_READ).ok()?;
        let pidl = link.GetIDList().ok()?;
        let target: windows::core::Result<IShellItem> = SHCreateItemFromIDList(pidl);
        CoTaskMemFree(Some(pidl as *const _));
        let target = target.ok()?;
        let attrs = target.GetAttributes(SFGAO_FOLDER | SFGAO_STREAM).ok()?;
        (attrs.0 & SFGAO_FOLDER.0 != 0 && attrs.0 & SFGAO_STREAM.0 == 0).then(|| parsing_name(&target)).flatten().map(PathBuf::from)
    }
}

/// File types Explorer itself browses into (zip and cab files, unless another app has taken them over).
pub fn opens_as_folder(ext: &str) -> bool {
    if !matches!(ext, "zip" | "cab") {
        return false;
    }
    FOLDER_EXT.with(|c| {
        *c.borrow_mut().entry(ext.to_string()).or_insert_with(|| {
            let progid = assoc_string(ASSOCSTR_PROGID, &format!(".{ext}")).unwrap_or_default();
            progid.eq_ignore_ascii_case("CompressedFolder") || progid.eq_ignore_ascii_case("CABFolder")
        })
    })
}

fn assoc_string(what: ASSOCSTR, assoc: &str) -> Option<String> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    unsafe {
        AssocQueryStringW(ASSOCF_NONE, what, &HSTRING::from(assoc), PCWSTR::null(), PWSTR(buf.as_mut_ptr()), &mut len).ok().ok()?;
    }
    Some(crate::win::from_wide(&buf)).filter(|s| !s.is_empty())
}

/// An array of shell items (they must share a parent for context menus and data objects).
pub fn item_array(paths: &[PathBuf]) -> Option<IShellItemArray> {
    unsafe {
        let mut pidls: Vec<*const ITEMIDLIST> = Vec::new();
        for p in paths {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(&HSTRING::from(p.as_os_str()), None, &mut pidl, 0, None).is_ok() && !pidl.is_null() {
                pidls.push(pidl);
            }
        }
        if pidls.is_empty() {
            return None;
        }
        let array = SHCreateShellItemArrayFromIDLists(&pidls);
        for &pidl in &pidls {
            CoTaskMemFree(Some(pidl as *const _));
        }
        array.map_err(|e| log!("item array failed: {e}")).ok()
    }
}

/// The shell's own data object for items (CF_HDROP, ID lists, drag images, virtual items).
pub fn data_object(paths: &[PathBuf]) -> Option<IDataObject> {
    unsafe { item_array(paths)?.BindToHandler(None, &BHID_DataObject).ok() }
}

// --- context menus ----------------------------------------------------------

/// A shell context menu, filled and kept alive while its items may still be chosen.
pub struct ShellMenu {
    cm: IContextMenu,
    hmenu: HMENU,
}

impl Drop for ShellMenu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.hmenu);
        }
    }
}

/// The context menu for items (they must share a parent folder).
pub fn item_menu(paths: &[PathBuf]) -> Option<IContextMenu> {
    unsafe { item_array(paths)?.BindToHandler(None, &BHID_SFUIObject).ok() }
}

/// The context menu for a folder's background (New, Paste, view extensions like "Open in Terminal").
pub fn background_menu(hwnd: HWND, dir: &Path) -> Option<IContextMenu> {
    unsafe {
        let folder: IShellFolder = item(dir)?.BindToHandler(None, &BHID_SFObject).ok()?;
        folder.CreateViewObject(hwnd).ok()
    }
}

/// A filled shell menu: its entries, and the canonical verbs it offers with their command ids.
pub struct Filled {
    pub entries: Vec<MenuEntry>,
    pub verbs: Vec<(String, u32)>,
}

impl Filled {
    pub fn verb(&self, name: &str) -> Option<u32> {
        self.verbs.iter().find(|(v, _)| v.eq_ignore_ascii_case(name)).map(|v| v.1)
    }
}

/// Fills `cm` and turns it into menu entries whose actions come from `action(id)`.
/// The menu stays alive (for [`invoke`]) until the next one replaces it.
pub fn menu_entries(cm: IContextMenu, extended: bool, action: &dyn Fn(u32) -> Action) -> Filled {
    let mut filled = Filled { entries: Vec::new(), verbs: Vec::new() };
    unsafe {
        let Ok(hmenu) = CreatePopupMenu() else { return filled };
        let flags = CMF_NORMAL | CMF_EXPLORE | if extended { CMF_EXTENDEDVERBS } else { 0 };
        if cm.QueryContextMenu(hmenu, 0, FIRST_ID, LAST_ID, flags).is_err() {
            let _ = DestroyMenu(hmenu);
            return filled;
        }
        filled.entries = walk(&cm, hmenu, action, 0, &mut filled.verbs);
        MENU.with(|m| *m.borrow_mut() = Some(ShellMenu { cm, hmenu }));
    }
    filled
}

unsafe fn walk(cm: &IContextMenu, menu: HMENU, action: &dyn Fn(u32) -> Action, depth: u32, verbs: &mut Vec<(String, u32)>) -> Vec<MenuEntry> {
    let cm2: Option<IContextMenu2> = cm.cast().ok();
    let cm3: Option<IContextMenu3> = cm.cast().ok();
    let mut out: Vec<MenuEntry> = Vec::new();
    for i in 0..GetMenuItemCount(menu).max(0) as u32 {
        let mut buf = [0u16; 260];
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_STRING | MIIM_BITMAP,
            dwTypeData: PWSTR(buf.as_mut_ptr()),
            cch: buf.len() as u32 - 1,
            ..Default::default()
        };
        if GetMenuItemInfoW(menu, i, true, &mut mii).is_err() {
            continue;
        }
        if mii.fType.0 & MFT_SEPARATOR.0 != 0 {
            if !matches!(out.last(), None | Some(MenuEntry::Separator)) {
                out.push(MenuEntry::Separator);
            }
            continue;
        }
        let label = clean_label(&String::from_utf16_lossy(&buf[..(mii.cch as usize).min(buf.len())]));
        if label.is_empty() {
            continue;
        }
        let bmp = mii.hbmpItem;
        // HBMMENU_CALLBACK (-1) and the system bitmaps (1..=11) aren't real bitmaps.
        let icon = (bmp.0 as isize > 11).then(|| crate::gfx::icons::hbitmap_pixels(bmp)).flatten();
        let entry = if !mii.hSubMenu.is_invalid() && depth < 4 {
            // Lazy submenus (Send to, New, Open with) fill themselves as they open.
            let (wp, lp) = (WPARAM(mii.hSubMenu.0 as usize), LPARAM(i as isize));
            if let Some(c3) = &cm3 {
                let _ = c3.HandleMenuMsg2(WM_INITMENUPOPUP, wp, lp, None);
            } else if let Some(c2) = &cm2 {
                let _ = c2.HandleMenuMsg(WM_INITMENUPOPUP, wp, lp);
            }
            MenuEntry::submenu(label, walk(cm, mii.hSubMenu, action, depth + 1, verbs))
        } else if (FIRST_ID..=LAST_ID).contains(&mii.wID) && mii.fState.0 & MFS_DISABLED.0 == 0 {
            let mut vb = [0u16; 128];
            let got = cm.GetCommandString((mii.wID - FIRST_ID) as usize, GCS_VERBW, None, PSTR(vb.as_mut_ptr() as *mut u8), vb.len() as u32);
            if got.is_ok() {
                verbs.push((crate::win::from_wide(&vb), mii.wID));
            }
            MenuEntry::item(label, action(mii.wID))
        } else {
            MenuEntry::disabled(label)
        };
        out.push(entry.with_pixels(icon));
    }
    while matches!(out.last(), Some(MenuEntry::Separator)) {
        out.pop();
    }
    out
}

/// "&Open with\tCtrl+O" -> "Open with".
fn clean_label(s: &str) -> String {
    let s = s.split('\t').next().unwrap_or("");
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                out.push('&');
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    // "(&N)"-style accelerators in East Asian menus.
    out.trim().trim_end_matches("()").trim().to_string()
}

fn invoke_info(hwnd: HWND, verb_a: PCSTR, verb_w: PCWSTR, pt: Option<POINT>) -> CMINVOKECOMMANDINFOEX {
    CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | if pt.is_some() { CMIC_MASK_PTINVOKE } else { 0 },
        hwnd,
        lpVerb: verb_a,
        lpVerbW: verb_w,
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: pt.unwrap_or_default(),
        ..Default::default()
    }
}

/// Runs item `id` of the open shell menu. Modal for some verbs: call outside the App borrow.
pub fn invoke(hwnd: HWND, id: u32, pt: Option<POINT>) {
    let Some(cm) = MENU.with(|m| m.borrow().as_ref().map(|m| m.cm.clone())) else { return };
    let offset = (id - FIRST_ID) as usize;
    let info = invoke_info(hwnd, PCSTR(offset as *const u8), PCWSTR(offset as *const u16), pt);
    unsafe {
        if let Err(e) = cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO) {
            log!("shell command {id} failed: {e}");
        }
    }
}

/// Runs a canonical verb ("properties", "Windows.ModernShare", "pintohome", "eject", "undelete") on items.
pub fn invoke_verb(hwnd: HWND, paths: &[PathBuf], verb: &str) -> bool {
    let Some(cm) = item_menu(paths) else { return false };
    let a: Vec<u8> = verb.bytes().chain(std::iter::once(0)).collect();
    let w = HSTRING::from(verb);
    unsafe {
        // Dynamic verbs only exist once the menu has been built.
        let Ok(hmenu) = CreatePopupMenu() else { return false };
        let _ = cm.QueryContextMenu(hmenu, 0, FIRST_ID, LAST_ID, CMF_NORMAL);
        let info = invoke_info(hwnd, PCSTR(a.as_ptr()), PCWSTR(w.as_ptr()), None);
        let ok = cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO).map_err(|e| log!("verb {verb} failed: {e}")).is_ok();
        let _ = DestroyMenu(hmenu);
        ok
    }
}

// --- Open With ---------------------------------------------------------------

pub struct Handler {
    pub name: String,
    pub icon: Option<IconPixels>,
    pub default: bool,
}

/// Apps registered for `ext` (recommended ones), the default first. Kept for [`open_with`].
pub fn handlers(ext: &str) -> Vec<Handler> {
    let mut list = Vec::new();
    let mut out = Vec::new();
    let assoc = format!(".{ext}");
    let default = assoc_string(ASSOCSTR_FRIENDLYAPPNAME, &assoc);
    unsafe {
        if let Ok(en) = SHAssocEnumHandlers(&HSTRING::from(&assoc), ASSOC_FILTER_RECOMMENDED) {
            loop {
                let mut h = [None];
                let mut got = 0;
                if en.Next(&mut h, Some(&mut got)).is_err() || got == 0 {
                    break;
                }
                let Some(h) = h[0].take() else { break };
                let name = h.GetUIName().map(|p| take(p)).unwrap_or_default();
                if name.is_empty() || out.iter().any(|o: &Handler| o.name == name) {
                    continue;
                }
                let mut loc = PWSTR::null();
                let mut index = 0;
                let icon = h
                    .GetIconLocation(&mut loc, &mut index)
                    .ok()
                    .map(|_| take(loc))
                    .filter(|l| !l.starts_with('@') && !l.is_empty())
                    .and_then(|l| crate::gfx::icons::load_shell_image(&l, 32, false));
                let is_default = default.as_deref() == Some(name.as_str());
                out.push(Handler { name, icon, default: is_default });
                list.push(h);
            }
        }
    }
    // Default app first, then alphabetical.
    let mut order: Vec<usize> = (0..out.len()).collect();
    order.sort_by(|&a, &b| out[b].default.cmp(&out[a].default).then_with(|| out[a].name.to_lowercase().cmp(&out[b].name.to_lowercase())));
    let mut sorted = Vec::new();
    let mut handlers = Vec::new();
    let mut out: Vec<Option<Handler>> = out.into_iter().map(Some).collect();
    let mut list: Vec<Option<IAssocHandler>> = list.into_iter().map(Some).collect();
    for i in order {
        sorted.extend(out[i].take());
        handlers.extend(list[i].take());
    }
    HANDLERS.with(|h| *h.borrow_mut() = handlers);
    sorted
}

/// Opens the items with handler `index` of the last [`handlers`] list.
pub fn open_with(index: usize, paths: &[PathBuf]) {
    let Some(h) = HANDLERS.with(|h| h.borrow().get(index).cloned()) else { return };
    // Elevated, the app would inherit admin rights: Explorer starts desktop apps instead.
    if crate::win::elevation::is_elevated() {
        let exe = unsafe { h.GetName().map(|p| take(p)).unwrap_or_default() };
        if exe.to_lowercase().ends_with(".exe") && Path::new(&exe).is_file() {
            for p in paths {
                crate::win::elevation::shell_open(&exe, Some(&format!("\"{}\"", p.display())), None);
            }
            return;
        }
    }
    let Some(data) = data_object(paths) else { return };
    unsafe {
        if let Err(e) = h.Invoke(&data) {
            log!("open with failed: {e}");
        }
    }
}

/// Windows' "Choose another app" dialog (modal).
pub fn choose_app(hwnd: HWND, path: &Path) {
    let file = HSTRING::from(path.as_os_str());
    let info = OPENASINFO {
        pcszFile: PCWSTR(file.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_REGISTER_EXT | OAIF_EXEC,
    };
    unsafe {
        let _ = SHOpenWithDialog(hwnd, &info);
    }
}

pub fn empty_recycle_bin(hwnd: HWND) {
    unsafe {
        let _ = SHEmptyRecycleBinW(hwnd, PCWSTR::null(), 0);
    }
}

// --- details -------------------------------------------------------------------

/// The properties Explorer's details pane shows for an item (label, formatted value).
pub fn details(path: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem2>(&HSTRING::from(path.as_os_str()), None) else { return out };
        let Ok(list) = item.GetPropertyDescriptionList::<IPropertyDescriptionList>(&PKEY_PropList_PreviewDetails) else { return out };
        let Ok(store) = item.GetPropertyStore::<IPropertyStore>(GPS_BESTEFFORT) else { return out };
        for i in 0..list.GetCount().unwrap_or(0) {
            let Ok(desc) = list.GetAt::<IPropertyDescription>(i) else { continue };
            let mut key = PROPERTYKEY::default();
            if desc.GetPropertyKey(&mut key).is_err() {
                continue;
            }
            let Ok(value) = store.GetValue(&key) else { continue };
            if value.is_empty() {
                continue;
            }
            let label = desc.GetDisplayName().map(|p| take(p)).unwrap_or_default();
            let text = desc.FormatForDisplay(&value, PDFF_DEFAULT).map(|p| take(p)).unwrap_or_default();
            // Strip the bidi marks the shell puts around dates.
            let text: String = text.chars().filter(|c| !matches!(c, '\u{200e}' | '\u{200f}')).collect();
            if !label.is_empty() && !text.trim().is_empty() {
                out.push((label, text));
            }
        }
    }
    out
}

// --- shortcuts and new files -------------------------------------------------------

/// `dir/name`, or "name (2)", "name (3)", ... if taken.
pub fn unique(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let dot = if ext.is_empty() { String::new() } else { format!(".{ext}") };
    for n in 1.. {
        let name = if n == 1 { format!("{stem}{dot}") } else { format!("{stem} ({n}){dot}") };
        let p = dir.join(name);
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

/// Creates "<name> - Shortcut.lnk" to `target` in `dir`.
pub fn create_shortcut(target: &Path, dir: &Path) -> Option<PathBuf> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(&HSTRING::from(target.as_os_str()), None, &mut pidl, 0, None).ok()?;
        let set = link.SetIDList(pidl);
        CoTaskMemFree(Some(pidl as *const _));
        set.ok()?;
        let base = title(target);
        let stem = Path::new(&base).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(base);
        let path = unique(dir, &format!("{stem} - Shortcut"), "lnk");
        link.cast::<IPersistFile>().ok()?.Save(&HSTRING::from(path.as_os_str()), true).ok()?;
        Some(path)
    }
}

#[derive(Clone, Debug)]
pub struct NewType {
    pub ext: String,
    pub name: String,
    template: Template,
}

#[derive(Clone, Debug)]
enum Template {
    Empty,
    Data(Vec<u8>),
    File(PathBuf),
}

fn reg_value(root: HKEY, key: &str, value: Option<&str>) -> Option<(REG_VALUE_TYPE, Vec<u8>)> {
    let key = HSTRING::from(key);
    let value = value.map(HSTRING::from);
    let vp = value.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    unsafe {
        let mut size = 0u32;
        let mut kind = REG_VALUE_TYPE(0);
        if RegGetValueW(root, &key, vp, RRF_RT_ANY, Some(&mut kind), None, Some(&mut size)).is_err() {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        RegGetValueW(root, &key, vp, RRF_RT_ANY, Some(&mut kind), Some(buf.as_mut_ptr() as *mut _), Some(&mut size)).ok().ok()?;
        buf.truncate(size as usize);
        Some((kind, buf))
    }
}

fn wide_strings(bytes: &[u8]) -> Vec<String> {
    let wide: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    wide.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}

fn reg_string(root: HKEY, key: &str, value: Option<&str>) -> Option<String> {
    reg_value(root, key, value).and_then(|(_, b)| wide_strings(&b).into_iter().next())
}

/// The file types in Explorer's New menu (from its own cache of ShellNew handlers), by name.
pub fn new_types() -> Vec<NewType> {
    if let Some(t) = NEW_TYPES.with(|t| t.borrow().clone()) {
        return t;
    }
    let cache = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Discardable\PostSetup\ShellNew";
    let mut exts = reg_value(HKEY_CURRENT_USER, cache, Some("Classes")).map(|(_, b)| wide_strings(&b)).unwrap_or_default();
    if !exts.iter().any(|e| e.eq_ignore_ascii_case(".txt")) {
        exts.push(".txt".into());
    }
    let mut kinds = super::fs::KindCache::default();
    let mut out: Vec<NewType> = Vec::new();
    // Shortcuts and libraries have their own commands (Make Shortcut; Libraries in the sidebar).
    let skip = [".lnk", ".library-ms"];
    for ext in exts.iter().filter(|e| e.starts_with('.') && !skip.iter().any(|s| e.eq_ignore_ascii_case(s))) {
        let progid = reg_string(HKEY_CLASSES_ROOT, ext, None);
        let keys = [Some(format!(r"{ext}\ShellNew")), progid.map(|p| format!(r"{ext}\{p}\ShellNew"))];
        let key = keys.into_iter().flatten().find(|k| {
            reg_value(HKEY_CLASSES_ROOT, k, Some("NullFile")).is_some()
                || reg_value(HKEY_CLASSES_ROOT, k, Some("Data")).is_some()
                || reg_value(HKEY_CLASSES_ROOT, k, Some("FileName")).is_some()
        });
        // A text document is always on offer, even where the new Notepad dropped its entry.
        let key = match key {
            Some(k) => k,
            None if ext.eq_ignore_ascii_case(".txt") => String::new(),
            None => continue,
        };
        let template = if key.is_empty() {
            Template::Empty
        } else if let Some(f) = reg_string(HKEY_CLASSES_ROOT, &key, Some("FileName")) {
            match template_path(&f) {
                Some(p) => Template::File(p),
                None => continue,
            }
        } else if let Some((kind, data)) = reg_value(HKEY_CLASSES_ROOT, &key, Some("Data")) {
            // REG_SZ data is written as text (without its terminator); binary as-is.
            if kind.0 == 1 {
                Template::Data(wide_strings(&data).concat().into_bytes())
            } else {
                Template::Data(data)
            }
        } else {
            Template::Empty
        };
        let e = ext.trim_start_matches('.').to_lowercase();
        let probe = Entry { ext: e.clone(), ..Default::default() };
        let name = kinds.kind(&probe);
        if out.iter().any(|t| t.name == name) {
            continue;
        }
        out.push(NewType { ext: e, name, template });
    }
    out.sort_by_key(|t| t.name.to_lowercase());
    NEW_TYPES.with(|t| *t.borrow_mut() = Some(out.clone()));
    out
}

fn template_path(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(name);
    if p.is_absolute() {
        return p.exists().then_some(p);
    }
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let candidates = [
        super::fs::known_folder(&FOLDERID_Templates),
        super::fs::known_folder(&FOLDERID_CommonTemplates),
        Some(PathBuf::from(windir).join("ShellNew")),
    ];
    candidates.into_iter().flatten().map(|d| d.join(&p)).find(|c| c.exists())
}

/// Creates "New <Type>.<ext>" in `dir` from the type's template.
pub fn create_new(dir: &Path, t: &NewType) -> std::io::Result<PathBuf> {
    let path = unique(dir, &format!("New {}", t.name), &t.ext);
    match &t.template {
        Template::Empty => std::fs::write(&path, [])?,
        Template::Data(d) => std::fs::write(&path, d)?,
        Template::File(f) => {
            std::fs::copy(f, &path)?;
        }
    }
    Ok(path)
}

// --- clipboard -------------------------------------------------------------------

fn drop_effect_format() -> u16 {
    unsafe { RegisterClipboardFormatW(windows::core::w!("Preferred DropEffect")) as u16 }
}

fn fmt(cf: u16) -> FORMATETC {
    FORMATETC { cfFormat: cf, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 }
}

/// Puts items on the clipboard to paste here or in Explorer; `cut` moves them when pasted.
pub fn set_clipboard(paths: &[PathBuf], cut: bool) -> bool {
    let Some(data) = data_object(paths) else { return false };
    CUT.with(|c| *c.borrow_mut() = if cut { paths.to_vec() } else { Vec::new() });
    unsafe {
        let effect = if cut { DROPEFFECT_MOVE.0 } else { DROPEFFECT_COPY.0 | DROPEFFECT_LINK.0 };
        if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, 4) {
            let p = GlobalLock(mem) as *mut u32;
            if !p.is_null() {
                *p = effect;
                let _ = GlobalUnlock(mem);
            }
            let medium = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: mem }, pUnkForRelease: std::mem::ManuallyDrop::new(None) };
            let _ = data.SetData(&fmt(drop_effect_format()), &medium, true);
        }
        if OleSetClipboard(&data).is_err() {
            return false;
        }
        // Keep it pasteable after DockSide quits.
        let _ = OleFlushClipboard();
    }
    true
}

/// Files (or virtual items) on the clipboard, and whether they were cut.
pub fn clipboard() -> Option<(IDataObject, bool)> {
    unsafe {
        let data = OleGetClipboard().ok()?;
        let items: windows::core::Result<IShellItemArray> = SHCreateShellItemArrayFromDataObject(&data);
        items.ok()?;
        let mut cut = false;
        if let Ok(mut medium) = data.GetData(&fmt(drop_effect_format())) {
            let mem: HGLOBAL = medium.u.hGlobal;
            let p = GlobalLock(mem) as *const u32;
            if !p.is_null() {
                cut = *p & DROPEFFECT_MOVE.0 != 0 && *p & DROPEFFECT_COPY.0 == 0;
                let _ = GlobalUnlock(mem);
            }
            windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
        }
        Some((data, cut))
    }
}

pub fn can_paste() -> bool {
    unsafe {
        let idlist = RegisterClipboardFormatW(windows::core::w!("Shell IDList Array"));
        IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() || IsClipboardFormatAvailable(idlist).is_ok()
    }
}

pub fn clear_clipboard() {
    CUT.with(|c| c.borrow_mut().clear());
    unsafe {
        let _ = OleSetClipboard(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_lose_accelerators() {
        assert_eq!(clean_label("&Open with\tCtrl+O"), "Open with");
        assert_eq!(clean_label("Save && Exit"), "Save & Exit");
    }

    #[test]
    fn paths_classify() {
        assert!(is_fs_path(Path::new("C:\\Users")));
        assert!(is_fs_path(Path::new("\\\\server\\share")));
        assert!(!is_fs_path(Path::new(THIS_PC)));
        assert!(!is_fs_path(Path::new("\\\\?\\usb#vid")));
    }

    #[test]
    fn parents_climb_to_roots() {
        assert_eq!(parent(Path::new("C:\\Users")), Some(PathBuf::from("C:\\")));
        assert_eq!(parent(Path::new("C:\\")), Some(PathBuf::from(THIS_PC)));
        assert_eq!(parent(Path::new("\\\\srv\\share")), Some(PathBuf::from("\\\\srv")));
        assert_eq!(parent(Path::new("\\\\srv")), Some(PathBuf::from(NETWORK)));
        assert_eq!(parent(Path::new(THIS_PC)), None);
    }
}
