//! Apps that install each update into a new versioned folder: Squirrel apps
//! (Discord, GitHub Desktop, … under `app-1.2.3`), MSIX packages under
//! `WindowsApps\Name_1.2.3.0_x64__publisher`, and the like. A pin saved with
//! one version's exe path is pointed at the version installed now, so updating
//! the app doesn't drop it (and its custom icon) from the Dock.

use std::path::{Component, Path, PathBuf};

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows::Win32::Storage::Packaging::Appx::{GetPackagePathByFullName, GetPackagesByPackageFamily};

/// The current install of `path` when it lives in a versioned folder and a
/// different (newer, or the only remaining) version is installed.
pub fn current(path: &str) -> Option<String> {
    let p = Path::new(path);
    if !p.is_absolute() || path.contains("::") {
        return None;
    }
    let comps: Vec<Component> = p.components().collect();
    // Folders only: the exe's own name stays as it is.
    let dirs = 1..comps.len().saturating_sub(1);

    // Squirrel keeps the previous version's folder around for a while after an
    // update, so its newest `app-*` folder wins even while the pinned one exists.
    for i in dirs.clone() {
        let name = comps[i].as_os_str().to_string_lossy();
        if !name.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("app-")) {
            continue;
        }
        let (parent, rest) = split_at(&comps, i);
        if let Some(newest) = newest_sibling(&parent, &name, &rest).filter(|n| !n.eq_ignore_ascii_case(&name)) {
            return Some(parent.join(newest).join(rest).to_string_lossy().into_owned());
        }
    }
    if p.exists() {
        return None;
    }
    // The first missing folder that names a version: an MSIX package, or any
    // other app that moved to a sibling folder with a different version.
    for i in dirs {
        let (parent, rest) = split_at(&comps, i);
        let dir = parent.join(comps[i]);
        if dir.exists() {
            continue;
        }
        let name = comps[i].as_os_str().to_string_lossy();
        let found = if parent.file_name().is_some_and(|f| f.eq_ignore_ascii_case("WindowsApps")) {
            // WindowsApps can't be listed without admin rights: ask the package manager.
            msix(&name, &rest)
        } else {
            newest_sibling(&parent, &name, &rest).map(|n| parent.join(n).join(&rest))
        };
        return found.map(|f| f.to_string_lossy().into_owned());
    }
    None
}

/// The folder holding component `i`, and the path below it.
fn split_at(comps: &[Component], i: usize) -> (PathBuf, PathBuf) {
    (comps[..i].iter().collect(), comps[i + 1..].iter().collect())
}

/// The highest-versioned folder next to `name` that differs from it only in
/// its version and still holds `rest`.
fn newest_sibling(parent: &Path, name: &str, rest: &Path) -> Option<String> {
    let (pre, _, suf) = split_version(name)?;
    std::fs::read_dir(parent)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            let (p, v, s) = split_version(&n)?;
            (p.eq_ignore_ascii_case(pre) && s.eq_ignore_ascii_case(suf)).then_some((v, n))
        })
        .filter(|(_, n)| parent.join(n).join(rest).is_file())
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, n)| n)
}

/// Splits a name around its first dotted version: `app-1.0.92` → (`app-`, [1, 0, 92], ``).
fn split_version(name: &str) -> Option<(&str, Vec<u64>, &str)> {
    let b = name.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut parts = Vec::new();
        loop {
            let s = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            parts.push(name[s..j].parse().unwrap_or(u64::MAX));
            if j + 1 < b.len() && b[j] == b'.' && b[j + 1].is_ascii_digit() {
                j += 1;
            } else {
                break;
            }
        }
        if parts.len() >= 2 {
            return Some((&name[..i], parts, &name[j..]));
        }
        i = j;
    }
    None
}

/// `rest` inside the newest installed package of the same family as the
/// package folder `full_name` (`Name_Version_Arch_ResourceId_PublisherId`).
fn msix(full_name: &str, rest: &Path) -> Option<PathBuf> {
    let parts: Vec<&str> = full_name.split('_').collect();
    if parts.len() != 5 {
        return None;
    }
    let family = format!("{}_{}", parts[0], parts[4]);
    package_full_names(&family)
        .into_iter()
        .filter_map(|full| {
            let version = full.split('_').nth(1).and_then(split_version).map(|(_, v, _)| v)?;
            let exe = PathBuf::from(package_path(&full)?).join(rest);
            exe.is_file().then_some((version, exe))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, exe)| exe)
}

/// Full names of the current user's installed packages in a family.
fn package_full_names(family: &str) -> Vec<String> {
    let fam = HSTRING::from(family);
    let (mut count, mut len) = (0u32, 0u32);
    unsafe {
        if GetPackagesByPackageFamily(&fam, &mut count, None, &mut len, PWSTR::null()) != ERROR_INSUFFICIENT_BUFFER
            || count == 0
        {
            return Vec::new();
        }
        let mut names = vec![PWSTR::null(); count as usize];
        let mut buf = vec![0u16; len as usize];
        if GetPackagesByPackageFamily(&fam, &mut count, Some(names.as_mut_ptr()), &mut len, PWSTR(buf.as_mut_ptr()))
            != ERROR_SUCCESS
        {
            return Vec::new();
        }
        names.iter().take(count as usize).filter_map(|n| n.to_string().ok()).collect()
    }
}

fn package_path(full_name: &str) -> Option<String> {
    let name = HSTRING::from(full_name);
    let mut len = 0u32;
    unsafe {
        let _ = GetPackagePathByFullName(&name, &mut len, PWSTR::null());
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        (GetPackagePathByFullName(&name, &mut len, PWSTR(buf.as_mut_ptr())) == ERROR_SUCCESS)
            .then(|| super::from_wide(&buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_versions() {
        assert_eq!(split_version("app-1.0.9267"), Some(("app-", vec![1, 0, 9267], "")));
        assert_eq!(
            split_version("Claude_4.23118.0.0_x64__pzs8sxrjxfjjc"),
            Some(("Claude_", vec![4, 23118, 0, 0], "_x64__pzs8sxrjxfjjc"))
        );
        assert_eq!(split_version("Discord"), None);
        assert_eq!(split_version("x64 build 7"), None);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dockside-versioned-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn squirrel_pin_follows_the_newest_app_folder() {
        let root = scratch("squirrel");
        touch(&root.join("app-3.6.7").join("GitHubDesktop.exe"));
        touch(&root.join("app-3.6.10").join("GitHubDesktop.exe"));
        // A newer folder without the exe (an update still unpacking) doesn't count.
        std::fs::create_dir_all(root.join("app-3.7.0")).unwrap();
        let old = root.join("app-3.6.7").join("GitHubDesktop.exe");
        let want = root.join("app-3.6.10").join("GitHubDesktop.exe");
        assert_eq!(current(&old.to_string_lossy()), Some(want.to_string_lossy().into_owned()));
        assert_eq!(current(&want.to_string_lossy()), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_versioned_folder_moves_to_the_installed_one() {
        let root = scratch("generic");
        touch(&root.join("Tool 2.4").join("bin").join("tool.exe"));
        let old = root.join("Tool 2.1").join("bin").join("tool.exe");
        let want = root.join("Tool 2.4").join("bin").join("tool.exe");
        assert_eq!(current(&old.to_string_lossy()), Some(want.to_string_lossy().into_owned()));
        // An existing non-Squirrel pin is left alone even when a newer version sits beside it.
        touch(&root.join("Tool 2.1").join("bin").join("tool.exe"));
        assert_eq!(current(&old.to_string_lossy()), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
