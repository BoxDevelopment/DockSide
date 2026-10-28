//! Keyboard shortcuts as the config writes them ("alt+space", "ctrl+shift+k",
//! or a modifier tapped on its own: "win"), the hook that spots those taps, and
//! the recorder Settings uses to capture a new shortcut.
//!
//! Combinations go to `RegisterHotKey` (which keeps working over admin windows);
//! taps need a low-level keyboard hook, since Windows has no hotkey for them.

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win;

/// Modifier bits, the same as RegisterHotKey's `MOD_*`.
pub const ALT: u32 = 1;
pub const CTRL: u32 = 2;
pub const SHIFT: u32 = 4;
pub const WIN: u32 = 8;

/// Longest press that still counts as a tap.
const TAP_MS: u32 = 500;
/// Two taps of a key closer together than this are a double tap.
pub const DOUBLE_TAP_MS: u32 = 300;
/// vkE8 is unassigned: sending it breaks up an Alt / Win "tap" without doing anything.
const MASK_VK: u16 = 0xE8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hotkey {
    /// Modifier bits and a virtual key.
    Combo { mods: u32, vk: u32 },
    /// One modifier (a single bit), pressed and released with nothing else.
    Tap(u32),
}

/// Keys with names of their own: (virtual key, config name, label).
const NAMED: [(u32, &str, &str); 30] = [
    (0x20, "space", "Space"),
    (0x0D, "enter", "Enter"),
    (0x09, "tab", "Tab"),
    (0x1B, "esc", "Esc"),
    (0x08, "backspace", "Backspace"),
    (0x2E, "delete", "Delete"),
    (0x2D, "insert", "Insert"),
    (0x24, "home", "Home"),
    (0x23, "end", "End"),
    (0x21, "pageup", "Page Up"),
    (0x22, "pagedown", "Page Down"),
    (0x25, "left", "\u{2190}"),
    (0x26, "up", "\u{2191}"),
    (0x27, "right", "\u{2192}"),
    (0x28, "down", "\u{2193}"),
    (0x13, "pause", "Pause"),
    (0x2C, "printscreen", "Print Screen"),
    (0xBA, ";", ";"),
    (0xBB, "=", "="),
    (0xBC, ",", ","),
    (0xBD, "-", "-"),
    (0xBE, ".", "."),
    (0xBF, "/", "/"),
    (0xC0, "`", "`"),
    (0xDB, "[", "["),
    (0xDC, "\\", "\\"),
    (0xDD, "]", "]"),
    (0xDE, "'", "'"),
    (0x6A, "num*", "Num *"),
    (0x6B, "numplus", "Num +"),
];

const MODS: [(u32, &str, &str, &str); 4] = [
    // (bit, config name, Windows label, Mac symbol), in Windows' display order.
    (WIN, "win", "Win", "\u{2303}"),
    (CTRL, "ctrl", "Ctrl", "\u{2318}"),
    (ALT, "alt", "Alt", "\u{2325}"),
    (SHIFT, "shift", "Shift", "\u{21E7}"),
];

/// A non-modifier key's config name and label.
fn key_name(vk: u32) -> Option<(String, String)> {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => {
            let c = (vk as u8 as char).to_string();
            Some((c.to_lowercase(), c))
        }
        0x60..=0x69 => Some((format!("num{}", vk - 0x60), format!("Num {}", vk - 0x60))),
        0x70..=0x87 => Some((format!("f{}", vk - 0x6F), format!("F{}", vk - 0x6F))),
        _ => NAMED.iter().find(|k| k.0 == vk).map(|k| (k.1.to_string(), k.2.to_string())),
    }
}

fn key_vk(name: &str) -> Option<u32> {
    match name {
        "return" => return Some(0x0D),
        "escape" => return Some(0x1B),
        "del" => return Some(0x2E),
        _ => {}
    }
    if let Some(k) = NAMED.iter().find(|k| k.1 == name) {
        return Some(k.0);
    }
    if name.len() == 1 && name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Some(name.to_ascii_uppercase().as_bytes()[0] as u32);
    }
    if let Some(n) = name.strip_prefix("num").and_then(|n| n.parse::<u32>().ok()).filter(|n| *n <= 9) {
        return Some(0x60 + n);
    }
    name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=24).contains(n)).map(|n| 0x6F + n)
}

