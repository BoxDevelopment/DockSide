//! A small iCalendar (.ics) reader: enough of RFC 5545 for subscribed
//! calendars (Google, Outlook.com, iCloud).
//!
//! Supports folded lines, timed events (UTC, floating or TZID; TZID times are
//! read as local time, which is right when the calendar uses your own zone),
//! DURATION, recurring events (DAILY / WEEKLY / MONTHLY / YEARLY with INTERVAL,
//! COUNT, UNTIL and BYDAY), EXDATE, moved occurrences (RECURRENCE-ID) and
//! cancellations. All-day events are skipped (the island shows meetings).

use std::collections::HashSet;

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc, Weekday};

use super::calendar::CalEvent;

/// Safety cap on generated occurrences per series.
const MAX_STEPS: usize = 20_000;

struct Prop {
    name: String,
    params: String,
    value: String,
}

#[derive(Default)]
struct VEvent {
    uid: String,
    summary: String,
    location: String,
    start: Option<(NaiveDateTime, bool)>,
    end: Option<(NaiveDateTime, bool)>,
    duration: Option<i64>,
    all_day: bool,
    rrule: Option<String>,
    exdates: Vec<i64>,
    recurrence_id: Option<i64>,
    cancelled: bool,
}

/// Events overlapping `[from, to)` (Unix seconds), sorted by start.
pub fn events_between(text: &str, from: i64, to: i64) -> Vec<CalEvent> {
    let events = parse(text);
    // Moved / cancelled single occurrences replace the series' original slot.
    let overridden: HashSet<(String, i64)> =
        events.iter().filter_map(|e| e.recurrence_id.map(|r| (e.uid.clone(), r))).collect();
    let mut out = Vec::new();
    for e in &events {
        if e.cancelled || e.all_day {
            continue;
        }
        let Some((start, utc)) = e.start else { continue };
        let length = match (e.end, e.duration) {
            (Some((end, end_utc)), _) => to_unix(end, end_utc) - to_unix(start, utc),
            (None, Some(d)) => d,
            _ => 0,
        }
        .max(0);
        let title = e.summary.trim();
        if title.is_empty() {
            continue;
        }
        let mut push = |s: i64| {
            if s < to && s + length.max(1) > from {
                out.push(CalEvent { title: title.to_string(), location: e.location.clone(), start: s, end: s + length });
            }
        };
        match (&e.rrule, e.recurrence_id) {
            (Some(rule), None) => {
                // Expand in local wall-clock time so a 9:00 meeting stays at 9:00 across DST.
                let local = if utc { Utc.from_utc_datetime(&start).with_timezone(&Local).naive_local() } else { start };
                for occ in expand(local, rule, to) {
                    let s = to_unix(occ, false);
                    if !e.exdates.contains(&s) && !overridden.contains(&(e.uid.clone(), s)) {
                        push(s);
                    }
                }
            }
            _ => push(to_unix(start, utc)),
        }
    }
    out.sort_by_key(|e| e.start);
    out.dedup_by(|a, b| a.start == b.start && a.title == b.title);
    out
}

fn parse(text: &str) -> Vec<VEvent> {
    let mut events = Vec::new();
    let mut cur: Option<VEvent> = None;
    // Nested components (VALARM) inside an event are skipped.
    let mut depth = 0;
    for line in unfold(text) {
        let Some(p) = prop(&line) else { continue };
        match (p.name.as_str(), p.value.to_ascii_uppercase().as_str()) {
            ("BEGIN", "VEVENT") => {
                cur = Some(VEvent::default());
                depth = 0;
                continue;
            }
            ("END", "VEVENT") => {
                events.extend(cur.take());
                continue;
            }
            ("BEGIN", _) if cur.is_some() => depth += 1,
            ("END", _) if cur.is_some() => depth -= 1,
            _ => {}
        }
        let Some(e) = cur.as_mut().filter(|_| depth == 0) else { continue };
        match p.name.as_str() {
            "UID" => e.uid = p.value,
            "SUMMARY" => e.summary = unescape(&p.value),
            "LOCATION" => e.location = unescape(&p.value),
            "DTSTART" => {
                e.all_day = is_date(&p);
                e.start = datetime(&p.value);
            }
            "DTEND" => e.end = datetime(&p.value),
            "DURATION" => e.duration = duration(&p.value),
            "RRULE" => e.rrule = Some(p.value),
            "EXDATE" => {
                for v in p.value.split(',') {
                    if let Some((dt, utc)) = datetime(v) {
                        e.exdates.push(to_unix(dt, utc));
                    }
                }
            }
            "RECURRENCE-ID" => e.recurrence_id = datetime(&p.value).map(|(dt, utc)| to_unix(dt, utc)),
            "STATUS" => e.cancelled = p.value.eq_ignore_ascii_case("CANCELLED"),
            _ => {}
        }
    }
    events
}

