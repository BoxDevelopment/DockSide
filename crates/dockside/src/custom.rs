//! Names and icons the user gave apps (Rename… and Change Icon… in the Dock
//! or Launchpad), looked up by an app's launch target so Launchpad and
//! Spotlight's Apps panel show what the Dock shows.
//!
//! Where they're kept: a pinned app's own `[[dock.apps]]` entry (`name`,
//! `icon`), and `[app_names]` / `[app_icons]` by exe file name (or, for an
//! app with no exe to go by, like a Store app, by its target).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use crate::config::Config;
use crate::win::apps;

#[derive(Default)]
struct State {
    /// Pinned apps: (lower-cased path, name, icon, lower-cased `match` exe).
    pinned: Vec<(String, Option<String>, Option<String>, Option<String>)>,
    /// `[app_names]` / `[app_icons]`, keys lower-cased.
    names: HashMap<String, String>,
    icons: HashMap<String, String>,
    /// A target's exe file name (lower case), resolved once: shortcuts are a COM call each.
    exe_of: HashMap<String, Option<String>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Takes in the config (at start and on every reload).
pub fn set(cfg: &Config) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.pinned = cfg
            .dock
            .apps
            .iter()
            .map(|a| (a.path.to_lowercase(), a.name.clone(), a.icon.clone(), a.match_exe.as_ref().map(|m| m.to_lowercase())))
            .collect();
        // The first of differently-cased keys wins, as in the menu bar (`AppNames::name_for`).
        let lower = |m: &std::collections::BTreeMap<String, String>| {
            let mut out = HashMap::new();
            for (k, v) in m {
                out.entry(k.to_lowercase()).or_insert_with(|| v.clone());
            }
            out
        };
        s.names = lower(&cfg.app_names);
        s.icons = lower(&cfg.app_icons);
    });
}

/// Programs that start other things (a folder, a script, a game launcher's
/// game): an entry they run isn't that program, so it's never keyed by it.
const HOSTS: &[&str] = &[
    "explorer.exe", "cmd.exe", "powershell.exe", "pwsh.exe", "wt.exe", "conhost.exe", "rundll32.exe", "mmc.exe",
    "msiexec.exe", "wscript.exe", "cscript.exe", "control.exe", "hh.exe", "update.exe", "steam.exe",
];

/// Takes in the exes behind the Start menu's apps (from Spotlight's index).
/// An exe that several entries run (Python and its docs, a game launcher's
/// games) or that only hosts them names none of them: those go by their own target.
pub fn set_app_exes(exes: &HashMap<String, String>) {
    let mut count: HashMap<&str, usize> = HashMap::new();
    for e in exes.values() {
        *count.entry(e.as_str()).or_default() += 1;
    }
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.exe_of.retain(|k, _| !k.starts_with("shell:"));
        for (target, exe) in exes {
            let own = count.get(exe.as_str()) == Some(&1) && !HOSTS.contains(&exe.as_str());
            s.exe_of.insert(target.clone(), own.then(|| exe.clone()));
        }
    });
}

/// The exe file name (lower case) behind a launch target: an .exe itself,
/// what a shortcut points at, or a Start menu entry's exe. `None` for Store
/// apps, URIs and the like.
pub fn exe_of(target: &str) -> Option<String> {
    let lower = target.to_lowercase();
    // Start menu entries first: some are an exe's path, but a shared one names none of them.
    if let Some(hit) = STATE.with(|s| s.borrow().exe_of.get(&lower).cloned()) {
        return hit;
    }
    if lower.ends_with(".exe") {
        return Some(apps::exe_name(target));
    }
    if !lower.ends_with(".lnk") {
        return None;
    }
    let exe = apps::resolve_lnk(target).filter(|t| t.to_lowercase().ends_with(".exe")).map(|t| apps::exe_name(&t));
    STATE.with(|s| s.borrow_mut().exe_of.insert(lower, exe.clone()));
    exe
}

/// The `[app_names]` / `[app_icons]` key for a target: its exe file name, or
/// the target itself when there's no exe to go by.
pub fn key_of(target: &str) -> String {
    exe_of(target).unwrap_or_else(|| target.to_string())
}