/// The modifier bit for a virtual key (either side), if it is one.
pub fn modifier(vk: u16) -> Option<u32> {
    match vk {
        0x10 | 0xA0 | 0xA1 => Some(SHIFT),
        0x11 | 0xA2 | 0xA3 => Some(CTRL),
        0x12 | 0xA4 | 0xA5 => Some(ALT),
        0x5B | 0x5C => Some(WIN),
        _ => None,
    }
}

impl Hotkey {
    /// Parses "alt+space", "ctrl+shift+k", "win+f1", or a lone modifier ("win").
    pub fn parse(spec: &str) -> Option<Hotkey> {
        let (mut mods, mut key) = (0, None);
        for part in spec.to_lowercase().split('+').map(str::trim) {
            let bit = match part {
                "ctrl" | "control" => Some(CTRL),
                "alt" | "option" => Some(ALT),
                "shift" => Some(SHIFT),
                "win" | "super" | "cmd" => Some(WIN),
                _ => None,
            };
            match bit {
                Some(b) => mods |= b,
                None if key.is_none() => key = Some(key_vk(part)?),
                None => return None,
            }
        }
        match key {
            Some(vk) => Some(Hotkey::Combo { mods, vk }),
            None if mods.count_ones() == 1 => Some(Hotkey::Tap(mods)),
            None => None,
        }
    }

    /// The config form: "ctrl+alt+space", "win".
    pub fn spec(&self) -> String {
        let (mods, vk) = match *self {
            Hotkey::Combo { mods, vk } => (mods, Some(vk)),
            Hotkey::Tap(m) => (m, None),
        };
        let mut parts: Vec<String> =
            [CTRL, ALT, SHIFT, WIN].iter().filter(|&&b| mods & b != 0).map(|&b| MODS.iter().find(|m| m.0 == b).unwrap().1.to_string()).collect();
        if let Some((name, _)) = vk.and_then(key_name) {
            parts.push(name);
        }
        parts.join("+")
    }

    /// For display: "⌥ Space" / "Alt+Space"; a tap is the key's name ("Win").
    pub fn label(&self, mac: bool) -> String {
        match *self {
            Hotkey::Tap(m) => MODS.iter().find(|x| x.0 == m).map_or_else(String::new, |x| x.2.to_string()),
            Hotkey::Combo { mods, vk } => {
                let key = key_name(vk).map_or_else(|| format!("Key {vk:#x}"), |k| k.1);
                mods_label(mods, mac) + if mac && key.chars().count() > 1 { " " } else { "" } + &key
            }
        }
    }

    /// Whether this can be a global shortcut without getting in the way of
    /// typing: anything but a function key needs Ctrl, Alt or Win.
    pub fn usable(&self) -> bool {
        match *self {
            Hotkey::Tap(_) => true,
            Hotkey::Combo { mods, vk } => mods & (CTRL | ALT | WIN) != 0 || (0x70..=0x87).contains(&vk),
        }
    }

    /// Whether `RegisterHotKey` takes it: false when Windows or another app already has it.
    pub fn available(&self) -> bool {
        let Hotkey::Combo { mods, vk } = *self else { return true };
        const PROBE: i32 = 0xBFFE;
        unsafe {
            let ok = RegisterHotKey(None, PROBE, HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT, vk).is_ok();
            if ok {
                let _ = UnregisterHotKey(None, PROBE);
            }
            ok
        }
    }
}

/// "⌃⌥" / "Win+Alt+" for modifier bits.
fn mods_label(mods: u32, mac: bool) -> String {
    if mac {
        // Mac order: ⌃ ⌥ ⇧ ⌘.
        [WIN, ALT, SHIFT, CTRL].iter().filter(|&&b| mods & b != 0).map(|&b| MODS.iter().find(|m| m.0 == b).unwrap().3).collect()
    } else {
        MODS.iter().filter(|m| mods & m.0 != 0).map(|m| format!("{}+", m.2)).collect()
    }
}

fn hhook(raw: isize) -> HHOOK {
    HHOOK(raw as _)
}

