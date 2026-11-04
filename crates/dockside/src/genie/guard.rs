//! Which processes the hook DLL may go into. Decided here, in DockSide, before
//! Windows is asked to load the DLL anywhere: a DLL that refuses to stay (DllMain
//! returning FALSE) has still been mapped, and a kernel anti-cheat (BattlEye,
//! EasyAntiCheat, Vanguard, ...) sees that image load and may flag the game.
//!
//! The name / path rules are pure (and unit tested); `probe` adds what needs the
//! system: the process's bitness, protection, integrity, folder and launcher.
//! Nothing here opens a process for more than PROCESS_QUERY_LIMITED_INFORMATION
//! (what Task Manager uses): asking an anti-cheat-protected game for memory access
//! would itself look like a cheat.

use std::path::Path;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_UNKNOWN;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessInformation, IsWow64Process2, OpenProcessToken, ProcessProtectionLevelInfo, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROTECTION_LEVEL_NONE,
    PROCESS_PROTECTION_LEVEL_INFORMATION,
};

/// Anti-cheat services, launchers and clients, and games whose anti-cheat reacts
/// to foreign modules (file names, `*` and `?` wildcards).
const ANTI_CHEAT_EXES: &[&str] = &[
    // BattlEye: the service, its launcher, and the `<game>_BE.exe` launchers games ship.
    "beservice.exe",
    "beservice_x64.exe",
    "bedaemon*.exe",
    "belauncher*.exe",
    "beclient*.exe",
    "*_be.exe",
    // EasyAntiCheat (and its EOS flavour).
    "easyanticheat*.exe",
    "start_protected_game.exe",
    "eaclauncher*.exe",
    // EA Javelin.
    "eaanticheat*.exe",
    // Riot Vanguard.
    "vgc.exe",
    "vgtray.exe",
    "vgk*.exe",
    // FACEIT and ESEA.
    "faceit*.exe",
    "esea*.exe",
    // Call of Duty (Ricochet).
    "cod.exe",
    "codlauncher*.exe",
    // nProtect GameGuard.
    "gamemon*.des",
    "gamemon*.exe",
    "ggerror*.exe",
    "gameguard*.exe",
    "npggnt*.exe",
    // XIGNCODE3.
    "xigncode*.exe",
    "*.xem",
    // PunkBuster.
    "pnkbstr*.exe",
    // Games that kick or ban for foreign modules.
    "robloxplayerbeta.exe",
    "valorant*.exe",
    "fortniteclient*.exe",
    "r5apex*.exe",
    "cs2.exe",
    "overwatch.exe",
    "wow.exe",
];

/// Folders whose programs never get the DLL (lowercase, matched anywhere in the
/// path): anti-cheat installs, and the folders games are installed to.
const NO_INJECT_DIRS: &[&str] = &[
    "\\battleye\\",
    "\\easyanticheat\\",
    "\\easyanticheat_eos\\",
    "\\eaanticheat\\",
    "\\riot vanguard\\",
    "\\faceit\\",
    "\\faceit ac\\",
    "\\esea\\",
    "\\gameguard\\",
    "\\xigncode\\",
    "\\xigncode3\\",
    "\\call of duty\\",
    "\\steamapps\\common\\",
    "\\epic games\\",
    "\\riot games\\",
    "\\xboxgames\\",
    "\\ea games\\",
    "\\ubisoft game launcher\\games\\",
    "\\gog galaxy\\games\\",
];

/// Anti-cheat folders a game ships beside (or a few levels above) its exe, e.g.
/// `<game>\BattlEye\BEClient_x64.dll` with `<game>\Binaries\Win64\Game.exe`.
const ANTI_CHEAT_DIRS: &[&str] = &["BattlEye", "EasyAntiCheat", "EasyAntiCheat_EOS", "EAAntiCheat", "GameGuard", "XIGNCODE", "XIGNCODE3"];

/// How many folders up from the exe's own `ANTI_CHEAT_DIRS` are looked for.
const ANTI_CHEAT_DIR_LEVELS: usize = 4;

