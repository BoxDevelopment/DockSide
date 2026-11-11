//! CalDAV (RFC 4791): iCloud, Fastmail, Nextcloud, Yahoo and the rest.
//!
//! Discovery is the standard walk: the server's current-user-principal, its
//! calendar-home-set, then the calendars in that home. Events come back as
//! iCalendar text from a calendar-query REPORT, read by `island::ics`.

use super::{oauth::base64_std, palette, parse_hex, Calendar, Event, Synced};

pub const ICLOUD: &str = "https://caldav.icloud.com/";

pub struct Collection {
    pub url: String,
    pub name: String,
    pub color: [u8; 3],
}

fn dav(method: &str, url: &str, user: &str, pass: &str, depth: &str, body: &str) -> Result<Node, String> {
    let auth = format!("Basic {}", base64_std(format!("{user}:{pass}").as_bytes()));
    let headers = [("Authorization", auth.as_str()), ("Depth", depth), ("Content-Type", "application/xml; charset=utf-8")];
    let reply = crate::net::request(method, url, &headers, Some(body.as_bytes()))?;
    Ok(parse(&String::from_utf8_lossy(&reply)))
}

/// `href` made absolute against `base`.
fn absolute(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let origin_end = base.find("://").map(|i| i + 3).and_then(|i| base[i..].find('/').map(|j| i + j)).unwrap_or(base.len());
    format!("{}{}", &base[..origin_end], if href.starts_with('/') { href.to_string() } else { format!("/{href}") })
}

/// The first `href` inside the first `<name>` of the reply.
fn href_in(root: &Node, name: &str) -> Option<String> {
    root.find(name).and_then(|n| n.find("href")).map(|h| h.text.trim().to_string()).filter(|h| !h.is_empty())
}

/// Every calendar (of events) the account has.
pub fn discover(server: &str, user: &str, pass: &str) -> Result<Vec<Collection>, String> {
    if !server.starts_with("https://") {
        return Err("the server address must start with https://".into());
    }
    let base = if server.ends_with('/') { server.to_string() } else { format!("{server}/") };
    const PRINCIPAL: &str = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
    // Servers that keep CalDAV elsewhere say where at /.well-known/caldav.
    let principal = match dav("PROPFIND", &base, user, pass, "0", PRINCIPAL).ok().and_then(|r| href_in(&r, "current-user-principal")) {
        Some(p) => p,
        None => {
            let well_known = absolute(&base, "/.well-known/caldav");
            let r = dav("PROPFIND", &well_known, user, pass, "0", PRINCIPAL)?;
            href_in(&r, "current-user-principal").ok_or("the server didn't say where your calendars are")?
        }
    };
    let principal = absolute(&base, &principal);
    const HOME: &str =
        r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/></d:prop></d:propfind>"#;
    let r = dav("PROPFIND", &principal, user, pass, "0", HOME)?;
    let home = absolute(&principal, &href_in(&r, "calendar-home-set").ok_or("the server has no calendar home for you")?);
    const LIST: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="http://apple.com/ns/ical/">
<d:prop><d:displayname/><d:resourcetype/><a:calendar-color/><c:supported-calendar-component-set/></d:prop></d:propfind>"#;
    let r = dav("PROPFIND", &home, user, pass, "1", LIST)?;
    let mut out = Vec::new();
    for resp in r.all("response") {
        let Some(href) = resp.find("href").map(|h| h.text.trim().to_string()) else { continue };
        let Some(rt) = resp.find("resourcetype") else { continue };
        if rt.find("calendar").is_none() {
            continue;
        }
        // Reminders lists are calendars of VTODOs; leave them out.
        if let Some(set) = resp.find("supported-calendar-component-set") {
            let comps: Vec<&str> = set.all("comp").iter().filter_map(|c| c.attr("name")).collect();
            if !comps.is_empty() && !comps.iter().any(|c| c.eq_ignore_ascii_case("VEVENT")) {
                continue;
            }
        }
        let name = resp.find("displayname").map(|n| n.text.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Calendar".into());
        let color = resp.find("calendar-color").and_then(|c| parse_hex(&c.text)).unwrap_or_else(|| palette(&name));
        out.push(Collection { url: absolute(&home, &href), name, color });
    }
    Ok(out)
}

pub fn events(server: &str, user: &str, pass: &str, account: u64, from: i64, to: i64) -> Result<Synced, String> {
    let cals = discover(server, user, pass)?;
    let stamp = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y%m%dT%H%M%SZ").to_string()).unwrap_or_default();
    let query = format!(
        r#"<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-data/></d:prop>
<c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:time-range start="{}" end="{}"/></c:comp-filter></c:comp-filter></c:filter>
</c:calendar-query>"#,
        stamp(from),
        stamp(to)
    );
    let mut out = Vec::new();
    let mut calendars = Vec::new();
    for c in cals {
        calendars.push(Calendar { name: c.name.clone(), color: c.color });
        let r = match dav("REPORT", &c.url, user, pass, "1", &query) {
            Ok(r) => r,
            Err(e) => {
                log!("calendar: one CalDAV calendar failed: {e}");
                continue;
            }
        };
        for data in r.all("calendar-data") {
            for (e, all_day) in crate::island::ics::occurrences(&data.text, from, to) {
                out.push(Event::from_cal(e, all_day, &c.name, c.color, account));
            }
        }
    }
    Ok((out, calendars))
}

// ---------------------------------------------------------------------------
// A small XML reader: enough for WebDAV multistatus replies.
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
pub struct Node {
    /// Local name, lowercase (namespaces dropped).
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub kids: Vec<Node>,
}

impl Node {
    /// The first descendant called `name`.
    pub fn find(&self, name: &str) -> Option<&Node> {
        self.kids.iter().find_map(|k| if k.name == name { Some(k) } else { k.find(name) })
    }

    /// Every descendant called `name` (not looking inside matches).
    pub fn all(&self, name: &str) -> Vec<&Node> {
        let mut out = Vec::new();
        for k in &self.kids {
            if k.name == name {
                out.push(k);
            } else {
                out.extend(k.all(name));
            }
        }
        out
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase()
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e < 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parses into a tree under a nameless root. Tolerant: never fails.
pub fn parse(xml: &str) -> Node {
    let mut stack: Vec<Node> = vec![Node::default()];
    let mut rest = xml;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&unescape(rest));
            }
            break;
        };
        if lt > 0 {
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&unescape(&rest[..lt]));
            }
        }
        rest = &rest[lt..];
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").unwrap_or(r.len());
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&r[..end]);
            }
            rest = r.get(end + 3..).unwrap_or("");
            continue;
        }
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |e| &rest[e + 3..]);
            continue;
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = rest.find('>').map_or("", |e| &rest[e + 1..]);
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            let name = local(name.trim());
            // Close up to the matching element (tolerating sloppy nesting).
            if stack.iter().skip(1).any(|n| n.name == name) {
                while stack.len() > 1 {
                    let n = stack.pop().unwrap_or_default();
                    let done = n.name == name;
                    if let Some(parent) = stack.last_mut() {
                        parent.kids.push(n);
                    }
                    if done {
                        break;
                    }
                }
            }
            continue;
        }
        let self_closing = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, attr_text) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        let node = Node { name: local(name), attrs: attrs(attr_text), ..Default::default() };
        if self_closing {
            if let Some(top) = stack.last_mut() {
                top.kids.push(node);
            }
        } else {
            stack.push(node);
        }
    }
    while stack.len() > 1 {
        let n = stack.pop().unwrap_or_default();
        if let Some(parent) = stack.last_mut() {
            parent.kids.push(n);
        }
    }
    stack.pop().unwrap_or_default()
}