/// Sends Alt / Win back up behind the masking key, so releasing it does nothing.
unsafe fn masked_release(info: &KBDLLHOOKSTRUCT) {
    let key = |vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, ..Default::default() } },
    };
    let ext = if info.flags.0 & LLKHF_EXTENDED.0 != 0 { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
    let inputs = [
        key(MASK_VK, 0, KEYBD_EVENT_FLAGS(0)),
        key(MASK_VK, 0, KEYEVENTF_KEYUP),
        key(info.vkCode as u16, info.scanCode as u16, KEYEVENTF_KEYUP | ext),
    ];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

// --- watching for taps ------------------------------------------------------

/// The modifier bit being watched, and where to post taps.
static TAP_MOD: AtomicU32 = AtomicU32::new(0);
static TAP_HWND: AtomicIsize = AtomicIsize::new(0);
static TAP_MSG: AtomicU32 = AtomicU32::new(0);
/// The watched key while it's down with nothing else pressed (0: none), when it went down,
/// and when the last tap ended.
static CANDIDATE: AtomicU32 = AtomicU32::new(0);
static DOWN_AT: AtomicU32 = AtomicU32::new(0);
static LAST_TAP: AtomicU32 = AtomicU32::new(0);

/// Posts `msg` to `hwnd` whenever `modifier` is tapped on its own: wParam 1, or 2
/// for the second tap of a quick double tap. Hooked while this is alive.
pub struct TapWatch {
    keys: isize,
    mouse: isize,
}

impl TapWatch {
    pub fn new(modifier: u32, hwnd: isize, msg: u32) -> Option<TapWatch> {
        TAP_MOD.store(modifier, Ordering::Relaxed);
        TAP_HWND.store(hwnd, Ordering::Relaxed);
        TAP_MSG.store(msg, Ordering::Relaxed);
        CANDIDATE.store(0, Ordering::Relaxed);
        unsafe {
            let keys = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(tap_key_hook), win::hinstance(), 0) {
                Ok(h) => h,
                Err(e) => {
                    log!("tap key hook failed: {e}");
                    return None;
                }
            };
            // Ctrl+click and Ctrl+wheel aren't taps either.
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(tap_mouse_hook), win::hinstance(), 0).map_or(0, |h| h.0 as isize);
            Some(TapWatch { keys: keys.0 as isize, mouse })
        }
    }
}

impl Drop for TapWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(hhook(self.keys));
            if self.mouse != 0 {
                let _ = UnhookWindowsHookEx(hhook(self.mouse));
            }
        }
        TAP_MOD.store(0, Ordering::Relaxed);
    }
}

