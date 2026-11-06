//! App categories for Tahoe's Apps panel and Launchpad's starting folders.
//! Windows doesn't say what kind of app something is (the Mac App Store
//! does), so a built-in list of well-known apps and words decides; anything
//! unknown is "Other".

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Category {
    Productivity,
    Creativity,
    Social,
    Entertainment,
    Games,
    Developer,
    Information,
    Utilities,
    Other,
}

impl Category {
    pub const ALL: [Category; 9] = [
        Category::Productivity,
        Category::Creativity,
        Category::Social,
        Category::Entertainment,
        Category::Games,
        Category::Developer,
        Category::Information,
        Category::Utilities,
        Category::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Category::Productivity => "Productivity & Finance",
            Category::Creativity => "Creativity",
            Category::Social => "Social",
            Category::Entertainment => "Entertainment",
            Category::Games => "Games",
            Category::Developer => "Developer Tools",
            Category::Information => "Information & Reading",
            Category::Utilities => "Utilities",
            Category::Other => "Other",
        }
    }
}

/// Words (matched at the start of a word of the app's name) per category, most specific first.
const RULES: &[(Category, &[&str])] = &[
    (
        Category::Games,
        &[
            "steam", "epic games", "xbox", "minecraft", "roblox", "battle.net", "ea app", "origin", "ubisoft", "gog galaxy",
            "riot client", "league of legends", "valorant", "fortnite", "solitaire", "game", "games", "playnite", "itch",
            "rockstar games", "overwatch", "counter-strike", "dota", "genshin", "hearthstone", "rocket league", "chess",
            "osu!", "retroarch", "dolphin emulator", "lutris", "heroic",
        ],
    ),
    (
        Category::Developer,
        &[
            "visual studio", "code", "vs code", "terminal", "powershell", "command prompt", "git", "github", "docker",
            "python", "node.js", "android studio", "intellij", "pycharm", "webstorm", "rider", "clion", "goland",
            "jetbrains", "postman", "wsl", "ubuntu", "debian", "windows subsystem", "sublime text", "notepad++", "cursor",
            "neovim", "vim", "emacs", "unity", "unreal", "godot", "insomnia", "dbeaver", "sql", "putty", "winscp",
            "filezilla", "developer", "sdk", "claude", "warp", "zed", "wireshark", "hyper-v",
        ],
    ),
    (
        Category::Social,
        &[
            "discord", "slack", "teams", "zoom", "skype", "whatsapp", "telegram", "signal", "messenger", "facebook",
            "instagram", "twitter", "x", "mastodon", "thunderbird", "outlook", "mail", "phone link", "webex", "element",
            "guilded", "teamspeak", "mumble", "people", "linkedin", "reddit", "bluesky",
        ],
    ),
    (
        Category::Creativity,
        &[
            "photoshop", "illustrator", "premiere", "after effects", "lightroom", "indesign", "creative cloud", "adobe",
            "figma", "blender", "gimp", "inkscape", "krita", "paint", "paint.net", "affinity", "davinci", "resolve",
            "obs", "audacity", "fl studio", "ableton", "reaper", "clipchamp", "canva", "photos", "camera", "snipping",
            "sketch", "capcut", "handbrake", "shotcut", "kdenlive", "cinema 4d", "maya", "3ds max", "zbrush", "fusion 360",
            "autocad", "sketchup", "photo", "video editor", "studio",
        ],
    ),
    (
        Category::Entertainment,
        &[
            "spotify", "netflix", "music", "media player", "vlc", "itunes", "apple tv", "apple music", "disney", "prime video",
            "hulu", "youtube", "twitch", "plex", "kodi", "jellyfin", "tidal", "deezer", "soundcloud", "movies & tv",
            "crunchyroll", "audible", "podcast", "mpv", "foobar", "musicbee", "winamp", "groove",
        ],
    ),
    (
        Category::Productivity,
        &[
            "word", "excel", "powerpoint", "onenote", "office", "microsoft 365", "access", "publisher", "project", "visio",
            "notion", "obsidian", "evernote", "todo", "to do", "calendar", "libreoffice", "openoffice", "acrobat",
            "pdf", "notes", "sticky notes", "trello", "asana", "clickup", "1password", "bitwarden", "keepass", "lastpass",
            "dropbox", "onedrive", "google drive", "icloud", "quicken", "money", "banking", "wps", "anki", "todoist",
            "calculator", "clock", "reminders", "planner", "copilot", "chatgpt",
        ],
    ),
    (
        Category::Information,
        &[
            "edge", "chrome", "firefox", "opera", "brave", "vivaldi", "safari", "arc", "zen browser", "tor browser",
            "weather", "news", "maps", "kindle", "books", "calibre", "feedly", "wikipedia", "dictionary", "translator",
            "reader", "stocks",
        ],
    ),
    (
        Category::Utilities,
        &[
            "settings", "control panel", "task manager", "file explorer", "explorer", "notepad", "7-zip", "winrar",
            "everything", "powertoys", "ccleaner", "cpu-z", "gpu-z", "hwinfo", "hwmonitor", "msi afterburner",
            "rufus", "nvidia", "amd", "intel", "realtek", "dolby", "logitech", "razer", "corsair", "steelseries",
            "windows", "remote desktop", "registry", "disk", "defragment", "cleanup", "recovery", "backup", "security",
            "defender", "antivirus", "malwarebytes", "vpn", "nordvpn", "proton", "wireguard", "openvpn", "tailscale",
            "character map", "magnifier", "narrator", "on-screen keyboard", "quick assist", "resource monitor",
            "system information", "event viewer", "services", "computer management", "odbc", "iscsi", "print",
            "snip", "voice recorder", "sound recorder", "dockside", "teamviewer", "anydesk", "utility", "tools", "manager",
            "driver", "update", "uninstaller", "bluetooth", "devices", "lenovo", "dell", "hp ", "asus", "acer", "msi",
        ],
    ),
];