/// System and shell processes the DLL never goes into (the DLL refuses them too).
const SYSTEM_EXES: &[&str] = &[
    "csrss.exe",
    "winlogon.exe",
    "logonui.exe",
    "lsass.exe",
    "dwm.exe",
    "fontdrvhost.exe",
    "consent.exe",
    "lockapp.exe",
    "searchhost.exe",
    "startmenuexperiencehost.exe",
    "shellexperiencehost.exe",
    "textinputhost.exe",
    "werfault.exe",
    "werfaultsecure.exe",
];

/// How many parents up a process started by an anti-cheat launcher is recognised.
const LAUNCHER_LEVELS: usize = 4;

/// `pattern` (lowercase, `*` = any run, `?` = any one character) against `text`
/// (lowercase).
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// `C:\Games\Foo\Foo.exe` -> `c:\games\foo\foo.exe`, `/` as `\`.
fn normalize(path: &str) -> String {
    path.trim().replace('/', "\\").to_lowercase()
}

fn file_name(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

/// The built-in list plus the user's `[windows] no_inject`, compiled.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rules {
    /// Lowercase; ones with a `\` match the whole path, the rest the file name.
    user: Vec<String>,
}

impl Rules {
    pub fn new(user: &[String]) -> Rules {
        Rules { user: user.iter().map(|p| normalize(p)).filter(|p| !p.is_empty() && p.chars().any(|c| c != '*')).collect() }
    }

    /// Why the program at `path` must not get the DLL, from its name and path
    /// alone; None when they allow it.
    pub fn blocked_path(&self, path: &str) -> Option<String> {
        let path = normalize(path);
        let name = file_name(&path);
        let stem = name.strip_suffix(".exe").unwrap_or(name);
        if let Some(p) = SYSTEM_EXES.iter().find(|p| wildcard(p, name)) {
            return Some(format!("system process ({p})"));
        }
        if let Some(p) = ANTI_CHEAT_EXES.iter().find(|p| wildcard(p, name)) {
            return Some(format!("anti-cheat ({p})"));
        }
        if let Some(d) = NO_INJECT_DIRS.iter().find(|d| path.contains(*d)) {
            return Some(format!("protected folder ({})", d.trim_matches('\\')));
        }
        for p in &self.user {
            let hit = if p.contains('\\') {
                wildcard(p, &path) || (!p.contains('*') && path.starts_with(&format!("{}\\", p.trim_end_matches('\\'))))
            } else {
                wildcard(p, name) || wildcard(p, stem)
            };
            if hit {
                return Some(format!("blacklisted ({p})"));
            }
        }
        None
    }

    /// Whether a program called `name` (just the file name) launches games under
    /// an anti-cheat: what it starts is kept out too.
    pub fn is_anti_cheat_launcher(name: &str) -> bool {
        let name = name.to_lowercase();
        ANTI_CHEAT_EXES.iter().any(|p| wildcard(p, &name))
    }
}

/// The folders to look in for ANTI_CHEAT_DIRS: the exe's own and a few above it,
/// short of the drive root.
pub fn anti_cheat_dir_candidates(path: &str) -> Vec<&Path> {
    let mut out = Vec::new();
    let mut dir = Path::new(path).parent();
    while let Some(d) = dir {
        // `C:\` (or a share root) holds no game.
        if d.parent().is_none() || out.len() >= ANTI_CHEAT_DIR_LEVELS {
            break;
        }
        out.push(d);
        dir = d.parent();
    }
    out
}

/// Whether one of `dirs` holds an anti-cheat folder, per `is_dir`.
pub fn has_anti_cheat_dir(dirs: &[&Path], is_dir: impl Fn(&Path) -> bool) -> Option<String> {
    for d in dirs {
        for name in ANTI_CHEAT_DIRS {
            let p = d.join(name);
            if is_dir(&p) {
                return Some(p.display().to_string());
            }
        }
    }
    None
}

// --- the system side ---------------------------------------------------------

/// Integrity level RID of a process token (0x2000 medium, 0x3000 high, ...).
fn integrity(process: HANDLE) -> Option<u32> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut size = 0;
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut size);
        let mut buf = vec![0u8; size.max(1) as usize];
        let got = GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr() as _), size, &mut size);
        let _ = CloseHandle(token);
        got.ok()?;
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        (count > 0).then(|| *GetSidSubAuthority(label.Label.Sid, count as u32 - 1))
    }
}

