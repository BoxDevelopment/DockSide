//! File operations through `IFileOperation`: Windows' own progress, "Replace or
//! Skip", elevation and Recycle Bin handling, plus an undo / redo stack for
//! Ctrl+Z / Ctrl+Y that Finder keeps itself (Explorer's isn't open to other apps).
//!
//! Every operation's results come back through a progress sink, and each undo
//! step is just another operation, so undoing records how to redo.
//!
//! Everything here can run a modal progress loop: call it outside the App borrow.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use windows::core::{implement, Interface, IUnknown, Result, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, IDataObject, CLSCTX_ALL};
use windows::Win32::UI::Shell::*;

use super::shell;

/// One step of an undo: what to do to an item to put things back.
enum Step {
    Delete(IShellItem),
    Move { item: IShellItem, to: IShellItem, name: String },
    Rename { item: IShellItem, name: String },
}

struct Undo {
    label: String,
    steps: Vec<Step>,
}

/// What an operation did to one top-level item, as reported by the shell.
enum Record {
    Created(IShellItem),
    Moved { new: IShellItem, from: IShellItem, name: String },
    Renamed { new: IShellItem, name: String },
    /// Recycled items can be moved back out of the Recycle Bin; permanently deleted ones can't.
    Deleted { recycled: IShellItem, from: IShellItem, name: String },
}

thread_local! {
    static UNDO: RefCell<Vec<Undo>> = const { RefCell::new(Vec::new()) };
    static REDO: RefCell<Vec<Undo>> = const { RefCell::new(Vec::new()) };
    /// Items dropped from another app without file paths (a phone's photos, say), waiting to be copied.
    static DROPPED: RefCell<Option<IDataObject>> = const { RefCell::new(None) };
}

const UNDO_LIMIT: usize = 50;

#[implement(IFileOperationProgressSink)]
struct Sink {
    records: Rc<RefCell<Vec<Record>>>,
    /// Only items landing directly in this folder are top-level (copying a folder reports its contents too).
    dest: Option<String>,
}

impl Sink {
    fn top(&self, folder: Option<&IShellItem>) -> bool {
        match (&self.dest, folder) {
            (None, _) => true,
            (Some(d), Some(f)) => shell::parsing_name(f).is_some_and(|n| n.eq_ignore_ascii_case(d)),
            (Some(_), None) => false,
        }
    }
}

fn name_of(item: &IShellItem) -> Option<String> {
    unsafe { item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING).ok().map(|p| {
        let s = p.to_string().unwrap_or_default();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        s
    }) }
}

