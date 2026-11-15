//! Export / Import Settings: the settings as a .zip another PC (or this one,
//! later) can load, without anything private in it.
//!
//! What goes in: `config.toml` (rebuilt from its values, so no comments come
//! along), the Launchpad layout, and a copy of every custom icon. What never
//! does: API keys, OAuth client ids and secrets, calendar links, sign-ins
//! (`calendars.dat`, `discord.dat`), the weather location, widget notes and
//! reminders (`widgets.json`), Photo and Video widgets' files, and the
//! Windows user name (paths under the user's folders become `%USERPROFILE%`
//! and friends).
//!
//! Importing keeps this PC's values for everything left out of an export,
//! so loading your own file doesn't sign you out or lose your keys.

use std::collections::{HashMap, HashSet};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use toml::Value;

/// Settings that never leave this PC, as `table.key`.
const PRIVATE: &[&str] = &[
    "dock.macosicons_key",
    "island.calendar_ics",
    "island.discord_client_id",
    "island.discord_client_secret",
    "calendar.google_client_id",
    "calendar.google_client_secret",
    "calendar.microsoft_client_id",
    "widgets.location",
    // Where Finder last was, and whether this PC has been set up: this PC's own.
    "finder.path",
    "general.onboarding_done",
];

/// Tables whose keys are names (exe files, Control Center modules), not settings.
const NAME_TABLES: &[&str] = &["app_names", "app_icons", "control_center"];

/// Image types a bundled icon can be.
const ICON_EXTS: &[&str] = &["png", "ico", "jpg", "jpeg", "bmp", "gif", "webp"];

/// Biggest icon copied, and biggest file read back in, in bytes.
const MAX_ICON: u64 = 16 << 20;
const MAX_BUNDLE: u64 = 128 << 20;
const MAX_ENTRY: usize = 32 << 20;

/// Where imported icons go (inside the settings folder).
const IMPORTED: &str = "icons\\imported";

/// A setting that looks like a secret, whatever it's called in later versions.
fn looks_private(key: &str) -> bool {
    let k = key.to_lowercase();
    k == "key" || k.ends_with("_key") || k.contains("secret") || k.contains("token") || k.contains("password")
        || k.ends_with("client_id") || k.ends_with("_ics")
}

pub fn default_file_name() -> String {
    format!("DockSide Settings {}.zip", chrono::Local::now().format("%Y-%m-%d"))
}

// --- Paths ---------------------------------------------------------------

/// Folders that hold the user's name, longest first, with what stands in for them.
fn user_folders(settings_dir: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::new();
    let real_settings = crate::config::dir();
    if settings_dir == real_settings {
        v.push((settings_dir.display().to_string(), "%APPDATA%\\DockSide".into()));
    }
    for var in ["LOCALAPPDATA", "APPDATA", "USERPROFILE"] {
        if let Ok(p) = std::env::var(var) {
            if !p.is_empty() {
                v.push((p.trim_end_matches('\\').to_string(), format!("%{var}%")));
            }
        }
    }
    v.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
    v
}

/// `s` with every user folder in it (any case) swapped for its variable.
fn generalize(s: &str, folders: &[(String, String)]) -> String {
    let mut out = s.to_string();
    for (folder, var) in folders {
        let lower_folder = folder.to_lowercase();
        let mut i = 0;
        while let Some(at) = out.to_lowercase()[i..].find(&lower_folder).map(|a| a + i) {
            out.replace_range(at..at + folder.len(), var);
            i = at + var.len();
        }
    }
    out
}

/// Expands the variables `generalize` writes, at the start of a value.
fn localize(s: &str) -> String {
    for var in ["USERPROFILE", "LOCALAPPDATA", "APPDATA"] {
        let token = format!("%{var}%");
        if s.len() >= token.len() && s[..token.len()].eq_ignore_ascii_case(&token) {
            if let Ok(v) = std::env::var(var) {
                return format!("{v}{}", &s[token.len()..]);
            }
        }
    }
    s.to_string()
}

