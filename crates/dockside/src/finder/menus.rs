//! Finder's context menus: macOS's items and order, with Explorer's commands
//! (Open With, Run as administrator, Copy as path, Share, ...) and the
//! shell's own menu, extensions included, under "More Options".

use std::collections::HashSet;
use std::path::PathBuf;

use windows::Win32::Foundation::RECT;
use windows::Win32::UI::Shell::IContextMenu;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

use super::fs::{Entry as FileEntry, KindCache, PlaceKind, SortKey};
use super::{ops, shell, xcmd, Col, FinderCmd, FinderEvent, FinderWindow, Group, Hit, Pane, View};
use crate::config::ShellItems;
use crate::menu::{Action, Entry};

/// Shell verbs Finder's own item menu already has (Open, Cut, Copy, Move to Trash,
/// Get Info, Rename, Make Shortcut, Copy as Path, Share, Run as Administrator, Put Back).
const ITEM_VERBS: &[&str] = &[
    "open", "opennewwindow", "opennewtab", "cut", "copy", "delete", "properties", "rename", "link", "copyaspath",
    "Windows.ModernShare", "runas", "undelete",
];
/// Shell submenus it already has.
const ITEM_MENUS: &[&str] = &["Open with"];
/// The same for a folder's background menu (Paste, Paste Shortcut, Undo, Get Info; View, Sort By, Group By, New).
const BACKGROUND_VERBS: &[&str] = &["paste", "pastelink", "properties", "undo", "redo", "refresh"];
const BACKGROUND_MENUS: &[&str] = &["View", "Sort by", "Group by", "New"];

/// An item's label (not a separator's).
fn label(e: &Entry) -> Option<&str> {
    match e {
        Entry::Item(i) => Some(&i.label),
        Entry::Separator => None,
    }
}
use crate::win::system::Shortcut;

const DEL: u16 = 0x2E;
const ENTER: u16 = 0x0D;
const SPACE: u16 = 0x20;
const F2: u16 = 0x71;
const PERIOD: u16 = 0xBE;

fn key(c: char) -> u16 {
    c as u16
}

/// Files that "Run as administrator" applies to.
fn runnable(ext: &str) -> bool {
    matches!(ext, "exe" | "bat" | "cmd" | "msi" | "com" | "lnk" | "msc" | "cpl")
}

impl FinderWindow {
    fn cmd(&self, c: FinderCmd) -> Action {
        Action::Finder { window: self.id, cmd: c }
    }

    /// The shell's own menu for items (or a folder's background): the classic
    /// menu's items, plus the verbs apps add to Windows 11's new menu (WinRAR,
    /// 7-Zip, "Open in Terminal", PowerToys), which the classic one leaves out.
    fn more_options(&self, cm: Option<IContextMenu>, extended: bool, paths: &[PathBuf], target: xcmd::Target) -> Option<shell::Filled> {
        let id = self.id;
        let action = move |n: u32| Action::Finder { window: id, cmd: FinderCmd::ShellCommand(n) };
        let mut filled = cm.map(|cm| shell::menu_entries(cm, extended, &action)).unwrap_or_default();
        let packaged = xcmd::entries(paths, target, &action);
        if !packaged.is_empty() {
            // Once Windows has loaded them itself, the classic menu has some of the same: keep one.
            let names: HashSet<String> = packaged.iter().filter_map(|e| label(e).map(str::to_lowercase)).collect();
            filled.entries.retain(|e| label(e).is_none_or(|l| !names.contains(&l.to_lowercase())));
            // After the classic menu's first group (its Open verbs), where Explorer puts them.
            let at = filled.entries.iter().position(|e| matches!(e, Entry::Separator)).unwrap_or(filled.entries.len());
            let tail = filled.entries.split_off(at);
            filled.entries.push(Entry::Separator);
            filled.entries.extend(packaged);
            filled.entries.push(Entry::Separator);
            filled.entries.extend(tail);
            filled.entries = tidy(std::mem::take(&mut filled.entries));
        }
        Some(filled).filter(|f| !f.entries.is_empty())
    }