unsafe extern "system" fn tap_key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let vk = info.vkCode as u16;
        let injected = info.flags.0 & LLKHF_INJECTED.0 != 0;
        let watched = TAP_MOD.load(Ordering::Relaxed);
        if !injected && vk != MASK_VK && watched != 0 {
            let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let ours = modifier(vk) == Some(watched);
            if down {
                // Key repeat sends more key-downs for the same key; anything else ends the tap.
                if !ours {
                    CANDIDATE.store(0, Ordering::Relaxed);
                } else if CANDIDATE.load(Ordering::Relaxed) != vk as u32 {
                    CANDIDATE.store(vk as u32, Ordering::Relaxed);
                    DOWN_AT.store(info.time, Ordering::Relaxed);
                }
            } else if ours && CANDIDATE.swap(0, Ordering::Relaxed) == vk as u32
                && info.time.wrapping_sub(DOWN_AT.load(Ordering::Relaxed)) <= TAP_MS
            {
                let double = info.time.wrapping_sub(LAST_TAP.swap(info.time, Ordering::Relaxed)) <= DOUBLE_TAP_MS;
                if double {
                    // A third tap starts over.
                    LAST_TAP.store(info.time.wrapping_sub(DOUBLE_TAP_MS + 1), Ordering::Relaxed);
                }
                win::post_wp(TAP_HWND.load(Ordering::Relaxed), TAP_MSG.load(Ordering::Relaxed), if double { 2 } else { 1 });
                // Releasing Win alone opens Start, and Alt alone an app's menu: mask them.
                if watched & (WIN | ALT) != 0 {
                    masked_release(info);
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

unsafe extern "system" fn tap_mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 && wp.0 as u32 != WM_MOUSEMOVE {
        CANDIDATE.store(0, Ordering::Relaxed);
    }
    CallNextHookEx(None, code, wp, lp)
}

// --- recording --------------------------------------------------------------

static CAPTURE_HWND: AtomicIsize = AtomicIsize::new(0);
static CAPTURE_MSG: AtomicU32 = AtomicU32::new(0);

/// How a recording ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    Cancel,
    /// The new shortcut's config form; empty when cleared.
    Set(String),
}

/// Captures a new shortcut. While alive it takes every key press, so the Win key
/// doesn't open Start and existing shortcuts don't fire; each one arrives as `msg`
/// at the window that started it, for [`Recorder::on_key`].
pub struct Recorder {
    hook: isize,
    /// Keys down now, as the hook saw them.
    down: Vec<u16>,
    /// Modifiers pressed since everything was last up.
    mods_seen: u32,
    /// Another key was pressed since everything was last up.
    keyed: bool,
    /// The outcome, kept until every key is up so no release leaks to an app.
    result: Option<Recorded>,
    /// Why the last combination wasn't taken.
    pub warning: Option<&'static str>,
}

impl Recorder {
    pub fn start(hwnd: HWND, msg: u32) -> Option<Recorder> {
        CAPTURE_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        CAPTURE_MSG.store(msg, Ordering::Relaxed);
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(capture_hook), win::hinstance(), 0) } {
            Ok(h) => Some(Recorder { hook: h.0 as isize, down: Vec::new(), mods_seen: 0, keyed: false, result: None, warning: None }),
            Err(e) => {
                log!("shortcut recorder hook failed: {e}");
                None
            }
        }
    }

    fn held_mods(&self) -> u32 {
        self.down.iter().filter_map(|&k| modifier(k)).fold(0, |a, b| a | b)
    }

    /// A key from the hook (`wp` as posted). `current` is the shortcut in use,
    /// which is allowed again even though DockSide itself holds it. Returns the
    /// outcome once recording is over.
    pub fn on_key(&mut self, wp: usize, current: &str) -> Option<Recorded> {
        let (vk, down) = ((wp & 0xFFFF) as u16, wp >> 16 != 0);
        if !down {
            self.down.retain(|&k| k != vk);
            let tap = !self.keyed && self.mods_seen.count_ones() == 1 && modifier(vk).is_some();
            if self.down.is_empty() {
                if self.result.is_none() && tap {
                    self.result = Some(Recorded::Set(Hotkey::Tap(self.mods_seen).spec()));
                }
                self.keyed = false;
                self.mods_seen = 0;
                return self.result.take();
            }
            return None;
        }
        if self.down.contains(&vk) {
            return None; // key repeat
        }
        self.down.push(vk);
        if self.result.is_some() {
            return None;
        }
        if let Some(m) = modifier(vk) {
            self.mods_seen |= m;
            return None;
        }
        self.keyed = true;
        let mods = self.held_mods();
        self.result = match vk {
            0x1B if mods == 0 => Some(Recorded::Cancel),
            0x08 | 0x2E if mods == 0 => Some(Recorded::Set(String::new())),
            _ => match key_name(vk as u32) {
                None => {
                    self.warning = Some("That key can\u{2019}t be used");
                    None
                }
                Some(_) => {
                    let hk = Hotkey::Combo { mods, vk: vk as u32 };
                    let same = Hotkey::parse(current) == Some(hk);
                    if !hk.usable() {
                        self.warning = Some("Add Ctrl, Alt or Win");
                        None
                    } else if !same && !hk.available() {
                        self.warning = Some("Taken by Windows or an app");
                        None
                    } else {
                        Some(Recorded::Set(hk.spec()))
                    }
                }
            },
        };
        if self.result.is_some() {
            self.warning = None;
        }
        None
    }

    /// What the recorder shows: the result, or the modifiers held so far.
    pub fn preview(&self, mac: bool) -> Option<String> {
        match &self.result {
            Some(Recorded::Set(spec)) => return Some(Hotkey::parse(spec).map_or_else(String::new, |h| h.label(mac))),
            Some(Recorded::Cancel) => return None,
            None => {}
        }
        let mods = self.held_mods();
        (mods != 0).then(|| mods_label(mods, mac) + if mac { "" } else { "\u{2026}" })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(hhook(self.hook));
        }
    }
}

