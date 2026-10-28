//! Windows commands and System Settings pages for Spotlight: "brightness 60",
//! "volume up", "low power mode", "lock", "dark mode", "bluetooth settings"…

use super::index::score_text;

/// Something Spotlight can do right away.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Lock,
    Sleep,
    Restart,
    ShutDown,
    LogOut,
    EmptyTrash,
    LowPowerMode(bool),
    PowerMode(PowerMode),
    DarkMode(bool),
    Mute(bool),
    Volume(u8),
    VolumeStep(i8),
    Brightness(u8),
    BrightnessStep(i8),
    ShowDesktop,
    Screenshot,
    TaskManager,
    ControlPanel,
    DockSideSettings,
    SetupAssistant,
    QuitDockSide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerMode {
    Efficiency,
    Balanced,
    Performance,
}

/// A matched command row: title, subtitle, icon glyph (Segoe Fluent Icons), command.
pub struct Found {
    pub title: String,
    pub subtitle: &'static str,
    pub glyph: char,
    pub command: Command,
    pub score: i32,
}

/// (title, search words, glyph, command); titles that depend on state are fixed up in `search`.
const COMMANDS: &[(&str, &str, char, Command)] = &[
    ("Lock Screen", "lock screen computer", '\u{E72E}', Command::Lock),
    ("Sleep", "sleep suspend standby", '\u{E708}', Command::Sleep),
    ("Restart", "restart reboot", '\u{E777}', Command::Restart),
    ("Shut Down", "shut down shutdown power off turn off", '\u{E7E8}', Command::ShutDown),
    ("Log Out", "log out logout sign out signout", '\u{F3B1}', Command::LogOut),
    ("Empty Trash", "empty trash recycle bin", '\u{E74D}', Command::EmptyTrash),
    ("Low Power Mode", "low power mode battery saver energy saver efficiency", '\u{EBAA}', Command::LowPowerMode(true)),
    ("Power Mode: Best Power Efficiency", "power mode best power efficiency", '\u{E945}', Command::PowerMode(PowerMode::Efficiency)),
    ("Power Mode: Balanced", "power mode balanced", '\u{E945}', Command::PowerMode(PowerMode::Balanced)),
    ("Power Mode: Best Performance", "power mode best performance high performance", '\u{E945}', Command::PowerMode(PowerMode::Performance)),
    ("Dark Mode", "dark mode light mode appearance theme night", '\u{E793}', Command::DarkMode(true)),
    ("Mute", "mute unmute sound audio silence", '\u{E74F}', Command::Mute(true)),
    ("Volume Up", "volume up louder sound", '\u{E995}', Command::VolumeStep(10)),
    ("Volume Down", "volume down quieter sound", '\u{E993}', Command::VolumeStep(-10)),
    ("Brightness Up", "brightness up brighter screen display", '\u{E706}', Command::BrightnessStep(10)),
    ("Brightness Down", "brightness down dimmer dim screen display", '\u{EC8A}', Command::BrightnessStep(-10)),
    ("Show Desktop", "show desktop hide windows", '\u{E7F4}', Command::ShowDesktop),
    ("Take Screenshot", "screenshot screen capture snip snipping", '\u{E924}', Command::Screenshot),
    ("Task Manager", "task manager processes activity monitor", '\u{E9D9}', Command::TaskManager),
    ("Control Panel", "control panel", '\u{E80F}', Command::ControlPanel),
    ("DockSide Settings", "dockside settings preferences menu bar dock", '\u{E713}', Command::DockSideSettings),
    ("DockSide Setup Assistant", "setup assistant onboarding welcome tour getting started guide", '\u{E8FC}', Command::SetupAssistant),
    ("Quit DockSide", "quit dockside exit close stop", '\u{E8BB}', Command::QuitDockSide),
];

