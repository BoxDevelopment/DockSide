//! Google Calendar (API v3): every calendar shown in the user's calendar list.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::Value;

use super::{parse_hex, palette, Calendar, Event, Synced};

const API: &str = "https://www.googleapis.com/calendar/v3";

fn get(token: &str, url: &str) -> Result<Value, String> {
    let auth = format!("Bearer {token}");
    let body = crate::net::request("GET", url, &[("Authorization", &auth), ("Accept", "application/json")], None).map_err(|e| {
        if e == "HTTP 403" {
            "Google refused: turn on the Google Calendar API for your Cloud project".to_string()
        } else {
            e
        }
    })?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

fn list(token: &str) -> Result<Vec<Value>, String> {
    let v = get(token, &format!("{API}/users/me/calendarList?maxResults=250"))?;
    Ok(v.get("items").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// The signed-in address: the primary calendar's id.
pub fn account_name(token: &str) -> Result<String, String> {
    let items = list(token)?;
    Ok(items
        .iter()
        .find(|c| c.get("primary").and_then(Value::as_bool) == Some(true))
        .and_then(|c| c.get("id").and_then(Value::as_str))
        .unwrap_or("Google")
        .to_string())
}

pub fn events(token: &str, account: u64, from: i64, to: i64) -> Result<Synced, String> {
    let items = list(token)?;
    // Calendars hidden from the list (or unticked in Google Calendar) stay out.
    let any_selected = items.iter().any(|c| c.get("selected").is_some());
    let shown: Vec<&Value> = items
        .iter()
        .filter(|c| c.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter(|c| !any_selected || c.get("selected").and_then(Value::as_bool) == Some(true) || c.get("primary").and_then(Value::as_bool) == Some(true))
        .collect();
    let (min, max) = (rfc3339(from), rfc3339(to));
    let mut out = Vec::new();
    let mut calendars = Vec::new();
    for c in shown {
        let Some(id) = c.get("id").and_then(Value::as_str) else { continue };
        let name = c.get("summaryOverride").or_else(|| c.get("summary")).and_then(Value::as_str).unwrap_or(id).to_string();
        let color = c.get("backgroundColor").and_then(Value::as_str).and_then(parse_hex).unwrap_or_else(|| palette(&name));
        calendars.push(Calendar { name: name.clone(), color });
        let mut page: Option<String> = None;
        for _ in 0..20 {
            let mut url = format!(
                "{API}/calendars/{}/events?singleEvents=true&maxResults=2500&timeMin={}&timeMax={}\
                 &fields=nextPageToken,items(summary,location,start,end,status,htmlLink)",
                crate::net::encode(id),
                crate::net::encode(&min),
                crate::net::encode(&max)
            );
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", crate::net::encode(p)));
            }
            let v = get(token, &url)?;
            for e in v.get("items").and_then(Value::as_array).into_iter().flatten() {
                if e.get("status").and_then(Value::as_str) == Some("cancelled") {
                    continue;
                }
                let (Some((start, all_day)), Some((end, _))) = (when(e.get("start")), when(e.get("end"))) else { continue };
                let title = e.get("summary").and_then(Value::as_str).unwrap_or("(No title)").trim().to_string();
                out.push(Event {
                    title,
                    location: e.get("location").and_then(Value::as_str).unwrap_or_default().to_string(),
                    start,
                    end: end.max(start),
                    all_day,
                    calendar: name.clone(),
                    color,
                    link: e.get("htmlLink").and_then(Value::as_str).unwrap_or_default().to_string(),
                    account,
                });
            }
            page = v.get("nextPageToken").and_then(Value::as_str).map(str::to_string);
            if page.is_none() {
                break;
            }
        }
    }
    Ok((out, calendars))
}

fn rfc3339(t: i64) -> String {
    Utc.timestamp_opt(t, 0).single().map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default()
}

/// `{"dateTime": "2026-11-10T09:00:00-05:00"}` or `{"date": "2026-11-10"}` (all-day).
fn when(v: Option<&Value>) -> Option<(i64, bool)> {
    let v = v?;
    if let Some(dt) = v.get("dateTime").and_then(Value::as_str) {
        return DateTime::parse_from_rfc3339(dt).ok().map(|d| (d.timestamp(), false));
    }
    let d = NaiveDate::parse_from_str(v.get("date")?.as_str()?, "%Y-%m-%d").ok()?;
    Some((local_midnight(d), true))
}

pub fn local_midnight(d: NaiveDate) -> i64 {
    let dt = d.and_hms_opt(0, 0, 0).unwrap_or_default();
    chrono::Local.from_local_datetime(&dt).earliest().map(|t| t.timestamp()).unwrap_or_else(|| dt.and_utc().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let v: Value = serde_json::from_str(r#"{"dateTime": "2026-11-10T09:00:00Z"}"#).unwrap();
        assert_eq!(when(Some(&v)), Some((1_794_301_200, false)));
        let v: Value = serde_json::from_str(r#"{"date": "2026-11-10"}"#).unwrap();
        assert!(when(Some(&v)).unwrap().1);
    }
}