fn expand(s: &str) -> String {
    // Values may use %VARS% themselves.
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find('%') {
        let Some(b) = rest[a + 1..].find('%') else { break };
        out.push_str(&rest[..a]);
        let name = &rest[a + 1..a + 1 + b];
        match std::env::var(name).ok().filter(|_| !name.is_empty()) {
            Some(v) => out.push_str(&v),
            None => out.push_str(&rest[a..a + b + 2]),
        }
        rest = &rest[a + b + 2..];
    }
    out.push_str(rest);
    out
}

fn map_strings(v: &mut Value, f: &dyn Fn(&str) -> String) {
    match v {
        Value::String(s) => *s = f(s),
        Value::Array(a) => a.iter_mut().for_each(|x| map_strings(x, f)),
        // Keys too: `[app_icons]` / `[app_names]` can be keyed by an app's path.
        Value::Table(t) => {
            *t = std::mem::take(t)
                .into_iter()
                .map(|(k, mut x)| {
                    map_strings(&mut x, f);
                    (f(&k), x)
                })
                .collect();
        }
        _ => {}
    }
}

fn map_json_strings(v: &mut serde_json::Value, f: &dyn Fn(&str) -> String) {
    match v {
        serde_json::Value::String(s) => *s = f(s),
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| map_json_strings(x, f)),
        // Keys too: some maps are keyed by app path.
        serde_json::Value::Object(o) => {
            *o = std::mem::take(o)
                .into_iter()
                .map(|(k, mut x)| {
                    map_json_strings(&mut x, f);
                    (f(&k), x)
                })
                .collect();
        }
        _ => {}
    }
}

/// Launchpad's launch counts (its Suggestions): how this PC is used, so they stay on it.
const LAUNCH_HISTORY: &str = "launches";

// --- What's private --------------------------------------------------------

/// Takes the private settings out of `cfg`; returns them, by `table.key`.
fn strip(cfg: &mut Value) -> HashMap<String, Value> {
    let mut taken = HashMap::new();
    let Some(root) = cfg.as_table_mut() else { return taken };
    // Top-level keys that look secret (none today).
    let top: Vec<String> = root.keys().filter(|k| !root[*k].is_table() && looks_private(k)).cloned().collect();
    for k in top {
        if let Some(v) = root.remove(&k) {
            taken.insert(k, v);
        }
    }
    for (table_name, table) in root.iter_mut() {
        if NAME_TABLES.contains(&table_name.as_str()) {
            continue;
        }
        let Some(t) = table.as_table_mut() else { continue };
        let keys: Vec<String> = t
            .keys()
            .filter(|k| PRIVATE.contains(&format!("{table_name}.{k}").as_str()) || looks_private(k))
            .cloned()
            .collect();
        for k in keys {
            if let Some(v) = t.remove(&k) {
                taken.insert(format!("{table_name}.{k}"), v);
            }
        }
    }
    // Photo and Video widgets point at the user's own pictures and videos.
    if let Some(items) = root.get_mut("widgets").and_then(|w| w.get_mut("items")).and_then(|i| i.as_array_mut()) {
        for item in items.iter_mut().filter_map(|i| i.as_table_mut()) {
            if let Some(p) = item.remove("path") {
                let id = item.get("id").and_then(|i| i.as_integer()).unwrap_or(0);
                let kind = item.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                taken.insert(format!("widgets.items#{id}#{kind}.path"), p);
            }
        }
    }
    taken
}