/// Our own integrity level: the DLL only goes into programs at or below it.
pub fn own_integrity() -> u32 {
    integrity(unsafe { GetCurrentProcess() }).unwrap_or(0x2000)
}

pub fn image_path(process: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// (pid, parent pid, exe name) of every process.
type ProcessList = Vec<(u32, u32, String)>;

/// A process list a moment old is reused (a start-up scan vets dozens of processes),
/// unless `pid` is missing from it.
fn process_list(pid: u32) -> ProcessList {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, ProcessList)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, list)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(2) && list.iter().any(|p| p.0 == pid) {
            return list.clone();
        }
    }
    let list = snapshot_processes();
    *cache = Some((Instant::now(), list.clone()));
    list
}

fn snapshot_processes() -> ProcessList {
    let mut procs = ProcessList::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return procs };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            procs.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..n])));
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    procs
}

/// Exe names of the parent chain of `pid`, nearest first.
fn ancestors(pid: u32) -> Vec<String> {
    let procs = process_list(pid);
    let mut out = Vec::new();
    let mut cur = pid;
    for _ in 0..LAUNCHER_LEVELS {
        let Some(&(_, parent, _)) = procs.iter().find(|p| p.0 == cur) else { break };
        if parent == 0 || parent == cur {
            break;
        }
        let Some(p) = procs.iter().find(|p| p.0 == parent) else { break };
        out.push(p.2.clone());
        cur = parent;
    }
    out
}

