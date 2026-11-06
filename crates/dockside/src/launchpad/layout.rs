//! Launchpad's arrangement (pages of apps and folders, as you left them) and
//! how often each app is opened from Spotlight or Launchpad (Tahoe's
//! Suggestions). Saved to `%APPDATA%\DockSide\launchpad.json`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::categories::{self, Category};

/// A Launchpad page: 7 columns by 5 rows.
pub const COLS: usize = 7;
pub const ROWS: usize = 5;
pub const PER_PAGE: usize = COLS * ROWS;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Slot {
    App { target: String },
    Folder { name: String, apps: Vec<String> },
}

impl Slot {
    pub fn app(target: &str) -> Slot {
        Slot::App { target: target.to_string() }
    }

    /// A stable id for animating this slot as it moves.
    pub fn id(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            Slot::App { target } => ("app", target).hash(&mut h),
            // A folder keeps its id while apps come and go: it's its first app's.
            Slot::Folder { apps, .. } => ("folder", apps.first()).hash(&mut h),
        }
        h.finish()
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct Launches {
    pub count: u32,
    /// Seconds since 1970.
    pub last: u64,
}

/// An app by name and launch target (hidden from Launchpad, or added to it).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Named {
    pub name: String,
    pub target: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    #[serde(default)]
    pub pages: Vec<Vec<Slot>>,
    #[serde(default)]
    pub launches: HashMap<String, Launches>,
    /// Apps taken out of Launchpad (and Spotlight's Apps panel).
    #[serde(default)]
    pub hidden: Vec<Named>,
    /// Apps put in that aren't in the Start menu.
    #[serde(default)]
    pub added: Vec<Named>,
}