fn attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while let Some(eq) = rest.find('=') {
        let key = local(rest[..eq].trim());
        let after = rest[eq + 1..].trim_start();
        let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
        let body = &after[1..];
        let end = body.find(q).unwrap_or(body.len());
        out.push((key, unescape(&body[..end])));
        rest = body.get(end + 1..).unwrap_or("").trim_start();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multistatus() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
 <d:response><d:href>/123/calendars/home/</d:href>
  <d:propstat><d:prop><d:displayname>Home &amp; Family</d:displayname>
   <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
   <x:calendar-color xmlns:x="http://apple.com/ns/ical/">#FF2968FF</x:calendar-color>
   <cal:supported-calendar-component-set><cal:comp name="VEVENT"/></cal:supported-calendar-component-set>
  </d:prop></d:propstat></d:response>
 <d:response><d:href>/123/calendars/tasks/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
   <cal:supported-calendar-component-set><cal:comp name='VTODO'/></cal:supported-calendar-component-set></d:prop></d:propstat></d:response>
 <d:response><d:href>/123/calendars/data.ics</d:href><d:propstat><d:prop>
  <cal:calendar-data><![CDATA[BEGIN:VCALENDAR
END:VCALENDAR]]></cal:calendar-data></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        let root = parse(xml);
        let rs = root.all("response");
        assert_eq!(rs.len(), 3);
        assert_eq!(rs[0].find("displayname").unwrap().text, "Home & Family");
        assert_eq!(parse_hex(&rs[0].find("calendar-color").unwrap().text), Some([255, 41, 104]));
        assert_eq!(rs[1].all("comp")[0].attr("name"), Some("VTODO"));
        assert!(rs[2].find("calendar-data").unwrap().text.starts_with("BEGIN:VCALENDAR"));
    }

    #[test]
    fn absolute_hrefs() {
        assert_eq!(absolute("https://caldav.icloud.com/", "/123/principal/"), "https://caldav.icloud.com/123/principal/");
        assert_eq!(absolute("https://a.example/x/", "https://p01.example/y/"), "https://p01.example/y/");
    }
}