/// Joins folded lines (continuations start with a space or tab).
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match (raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')), lines.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => lines.push(raw.to_string()),
        }
    }
    lines
}

/// "NAME;PARAM=x;PARAM2=\"a:b\":value"
fn prop(line: &str) -> Option<Prop> {
    let mut quoted = false;
    let colon = line.char_indices().find(|&(_, c)| {
        if c == '"' {
            quoted = !quoted;
        }
        c == ':' && !quoted
    })?;
    let (head, value) = (&line[..colon.0], &line[colon.0 + 1..]);
    let (name, params) = head.split_once(';').unwrap_or((head, ""));
    Some(Prop { name: name.trim().to_ascii_uppercase(), params: params.to_ascii_uppercase(), value: value.trim().to_string() })
}

fn is_date(p: &Prop) -> bool {
    p.params.contains("VALUE=DATE") && !p.params.contains("VALUE=DATE-TIME") || (p.value.len() == 8 && !p.value.contains('T'))
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push(' '),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// "20261019T090000Z" (UTC), "20261019T090000" (local) or "20261019" (date). Returns (time, is_utc).
fn datetime(v: &str) -> Option<(NaiveDateTime, bool)> {
    let v = v.trim();
    let utc = v.ends_with('Z');
    let v = v.trim_end_matches('Z');
    if let Ok(dt) = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S") {
        return Some((dt, utc));
    }
    NaiveDate::parse_from_str(v, "%Y%m%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)).map(|dt| (dt, false))
}

fn to_unix(dt: NaiveDateTime, utc: bool) -> i64 {
    if utc {
        dt.and_utc().timestamp()
    } else {
        Local.from_local_datetime(&dt).earliest().map(|t| t.timestamp()).unwrap_or_else(|| dt.and_utc().timestamp())
    }
}

/// "PT1H30M", "P1D", "-PT15M" → seconds.
fn duration(v: &str) -> Option<i64> {
    let (sign, v) = match v.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, v.strip_prefix('+').unwrap_or(v)),
    };
    let v = v.strip_prefix('P')?;
    let (mut total, mut num) = (0i64, String::new());
    for c in v.chars() {
        match c {
            '0'..='9' => num.push(c),
            'T' => {}
            'W' | 'D' | 'H' | 'M' | 'S' => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += n * match c {
                    'W' => 604_800,
                    'D' => 86_400,
                    'H' => 3_600,
                    'M' => 60,
                    _ => 1,
                };
            }
            _ => return None,
        }
    }
    Some(sign * total)
}

