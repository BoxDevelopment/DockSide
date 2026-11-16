//! Curated Mac-style menus for apps whose own menus can't be read (Electron
//! and Chromium apps, browsers, ...): `app_menus.toml`, shipped with DockSide,
//! and the user's own `%APPDATA%\DockSide\app_menus.toml`, whose apps replace
//! the shipped ones. The user's file is read again when it changes.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::SystemTime;

use serde::Deserialize;

use crate::menu::{Action, Entry};

const BUNDLED: &str = include_str!("../app_menus.toml");

#[derive(Deserialize, Default)]
struct PackFile {
    #[serde(default)]
    app: Vec<App>,
}

/// One app's menus.
#[derive(Deserialize, Clone, Debug)]
pub struct App {
    /// Exe file names it's for (any case).
    pub exes: Vec<String>,
    #[serde(default)]
    pub menu: Vec<Menu>,
}

/// One title on the bar and its items.
#[derive(Deserialize, Clone, Debug)]
pub struct Menu {
    pub title: String,
    #[serde(default)]
    pub items: Vec<PackItem>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum PackItem {
    /// "-": a separator (any other text: an item that does nothing).
    Separator(String),
    Item {
        name: String,
        /// A shortcut to send the app ("Ctrl+Shift+N").
        #[serde(default)]
        keys: Option<String>,
        /// A link or path to open.
        #[serde(default)]
        open: Option<String>,
        /// A submenu.
        #[serde(default)]
        items: Vec<PackItem>,
    },
}

impl App {
    pub fn titles(&self) -> Vec<String> {
        self.menu.iter().map(|m| m.title.clone()).collect()
    }
}

struct Loaded {
    apps: Vec<Rc<App>>,
    /// The user's file's modified time when read (`None`: there was none).
    user_mtime: Option<SystemTime>,
}

thread_local! {
    static PACKS: RefCell<Option<Loaded>> = const { RefCell::new(None) };
}

fn user_path() -> PathBuf {
    crate::config::dir().join("app_menus.toml")
}

fn parse(text: &str, what: &str) -> Vec<App> {
    match toml::from_str::<PackFile>(text) {
        Ok(f) => f.app,
        Err(e) => {
            log!("app menus: {what} has a mistake: {e}");
            Vec::new()
        }
    }
}

fn load(user_mtime: Option<SystemTime>) -> Loaded {
    let user = if user_mtime.is_some() {
        std::fs::read_to_string(user_path()).map(|t| parse(&t, "app_menus.toml")).unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut apps: Vec<Rc<App>> = user.into_iter().map(Rc::new).collect();
    for a in parse(BUNDLED, "the shipped app_menus.toml") {
        // The user's apps come first and replace shipped ones for the same exe.
        let taken = a.exes.iter().any(|e| apps.iter().any(|u| u.exes.iter().any(|x| x.eq_ignore_ascii_case(e))));
        if !taken {
            apps.push(Rc::new(a));
        }
    }
    Loaded { apps, user_mtime }
}

/// The curated menus for the app at `exe` (a path or file name), if any.
pub fn find(exe: &str) -> Option<Rc<App>> {
    let name = crate::win::apps::exe_name(exe);
    let mtime = std::fs::metadata(user_path()).and_then(|m| m.modified()).ok();
    PACKS.with(|p| {
        let mut p = p.borrow_mut();
        if p.as_ref().is_none_or(|l| l.user_mtime != mtime) {
            *p = Some(load(mtime));
        }
        p.as_ref()?.apps.iter().find(|a| a.exes.iter().any(|e| e.eq_ignore_ascii_case(&name))).cloned()
    })
}

/// Menu entries for curated items.
pub fn entries(items: &[PackItem]) -> Vec<Entry> {
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        match it {
            PackItem::Separator(s) if s.trim() == "-" => {
                if out.last().is_some_and(|e| !matches!(e, Entry::Separator)) {
                    out.push(Entry::Separator);
                }
            }
            // Any other plain string: a label that does nothing.
            PackItem::Separator(s) => out.push(Entry::disabled(s.clone())),
            PackItem::Item { name, keys, open, items } => {
                let sc = keys.as_deref().and_then(crate::menu::parse_accel).filter(|s| s.vk != 0);
                out.push(if !items.is_empty() {
                    Entry::submenu(name.clone(), entries(items))
                } else if let Some(url) = open {
                    let e = Entry::item(name.clone(), Action::Open(url.clone()));
                    match sc {
                        Some(sc) => e.shortcut(sc),
                        None => e,
                    }
                } else if let Some(sc) = sc {
                    Entry::keys(name.clone(), sc)
                } else {
                    Entry::disabled(name.clone())
                });
            }
        }
    }
    if matches!(out.last(), Some(Entry::Separator)) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_menus_parse_and_every_shortcut_can_be_sent() {
        let apps = parse(BUNDLED, "test");
        assert!(apps.len() >= 10, "{} apps", apps.len());
        fn check(items: &[PackItem], app: &str) {
            for it in items {
                if let PackItem::Item { name, keys, items, .. } = it {
                    if let Some(k) = keys {
                        let sc = crate::menu::parse_accel(k);
                        assert!(sc.is_some_and(|s| s.vk != 0), "{app}: {name}: can't send {k:?}");
                    }
                    check(items, app);
                } else if let PackItem::Separator(s) = it {
                    assert_eq!(s, "-", "{app}: separators are \"-\"");
                }
            }
        }
        for a in &apps {
            for m in &a.menu {
                check(&m.items, &a.exes[0]);
            }
        }
        assert!(apps.iter().any(|a| a.exes.iter().any(|e| e == "claude.exe")));
    }
}