impl IFileOperationProgressSink_Impl for Sink_Impl {
    fn StartOperations(&self) -> Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _: HRESULT) -> Result<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _: u32, _: Option<&IShellItem>, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn PostRenameItem(&self, _: u32, item: Option<&IShellItem>, _: &PCWSTR, hr: HRESULT, new: Option<&IShellItem>) -> Result<()> {
        if let (true, Some(item), Some(new)) = (hr.is_ok(), item, new) {
            if let Some(name) = name_of(item) {
                self.records.borrow_mut().push(Record::Renamed { new: new.clone(), name });
            }
        }
        Ok(())
    }
    fn PreMoveItem(&self, _: u32, _: Option<&IShellItem>, _: Option<&IShellItem>, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn PostMoveItem(&self, _: u32, item: Option<&IShellItem>, dest: Option<&IShellItem>, _: &PCWSTR, hr: HRESULT, new: Option<&IShellItem>) -> Result<()> {
        if let (true, true, Some(item), Some(new)) = (hr.is_ok(), self.top(dest), item, new) {
            let from = unsafe { item.GetParent().ok() };
            if let (Some(from), Some(name)) = (from, name_of(item)) {
                self.records.borrow_mut().push(Record::Moved { new: new.clone(), from, name });
            }
        }
        Ok(())
    }
    fn PreCopyItem(&self, _: u32, _: Option<&IShellItem>, _: Option<&IShellItem>, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn PostCopyItem(&self, _: u32, _: Option<&IShellItem>, dest: Option<&IShellItem>, _: &PCWSTR, hr: HRESULT, new: Option<&IShellItem>) -> Result<()> {
        if let (true, true, Some(new)) = (hr.is_ok(), self.top(dest), new) {
            self.records.borrow_mut().push(Record::Created(new.clone()));
        }
        Ok(())
    }
    fn PreDeleteItem(&self, _: u32, _: Option<&IShellItem>) -> Result<()> {
        Ok(())
    }
    fn PostDeleteItem(&self, _: u32, item: Option<&IShellItem>, hr: HRESULT, recycled: Option<&IShellItem>) -> Result<()> {
        if let (true, Some(item), Some(recycled)) = (hr.is_ok(), item, recycled) {
            let from = unsafe { item.GetParent().ok() };
            if let (Some(from), Some(name)) = (from, name_of(item)) {
                self.records.borrow_mut().push(Record::Deleted { recycled: recycled.clone(), from, name });
            }
        }
        Ok(())
    }
    fn PreNewItem(&self, _: u32, _: Option<&IShellItem>, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn PostNewItem(&self, _: u32, _: Option<&IShellItem>, _: &PCWSTR, _: &PCWSTR, _: u32, hr: HRESULT, new: Option<&IShellItem>) -> Result<()> {
        if let (true, Some(new)) = (hr.is_ok(), new) {
            self.records.borrow_mut().push(Record::Created(new.clone()));
        }
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> Result<()> {
        Ok(())
    }
}

fn new_op(hwnd: HWND, flags: FILEOPERATION_FLAGS) -> Option<IFileOperation> {
    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL).map_err(|e| log!("IFileOperation: {e}")).ok()?;
        op.SetOperationFlags(flags).ok()?;
        if !hwnd.is_invalid() {
            let _ = op.SetOwnerWindow(hwnd);
        }
        Some(op)
    }
}

/// Runs the queued operation; returns the records of what it did.
fn perform(op: &IFileOperation, dest: Option<&Path>) -> Vec<Record> {
    let records = Rc::new(RefCell::new(Vec::new()));
    let dest = dest.and_then(shell::item).and_then(|i| shell::parsing_name(&i));
    let sink: IFileOperationProgressSink = Sink { records: records.clone(), dest }.into();
    unsafe {
        let cookie = op.Advise(&sink).ok();
        if let Err(e) = op.PerformOperations() {
            // Cancelling is reported as an error too.
            log!("file operation ended: {e}");
        }
        if let Some(c) = cookie {
            let _ = op.Unadvise(c);
        }
    }
    drop(sink);
    let out = std::mem::take(&mut *records.borrow_mut());
    out
}

/// How to put back what the records did, last first.
fn inverse(records: &[Record]) -> Vec<Step> {
    records
        .iter()
        .rev()
        .map(|r| match r {
            Record::Created(new) => Step::Delete(new.clone()),
            Record::Moved { new, from, name } => Step::Move { item: new.clone(), to: from.clone(), name: name.clone() },
            Record::Renamed { new, name } => Step::Rename { item: new.clone(), name: name.clone() },
            Record::Deleted { recycled, from, name } => Step::Move { item: recycled.clone(), to: from.clone(), name: name.clone() },
        })
        .collect()
}

/// Where the records left things (to select them afterwards).
fn results(records: &[Record]) -> Vec<PathBuf> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Created(n) | Record::Moved { new: n, .. } | Record::Renamed { new: n, .. } => shell::parsing_name(n).map(PathBuf::from),
            Record::Deleted { .. } => None,
        })
        .collect()
}

