//! Synced lyrics for Now Playing, from LRCLIB (lrclib.net: a free, open
//! lyrics database with no account or key). Each new song is looked up once,
//! by title and artist, on a worker thread; the island shows the line being
//! sung. Only the title, artist and length leave the machine.

use std::collections::HashMap;
use std::sync::Mutex;

/// A song's identity for lookups: (title, artist).
pub type Key = (String, String);
/// Timed lines: (seconds, text), in order.
pub type Lines = Vec<(f32, String)>;

/// A song's timed lines (seconds, text), in order; empty when none were found.
pub struct Found {
    pub key: Key,
    pub lines: Lines,
}

/// Finished lookups for the island, and every song looked up this run.
static DONE: Mutex<Vec<Found>> = Mutex::new(Vec::new());
static CACHE: Mutex<Option<HashMap<Key, Lines>>> = Mutex::new(None);
/// Songs still being looked up.
static ASKED: Mutex<Vec<Key>> = Mutex::new(Vec::new());

pub fn drain() -> Vec<Found> {
    DONE.lock().map(|mut d| std::mem::take(&mut *d)).unwrap_or_default()
}

/// The lines for `key` if they were already looked up; otherwise starts a
/// lookup (once) and returns `None`. `duration` (seconds, 0 if unknown) picks
/// the right recording when several match.
pub fn lookup(key: &Key, duration: f32) -> Option<Lines> {
    if let Some(lines) = CACHE.lock().ok()?.as_ref().and_then(|c| c.get(key)).cloned() {
        return Some(lines);
    }
    {
        let mut asked = ASKED.lock().ok()?;
        if asked.contains(key) {
            return None;
        }
        asked.push(key.clone());
    }
    let key = key.clone();
    std::thread::spawn(move || {
        let lines = match fetch(&key, duration) {
            Ok(lines) => lines,
            Err(e) => {
                log!("lyrics: {} – {}: {e}", key.1, key.0);
                Vec::new()
            }
        };
        if let Ok(mut c) = CACHE.lock() {
            c.get_or_insert_with(HashMap::new).insert(key.clone(), lines.clone());
        }
        if let Ok(mut a) = ASKED.lock() {
            a.retain(|k| *k != key);
        }
        if let Ok(mut d) = DONE.lock() {
            d.push(Found { key, lines });
        }
        super::holding::signal();
    });
    None
}

fn fetch((title, artist): &Key, duration: f32) -> Result<Lines, String> {
    let url = format!(
        "https://lrclib.net/api/search?track_name={}&artist_name={}",
        crate::net::encode(title),
        crate::net::encode(artist)
    );
    let body = crate::net::get(&url)?;
    let results: Vec<serde_json::Value> = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    Ok(pick(&results, duration).map(parse_lrc).unwrap_or_default())
}

/// The synced lyrics of the result closest in length to the song (within a
/// few seconds when the length is known).
fn pick(results: &[serde_json::Value], duration: f32) -> Option<&str> {
    results
        .iter()
        .filter_map(|r| Some((r.get("syncedLyrics")?.as_str()?, r.get("duration").and_then(|d| d.as_f64()).unwrap_or(0.0) as f32)))
        .filter(|(text, len)| !text.trim().is_empty() && (duration <= 0.0 || (len - duration).abs() <= 4.0))
        // (The first wins a tie, and when the length isn't known.)
        .min_by(|a, b| {
            let off = |len: f32| if duration > 0.0 { (len - duration).abs() } else { 0.0 };
            off(a.1).total_cmp(&off(b.1))
        })
        .map(|(text, _)| text)
}

/// LRC: `[mm:ss.xx] text` lines, a line may carry several times; tags like
/// `[ar:Artist]` are skipped.
pub fn parse_lrc(text: &str) -> Lines {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut rest = line.trim();
        let mut times = Vec::new();
        while let Some(tag) = rest.strip_prefix('[') {
            let Some((stamp, after)) = tag.split_once(']') else { break };
            match parse_time(stamp) {
                Some(t) => times.push(t),
                None => break,
            }
            rest = after;
        }
        let words = rest.trim().to_string();
        out.extend(times.into_iter().map(|t| (t, words.clone())));
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn parse_time(stamp: &str) -> Option<f32> {
    let (m, s) = stamp.split_once(':')?;
    let m: f32 = m.trim().parse().ok()?;
    let s: f32 = s.trim().parse().ok()?;
    (m >= 0.0 && (0.0..60.0).contains(&s)).then_some(m * 60.0 + s)
}

/// The line being sung at `pos` seconds (shown a moment early, as it's read
/// ahead of being sung): its index, or `None` before the first.
pub fn current(lines: &[(f32, String)], pos: f32) -> Option<usize> {
    const LEAD: f32 = 0.25;
    lines.iter().rposition(|(t, _)| *t <= pos + LEAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc() {
        let lines = parse_lrc("[ar:M83]\n[00:12.50]Waiting in a car\n[00:10.00][01:00.00]Hey\n[00:20.00]\nnot a line");
        assert_eq!(
            lines,
            vec![(10.0, "Hey".into()), (12.5, "Waiting in a car".into()), (20.0, String::new()), (60.0, "Hey".into())]
        );
        assert_eq!(current(&lines, 5.0), None);
        assert_eq!(current(&lines, 12.3), Some(1));
        assert_eq!(current(&lines, 30.0), Some(2));
    }

    #[test]
    fn picks_matching_length() {
        let results: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"duration": 300, "syncedLyrics": "[00:01.00]long"},
                {"duration": 243, "syncedLyrics": null, "plainLyrics": "x"},
                {"duration": 244, "syncedLyrics": "[00:01.00]right"}]"#,
        )
        .unwrap();
        assert_eq!(pick(&results, 243.0), Some("[00:01.00]right"));
        assert_eq!(pick(&results, 100.0), None);
    }
}