    /// Adds the shell's items to the end of a menu: under "More Options", or
    /// (`[finder] shell_items = "inline"`) right in it, without the ones whose
    /// verbs (or submenus, by name) Finder's own items already cover.
    fn add_shell_items(&self, v: &mut Vec<Entry>, more: Option<shell::Filled>, covered_verbs: &[&str], covered_menus: &[&str]) {
        let Some(m) = more else { return };
        v.push(Entry::Separator);
        match self.shell_items {
            ShellItems::Submenu => v.push(Entry::submenu("More Options", m.entries)),
            ShellItems::Inline => {
                let ids: HashSet<u32> =
                    m.verbs.iter().filter(|(verb, _)| covered_verbs.iter().any(|c| c.eq_ignore_ascii_case(verb))).map(|(_, id)| *id).collect();
                let keep = |e: &Entry| match e {
                    Entry::Separator => true,
                    Entry::Item(i) => {
                        let shell_id = match &i.action {
                            Some(Action::Finder { cmd: FinderCmd::ShellCommand(n), .. }) => Some(*n),
                            _ => None,
                        };
                        !shell_id.is_some_and(|n| ids.contains(&n))
                            && !(i.action.is_none() && covered_menus.iter().any(|c| c.eq_ignore_ascii_case(&i.label)))
                    }
                };
                v.extend(m.entries.into_iter().filter(keep));
            }
        }
    }

    pub fn on_right_click(&mut self, x: f32, y: f32, shift: bool, kinds: &mut KindCache) -> FinderEvent {
        if self.rename.is_some() {
            self.commit_rename(kinds);
        }
        if self.quicklook.is_some() {
            return FinderEvent::None;
        }
        self.search_focus = false;
        self.address = None;
        let hit = self.hit_at(x, y);
        // Right-clicking an unselected item selects it first.
        match hit {
            Some(Hit::Item(i)) if !self.selected.contains(&i) => {
                self.selected = [i].into();
                self.anchor = Some(i);
                self.cursor = Some(i);
            }
            Some(Hit::Column(c, j)) if self.columns.get(c).and_then(|c| c.selected) != Some(j) => self.select_column(c, j, kinds),
            Some(Hit::ColumnBg(c)) => {
                self.columns.truncate(c + 1);
                self.columns[c].selected = None;
                self.active_col = c;
            }
            // Right-clicking the folder's blank space deselects, like Finder.
            Some(Hit::Content) if self.view != View::Columns => {
                self.selected.clear();
                self.anchor = None;
            }
            _ => {}
        }
        self.dirty = true;
        let entries = match hit {
            Some(Hit::Item(_) | Hit::Column(..)) => self.item_menu(shift),
            Some(Hit::Place(i)) => self.place_menu(i, shift),
            Some(Hit::Header(_)) => self.column_menu(),
            Some(Hit::Content | Hit::ColumnBg(_)) => self.background_menu(shift),
            _ => return FinderEvent::None,
        };
        self.menu_at(entries, x, y)
    }

    /// Shift+F10 or the Menu key: the selection's menu (or the folder's) at the selection.
    pub(super) fn key_menu(&mut self) -> FinderEvent {
        let has_sel = !self.selection().is_empty();
        let vr = self.view_rect();
        let (x, y) = match self.cursor.filter(|_| self.view != View::Columns).and_then(|i| self.layout.cells.get(i)) {
            Some(r) if self.view == View::Gallery => (r.cx() - self.scroll.offset(), r.cy()),
            Some(r) => (r.cx().min(vr.right() - 20.0), (r.cy() - self.scroll.offset()).clamp(vr.y, vr.bottom())),
            None => (vr.x + 40.0, vr.y + 40.0),
        };
        let entries = if has_sel { self.item_menu(false) } else { self.background_menu(false) };
        self.menu_at(entries, x, y)
    }

    fn menu_at(&self, entries: Vec<Entry>, x: f32, y: f32) -> FinderEvent {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        FinderEvent::Menu(entries, rc.left as f32 / self.scale + x, rc.top as f32 / self.scale + y)
    }