/// Puts this PC's private settings back into an imported config.
fn restore(cfg: &mut Value, mine: HashMap<String, Value>) {
    let Some(root) = cfg.as_table_mut() else { return };
    for (path, value) in mine {
        if let Some(rest) = path.strip_prefix("widgets.items#") {
            let Some((id, kind)) = rest.strip_suffix(".path").and_then(|r| r.split_once('#')) else { continue };
            let Ok(id) = id.parse::<i64>() else { continue };
            // The same widget: same id and kind.
            let items = root.get_mut("widgets").and_then(|w| w.get_mut("items")).and_then(|i| i.as_array_mut());
            if let Some(item) = items.and_then(|items| {
                items.iter_mut().filter_map(|i| i.as_table_mut()).find(|i| {
                    i.get("id").and_then(|v| v.as_integer()) == Some(id) && i.get("kind").and_then(|k| k.as_str()) == Some(kind)
                })
            }) {
                item.insert("path".into(), value);
            }
            continue;
        }
        match path.split_once('.') {
            Some((table, key)) => {
                let t = root.entry(table.to_string()).or_insert_with(|| Value::Table(Default::default()));
                if let Some(t) = t.as_table_mut() {
                    t.insert(key.to_string(), value);
                }
            }
            None => {
                root.insert(path, value);
            }
        }
    }
}

// --- Export --------------------------------------------------------------