/// Commands matching `raw` (any case). `low_power` / `dark` are the current
/// states, so toggles read "Turn … On/Off".
pub fn search(raw: &str, low_power: bool, dark: bool) -> Vec<Found> {
    let q = raw.trim().to_lowercase();
    let mut out = Vec::new();
    if q.len() < 2 {
        return out;
    }
    if let Some(f) = parse_level(&q) {
        out.push(f);
    }
    for &(title, words, glyph, command) in COMMANDS {
        let by_title = score_text(&title.to_lowercase(), &q);
        let by_words = words.split(' ').filter_map(|w| score_text(w, &q)).max().map(|s| s - 50);
        // Multi-word queries ("low power") match the phrase.
        let by_phrase = (q.contains(' ') && words.contains(q.as_str())).then_some(850);
        let Some(score) = [by_title, by_words, by_phrase].into_iter().flatten().max() else { continue };
        let (title, command) = match command {
            Command::LowPowerMode(_) => {
                (format!("Turn Low Power Mode {}", if low_power { "Off" } else { "On" }), Command::LowPowerMode(!low_power))
            }
            Command::DarkMode(_) => (format!("Turn Dark Mode {}", if dark { "Off" } else { "On" }), Command::DarkMode(!dark)),
            Command::Mute(_) if q.starts_with("unmute") => ("Unmute".to_string(), Command::Mute(false)),
            c => (title.to_string(), c),
        };
        out.push(Found { title, subtitle: "Command", glyph, command, score });
    }
    out.sort_by(|a, b| b.score.cmp(&a.score));
    out
}

/// "brightness 60", "brightness 60%", "volume 30", "vol 0".
fn parse_level(q: &str) -> Option<Found> {
    let (word, rest) = q.split_once(' ')?;
    let n: u8 = rest.trim().trim_end_matches('%').trim().parse().ok().filter(|n| *n <= 100)?;
    let (title, glyph, command) = if "brightness".starts_with(word) && word.len() >= 3 {
        (format!("Set Brightness to {n}%"), '\u{E706}', Command::Brightness(n))
    } else if "volume".starts_with(word) && word.len() >= 3 {
        (format!("Set Volume to {n}%"), '\u{E767}', Command::Volume(n))
    } else {
        return None;
    };
    Some(Found { title, subtitle: "Command", glyph, command, score: 2000 })
}