fn push_undo(label: &str, steps: Vec<Step>) {
    if steps.is_empty() {
        return;
    }
    UNDO.with(|u| {
        let mut u = u.borrow_mut();
        u.push(Undo { label: label.to_string(), steps });
        if u.len() > UNDO_LIMIT {
            u.remove(0);
        }
    });
    REDO.with(|r| r.borrow_mut().clear());
}

fn finish(label: &str, records: Vec<Record>) -> Vec<PathBuf> {
    let out = results(&records);
    push_undo(label, inverse(&records));
    out
}

/// "Undo Move", or None when there's nothing to undo.
pub fn undo_label() -> Option<String> {
    UNDO.with(|u| u.borrow().last().map(|u| format!("Undo {}", u.label)))
}

pub fn redo_label() -> Option<String> {
    REDO.with(|r| r.borrow().last().map(|u| format!("Redo {}", u.label)))
}

fn apply(hwnd: HWND, steps: &[Step]) -> Vec<Record> {
    let flags = FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOF_NOCONFIRMATION | FOF_NOCONFIRMMKDIR | FOF_WANTNUKEWARNING;
    let Some(op) = new_op(hwnd, flags) else { return Vec::new() };
    unsafe {
        for s in steps {
            let _ = match s {
                Step::Delete(item) => op.DeleteItem(item, None),
                Step::Move { item, to, name } => op.MoveItem(item, to, &HSTRING::from(name.as_str()), None),
                Step::Rename { item, name } => op.RenameItem(item, &HSTRING::from(name.as_str()), None),
            };
        }
    }
    perform(&op, None)
}

/// Ctrl+Z: returns the items it brought back.
pub fn undo(hwnd: HWND) -> Vec<PathBuf> {
    let Some(u) = UNDO.with(|u| u.borrow_mut().pop()) else { return Vec::new() };
    let records = apply(hwnd, &u.steps);
    let out = results(&records);
    let steps = inverse(&records);
    if !steps.is_empty() {
        REDO.with(|r| r.borrow_mut().push(Undo { label: u.label, steps }));
    }
    out
}

/// Ctrl+Y / Ctrl+Shift+Z.
pub fn redo(hwnd: HWND) -> Vec<PathBuf> {
    let Some(u) = REDO.with(|r| r.borrow_mut().pop()) else { return Vec::new() };
    let records = apply(hwnd, &u.steps);
    let out = results(&records);
    let steps = inverse(&records);
    if !steps.is_empty() {
        UNDO.with(|s| s.borrow_mut().push(Undo { label: u.label, steps }));
    }
    out
}

/// Records something done outside `IFileOperation` (an inline rename, a new folder) for Ctrl+Z.
pub fn record_rename(new_path: &Path, old_name: &str) {
    if let Some(item) = shell::item(new_path) {
        push_undo("Rename", vec![Step::Rename { item, name: old_name.to_string() }]);
    }
}

pub fn record_created(label: &str, paths: &[PathBuf]) {
    push_undo(label, paths.iter().filter_map(|p| shell::item(p)).map(Step::Delete).collect());
}

/// What a drop or paste does with its items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropOp {
    Copy,
    Move,
    /// Create shortcuts to the items (Alt-drag, or "Create Shortcuts Here").
    Link,
}

fn same_parent(paths: &[PathBuf], dest: &Path) -> bool {
    !paths.is_empty() && paths.iter().all(|p| p.parent().is_some_and(|d| d.as_os_str().eq_ignore_ascii_case(dest.as_os_str())))
}

/// Copies or moves items into `dest`. Copies into the items' own folder get "- Copy" names.
pub fn transfer(hwnd: HWND, paths: &[PathBuf], dest: &Path, op: DropOp) -> Vec<PathBuf> {
    if op == DropOp::Link {
        return shortcuts(paths, dest);
    }
    let Some(items) = shell::item_array(paths) else { return Vec::new() };
    let copy = op == DropOp::Copy;
    let collide = if copy && same_parent(paths, dest) { FOF_RENAMEONCOLLISION } else { FILEOPERATION_FLAGS(0) };
    run_transfer(hwnd, &items.cast::<IUnknown>().unwrap(), dest, copy, collide)
}