/// Why process `process` (opened for PROCESS_QUERY_LIMITED_INFORMATION) must not
/// get the DLL; None when it may.
pub fn probe(process: HANDLE, pid: u32, rules: &Rules, own_integrity: u32) -> Option<String> {
    unsafe {
        // The DLL is 64-bit only.
        let mut machine = IMAGE_FILE_MACHINE_UNKNOWN;
        if IsWow64Process2(process, &mut machine, None).is_err() {
            return Some("bitness unknown".into());
        }
        if machine != IMAGE_FILE_MACHINE_UNKNOWN {
            return Some("32-bit".into());
        }
        // Protected processes (and their light flavour, e.g. anti-malware) are off limits.
        let mut prot = PROCESS_PROTECTION_LEVEL_INFORMATION::default();
        let got = GetProcessInformation(process, ProcessProtectionLevelInfo, &mut prot as *mut _ as _, std::mem::size_of::<PROCESS_PROTECTION_LEVEL_INFORMATION>() as u32);

        if got.is_err() {
            return Some("protection unknown".into());
        }
        if prot.ProtectionLevel != PROTECTION_LEVEL_NONE {
            return Some("protected process".into());
        }
    }
    match integrity(process) {
        Some(level) if level <= own_integrity => {}
        Some(_) => return Some("higher integrity than DockSide".into()),
        None => return Some("integrity unknown".into()),
    }
    let Some(path) = image_path(process) else { return Some("path unknown".into()) };
    if let Some(why) = rules.blocked_path(&path) {
        return Some(why);
    }
    if let Some(dir) = has_anti_cheat_dir(&anti_cheat_dir_candidates(&path), |p| p.is_dir()) {
        return Some(format!("anti-cheat beside it ({dir})"));
    }
    if let Some(l) = ancestors(pid).into_iter().find(|n| Rules::is_anti_cheat_launcher(n)) {
        return Some(format!("started by anti-cheat launcher ({l})"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard("*_be.exe", "dayz_be.exe"));
        assert!(wildcard("bedaemon*.exe", "bedaemon.exe"));
        assert!(wildcard("bedaemon*.exe", "bedaemon_x64.exe"));
        assert!(wildcard("g?me.exe", "game.exe"));
        assert!(wildcard("*", "anything"));
        assert!(wildcard("a*b*c", "axxbyyc"));
        assert!(!wildcard("a*b*c", "axxbyy"));
        assert!(!wildcard("*_be.exe", "be.exe"));
        assert!(!wildcard("cod.exe", "code.exe"));
        assert!(!wildcard("game.exe", "game.exe.bak"));
    }

    #[test]
    fn battleye_names() {
        let r = Rules::default();
        for p in [
            r"C:\Program Files (x86)\Common Files\BattlEye\BEService.exe",
            r"C:\Windows\BEService_x64.exe",
            r"D:\Tools\BEDaemon.exe",
            r"D:\Launchers\BELauncher.exe",
            r"D:\Games\DayZ\DayZ_BE.exe",
            r"D:\Games\Arma 3\arma3battleye\arma3_BE.exe",
        ] {
            assert!(r.blocked_path(p).is_some(), "{p}");
        }
    }

    #[test]
    fn other_anti_cheats() {
        let r = Rules::default();
        for p in [
            r"D:\Games\Foo\EasyAntiCheat\EasyAntiCheat_EOS_Setup.exe",
            r"D:\Games\Foo\start_protected_game.exe",
            r"C:\Program Files\Riot Vanguard\vgtray.exe",
            r"C:\Program Files\FACEIT AC\faceitclient.exe",
            r"C:\Program Files\FACEIT\FACEIT.exe",
            r"C:\Program Files\ESEA\eseaclient.exe",
            r"C:\Games\Lineage\GameGuard\GameMon64.des",
            r"C:\Games\Foo\XIGNCODE\x3.xem",
            r"C:\Games\Foo\EAAntiCheat.GameServiceLauncher.exe",
            r"C:\Windows\System32\dwm.exe",
        ] {
            assert!(r.blocked_path(p).is_some(), "{p}");
        }
    }

    #[test]
    fn ordinary_apps_allowed() {
        let r = Rules::default();
        for p in [
            r"C:\Windows\explorer.exe",
            r"C:\Program Files\Microsoft VS Code\Code.exe",
            r"C:\Program Files\Mozilla Firefox\firefox.exe",
            r"C:\Program Files\Beekeeper Studio\Beekeeper Studio.exe",
            r"C:\Windows\System32\notepad.exe",
        ] {
            assert_eq!(r.blocked_path(p), None, "{p}");
        }
    }

    #[test]
    fn user_blacklist() {
        let r = Rules::new(&["Discord.exe".into(), "obs*".into(), "spotify".into(), r"D:\Games\*".into(), r"C:/Tools/Portable".into(), "*".into(), " ".into()]);
        assert!(r.blocked_path(r"C:\Users\me\AppData\Local\Discord\app-1.0\Discord.exe").is_some());
        assert!(r.blocked_path(r"C:\Program Files\obs-studio\bin\64bit\obs64.exe").is_some());
        assert!(r.blocked_path(r"C:\Users\me\AppData\Roaming\Spotify\Spotify.exe").is_some());
        assert!(r.blocked_path(r"D:\Games\Something\game.exe").is_some());
        assert!(r.blocked_path(r"C:\Tools\Portable\sub\app.exe").is_some());
        assert!(r.blocked_path(r"C:\Tools\PortableX\app.exe").is_none());
        // A lone `*` is ignored rather than blocking everything.
        assert!(r.blocked_path(r"C:\Windows\explorer.exe").is_none());
    }

    #[test]
    fn anti_cheat_folders() {
        let exe = r"D:\Games\Squad\SquadGame\Binaries\Win64\SquadGame.exe";
        let dirs = anti_cheat_dir_candidates(exe);
        assert_eq!(dirs.len(), 4);
        assert_eq!(dirs[0], Path::new(r"D:\Games\Squad\SquadGame\Binaries\Win64"));
        assert_eq!(dirs[3], Path::new(r"D:\Games\Squad"));
        let hit = has_anti_cheat_dir(&dirs, |p| p == Path::new(r"D:\Games\Squad\BattlEye"));
        assert!(hit.is_some());
        assert!(has_anti_cheat_dir(&dirs, |_| false).is_none());
        // Never the drive root.
        assert!(anti_cheat_dir_candidates(r"C:\app.exe").is_empty());
    }

    #[test]
    fn launchers() {
        assert!(Rules::is_anti_cheat_launcher("DayZ_BE.exe"));
        assert!(Rules::is_anti_cheat_launcher("start_protected_game.exe"));
        assert!(!Rules::is_anti_cheat_launcher("explorer.exe"));
        assert!(!Rules::is_anti_cheat_launcher("steam.exe"));
    }
}