/// System Settings pages: (title, search words, ms-settings URI).
pub const SETTINGS_PAGES: &[(&str, &str, &str)] = &[
    ("System", "system", "ms-settings:"),
    ("Display", "display screen resolution scale monitor brightness hdr", "ms-settings:display"),
    ("Night Light", "night light blue light warm", "ms-settings:nightlight"),
    ("Multiple Displays", "multiple displays monitors arrange projector", "ms-settings:display-advanced"),
    ("Sound", "sound audio speakers microphone output input", "ms-settings:sound"),
    ("Volume Mixer", "volume mixer app volume", "ms-settings:apps-volume"),
    ("Sound Devices", "sound devices audio", "ms-settings:sound-devices"),
    ("Notifications", "notifications alerts banners", "ms-settings:notifications"),
    ("Focus", "focus do not disturb dnd quiet hours", "ms-settings:focus"),
    ("Power & Battery", "power battery sleep energy saver battery saver", "ms-settings:powersleep"),
    ("Battery Saver", "battery saver energy saver low power", "ms-settings:batterysaver"),
    ("Storage", "storage disk space cleanup drives", "ms-settings:storagesense"),
    ("Nearby Sharing", "nearby sharing share airdrop", "ms-settings:crossdevice"),
    ("Multitasking", "multitasking snap windows virtual desktops", "ms-settings:multitasking"),
    ("Activation", "activation product key license", "ms-settings:activation"),
    ("Troubleshoot", "troubleshoot troubleshooters fix problems", "ms-settings:troubleshoot"),
    ("Recovery", "recovery reset this pc restore advanced startup", "ms-settings:recovery"),
    ("Projecting to This PC", "projecting miracast wireless display", "ms-settings:project"),
    ("Remote Desktop", "remote desktop rdp", "ms-settings:remotedesktop"),
    ("Clipboard", "clipboard history", "ms-settings:clipboard"),
    ("About", "about this pc device specifications rename pc", "ms-settings:about"),
    ("Bluetooth & Devices", "bluetooth devices pair", "ms-settings:bluetooth"),
    ("Printers & Scanners", "printers scanners print", "ms-settings:printers"),
    ("Mouse", "mouse pointer scroll speed", "ms-settings:mousetouchpad"),
    ("Touchpad", "touchpad trackpad gestures", "ms-settings:devices-touchpad"),
    ("Pen & Windows Ink", "pen ink stylus", "ms-settings:pen"),
    ("AutoPlay", "autoplay usb removable", "ms-settings:autoplay"),
    ("USB", "usb", "ms-settings:usb"),
    ("Cameras", "cameras webcam", "ms-settings:camera"),
    ("Network & Internet", "network internet", "ms-settings:network"),
    ("Wi-Fi", "wifi wi-fi wireless networks", "ms-settings:network-wifi"),
    ("Ethernet", "ethernet wired lan", "ms-settings:network-ethernet"),
    ("VPN", "vpn virtual private network", "ms-settings:network-vpn"),
    ("Mobile Hotspot", "mobile hotspot tethering share internet", "ms-settings:network-mobilehotspot"),
    ("Airplane Mode", "airplane mode flight", "ms-settings:network-airplanemode"),
    ("Proxy", "proxy", "ms-settings:network-proxy"),
    ("Advanced Network Settings", "advanced network adapters", "ms-settings:network-advancedsettings"),
    ("Personalization", "personalization personalize", "ms-settings:personalization"),
    ("Background", "background wallpaper desktop picture", "ms-settings:personalization-background"),
    ("Colors", "colors accent color dark mode light mode transparency", "ms-settings:colors"),
    ("Themes", "themes", "ms-settings:themes"),
    ("Lock Screen Settings", "lock screen wallpaper", "ms-settings:lockscreen"),
    ("Start", "start menu", "ms-settings:personalization-start"),
    ("Taskbar", "taskbar tray system tray", "ms-settings:taskbar"),
    ("Fonts", "fonts typefaces", "ms-settings:fonts"),
    ("Device Usage", "device usage", "ms-settings:deviceusage"),
    ("Installed Apps", "installed apps uninstall programs remove", "ms-settings:appsfeatures"),
    ("Default Apps", "default apps browser file associations open with", "ms-settings:defaultapps"),
    ("Startup Apps", "startup apps login items launch at login", "ms-settings:startupapps"),
    ("Optional Features", "optional features", "ms-settings:optionalfeatures"),
    ("Apps for Websites", "apps for websites links", "ms-settings:appsforwebsites"),
    ("Video Playback", "video playback hdr", "ms-settings:videoplayback"),
    ("Your Info", "account your info profile picture", "ms-settings:yourinfo"),
    ("Email & Accounts", "email accounts", "ms-settings:emailandaccounts"),
    ("Sign-in Options", "sign in options password pin windows hello fingerprint face", "ms-settings:signinoptions"),
    ("Family", "family parental", "ms-settings:family-group"),
    ("Other Users", "other users accounts", "ms-settings:otherusers"),
    ("Windows Backup", "backup onedrive sync", "ms-settings:backup"),
    ("Date & Time", "date time clock timezone time zone", "ms-settings:dateandtime"),
    ("Language & Region", "language region locale format", "ms-settings:regionlanguage"),
    ("Typing", "typing autocorrect spelling keyboard", "ms-settings:typing"),
    ("Keyboard", "keyboard input layout", "ms-settings:keyboard"),
    ("Speech", "speech voice recognition", "ms-settings:speech"),
    ("Gaming", "gaming game mode", "ms-settings:gaming-gamemode"),
    ("Game Bar", "game bar xbox", "ms-settings:gaming-gamebar"),
    ("Captures", "captures recordings game clips", "ms-settings:gaming-gamedvr"),
    ("Accessibility", "accessibility ease of access", "ms-settings:easeofaccess"),
    ("Text Size", "text size bigger font larger", "ms-settings:easeofaccess-display"),
    ("Visual Effects", "visual effects animations transparency scrollbars", "ms-settings:easeofaccess-visualeffects"),
    ("Mouse Pointer & Touch", "mouse pointer cursor size color touch", "ms-settings:easeofaccess-mousepointer"),
    ("Magnifier", "magnifier zoom", "ms-settings:easeofaccess-magnifier"),
    ("Color Filters", "color filters colorblind grayscale", "ms-settings:easeofaccess-colorfilter"),
    ("Contrast Themes", "contrast high contrast", "ms-settings:easeofaccess-highcontrast"),
    ("Narrator", "narrator screen reader", "ms-settings:easeofaccess-narrator"),
    ("Captions", "captions subtitles live captions", "ms-settings:easeofaccess-closedcaptioning"),
    ("Privacy & Security", "privacy security", "ms-settings:privacy"),
    ("Windows Security", "windows security defender antivirus virus firewall", "ms-settings:windowsdefender"),
    ("Find My Device", "find my device lost", "ms-settings:findmydevice"),
    ("Location", "location gps privacy", "ms-settings:privacy-location"),
    ("Camera Permissions", "camera permissions privacy", "ms-settings:privacy-webcam"),
    ("Microphone Permissions", "microphone mic permissions privacy", "ms-settings:privacy-microphone"),
    ("For Developers", "developers developer mode sudo", "ms-settings:developers"),
    ("Windows Update", "windows update updates upgrade patch", "ms-settings:windowsupdate"),
    ("Update History", "update history", "ms-settings:windowsupdate-history"),
    ("Delivery Optimization", "delivery optimization", "ms-settings:delivery-optimization"),
];