fn run_transfer(hwnd: HWND, items: &IUnknown, dest: &Path, copy: bool, extra: FILEOPERATION_FLAGS) -> Vec<PathBuf> {
    let (Some(op), Some(to)) = (new_op(hwnd, FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR | extra), shell::item(dest)) else { return Vec::new() };
    let queued = unsafe { if copy { op.CopyItems(items, &to) } else { op.MoveItems(items, &to) } };
    if let Err(e) = queued {
        log!("queue transfer failed: {e}");
        return Vec::new();
    }
    finish(if copy { "Copy" } else { "Move" }, perform(&op, Some(dest)))
}

/// Keeps a drop that has no file paths (virtual items) for [`transfer_dropped`].
pub fn keep_dropped(data: IDataObject) {
    DROPPED.with(|d| *d.borrow_mut() = Some(data));
}

pub fn transfer_dropped(hwnd: HWND, dest: &Path, copy: bool) -> Vec<PathBuf> {
    let Some(data) = DROPPED.with(|d| d.borrow_mut().take()) else { return Vec::new() };
    run_transfer(hwnd, &data.cast::<IUnknown>().unwrap(), dest, copy, FILEOPERATION_FLAGS(0))
}

/// Pastes the clipboard's items into `dest` (moving them if they were cut).
pub fn paste(hwnd: HWND, dest: &Path) -> Vec<PathBuf> {
    let Some((data, cut)) = shell::clipboard() else { return Vec::new() };
    let paths = super::dnd::hdrop_paths(&data);
    // Pasting a copy where the originals are makes "- Copy"s, like Explorer.
    let collide = if !cut && same_parent(&paths, dest) { FOF_RENAMEONCOLLISION } else { FILEOPERATION_FLAGS(0) };
    let out = run_transfer(hwnd, &data.cast::<IUnknown>().unwrap(), dest, !cut, collide);
    if cut && !out.is_empty() {
        shell::clear_clipboard();
    }
    out
}

/// Moves to the Recycle Bin (`recycle`) or deletes for good (Windows asks first).
pub fn delete(hwnd: HWND, paths: &[PathBuf], recycle: bool) -> bool {
    let Some(items) = shell::item_array(paths) else { return false };
    let flags = if recycle {
        FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING
    } else {
        FOF_WANTNUKEWARNING
    };
    let Some(op) = new_op(hwnd, flags) else { return false };
    if unsafe { op.DeleteItems(&items) }.is_err() {
        return false;
    }
    let records = perform(&op, None);
    let any = !records.is_empty() || !recycle;
    push_undo("Move to Trash", inverse(&records));
    any
}

/// macOS's Duplicate (Ctrl+D): "name - Copy" beside each item.
pub fn duplicate(hwnd: HWND, paths: &[PathBuf]) -> Vec<PathBuf> {
    let Some(dir) = paths.first().and_then(|p| p.parent()).map(Path::to_path_buf) else { return Vec::new() };
    let Some(items) = shell::item_array(paths) else { return Vec::new() };
    let Some(op) = new_op(hwnd, FOF_ALLOWUNDO | FOF_RENAMEONCOLLISION | FOF_NOCONFIRMMKDIR) else { return Vec::new() };
    let Some(to) = shell::item(&dir) else { return Vec::new() };
    if unsafe { op.CopyItems(&items, &to) }.is_err() {
        return Vec::new();
    }
    finish("Duplicate", perform(&op, Some(&dir)))
}

/// Shortcuts to `paths` in `dest` (Explorer's "Create shortcut", macOS's Make Alias).
pub fn shortcuts(paths: &[PathBuf], dest: &Path) -> Vec<PathBuf> {
    let made: Vec<PathBuf> = paths.iter().filter_map(|p| shell::create_shortcut(p, dest)).collect();
    record_created("Make Shortcut", &made);
    made
}

