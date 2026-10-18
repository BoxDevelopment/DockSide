//! Web search engines for Spotlight's "Search the web" result.

use crate::config::SpotlightCfg;

/// (config value, display name, URL template with `%s` for the query).
pub const ENGINES: [(&str, &str, &str); 5] = [
    ("google", "Google", "https://www.google.com/search?q=%s"),
    ("bing", "Bing", "https://www.bing.com/search?q=%s"),
    ("duckduckgo", "DuckDuckGo", "https://duckduckgo.com/?q=%s"),
    ("kagi", "Kagi", "https://kagi.com/search?q=%s"),
    ("brave", "Brave", "https://search.brave.com/search?q=%s"),
];

/// A custom template is usable if it's an http(s) URL.
pub fn valid_template(t: &str) -> bool {
    let t = t.trim().to_lowercase();
    (t.starts_with("https://") || t.starts_with("http://")) && t.len() > "https://".len()
}

/// Display name for a custom template: its host without "www." ("search.example.com").
fn host_name(template: &str) -> String {
    let rest = template.trim().split_once("://").map(|(_, r)| r).unwrap_or(template);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.trim_start_matches("www.").to_string()
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Engine display name and the URL to open for `query`. Unknown engines and
/// unusable custom templates fall back to Google.
pub fn search(cfg: &SpotlightCfg, query: &str) -> (String, String) {
    let q = encode(query.trim());
    if cfg.search_engine.eq_ignore_ascii_case("custom") && valid_template(&cfg.custom_search_url) {
        let t = cfg.custom_search_url.trim();
        // No placeholder: append the query.
        let url = if t.contains("%s") { t.replace("%s", &q) } else { format!("{t}{q}") };
        return (host_name(t), url);
    }
    let (_, name, template) =
        ENGINES.iter().find(|e| e.0.eq_ignore_ascii_case(&cfg.search_engine)).copied().unwrap_or(ENGINES[0]);
    (name.to_string(), template.replace("%s", &q))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(engine: &str, custom: &str) -> SpotlightCfg {
        SpotlightCfg {
            enabled: true,
            hotkey: "alt+space".into(),
            search_engine: engine.into(),
            custom_search_url: custom.into(),
        }
    }

    #[test]
    fn builtin_engines() {
        assert_eq!(search(&cfg("duckduckgo", ""), "rust lang"), ("DuckDuckGo".into(), "https://duckduckgo.com/?q=rust+lang".into()));
        assert_eq!(search(&cfg("KAGI", ""), "a&b").1, "https://kagi.com/search?q=a%26b");
    }

    #[test]
    fn custom_engine() {
        let c = cfg("custom", "https://www.startpage.com/do/search?q=%s");
        assert_eq!(search(&c, "x y"), ("startpage.com".into(), "https://www.startpage.com/do/search?q=x+y".into()));
        // Without %s the query is appended.
        assert_eq!(search(&cfg("custom", "https://ex.com/?q="), "hi").1, "https://ex.com/?q=hi");
    }

    #[test]
    fn falls_back_to_google() {
        assert_eq!(search(&cfg("custom", "javascript:alert(1)"), "x").0, "Google");
        assert_eq!(search(&cfg("nonsense", ""), "x").0, "Google");
    }
}