fn weekday(code: &str) -> Option<Weekday> {
    Some(match code {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

/// The `n`th (1-based, negative from the end) `wd` of a month.
fn nth_weekday(year: i32, month: u32, wd: Weekday, n: i32) -> Option<NaiveDate> {
    if n > 0 {
        let first = NaiveDate::from_ymd_opt(year, month, 1)?;
        let offset = (7 + wd.num_days_from_monday() as i64 - first.weekday().num_days_from_monday() as i64) % 7;
        let d = first + Duration::days(offset + 7 * (n as i64 - 1));
        (d.month() == month).then_some(d)
    } else {
        let next = if month == 12 { NaiveDate::from_ymd_opt(year + 1, 1, 1)? } else { NaiveDate::from_ymd_opt(year, month + 1, 1)? };
        let last = next - Duration::days(1);
        let offset = (7 + last.weekday().num_days_from_monday() as i64 - wd.num_days_from_monday() as i64) % 7;
        let d = last - Duration::days(offset + 7 * (-n as i64 - 1));
        (d.month() == month).then_some(d)
    }
}

fn add_months(year: i32, month: u32, add: i64) -> (i32, u32) {
    let m0 = year as i64 * 12 + (month as i64 - 1) + add;
    ((m0.div_euclid(12)) as i32, (m0.rem_euclid(12) + 1) as u32)
}

/// Occurrence start times (local wall clock) of a recurring event, in order,
/// until they pass `until_unix`.
fn expand(start: NaiveDateTime, rule: &str, until_unix: i64) -> Vec<NaiveDateTime> {
    let mut freq = "";
    let mut interval = 1i64;
    let mut count: Option<usize> = None;
    let mut until: Option<i64> = None;
    let mut byday: Vec<(i32, Weekday)> = Vec::new();
    for part in rule.split(';') {
        let Some((k, v)) = part.split_once('=') else { continue };
        match k.to_ascii_uppercase().as_str() {
            "FREQ" => freq = if v.eq_ignore_ascii_case("DAILY") {
                "D"
            } else if v.eq_ignore_ascii_case("WEEKLY") {
                "W"
            } else if v.eq_ignore_ascii_case("MONTHLY") {
                "M"
            } else if v.eq_ignore_ascii_case("YEARLY") {
                "Y"
            } else {
                ""
            },
            "INTERVAL" => interval = v.parse().unwrap_or(1).max(1),
            "COUNT" => count = v.parse().ok(),
            "UNTIL" => until = datetime(v).map(|(dt, utc)| to_unix(dt, utc)),
            "BYDAY" => {
                for d in v.split(',') {
                    let d = d.trim().to_ascii_uppercase();
                    let (n, code) = d.split_at(d.len().saturating_sub(2));
                    if let Some(wd) = weekday(code) {
                        byday.push((n.parse().unwrap_or(0), wd));
                    }
                }
            }
            _ => {}
        }
    }
    let time = start.time();
    let mut out = Vec::new();
    let mut emitted = 0usize;
    let mut accept = |dt: NaiveDateTime, out: &mut Vec<NaiveDateTime>| -> bool {
        if dt < start {
            return true;
        }
        let unix = to_unix(dt, false);
        if until.is_some_and(|u| unix > u) || count.is_some_and(|c| emitted >= c) || unix > until_unix {
            return false;
        }
        emitted += 1;
        out.push(dt);
        true
    };
    let date = start.date();
    match freq {
        "D" => {
            for step in 0..MAX_STEPS as i64 {
                let d = date + Duration::days(step * interval);
                if !byday.is_empty() && !byday.iter().any(|(_, wd)| *wd == d.weekday()) {
                    continue;
                }
                if !accept(d.and_time(time), &mut out) {
                    break;
                }
            }
        }
        "W" => {
            let mut days: Vec<Weekday> = byday.iter().map(|(_, wd)| *wd).collect();
            if days.is_empty() {
                days.push(date.weekday());
            }
            days.sort_by_key(|d| d.num_days_from_monday());
            let monday = date - Duration::days(date.weekday().num_days_from_monday() as i64);
            'weeks: for step in 0..MAX_STEPS as i64 {
                let week = monday + Duration::weeks(step * interval);
                for wd in &days {
                    let d = week + Duration::days(wd.num_days_from_monday() as i64);
                    if !accept(d.and_time(time), &mut out) {
                        break 'weeks;
                    }
                }
            }
        }
        "M" => {
            'months: for step in 0..MAX_STEPS as i64 {
                let (y, m) = add_months(date.year(), date.month(), step * interval);
                let mut days: Vec<NaiveDate> = if byday.iter().any(|(n, _)| *n != 0) {
                    byday.iter().filter_map(|(n, wd)| nth_weekday(y, m, *wd, *n)).collect()
                } else {
                    NaiveDate::from_ymd_opt(y, m, date.day()).into_iter().collect()
                };
                days.sort();
                for d in days {
                    if !accept(d.and_time(time), &mut out) {
                        break 'months;
                    }
                }
            }
        }
        "Y" => {
            for step in 0..(MAX_STEPS as i64 / 50) {
                let Some(d) = NaiveDate::from_ymd_opt(date.year() + (step * interval) as i32, date.month(), date.day()) else {
                    continue;
                };
                if !accept(d.and_time(time), &mut out) {
                    break;
                }
            }
        }
        _ => {
            accept(start, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(s: &str) -> i64 {
        to_unix(NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S").unwrap(), false)
    }

    const CAL: &str = "BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VEVENT\r
UID:one\r
SUMMARY:Design review\\, round 2\r
LOCATION:Room 4\r
DTSTART:20261020T140000Z\r
DTEND:20261020T150000Z\r
BEGIN:VALARM\r
TRIGGER:-PT10M\r
SUMMARY:alarm text\r
END:VALARM\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:standup\r
SUMMARY:Stand\r
 up\r
DTSTART;TZID=Europe/London:20261005T093000\r
DURATION:PT15M\r
RRULE:FREQ=WEEKLY;BYDAY=MO,WE,FR\r
EXDATE;TZID=Europe/London:20261021T093000\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:standup\r
RECURRENCE-ID;TZID=Europe/London:20261023T093000\r
SUMMARY:Standup (moved)\r
DTSTART;TZID=Europe/London:20261023T110000\r
DTEND;TZID=Europe/London:20261023T111500\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:holiday\r
SUMMARY:Holiday\r
DTSTART;VALUE=DATE:20261020\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:gone\r
SUMMARY:Cancelled thing\r
STATUS:CANCELLED\r
DTSTART:20261020T160000Z\r
END:VEVENT\r
END:VCALENDAR\r
";

    #[test]
    fn single_events_and_skips() {
        let from = Utc.with_ymd_and_hms(2026, 10, 20, 0, 0, 0).unwrap().timestamp();
        let ev = events_between(CAL, from, from + 86_400);
        let titles: Vec<&str> = ev.iter().map(|e| e.title.as_str()).collect();
        assert!(titles.contains(&"Design review, round 2"), "{titles:?}");
        assert!(!titles.contains(&"Holiday"));
        assert!(!titles.contains(&"Cancelled thing"));
        assert!(!titles.contains(&"alarm text"));
        let review = ev.iter().find(|e| e.title.starts_with("Design")).unwrap();
        assert_eq!(review.end - review.start, 3600);
        assert_eq!(review.location, "Room 4");
    }

    #[test]
    fn weekly_recurrence_exdate_and_override() {
        let from = local("20261019T000000");
        let ev = events_between(CAL, from, from + 7 * 86_400);
        let standups: Vec<i64> = ev.iter().filter(|e| e.title == "Standup").map(|e| e.start).collect();
        // Mon 19th; Wed 21st is excluded; Fri 23rd moved to 11:00.
        assert_eq!(standups, vec![local("20261019T093000")]);
        let moved = ev.iter().find(|e| e.title == "Standup (moved)").unwrap();
        assert_eq!(moved.start, local("20261023T110000"));
        assert_eq!(ev.iter().find(|e| e.title == "Standup").unwrap().end - local("20261019T093000"), 900);
    }

    #[test]
    fn rule_variants() {
        let start = NaiveDateTime::parse_from_str("20260105T100000", "%Y%m%dT%H%M%S").unwrap();
        let far = local("20280101T000000");
        assert_eq!(expand(start, "FREQ=DAILY;COUNT=3", far).len(), 3);
        assert_eq!(expand(start, "FREQ=DAILY;INTERVAL=2;COUNT=3", far)[2].day(), 9);
        let until = expand(start, "FREQ=WEEKLY;UNTIL=20260120T000000Z", far);
        assert_eq!(until.len(), 3);
        // Second Tuesday of each month.
        let m = expand(start, "FREQ=MONTHLY;BYDAY=2TU;COUNT=2", far);
        assert_eq!((m[0].day(), m[1].day()), (13, 10));
        // Last Friday.
        let last = expand(start, "FREQ=MONTHLY;BYDAY=-1FR;COUNT=1", far);
        assert_eq!(last[0].day(), 30);
        assert_eq!(expand(start, "FREQ=YEARLY;COUNT=2", far)[1].year(), 2027);
    }

    #[test]
    fn durations() {
        assert_eq!(duration("PT1H30M"), Some(5400));
        assert_eq!(duration("P1D"), Some(86_400));
        assert_eq!(duration("-PT15M"), Some(-900));
        assert_eq!(duration("bogus"), None);
    }
}