/// Settings pages matching `raw`, best first, with their scores.
pub fn settings_pages(raw: &str) -> Vec<(i32, &'static str, &'static str)> {
    let mut q = raw.trim().to_lowercase();
    // "bluetooth settings" → "bluetooth".
    for suffix in [" settings", " setting", " prefs", " preferences"] {
        if let Some(stripped) = q.strip_suffix(suffix) {
            q = stripped.to_string();
        }
    }
    if q.len() < 2 {
        return Vec::new();
    }
    let mut out: Vec<(i32, &str, &str)> = SETTINGS_PAGES
        .iter()
        .filter_map(|&(title, words, uri)| {
            let by_title = score_text(&title.to_lowercase(), &q);
            let by_words = words.split(' ').filter_map(|w| score_text(w, &q)).max().map(|s| s - 60);
            let by_phrase = (q.contains(' ') && words.contains(q.as_str())).then_some(820);
            let s = [by_title, by_words, by_phrase].into_iter().flatten().max()?;
            Some((s, title, uri))
        })
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(search("brightness 60", false, false)[0].command, Command::Brightness(60));
        assert_eq!(search("vol 30%", false, false)[0].command, Command::Volume(30));
        assert!(parse_level("brightness 200").is_none());
    }

    #[test]
    fn toggles_follow_state() {
        let on = search("low power", false, false);
        assert_eq!(on[0].command, Command::LowPowerMode(true));
        assert_eq!(on[0].title, "Turn Low Power Mode On");
        assert_eq!(search("low power", true, false)[0].command, Command::LowPowerMode(false));
        assert_eq!(search("dark mode", true, true)[0].title, "Turn Dark Mode Off");
    }

    #[test]
    fn setup_assistant() {
        assert_eq!(search("setup assistant", false, false)[0].command, Command::SetupAssistant);
        assert_eq!(search("onboarding", false, false)[0].command, Command::SetupAssistant);
    }

    #[test]
    fn quit() {
        assert_eq!(search("quit", false, false)[0].command, Command::QuitDockSide);
        assert_eq!(search("quit dockside", false, false)[0].command, Command::QuitDockSide);
        assert_eq!(search("exit", false, false)[0].command, Command::QuitDockSide);
    }

    #[test]
    fn settings_lookup() {
        assert_eq!(settings_pages("bluetooth settings")[0].2, "ms-settings:bluetooth");
        assert_eq!(settings_pages("wallpaper")[0].2, "ms-settings:personalization-background");
        assert_eq!(settings_pages("windows update")[0].2, "ms-settings:windowsupdate");
    }
}
