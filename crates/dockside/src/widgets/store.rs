//! What people type into widgets: notes (one per Notes widget, by id) and the
//! shared reminders list, kept in `%APPDATA%\DockSide\widgets.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Reminder {
    pub text: String,
    #[serde(default)]
    pub done: bool,
    /// When it was ticked off: it stays (struck through) for a moment, so a
    /// mis-click can be undone.
    #[serde(skip)]
    pub done_at: Option<Instant>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Store {
    #[serde(default)]
    pub notes: BTreeMap<u32, String>,
    #[serde(default)]
    pub reminders: Vec<Reminder>,
    #[serde(skip)]
    dirty: bool,
}

/// How long a completed reminder lingers before it's cleared.
const LINGER_SECS: f32 = 1.5;

fn path() -> PathBuf {
    crate::config::dir().join("widgets.json")
}

impl Store {
    pub fn load() -> Store {
        let mut s: Store = std::fs::read_to_string(path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).map_err(|e| log!("widgets.json not read: {e}")).ok())
            .unwrap_or_default();
        s.reminders.retain(|r| !r.done);
        s
    }

    pub fn note(&self, id: u32) -> &str {
        self.notes.get(&id).map(String::as_str).unwrap_or("")
    }

    pub fn set_note(&mut self, id: u32, text: String) {
        if self.note(id) != text {
            if text.is_empty() {
                self.notes.remove(&id);
            } else {
                self.notes.insert(id, text);
            }
            self.dirty = true;
        }
    }

    pub fn add_reminder(&mut self, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            self.reminders.push(Reminder { text: text.to_string(), ..Default::default() });
            self.dirty = true;
        }
    }

    pub fn toggle_reminder(&mut self, i: usize) {
        if let Some(r) = self.reminders.get_mut(i) {
            r.done = !r.done;
            r.done_at = r.done.then(Instant::now);
            self.dirty = true;
        }
    }

    pub fn open_reminders(&self) -> usize {
        self.reminders.iter().filter(|r| !r.done).count()
    }

    /// Clears reminders that have lingered long enough; true if any went.
    pub fn sweep(&mut self) -> bool {
        let before = self.reminders.len();
        self.reminders.retain(|r| r.done_at.is_none_or(|t| t.elapsed().as_secs_f32() < LINGER_SECS));
        let changed = self.reminders.len() != before;
        self.dirty |= changed;
        changed
    }

    pub fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let result = serde_json::to_string_pretty(self)
            .map_err(anyhow::Error::from)
            .and_then(|t| std::fs::write(path(), t).map_err(anyhow::Error::from));
        if let Err(e) = result {
            log!("could not save widgets.json: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminders_linger_then_clear() {
        let mut s = Store::default();
        s.add_reminder("  milk ");
        s.add_reminder("   ");
        assert_eq!(s.reminders.len(), 1);
        assert_eq!(s.reminders[0].text, "milk");
        s.toggle_reminder(0);
        assert_eq!(s.open_reminders(), 0);
        assert!(!s.sweep());
        s.reminders[0].done_at = Some(Instant::now() - std::time::Duration::from_secs(5));
        assert!(s.sweep());
        assert!(s.reminders.is_empty());
    }

    #[test]
    fn notes_round_trip() {
        let mut s = Store::default();
        s.set_note(3, "hello\nworld".into());
        let text = serde_json::to_string(&s).unwrap();
        let back: Store = serde_json::from_str(&text).unwrap();
        assert_eq!(back.note(3), "hello\nworld");
        assert_eq!(back.note(4), "");
    }
}