/// Whether `word` starts a word of `name` (both lower case). Multi-word rules match as a phrase.
fn has_word(name: &str, word: &str) -> bool {
    let mut at = 0;
    while let Some(i) = name[at..].find(word) {
        let start = at + i;
        let before_ok = start == 0 || !name[..start].chars().next_back().is_some_and(char::is_alphanumeric);
        let end = start + word.len();
        let after_ok = word.ends_with(' ') || end == name.len() || !name[end..].chars().next().is_some_and(char::is_alphanumeric);
        if before_ok && after_ok {
            return true;
        }
        at = start + word.len().max(1);
        if at >= name.len() {
            break;
        }
    }
    false
}

/// The category for an app, from its display name and launch target.
pub fn of(name: &str, target: &str) -> Category {
    let name = name.to_lowercase();
    let target = target.to_lowercase();
    if target.starts_with("steam://") || target.contains("steamapps") || target.starts_with("com.epicgames") || target.contains("\\games\\") {
        return Category::Games;
    }
    for (cat, words) in RULES {
        if words.iter().any(|w| has_word(&name, w)) {
            return *cat;
        }
    }
    Category::Other
}

/// Start menu clutter that isn't an app: uninstallers, readmes, help files, links to websites.
pub fn is_clutter(name: &str, target: &str) -> bool {
    let n = name.to_lowercase();
    let t = target.to_lowercase();
    const WORDS: &[&str] = &[
        "uninstall", "readme", "read me", "release notes", "documentation", "user guide", "user manual", "manual",
        "help", "license", "website", "web site", "on the web", "homepage", "changelog", "what's new", "faq",
        "support center", "online support",
    ];
    if WORDS.iter().any(|w| has_word(&n, w)) {
        return true;
    }
    [".chm", ".txt", ".pdf", ".htm", ".html", ".rtf", ".md"].iter().any(|ext| t.ends_with(ext))
        || (t.starts_with("http://") || t.starts_with("https://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_well_known_apps() {
        assert_eq!(of("Visual Studio Code", ""), Category::Developer);
        assert_eq!(of("Discord", ""), Category::Social);
        assert_eq!(of("Spotify", ""), Category::Entertainment);
        assert_eq!(of("Steam", ""), Category::Games);
        assert_eq!(of("Excel", ""), Category::Productivity);
        assert_eq!(of("Google Chrome", ""), Category::Information);
        assert_eq!(of("Task Manager", ""), Category::Utilities);
        assert_eq!(of("Zebra", ""), Category::Other);
        // "code" mustn't match inside other words.
        assert_eq!(of("Barcode Maker", ""), Category::Other);
    }

    #[test]
    fn spots_clutter() {
        assert!(is_clutter("Uninstall Foo", "C:\\foo\\unins000.exe"));
        assert!(is_clutter("Foo Help", "C:\\foo\\help.chm"));
        assert!(!is_clutter("Foo", "C:\\foo\\foo.exe"));
    }
}