    fn item_menu(&mut self, shift: bool) -> Vec<Entry> {
        use FinderCmd::*;
        let sel: Vec<FileEntry> = self.selection().into_iter().cloned().collect();
        let Some(first) = sel.first().cloned() else { return Vec::new() };
        let paths: Vec<PathBuf> = sel.iter().map(|e| e.path.clone()).collect();
        let one = sel.len() == 1;
        let in_trash = first.deleted.is_some() || self.cwd.as_os_str().eq_ignore_ascii_case(shell::RECYCLE_BIN);
        let fs = sel.iter().all(|e| e.fs);
        let kinds: Vec<(bool, String)> = sel.iter().map(|e| (e.is_dir, e.ext.clone())).collect();
        let more = self.more_options(shell::item_menu(&paths), shift, &paths, xcmd::Target::Items(kinds));
        let mut v = vec![Entry::item("Open", self.cmd(Open(first.path.clone())))];
        if first.is_dir && one {
            v.push(Entry::item("Open in New Tab", self.cmd(OpenTab(first.path.clone()))));
            v.push(Entry::item("Open in New Window", self.cmd(OpenWindow(first.path.clone()))));
        }
        if !first.is_dir && !in_trash {
            let mut with: Vec<Entry> = shell::handlers(&first.ext)
                .into_iter()
                .enumerate()
                .map(|(i, h)| {
                    let label = if h.default { format!("{} (default)", h.name) } else { h.name };
                    Entry::item(label, self.cmd(OpenWith(i, paths.clone()))).with_pixels(h.icon)
                })
                .collect();
            if !with.is_empty() {
                with.push(Entry::Separator);
            }
            with.push(Entry::item("Choose Another App…", self.cmd(ChooseApp(first.path.clone()))));
            v.push(Entry::submenu("Open With", with));
        }
        if one && runnable(&first.ext) {
            v.push(Entry::item("Run as Administrator", self.cmd(RunAsAdmin(first.path.clone()))));
        }
        if in_trash {
            v.push(Entry::item("Put Back", self.cmd(Verb(paths.clone(), "undelete"))));
        }
        v.push(Entry::Separator);
        if in_trash {
            v.push(Entry::item("Delete Immediately…", self.cmd(Delete { paths: paths.clone(), recycle: false })).shortcut(Shortcut::new("", DEL, "Delete")));
        } else {
            v.push(Entry::item("Move to Trash", self.cmd(Delete { paths: paths.clone(), recycle: true })).shortcut(Shortcut::new("", DEL, "Delete")));
            v.push(Entry::item("Delete Immediately…", self.cmd(Delete { paths: paths.clone(), recycle: false })).shortcut(Shortcut::new("s", DEL, "Delete")));
        }
        v.push(Entry::Separator);
        v.push(Entry::item("Get Info", self.cmd(Properties(paths.clone()))).shortcut(Shortcut::new("a", ENTER, "Enter")));
        if !in_trash {
            if one && first.fs {
                v.push(Entry::item("Rename", self.cmd(Rename)).shortcut(Shortcut::new("", F2, "F2")));
            }
            if fs {
                let label = if one { format!("Compress “{}”", first.label(self.show_ext)) } else { format!("Compress {} Items", sel.len()) };
                v.push(Entry::item(label, self.cmd(Compress(paths.clone()))));
            }
            if one && fs && matches!(first.ext.as_str(), "zip") {
                v.push(Entry::item("Extract All", self.cmd(Extract(first.path.clone()))));
            }
            if fs {
                v.push(Entry::item("Duplicate", self.cmd(Duplicate(paths.clone()))).shortcut(Shortcut::new("c", key('D'), "D")));
                v.push(Entry::item("Make Shortcut", self.cmd(MakeShortcut(paths.clone()))));
            }
            let ql = if one { format!("Quick Look “{}”", first.label(self.show_ext)) } else { format!("Quick Look {} Items", sel.len()) };
            v.push(Entry::item(ql, self.cmd(QuickLook)).shortcut(Shortcut::new("", SPACE, "Space")));
            v.push(Entry::Separator);
            v.push(Entry::item("Cut", self.cmd(Cut(paths.clone()))).shortcut(Shortcut::new("c", key('X'), "X")));
            v.push(Entry::item("Copy", self.cmd(Copy(paths.clone()))).shortcut(Shortcut::new("c", key('C'), "C")));
            if one && first.is_dir && shell::can_paste() {
                v.push(Entry::item("Paste Into Folder", self.cmd(Paste(first.path.clone()))));
            }
        }
        if fs {
            let label = if one { "Copy as Path" } else { "Copy as Paths" };
            v.push(Entry::item(label, self.cmd(CopyPath { paths: paths.clone(), quoted: true })).shortcut(Shortcut::new("cs", key('C'), "C")));
        }
        if let Some(id) = more.as_ref().and_then(|m| m.verb("Windows.ModernShare")) {
            v.push(Entry::item("Share…", self.cmd(ShellCommand(id))));
        }
        v.push(Entry::Separator);
        if one && first.is_dir {
            if self.pins.contains(&first.path) {
                v.push(Entry::item("Remove from Sidebar", self.cmd(Unpin(first.path.clone()))));
            } else {
                v.push(Entry::item("Add to Sidebar", self.cmd(Pin(first.path.clone()))));
            }
            if first.fs {
                v.push(Entry::item("New Terminal at Folder", self.cmd(Terminal(first.path.clone()))));
            }
        }
        if fs {
            v.push(Entry::item("Show in File Explorer", self.cmd(Reveal(first.path.clone()))));
        }
        self.add_shell_items(&mut v, more, ITEM_VERBS, ITEM_MENUS);
        tidy(v)
    }