fn path() -> PathBuf {
    crate::config::dir().join("launchpad.json")
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl State {
    pub fn load() -> State {
        std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        match serde_json::to_string_pretty(self) {
            Ok(t) => {
                if let Err(e) = std::fs::write(path(), t) {
                    log!("could not save launchpad.json: {e}");
                }
            }
            Err(e) => log!("could not save launchpad.json: {e}"),
        }
    }

    /// Forgets the arrangement (it's rebuilt from scratch next time).
    pub fn reset_layout() {
        let mut s = State::load();
        s.pages.clear();
        s.save();
    }

    /// Takes an app out of Launchpad (wherever it is, folders too).
    pub fn hide(&mut self, name: &str, target: &str) {
        for page in &mut self.pages {
            page.retain_mut(|slot| match slot {
                Slot::App { target: t } => t != target,
                Slot::Folder { apps, .. } => {
                    apps.retain(|t| t != target);
                    !apps.is_empty()
                }
            });
        }
        self.tidy();
        // An added app is simply removed; anything else is remembered as hidden.
        let before = self.added.len();
        self.added.retain(|a| a.target != target);
        if self.added.len() == before && !self.hidden.iter().any(|h| h.target == target) {
            self.hidden.push(Named { name: name.to_string(), target: target.to_string() });
        }
    }

    pub fn record_launch(target: &str) {
        let mut s = State::load();
        let e = s.launches.entry(target.to_string()).or_default();
        e.count += 1;
        e.last = now();
        s.save();
    }

    /// Up to `n` apps worth suggesting: opened often, and lately.
    pub fn suggestions(&self, n: usize) -> Vec<String> {
        let t = now();
        let mut v: Vec<(&String, f64)> = self
            .launches
            .iter()
            .map(|(k, l)| {
                let days = t.saturating_sub(l.last) as f64 / 86_400.0;
                (k, l.count as f64 / (1.0 + days))
            })
            .collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v.into_iter().take(n).map(|(k, _)| k.clone()).collect()
    }

    /// Brings the arrangement up to date with the installed `apps` (name,
    /// target): uninstalled apps leave, new ones join the last page. The first
    /// time, apps are laid out by name with system tools in a Utilities folder.
    pub fn reconcile(&mut self, apps: &[(String, String)]) {
        let installed: HashSet<&str> = apps.iter().map(|(_, t)| t.as_str()).collect();
        if self.pages.iter().all(Vec::is_empty) {
            self.pages = default_pages(apps);
            return;
        }
        let mut seen = HashSet::new();
        for page in &mut self.pages {
            page.retain_mut(|slot| match slot {
                Slot::App { target } => installed.contains(target.as_str()) && seen.insert(target.clone()),
                Slot::Folder { apps, .. } => {
                    apps.retain(|t| installed.contains(t.as_str()) && seen.insert(t.clone()));
                    !apps.is_empty()
                }
            });
        }
        let mut new: Vec<&(String, String)> = apps.iter().filter(|(_, t)| !seen.contains(t)).collect();
        new.sort_by_key(|(n, _)| n.to_lowercase());
        for (_, t) in new {
            self.push(Slot::app(t));
        }
        self.tidy();
    }

    /// Adds a slot at the end, starting a page when the last is full.
    pub fn push(&mut self, slot: Slot) {
        match self.pages.last_mut() {
            Some(p) if p.len() < PER_PAGE => p.push(slot),
            _ => self.pages.push(vec![slot]),
        }
    }

    /// Inserts `slot` at `index` on `page`, pushing what no longer fits onto the next pages.
    pub fn insert(&mut self, page: usize, index: usize, slot: Slot) {
        while self.pages.len() <= page {
            self.pages.push(Vec::new());
        }
        let i = index.min(self.pages[page].len());
        self.pages[page].insert(i, slot);
        let mut p = page;
        while self.pages[p].len() > PER_PAGE {
            let spill = self.pages[p].pop().unwrap();
            if p + 1 == self.pages.len() {
                self.pages.push(Vec::new());
            }
            self.pages[p + 1].insert(0, spill);
            p += 1;
        }
    }

    /// Drops empty pages (keeping one).
    pub fn tidy(&mut self) {
        self.pages.retain(|p| !p.is_empty());
        if self.pages.is_empty() {
            self.pages.push(Vec::new());
        }
    }
}

fn default_pages(apps: &[(String, String)]) -> Vec<Vec<Slot>> {
    let mut sorted: Vec<&(String, String)> = apps.iter().collect();
    sorted.sort_by_key(|(n, _)| n.to_lowercase());
    let mut utilities = Vec::new();
    let mut s = State::default();
    for (name, target) in sorted {
        if categories::of(name, target) == Category::Utilities {
            utilities.push(target.clone());
        } else {
            s.push(Slot::app(target));
        }
    }
    if utilities.len() > 1 {
        s.push(Slot::Folder { name: "Utilities".into(), apps: utilities });
    } else if let Some(t) = utilities.pop() {
        s.push(Slot::App { target: t });
    }
    s.tidy();
    s.pages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps(names: &[&str]) -> Vec<(String, String)> {
        names.iter().map(|n| (n.to_string(), format!("t:{n}"))).collect()
    }

    #[test]
    fn insert_spills_onto_next_page() {
        let mut s = State::default();
        for i in 0..PER_PAGE {
            s.push(Slot::app(&format!("a{i}")));
        }
        s.insert(0, 0, Slot::app("new"));
        assert_eq!(s.pages.len(), 2);
        assert_eq!(s.pages[0].len(), PER_PAGE);
        assert_eq!(s.pages[1], vec![Slot::app(&format!("a{}", PER_PAGE - 1))]);
    }

    #[test]
    fn reconcile_removes_and_adds() {
        let mut s = State::default();
        s.reconcile(&apps(&["Zed", "Alpha", "Task Manager", "Settings"]));
        assert_eq!(s.pages[0][0], Slot::app("t:Alpha"));
        assert!(matches!(&s.pages[0][2], Slot::Folder { apps, .. } if apps.len() == 2));
        s.reconcile(&apps(&["Zed", "Settings", "Beta"]));
        assert_eq!(s.pages[0][0], Slot::app("t:Zed"));
        // The folder lost one app but stays a folder.
        assert!(matches!(&s.pages[0][1], Slot::Folder { apps, .. } if apps.len() == 1));
        assert_eq!(s.pages[0][2], Slot::app("t:Beta"));
    }
}