/// Icon settings: (`table.key` or `dock.apps#i`, value).
fn icon_refs(cfg: &mut Value) -> Vec<&mut String> {
    let mut out: Vec<&mut String> = Vec::new();
    let Some(root) = cfg.as_table_mut() else { return out };
    for (name, table) in root.iter_mut() {
        let Some(t) = table.as_table_mut() else { continue };
        match name.as_str() {
            "dock" => {
                for (k, v) in t.iter_mut() {
                    match (k.as_str(), v) {
                        ("trash_icon" | "trash_icon_full", Value::String(s)) => out.push(s),
                        ("apps", Value::Array(apps)) => {
                            for app in apps.iter_mut().filter_map(|a| a.as_table_mut()) {
                                if let Some(Value::String(s)) = app.get_mut("icon") {
                                    out.push(s);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "app_icons" => {
                for (_, v) in t.iter_mut() {
                    if let Value::String(s) = v {
                        out.push(s);
                    }
                }
            }
            "menubar" => {
                if let Some(Value::String(s)) = t.get_mut("logo") {
                    out.push(s);
                }
            }
            _ => {}
        }
    }
    out
}

/// Writes the settings in `dir` (DockSide's settings folder) to a .zip at
/// `out`; returns a sentence saying what's in it.
pub fn export(dir: &Path, out: &Path) -> Result<String> {
    let text = std::fs::read_to_string(dir.join("config.toml")).context("reading config.toml")?;
    let mut cfg: Value = toml::from_str(&text).context("config.toml is not valid TOML")?;
    strip(&mut cfg);

    // Custom icons travel in the bundle; their settings point where import puts them.
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut by_source: HashMap<String, String> = HashMap::new();
    let mut names: HashSet<String> = HashSet::new();
    for value in icon_refs(&mut cfg) {
        let source = expand(value);
        let p = Path::new(&source);
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !p.is_absolute() || !ICON_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let name = match by_source.get(&source.to_lowercase()) {
            Some(n) => n.clone(),
            None => {
                let Ok(meta) = std::fs::metadata(p) else { continue };
                if !meta.is_file() || meta.len() > MAX_ICON {
                    continue;
                }
                let Ok(bytes) = std::fs::read(p) else { continue };
                let name = unique_name(p, &mut names);
                files.push((format!("icons/{name}"), bytes));
                by_source.insert(source.to_lowercase(), name.clone());
                name
            }
        };
        *value = format!("%APPDATA%\\DockSide\\{IMPORTED}\\{name}");
    }

    let folders = user_folders(dir);
    map_strings(&mut cfg, &|s| generalize(s, &folders));
    let body = toml::to_string_pretty(&cfg).context("writing the settings")?;
    let config = format!(
        "# DockSide settings, exported {}.\n# Import them in System Settings > General > Configuration.\n\n{body}",
        chrono::Local::now().format("%Y-%m-%d")
    );

    let mut entries = vec![
        (
            "manifest.json".to_string(),
            serde_json::to_vec_pretty(&serde_json::json!({ "app": "DockSide", "format": 1, "version": env!("CARGO_PKG_VERSION") }))?,
        ),
        ("config.toml".to_string(), config.into_bytes()),
    ];
    if let Ok(t) = std::fs::read_to_string(dir.join("launchpad.json")) {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&t) {
            if let Some(o) = json.as_object_mut() {
                o.remove(LAUNCH_HISTORY);
            }
            map_json_strings(&mut json, &|s| generalize(s, &folders));
            entries.push(("launchpad.json".to_string(), serde_json::to_vec_pretty(&json)?));
        }
    }
    let icons = files.len();
    entries.extend(files);
    std::fs::write(out, zip::write(&entries)?).with_context(|| format!("writing {}", out.display()))?;

    let icons = match icons {
        0 => String::new(),
        1 => " and 1 custom icon".into(),
        n => format!(" and {n} custom icons"),
    };
    Ok(format!(
        "Saved your settings, Launchpad layout{icons} to {}.\n\nLeft out: API keys, calendar and Discord sign-ins and \
         links, your weather location, widget notes and reminders, Photo and Video widgets' files, and your Windows \
         user name.",
        out.display()
    ))
}

fn unique_name(p: &Path, taken: &mut HashSet<String>) -> String {
    let stem: String = p
        .file_stem()
        .map(|s| s.to_string_lossy().chars().map(|c| if c.is_alphanumeric() || "-_ ().".contains(c) { c } else { '_' }).collect())
        .unwrap_or_else(|| "icon".into());
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut name = format!("{stem}.{ext}");
    let mut n = 2;
    while !taken.insert(name.to_lowercase()) {
        name = format!("{stem}-{n}.{ext}");
        n += 1;
    }
    name
}

// --- Import --------------------------------------------------------------

/// A bundled icon's own file name, if `entry` is one we'd take in.
fn icon_entry(entry: &str) -> Option<&str> {
    let name = entry.strip_prefix("icons/")?;
    let bad = name.is_empty() || name.contains(['/', '\\', ':']) || name.starts_with('.') || name.contains("..");
    let ext = Path::new(name).extension()?.to_string_lossy().to_lowercase();
    (!bad && ICON_EXTS.contains(&ext.as_str())).then_some(name)
}

/// Loads the settings in the .zip at `file` into `dir` (DockSide's settings
/// folder). The current config.toml is kept as `config.backup-<date and time>.toml`.
pub fn import(dir: &Path, file: &Path) -> Result<()> {
    let size = std::fs::metadata(file).with_context(|| format!("reading {}", file.display()))?.len();
    if size > MAX_BUNDLE {
        bail!("{} is too big to be a DockSide settings file", file.display());
    }
    let bytes = std::fs::read(file)?;
    let entries = zip::read(&bytes).context("this isn't a DockSide settings file (it isn't a .zip)")?;
    let get = |name: &str| entries.iter().find(|(n, _)| n == name).map(|(_, b)| b.as_slice());
    let Some(config) = get("config.toml") else { bail!("this isn't a DockSide settings file (it has no config.toml)") };
    let text = std::str::from_utf8(config).context("config.toml isn't text")?;
    let mut cfg: Value = toml::from_str(text).context("config.toml in the file is not valid TOML")?;
    // Nothing private comes in, even if someone put it there: this PC's own stays.
    strip(&mut cfg);
    map_strings(&mut cfg, &localize);
    let cfg_path = dir.join("config.toml");
    let current = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    if let Ok(mut mine) = toml::from_str::<Value>(&current) {
        restore(&mut cfg, strip(&mut mine));
    }
    let out = toml::to_string_pretty(&cfg).context("writing the settings")?;
    crate::config::check(&out).context("the settings in this file can't be used")?;

    // Icons first, then Launchpad, then config.toml last: the file watcher reloads once.
    let icon_dir = dir.join(IMPORTED);
    for (name, data) in &entries {
        if let Some(icon) = icon_entry(name) {
            std::fs::create_dir_all(&icon_dir)?;
            std::fs::write(icon_dir.join(icon), data)?;
        }
    }
    if let Some(lp) = get("launchpad.json") {
        let mut json: serde_json::Value = serde_json::from_slice(lp).context("launchpad.json in the file is damaged")?;
        map_json_strings(&mut json, &localize);
        // This PC's launch history stays (and none comes in).
        let mine = std::fs::read_to_string(dir.join("launchpad.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|mut m| m.as_object_mut().and_then(|o| o.remove(LAUNCH_HISTORY)));
        if let Some(o) = json.as_object_mut() {
            o.remove(LAUNCH_HISTORY);
            if let Some(h) = mine {
                o.insert(LAUNCH_HISTORY.into(), h);
            }
        }
        std::fs::write(dir.join("launchpad.json"), serde_json::to_vec_pretty(&json)?)?;
    }
    if !current.is_empty() {
        let backup = format!("config.backup-{}.toml", chrono::Local::now().format("%Y-%m-%d-%H%M%S"));
        std::fs::write(dir.join(backup), &current).context("backing up config.toml")?;
    }
    let header = format!("# Imported from {} on {}.\n\n", file.file_name().map(|f| f.to_string_lossy()).unwrap_or_default(), chrono::Local::now().format("%Y-%m-%d"));
    std::fs::write(&cfg_path, format!("{header}{out}"))?;
    Ok(())
}

/// The few bits of .zip this needs: write stored-or-deflated entries, read
/// them back (from our own exports, or one re-zipped by Explorer).
mod zip {
    use anyhow::{bail, Result};

    use super::MAX_ENTRY;
    use crate::island::encode::crc32_update;

    fn crc(data: &[u8]) -> u32 {
        !crc32_update(!0, data)
    }

    pub fn write(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let packed = miniz_oxide::deflate::compress_to_vec(data, 6);
            let (method, body): (u16, &[u8]) = if packed.len() < data.len() { (8, &packed) } else { (0, data) };
            let offset = u32::try_from(out.len())?;
            let (c, size, psize, nlen) = (crc(data), u32::try_from(data.len())?, u32::try_from(body.len())?, name.len() as u16);
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0, 0, 0x21, 0]); // time 00:00, date 1980-01-01
            for v in [c, psize, size] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&nlen.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(body);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0]);
            central.extend_from_slice(&0x0800u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0, 0, 0x21, 0]);
            for v in [c, psize, size] {
                central.extend_from_slice(&v.to_le_bytes());
            }
            central.extend_from_slice(&nlen.to_le_bytes());
            central.extend_from_slice(&[0; 12]); // extra, comment, disk, attributes
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let count = u16::try_from(entries.len())?;
        let dir_at = u32::try_from(out.len())?;
        let dir_len = u32::try_from(central.len())?;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&dir_len.to_le_bytes());
        out.extend_from_slice(&dir_at.to_le_bytes());
        out.extend_from_slice(&[0; 2]);
        Ok(out)
    }

    fn u16_at(b: &[u8], i: usize) -> Result<u16> {
        Ok(u16::from_le_bytes(b.get(i..i + 2).ok_or_else(|| anyhow::anyhow!("cut short"))?.try_into()?))
    }

    fn u32_at(b: &[u8], i: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(b.get(i..i + 4).ok_or_else(|| anyhow::anyhow!("cut short"))?.try_into()?))
    }

    /// Every file entry: (name, contents). Folders are skipped.
    pub fn read(b: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
        // The end record is in the last 22 + 65,535 bytes.
        let from = b.len().saturating_sub(22 + 0xFFFF);
        let Some(end) = (from..b.len().saturating_sub(21)).rev().find(|&i| u32_at(b, i).ok() == Some(0x0605_4b50)) else {
            bail!("no zip directory");
        };
        let count = u16_at(b, end + 10)? as usize;
        let mut at = u32_at(b, end + 16)? as usize;
        let mut out = Vec::new();
        for _ in 0..count.min(4096) {
            if u32_at(b, at)? != 0x0201_4b50 {
                bail!("damaged zip directory");
            }
            let method = u16_at(b, at + 10)?;
            let want_crc = u32_at(b, at + 16)?;
            let psize = u32_at(b, at + 20)? as usize;
            let size = u32_at(b, at + 24)? as usize;
            let (nlen, xlen, clen) = (u16_at(b, at + 28)? as usize, u16_at(b, at + 30)? as usize, u16_at(b, at + 32)? as usize);
            let local = u32_at(b, at + 42)? as usize;
            let name = String::from_utf8_lossy(b.get(at + 46..at + 46 + nlen).unwrap_or_default()).replace('\\', "/");
            at += 46 + nlen + xlen + clen;
            if name.ends_with('/') || size > MAX_ENTRY {
                continue;
            }
            if u32_at(b, local)? != 0x0403_4b50 {
                bail!("damaged zip entry");
            }
            let start = local + 30 + u16_at(b, local + 26)? as usize + u16_at(b, local + 28)? as usize;
            let Some(raw) = b.get(start..start + psize) else { bail!("damaged zip entry") };
            let data = match method {
                0 => raw.to_vec(),
                8 => miniz_oxide::inflate::decompress_to_vec_with_limit(raw, MAX_ENTRY).map_err(|_| anyhow::anyhow!("damaged zip entry"))?,
                _ => continue,
            };
            if crc(&data) != want_crc {
                bail!("{name} in the zip is damaged");
            }
            out.push((name, data));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dockside-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Every secret, the location, notes and the user name are planted, then
    /// looked for in every file of the export: none may turn up.
    #[test]
    fn export_leaks_nothing() {
        let dir = temp("leak");
        let home = std::env::var("USERPROFILE").unwrap();
        let user = Path::new(&home).file_name().unwrap().to_string_lossy().into_owned();
        let icon = dir.join("my icon.png");
        std::fs::write(&icon, b"\x89PNG not really").unwrap();
        let cfg = format!(
            r#"# a comment with SECRET-COMMENT in it
[general]
onboarding_done = true
[dock]
macosicons_key = "SECRET-MACOS"
trash_icon = '{icon}'
[[dock.apps]]
name = "Tool"
path = '{home}\AppData\Local\Tool\tool.exe'
icon = '{icon}'
[island]
calendar_ics = "https://cal.example/SECRET-ICS.ics"
discord_client_id = "SECRET-DID"
discord_client_secret = "SECRET-DSEC"
[calendar]
google_client_id = "SECRET-GID"
google_client_secret = "SECRET-GSEC"
microsoft_client_id = "SECRET-MID"
future_api_token = "SECRET-FUTURE"
[widgets]
location = "SECRET-PLACE"
[[widgets.items]]
id = 3
kind = "photo"
size = "small"
x = 0
y = 0
path = '{home}\Pictures\SECRET-PHOTO.jpg'
[finder]
path = '{home}\SECRET-FOLDER'
[app_icons]
"code.exe" = '{icon}'
'shell:AppsFolder\{home}\Apps\tool.exe' = '{icon}'
[app_names]
'{home}\Desktop\thing.lnk' = "Thing"
"#,
            icon = icon.display()
        );
        std::fs::write(dir.join("config.toml"), cfg).unwrap();
        let lnk = serde_json::json!(format!("{home}\\Desktop\\x.lnk"));
        let launched = serde_json::json!(format!("shell:AppsFolder\\{home}\\SECRET-LAUNCHED.exe"));
        std::fs::write(
            dir.join("launchpad.json"),
            format!(r#"{{"pages":[[{{"kind":"app","target":{lnk}}}]],"launches":{{{launched}:{{"count":2,"last":1}}}},"hidden":[{{"name":"y","target":{lnk}}}]}}"#),
        )
        .unwrap();
        std::fs::write(dir.join("widgets.json"), r#"{"notes":{"1":"SECRET-NOTE"}}"#).unwrap();
        std::fs::write(dir.join("calendars.dat"), "SECRET-CALDAT").unwrap();

        let out = dir.join("out.zip");
        export(&dir, &out).unwrap();
        let entries = zip::read(&std::fs::read(&out).unwrap()).unwrap();
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"config.toml") && names.contains(&"launchpad.json"), "{names:?}");
        assert_eq!(names.iter().filter(|n| n.starts_with("icons/")).count(), 1, "one copy of the shared icon: {names:?}");
        for (name, data) in &entries {
            let text = String::from_utf8_lossy(data).to_lowercase();
            assert!(!text.contains("secret"), "{name} leaks: {text}");
            assert!(!text.contains(&format!("\\{}\\", user.to_lowercase())), "{name} has the user name: {text}");
        }
        let config = String::from_utf8_lossy(&entries.iter().find(|(n, _)| n == "config.toml").unwrap().1).into_owned();
        assert!(config.contains("%LOCALAPPDATA%\\\\Tool\\\\tool.exe") || config.contains("%LOCALAPPDATA%\\Tool\\tool.exe"), "{config}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Importing keeps this PC's private settings and puts icons and paths back in place.
    #[test]
    fn import_round_trip_keeps_private() {
        let dir = temp("import");
        let home = std::env::var("USERPROFILE").unwrap();
        let icon = dir.join("a.png");
        std::fs::write(&icon, b"png").unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[dock]\nmacosicons_key = \"MINE\"\ntile_size = 60.0\n[[dock.apps]]\nname = \"Tool\"\npath = '{home}\\x.exe'\nicon = '{}'\n[widgets]\nlocation = \"Here\"\n",
                icon.display()
            ),
        )
        .unwrap();
        let out = dir.join("out.zip");
        export(&dir, &out).unwrap();
        // This PC then changes, and the bundle is brought back.
        std::fs::write(dir.join("config.toml"), "[dock]\nmacosicons_key = \"MINE2\"\ntile_size = 40.0\n[widgets]\nlocation = \"There\"\n").unwrap();
        import(&dir, &out).unwrap();
        let cfg = crate::config::load(&dir.join("config.toml")).unwrap();
        assert_eq!(cfg.dock.macosicons_key, "MINE2");
        assert_eq!(cfg.widgets.location, "There");
        assert_eq!(cfg.dock.tile_size, 60.0);
        let app = cfg.dock.apps.iter().find(|a| a.name.as_deref() == Some("Tool")).unwrap();
        assert!(app.path.eq_ignore_ascii_case(&format!("{home}\\x.exe")), "{}", app.path);
        let icon = app.icon.as_deref().unwrap();
        assert!(icon.contains("imported") && Path::new(icon).file_name().unwrap() == "a.png", "{icon}");
        assert!(std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("config.backup-")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsafe_entry_names_are_refused() {
        assert_eq!(icon_entry("icons/a.png"), Some("a.png"));
        for bad in ["icons/../x.png", "icons/a/b.png", "icons/x.exe", "config.toml", "icons/.png", "icons/c:x.png"] {
            assert_eq!(icon_entry(bad), None, "{bad}");
        }
    }

    #[test]
    fn generalize_any_case() {
        let f = vec![("C:\\Users\\Bob".to_string(), "%USERPROFILE%".to_string())];
        assert_eq!(generalize("c:\\users\\bob\\x.exe", &f), "%USERPROFILE%\\x.exe");
        assert_eq!(generalize("\"C:\\Users\\Bob\\a\" C:\\Users\\Bob\\b", &f), "\"%USERPROFILE%\\a\" %USERPROFILE%\\b");
    }
}

#[cfg(test)]
mod live {
    /// Exports the real settings to `DOCKSIDE_EXPORT_OUT`: `cargo test live_export -- --ignored`.
    #[test]
    #[ignore]
    fn live_export() {
        let Ok(out) = std::env::var("DOCKSIDE_EXPORT_OUT") else { return };
        println!("{}", super::export(&crate::config::dir(), std::path::Path::new(&out)).unwrap());
    }
}