/// The pinned Dock app a target is: the same path, or the same exe.
pub fn pinned_path(target: &str, cfg: &Config) -> Option<String> {
    let lower = target.to_lowercase();
    if let Some(a) = cfg.dock.apps.iter().find(|a| a.path.to_lowercase() == lower) {
        return Some(a.path.clone());
    }
    let exe = exe_of(target)?;
    cfg.dock.apps.iter().find(|a| a.match_exe.as_ref().map(|m| m.to_lowercase()).or_else(|| exe_of(&a.path)).as_deref() == Some(&exe)).map(|a| a.path.clone())
}

/// What the user calls an app, if they renamed it.
pub fn name(target: &str) -> Option<String> {
    let lower = target.to_lowercase();
    let exe = exe_of(target);
    STATE.with(|s| {
        let s = s.borrow();
        // The app's own key first: apps sharing an exe (web apps, launchers) can differ.
        s.names
            .get(&lower)
            .or_else(|| exe.as_ref().and_then(|e| s.names.get(e)))
            .cloned()
            // Pinned with no exe key (a Store app, say): its Dock entry's name.
            .or_else(|| s.pinned.iter().find(|(p, ..)| *p == lower).and_then(|(_, n, ..)| n.clone()))
    })
}

/// The icon the user chose for an app, if any: a pinned app's own, then by exe, then by target.
pub fn icon(target: &str) -> Option<String> {
    let lower = target.to_lowercase();
    let exe = exe_of(target);
    let pinned_by_exe = |s: &State| -> Option<String> {
        let exe = exe.as_ref()?;
        s.pinned.iter().find_map(|(path, _, icon, m)| {
            let icon = icon.as_ref()?;
            let pexe = m.clone().or_else(|| path.ends_with(".exe").then(|| apps::exe_name(path)));
            (pexe.as_ref() == Some(exe)).then(|| icon.clone())
        })
    };
    let found = STATE.with(|s| {
        let s = s.borrow();
        s.pinned
            .iter()
            .find(|(p, ..)| *p == lower)
            .and_then(|(_, _, icon, _)| icon.clone())
            .or_else(|| s.icons.get(&lower).cloned())
            .or_else(|| pinned_by_exe(&s))
            .or_else(|| exe.as_ref().and_then(|e| s.icons.get(e).cloned()))
    });
    // Pinned shortcuts and Start menu entries compare by their exe too (resolved outside the borrow).
    found.or_else(|| {
        let exe = exe.as_ref()?;
        let lnks: Vec<(String, String)> = STATE.with(|s| {
            s.borrow()
                .pinned
                .iter()
                .filter(|(p, _, _, m)| m.is_none() && !p.ends_with(".exe"))
                .filter_map(|(p, _, i, _)| Some((p.clone(), i.clone()?)))
                .collect()
        });
        lnks.into_iter().find(|(p, _)| exe_of(p).as_ref() == Some(exe)).map(|(_, i)| i)
    })
}

/// A logo picked from Dashboard Icons (saved as `dash-*.png`): drawn on a
/// tile, where other custom icons are already tile-shaped.
pub fn is_library_logo(path: &str) -> bool {
    Path::new(path).file_name().is_some_and(|f| f.to_string_lossy().starts_with("dash-"))
}

#[cfg(test)]
mod tests {
    /// A custom icon loads as Launchpad loads it (a shell thumbnail), with its
    /// see-through corners kept: `cargo test thumb_keeps_alpha -- --ignored`.
    #[test]
    #[ignore]
    fn thumb_keeps_alpha() {
        let Ok(path) = std::env::var("DOCKSIDE_ICON") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let px = crate::gfx::icons::load_shell_image(&path, 128, true).expect("loads");
        let corner = px.data[3];
        let mid = px.data[(px.height / 2 * px.width + px.width / 2) as usize * 4 + 3];
        println!("{}x{} corner alpha {corner}, middle alpha {mid}", px.width, px.height);
        assert!(corner < 40 && mid > 200);
    }
}