    fn background_menu(&mut self, shift: bool) -> Vec<Entry> {
        use FinderCmd::*;
        let dir = self.target_dir();
        let fs_dir = shell::is_fs_path(&dir) && dir.is_dir();
        let in_trash = dir.as_os_str().eq_ignore_ascii_case(shell::RECYCLE_BIN);
        let more = self.more_options(shell::background_menu(self.hwnd, &dir), shift, std::slice::from_ref(&dir), xcmd::Target::Background);
        let mut v = Vec::new();
        if fs_dir {
            v.push(Entry::item("New Folder", self.cmd(NewFolder)).shortcut(Shortcut::new("cs", key('N'), "N")));
            let types: Vec<Entry> =
                shell::new_types().into_iter().enumerate().map(|(i, t)| Entry::item(t.name, self.cmd(NewFile(i)))).collect();
            v.push(Entry::submenu("New", types));
            v.push(Entry::Separator);
        }
        if !in_trash {
            let paste = shell::can_paste();
            let item = |label: &str, c: FinderCmd| if paste { Entry::item(label, self.cmd(c)) } else { Entry::disabled(label) };
            v.push(item("Paste", Paste(dir.clone())).shortcut(Shortcut::new("c", key('V'), "V")));
            if fs_dir {
                v.push(item("Paste Shortcut", PasteShortcut(dir.clone())));
            }
        }
        match ops::undo_label() {
            Some(l) => v.push(Entry::item(l, self.cmd(Undo)).shortcut(Shortcut::new("c", key('Z'), "Z"))),
            None => v.push(Entry::disabled("Undo")),
        }
        if let Some(l) = ops::redo_label() {
            v.push(Entry::item(l, self.cmd(Redo)).shortcut(Shortcut::new("c", key('Y'), "Y")));
        }
        v.push(Entry::Separator);
        v.push(Entry::item("Get Info", self.cmd(Properties(vec![dir.clone()]))).shortcut(Shortcut::new("a", ENTER, "Enter")));
        if in_trash {
            v.push(Entry::item("Empty Trash", self.cmd(EmptyTrash)));
        }
        v.push(Entry::Separator);
        let view = |label: &str, vw: View, k: char| Entry::item(label, self.cmd(SetView(vw))).checked(self.view == vw).shortcut(Shortcut::new("c", key(k), static_char(k)));
        v.push(view("as Icons", View::Icons, '1'));
        v.push(view("as List", View::List, '2'));
        v.push(view("as Columns", View::Columns, '3'));
        v.push(view("as Gallery", View::Gallery, '4'));
        v.push(Entry::submenu("Sort By", self.sort_entries()));
        v.push(Entry::submenu("Group By", self.group_entries_menu()));
        v.push(Entry::submenu("Show", self.show_entries()));
        v.push(Entry::Separator);
        v.push(Entry::item("Go to Folder…", self.cmd(EditAddress)).shortcut(Shortcut::new("cs", key('G'), "G")));
        if fs_dir {
            v.push(Entry::item("New Terminal at Folder", self.cmd(Terminal(dir.clone()))));
            v.push(Entry::item("Open in File Explorer", self.cmd(Explorer(dir.clone()))));
            v.push(Entry::item("Copy as Path", self.cmd(CopyPath { paths: vec![dir.clone()], quoted: true })));
        }
        if !self.pins.contains(&dir) && !self.places.iter().any(|p| p.path == dir) {
            v.push(Entry::item("Add to Sidebar", self.cmd(Pin(dir.clone()))));
        }
        self.add_shell_items(&mut v, more, BACKGROUND_VERBS, BACKGROUND_MENUS);
        tidy(v)
    }

