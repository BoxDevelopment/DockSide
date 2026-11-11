//! Outlook.com, Hotmail and Microsoft 365 calendars through Microsoft Graph.

use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

use super::{palette, parse_hex, Calendar, Event, Synced};

const API: &str = "https://graph.microsoft.com/v1.0";

fn get(token: &str, url: &str) -> Result<Value, String> {
    let auth = format!("Bearer {token}");
    let headers = [("Authorization", auth.as_str()), ("Accept", "application/json"), ("Prefer", "outlook.timezone=\"UTC\"")];
    let body = crate::net::request("GET", url, &headers, None)?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

pub fn account_name(token: &str) -> Result<String, String> {
    let v = get(token, &format!("{API}/me?$select=mail,userPrincipalName"))?;
    Ok(["mail", "userPrincipalName"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("Outlook")
        .to_string())
}

pub fn events(token: &str, account: u64, from: i64, to: i64) -> Result<Synced, String> {
    let cals = get(token, &format!("{API}/me/calendars?$select=id,name,hexColor,color&$top=100"))?;
    let (start, end) = (iso(from), iso(to));
    let mut out = Vec::new();
    let mut calendars = Vec::new();
    for c in cals.get("value").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = c.get("id").and_then(Value::as_str) else { continue };
        let name = c.get("name").and_then(Value::as_str).unwrap_or("Calendar").to_string();
        let color = c
            .get("hexColor")
            .and_then(Value::as_str)
            .and_then(parse_hex)
            .or_else(|| c.get("color").and_then(Value::as_str).and_then(named_color))
            .unwrap_or_else(|| palette(&name));
        calendars.push(Calendar { name: name.clone(), color });
        let mut url = Some(format!(
            "{API}/me/calendars/{}/calendarView?startDateTime={start}&endDateTime={end}&$top=500\
             &$select=subject,location,start,end,isAllDay,isCancelled,webLink",
            crate::net::encode(id)
        ));
        for _ in 0..20 {
            let Some(u) = url.take() else { break };
            let v = get(token, &u)?;
            for e in v.get("value").and_then(Value::as_array).into_iter().flatten() {
                if e.get("isCancelled").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let all_day = e.get("isAllDay").and_then(Value::as_bool) == Some(true);
                let (Some(s), Some(en)) = (when(e.get("start"), all_day), when(e.get("end"), all_day)) else { continue };
                let title = e.get("subject").and_then(Value::as_str).unwrap_or("(No title)").trim().to_string();
                let location = e.get("location").and_then(|l| l.get("displayName")).and_then(Value::as_str).unwrap_or_default().to_string();
                out.push(Event {
                    title,
                    location,
                    start: s,
                    end: en.max(s),
                    all_day,
                    calendar: name.clone(),
                    color,
                    link: e.get("webLink").and_then(Value::as_str).unwrap_or_default().to_string(),
                    account,
                });
            }
            url = v.get("@odata.nextLink").and_then(Value::as_str).map(str::to_string);
        }
    }
    Ok((out, calendars))
}

fn iso(t: i64) -> String {
    Utc.timestamp_opt(t, 0).single().map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default()
}

/// `{"dateTime": "2026-11-10T14:00:00.0000000", "timeZone": "UTC"}`. All-day
/// events are dates: midnight to midnight wherever you are.
fn when(v: Option<&Value>, all_day: bool) -> Option<i64> {
    let s = v?.get("dateTime")?.as_str()?;
    if all_day {
        let d = NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()?;
        return Some(super::google::local_midnight(d));
    }
    let dt = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")).ok()?;
    Some(dt.and_utc().timestamp())
}

/// Outlook's named calendar colours.
fn named_color(name: &str) -> Option<[u8; 3]> {
    Some(match name {
        "lightBlue" => [0, 120, 212],
        "lightGreen" => [16, 124, 16],
        "lightOrange" => [202, 80, 16],
        "lightGray" => [105, 121, 126],
        "lightYellow" => [195, 156, 0],
        "lightTeal" => [3, 131, 135],
        "lightPink" => [227, 0, 140],
        "lightBrown" => [142, 86, 46],
        "lightRed" => [209, 52, 56],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_times() {
        let v: Value = serde_json::from_str(r#"{"dateTime": "2026-11-10T09:00:00.0000000", "timeZone": "UTC"}"#).unwrap();
        assert_eq!(when(Some(&v), false), Some(1_794_301_200));
    }
}