/// Swallows every real key press, handing it to the recording window
/// (wParam: virtual key, plus 1 << 16 for a press).
unsafe extern "system" fn capture_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        if info.flags.0 & LLKHF_INJECTED.0 == 0 {
            let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN) as usize;
            win::post_wp(CAPTURE_HWND.load(Ordering::Relaxed), CAPTURE_MSG.load(Ordering::Relaxed), info.vkCode as usize | down << 16);
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(Hotkey::parse("alt+space"), Some(Hotkey::Combo { mods: ALT, vk: 0x20 }));
        assert_eq!(Hotkey::parse("Ctrl+Alt+Space"), Some(Hotkey::Combo { mods: CTRL | ALT, vk: 0x20 }));
        assert_eq!(Hotkey::parse("win+f1"), Some(Hotkey::Combo { mods: WIN, vk: 0x70 }));
        assert_eq!(Hotkey::parse("ctrl+k"), Some(Hotkey::Combo { mods: CTRL, vk: b'K' as u32 }));
        assert_eq!(Hotkey::parse("ctrl+shift+/"), Some(Hotkey::Combo { mods: CTRL | SHIFT, vk: 0xBF }));
        assert_eq!(Hotkey::parse("win"), Some(Hotkey::Tap(WIN)));
        assert_eq!(Hotkey::parse("Shift"), Some(Hotkey::Tap(SHIFT)));
        assert_eq!(Hotkey::parse("alt+banana"), None);
        assert_eq!(Hotkey::parse("ctrl+alt"), None);
        assert_eq!(Hotkey::parse("a+b"), None);
        assert_eq!(Hotkey::parse(""), None);
    }

    #[test]
    fn round_trip() {
        for spec in ["alt+space", "ctrl+shift+k", "win+f1", "ctrl+alt+;", "win", "ctrl", "alt+num5", "ctrl+pageup"] {
            assert_eq!(Hotkey::parse(spec).unwrap().spec(), spec);
        }
        assert_eq!(Hotkey::parse("shift+ctrl+space").unwrap().spec(), "ctrl+shift+space");
    }

    #[test]
    fn labels() {
        let alt_space = Hotkey::parse("alt+space").unwrap();
        assert_eq!(alt_space.label(true), "\u{2325} Space");
        assert_eq!(alt_space.label(false), "Alt+Space");
        assert_eq!(Hotkey::parse("ctrl+shift+k").unwrap().label(true), "\u{21E7}\u{2318}K");
        assert_eq!(Hotkey::parse("win").unwrap().label(true), "Win");
    }

    #[test]
    fn usable() {
        assert!(Hotkey::parse("alt+space").unwrap().usable());
        assert!(Hotkey::parse("f5").unwrap().usable());
        assert!(!Hotkey::parse("shift+k").unwrap().usable());
        assert!(!Hotkey::parse("space").unwrap().usable());
    }

    /// Feeds the recorder key events without its hook.
    fn recorder() -> Recorder {
        Recorder { hook: 0, down: Vec::new(), mods_seen: 0, keyed: false, result: None, warning: None }
    }

    fn press(r: &mut Recorder, vk: u16, down: bool) -> Option<Recorded> {
        r.on_key(vk as usize | (down as usize) << 16, "alt+space")
    }

    #[test]
    fn records_taps_and_combos() {
        let mut r = recorder();
        assert_eq!(press(&mut r, 0x5B, true), None);
        assert_eq!(press(&mut r, 0x5B, true), None); // repeat
        assert_eq!(press(&mut r, 0x5B, false), Some(Recorded::Set("win".into())));

        // The current shortcut is accepted even though DockSide holds it.
        assert_eq!(press(&mut r, 0xA4, true), None);
        assert_eq!(press(&mut r, 0x20, true), None);
        assert_eq!(r.preview(true).as_deref(), Some("\u{2325} Space"));
        assert_eq!(press(&mut r, 0x20, false), None);
        assert_eq!(press(&mut r, 0xA4, false), Some(Recorded::Set("alt+space".into())));

        // Two modifiers alone aren't a tap.
        press(&mut r, 0xA2, true);
        press(&mut r, 0xA0, true);
        press(&mut r, 0xA0, false);
        assert_eq!(press(&mut r, 0xA2, false), None);

        // Shift+letter would get in the way of typing.
        press(&mut r, 0xA0, true);
        press(&mut r, 0x4B, true);
        assert!(r.warning.is_some());
        press(&mut r, 0x4B, false);
        assert_eq!(press(&mut r, 0xA0, false), None);

        assert_eq!(press(&mut r, 0x1B, true), None);
        assert_eq!(press(&mut r, 0x1B, false), Some(Recorded::Cancel));
        assert_eq!(press(&mut r, 0x08, true), None);
        assert_eq!(press(&mut r, 0x08, false), Some(Recorded::Set(String::new())));
    }
}
