//! Signing in through the browser: OAuth 2.0's authorization code flow with
//! PKCE and a loopback redirect (RFC 8252), the way desktop apps are meant to.
//!
//! DockSide listens on a random port of 127.0.0.1, opens the provider's sign-in
//! page in the default browser, and waits for the browser to come back with a
//! code, which it trades for tokens.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long the browser has to come back.
const WAIT: Duration = Duration::from_secs(300);

/// One provider's OAuth endpoints and app registration.
pub struct Client<'a> {
    pub authorize: &'a str,
    pub token: &'a str,
    pub client_id: &'a str,
    /// Google's "Desktop app" clients have one (it isn't secret); Microsoft's public clients don't.
    pub client_secret: Option<&'a str>,
    pub scope: &'a str,
    /// "http://127.0.0.1" or "http://localhost": the port is added.
    pub redirect_host: &'a str,
    /// Extra parameters for the sign-in page.
    pub extra: &'a [(&'a str, &'a str)],
}

pub struct Tokens {
    pub access: String,
    pub refresh: String,
    /// Unix seconds.
    pub expires_at: i64,
}

/// Runs the whole sign-in: browser, redirect, code exchange.
pub fn sign_in(c: &Client) -> Result<Tokens, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("couldn't listen for the sign-in: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("{}:{port}", c.redirect_host);
    let verifier = base64url(&random(48));
    let challenge = base64url(&sha256(verifier.as_bytes()));
    let state = base64url(&random(16));
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        c.authorize,
        enc(c.client_id),
        enc(&redirect),
        enc(c.scope),
        state,
        challenge
    );
    for (k, v) in c.extra {
        url.push_str(&format!("&{k}={}", enc(v)));
    }
    crate::win::system::open(&url);

    let code = wait_for_code(&listener, &state)?;
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect),
        ("code_verifier", verifier),
    ];
    if let Some(s) = c.client_secret {
        form.push(("client_secret", s.to_string()));
    }
    token_request(c, form)
}

/// A new access token from a refresh token (some providers also send a new refresh token).
pub fn refresh(c: &Client, refresh_token: &str) -> Result<Tokens, String> {
    let mut form = vec![("grant_type", "refresh_token".to_string()), ("refresh_token", refresh_token.to_string())];
    if let Some(s) = c.client_secret {
        form.push(("client_secret", s.to_string()));
    }
    // Microsoft wants the scope again.
    form.push(("scope", c.scope.to_string()));
    let mut t = token_request(c, form)?;
    if t.refresh.is_empty() {
        t.refresh = refresh_token.to_string();
    }
    Ok(t)
}

fn token_request(c: &Client, fields: Vec<(&str, String)>) -> Result<Tokens, String> {
    let mut body = format!("client_id={}", enc(c.client_id));
    for (k, v) in fields {
        body.push_str(&format!("&{k}={}", enc(&v)));
    }
    let headers = [("Content-Type", "application/x-www-form-urlencoded"), ("Accept", "application/json")];
    let reply = crate::net::request("POST", c.token, &headers, Some(body.as_bytes())).map_err(|e| {
        if e.starts_with("HTTP 4") {
            format!("the sign-in was refused ({e}); check the client ID and secret")
        } else {
            e
        }
    })?;
    let v: Value = serde_json::from_slice(&reply).map_err(|e| e.to_string())?;
    let access = v.get("access_token").and_then(Value::as_str).unwrap_or_default().to_string();
    if access.is_empty() {
        return Err("no access token came back".into());
    }
    Ok(Tokens {
        access,
        refresh: v.get("refresh_token").and_then(Value::as_str).unwrap_or_default().to_string(),
        expires_at: super::now() + v.get("expires_in").and_then(Value::as_i64).unwrap_or(3600) - 60,
    })
}

/// Waits for the browser's redirect and answers it with a page saying it's done.
fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let until = Instant::now() + WAIT;
    while Instant::now() < until {
        if super::cancelled() {
            return Err("cancelled".into());
        }
        let (mut stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(150));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        // "GET /?code=...&state=... HTTP/1.1"
        let Some(target) = head.lines().next().and_then(|l| l.split_whitespace().nth(1)) else { continue };
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let param = |name: &str| {
            query.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| decode(v)))
        };
        // Browsers also ask for /favicon.ico.
        if param("code").is_none() && param("error").is_none() {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        let result = match (param("code"), param("error"), param("state")) {
            (_, Some(err), _) => Err(match err.as_str() {
                "access_denied" => "you declined the sign-in".to_string(),
                _ => format!("the sign-in failed: {}", param("error_description").unwrap_or(err)),
            }),
            (Some(_), _, s) if s.as_deref() != Some(state) => Err("the sign-in came back for a different request".into()),
            (Some(code), _, _) => Ok(code),
            _ => Err("the sign-in came back without a code".into()),
        };
        let (title, line) = match &result {
            Ok(_) => ("You're signed in", "You can close this tab and go back to DockSide."),
            Err(_) => ("Sign-in didn't finish", "Go back to DockSide to try again."),
        };
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>DockSide</title>\
             <body style=\"font-family:-apple-system,'Segoe UI',sans-serif;display:grid;place-items:center;height:90vh;color:#222\">\
             <div style=\"text-align:center\"><h2>{title}</h2><p style=\"color:#666\">{line}</p></div>"
        );
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.write_all(reply.as_bytes());
        return result;
    }
    Err("the browser didn't come back in time".into())
}

fn enc(s: &str) -> String {
    crate::net::encode(s)
}

/// Percent-decoding (with '+' as a space) for the redirect's query.
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Small crypto helpers
// ---------------------------------------------------------------------------

pub fn random(n: usize) -> Vec<u8> {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut buf = vec![0u8; n];
    let status = unsafe { BCryptGenRandom(BCRYPT_ALG_HANDLE::default(), &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status.is_err() {
        // Not expected; fall back to something still unpredictable enough for a one-off state.
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let mut x = seed as u64 ^ (std::process::id() as u64) << 32;
        for b in &mut buf {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
    }
    buf
}

pub fn base64url(data: &[u8]) -> String {
    base64(data, b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_", false)
}

pub fn base64_std(data: &[u8]) -> String {
    base64(data, b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/", true)
}

fn base64(data: &[u8], abc: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(data.len() * 4 / 3 + 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        let chars = chunk.len() + 1;
        for i in 0..4 {
            if i < chars {
                out.push(abc[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// SHA-256 (for the PKCE code challenge).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be,
        0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa,
        0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85,
        0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
        0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f,
        0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known() {
        let hex: String = sha256(b"abc").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn pkce_rfc7636_example() {
        // RFC 7636 appendix B.
        let challenge = base64url(&sha256(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn base64_padding() {
        assert_eq!(base64_std(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_std(b"ab"), "YWI=");
        assert_eq!(base64url(b"ab"), "YWI");
    }

    #[test]
    fn decodes_query() {
        assert_eq!(decode("4%2F0Ab_x+y"), "4/0Ab_x y");
        assert_eq!(decode("100%"), "100%");
    }
}