/// An empty zip archive (just the end-of-central-directory record).
const EMPTY_ZIP: [u8; 22] = [0x50, 0x4B, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Compresses items into "name.zip" (or "Archive.zip") beside them. Like
/// Explorer's "Send to > Compressed folder", the items are dropped on the
/// shell's zip folder, which compresses them in the background with Windows'
/// own progress; Windows' tar.exe is the fallback.
pub fn compress(_hwnd: HWND, paths: &[PathBuf]) -> Option<PathBuf> {
    let dir = paths.first()?.parent()?.to_path_buf();
    let stem = match paths {
        [one] => one.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Archive".into()),
        _ => "Archive".into(),
    };
    let zip = shell::unique(&dir, &stem, "zip");
    std::fs::write(&zip, EMPTY_ZIP).ok()?;
    if !drop_into(&zip, paths) && !tar_zip(&dir, &zip, paths) {
        let _ = std::fs::remove_file(&zip);
        return None;
    }
    record_created("Compress", &[zip.clone()]);
    Some(zip)
}

/// Drops items on a shell folder's drop target (how zip folders take files).
fn drop_into(folder: &Path, paths: &[PathBuf]) -> bool {
    use windows::Win32::Foundation::POINTL;
    use windows::Win32::System::Ole::{IDropTarget, DROPEFFECT, DROPEFFECT_COPY};
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    (|| unsafe {
        let target: IDropTarget = shell::item(folder)?.BindToHandler(None, &BHID_SFUIObject).ok()?;
        let data = shell::data_object(paths)?;
        let mut effect: DROPEFFECT = DROPEFFECT_COPY;
        let pt = POINTL::default();
        target.DragEnter(&data, MODIFIERKEYS_FLAGS(0), pt, &mut effect).ok()?;
        if effect.0 & DROPEFFECT_COPY.0 == 0 {
            let _ = target.DragLeave();
            return None;
        }
        target.Drop(&data, MODIFIERKEYS_FLAGS(0), pt, &mut effect).ok()?;
        Some(())
    })()
    .is_some()
}

fn tar_zip(dir: &Path, zip: &Path, paths: &[PathBuf]) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let names: Vec<_> = paths.iter().filter_map(|p| p.file_name()).collect();
    std::process::Command::new("tar")
        .arg("-a")
        .arg("-c")
        .arg("-f")
        .arg(zip)
        .arg("-C")
        .arg(dir)
        .args(names)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
}

/// Extract All: the archive's contents into a folder named after it.
pub fn extract(hwnd: HWND, archive: &Path) -> Option<PathBuf> {
    let dir = archive.parent()?;
    let stem = archive.file_stem()?.to_string_lossy().into_owned();
    let dest = shell::unique(dir, &stem, "");
    std::fs::create_dir(&dest).ok()?;
    let ok = (|| unsafe {
        let src = shell::item(archive)?;
        let en: IEnumShellItems = src.BindToHandler(None, &BHID_EnumItems).ok()?;
        let op = new_op(hwnd, FOF_NOCONFIRMMKDIR)?;
        let to = shell::item(&dest)?;
        let mut any = false;
        loop {
            let mut item = [None];
            let mut got = 0;
            if en.Next(&mut item, Some(&mut got)).is_err() || got == 0 {
                break;
            }
            if let Some(i) = item[0].take() {
                any |= op.CopyItem(&i, &to, PCWSTR::null(), None).is_ok();
            }
        }
        if !any {
            return Some(());
        }
        perform(&op, None);
        let aborted = op.GetAnyOperationsAborted().map(|b| b.as_bool()).unwrap_or(false);
        (!aborted).then_some(())
    })();
    if ok.is_none() {
        let _ = std::fs::remove_dir_all(&dest);
        return None;
    }
    record_created("Extract", &[dest.clone()]);
    Some(dest)
}