    fn sort_entries(&self) -> Vec<Entry> {
        let mut keys = vec![
            (SortKey::Name, "Name"),
            (SortKey::Kind, "Kind"),
            (SortKey::Modified, "Date Modified"),
            (SortKey::Created, "Date Created"),
            (SortKey::Size, "Size"),
        ];
        if self.searching.is_some() || self.cwd.as_os_str().eq_ignore_ascii_case(shell::RECYCLE_BIN) {
            keys.push((SortKey::Location, "Location"));
        }
        if self.cwd.as_os_str().eq_ignore_ascii_case(shell::RECYCLE_BIN) {
            keys.push((SortKey::Deleted, "Date Deleted"));
        }
        let mut v: Vec<Entry> = keys.into_iter().map(|(k, l)| Entry::item(l, self.cmd(FinderCmd::Sort(k))).checked(self.sort == k)).collect();
        v.push(Entry::Separator);
        // Choosing the current key again flips the order.
        let flip = self.cmd(FinderCmd::Sort(self.sort));
        v.push(Entry::item("Ascending", flip.clone()).checked(self.ascending));
        v.push(Entry::item("Descending", flip).checked(!self.ascending));
        v
    }

    fn group_entries_menu(&self) -> Vec<Entry> {
        [(Group::None, "None"), (Group::Name, "Name"), (Group::Kind, "Kind"), (Group::Modified, "Date Modified"), (Group::Size, "Size")]
            .into_iter()
            .map(|(g, l)| Entry::item(l, self.cmd(FinderCmd::GroupBy(g))).checked(self.group == g))
            .collect()
    }

    fn show_entries(&self) -> Vec<Entry> {
        vec![
            Entry::item("Hidden Files", self.cmd(FinderCmd::ToggleHidden)).checked(self.show_hidden).shortcut(Shortcut::new("cs", PERIOD, ".")),
            Entry::item("File Name Extensions", self.cmd(FinderCmd::ToggleExtensions)).checked(self.show_ext),
            Entry::Separator,
            Entry::item("Preview Pane", self.cmd(FinderCmd::SetPane(Pane::Preview))).checked(self.pane == Pane::Preview).shortcut(Shortcut::new("a", key('P'), "P")),
            Entry::item("Details Pane", self.cmd(FinderCmd::SetPane(Pane::Details))).checked(self.pane == Pane::Details).shortcut(Shortcut::new("as", key('P'), "P")),
        ]
    }

    fn place_menu(&mut self, i: usize, shift: bool) -> Vec<Entry> {
        use FinderCmd::*;
        let Some(place) = self.places.get(i).cloned() else { return Vec::new() };
        let p = place.path.clone();
        let mut v = vec![
            Entry::item("Open in New Tab", self.cmd(OpenTab(p.clone()))),
            Entry::item("Open in New Window", self.cmd(OpenWindow(p.clone()))),
            Entry::Separator,
            Entry::item("Get Info", self.cmd(Properties(vec![p.clone()]))),
        ];
        match place.kind {
            PlaceKind::Removable => v.push(Entry::item("Eject", self.cmd(Verb(vec![p.clone()], "eject")))),
            PlaceKind::Trash => v.push(Entry::item("Empty Trash", self.cmd(EmptyTrash))),
            PlaceKind::Pinned => v.push(Entry::item("Remove from Sidebar", self.cmd(Unpin(p.clone())))),
            _ => {}
        }
        if shell::is_fs_path(&p) {
            v.push(Entry::item("Show in File Explorer", self.cmd(Explorer(p.clone()))));
        }
        let kinds = vec![(true, String::new())];
        let more = self.more_options(shell::item_menu(std::slice::from_ref(&p)), shift, std::slice::from_ref(&p), xcmd::Target::Items(kinds));
        self.add_shell_items(&mut v, more, ITEM_VERBS, ITEM_MENUS);
        tidy(v)
    }

    /// Right-clicking the list's header: which columns to show.
    fn column_menu(&self) -> Vec<Entry> {
        let mut v: Vec<Entry> = vec![Entry::disabled("Name").checked(true)];
        for c in [Col::Modified, Col::Created, Col::Size, Col::Kind, Col::Location] {
            v.push(Entry::item(c.label(), self.cmd(FinderCmd::ToggleColumn(c))).checked(self.list_cols.contains(&c)));
        }
        v.push(Entry::Separator);
        v.push(Entry::submenu("Group By", self.group_entries_menu()));
        v
    }
}

fn static_char(c: char) -> &'static str {
    match c {
        '1' => "1",
        '2' => "2",
        '3' => "3",
        _ => "4",
    }
}

/// No leading, trailing or doubled separators.
fn tidy(v: Vec<Entry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(v.len());
    for e in v {
        if matches!(e, Entry::Separator) && matches!(out.last(), None | Some(Entry::Separator)) {
            continue;
        }
        out.push(e);
    }
    while matches!(out.last(), Some(Entry::Separator)) {
        out.pop();
    }
    out
}
