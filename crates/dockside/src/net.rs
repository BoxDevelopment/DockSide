//! Blocking HTTPS requests through WinHTTP, for background threads. (WinRT's
//! HttpClient sits on WinINet, which fails the TLS handshake with some
//! servers, Open-Meteo's among them.)

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

/// GETs `url`, returning the body of a 200 response.
pub fn get(url: &str) -> Result<Vec<u8>, String> {
    request("GET", url, &[], None)
}

/// Sends a request with extra `headers` and an optional `body`, returning the
/// body of a 2xx response.
pub fn request(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<Vec<u8>, String> {
    match request_with(method, url, headers, body, 15_000)? {
        (status, body) if (200..300).contains(&status) => Ok(body),
        (status, _) => Err(format!("HTTP {status}")),
    }
}

/// [`request`] that waits up to `receive_ms` for the reply (an AI answer can
/// take a while) and returns the status with the body, whatever the status
/// (an API's error body says what went wrong).
pub fn request_with(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>, receive_ms: i32) -> Result<(u32, Vec<u8>), String> {
    let (secure, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(r), _) => (true, r),
        (None, Some(r)) => (false, r),
        _ => return Err("not a web link".into()),
    };
    let (authority, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    // host:port, or [v6]:port
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.contains(']') && p.parse::<u16>().is_ok() => (h, p.parse::<u16>().unwrap_or(0)),
        _ => (authority, if secure { INTERNET_DEFAULT_HTTPS_PORT } else { INTERNET_DEFAULT_HTTP_PORT }),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    // Unencrypted only to this PC itself (a local model): never send a key over a network in the clear.
    if !secure && !matches!(host.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1") {
        return Err("plain http is only allowed to this PC (localhost)".into());
    }
    let fail = |what: &str| format!("{what}: {}", windows::core::Error::from_win32());
    unsafe {
        let agent = HSTRING::from(format!("DockSide/{}", env!("CARGO_PKG_VERSION")));
        let session = WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0);
        if session.is_null() {
            return Err(fail("open"));
        }
        let _ = WinHttpSetTimeouts(session, 10_000, 10_000, 15_000, receive_ms);
        let connect = WinHttpConnect(session, &HSTRING::from(host), port, 0);
        let request = if connect.is_null() {
            std::ptr::null_mut()
        } else {
            let flags = if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
            WinHttpOpenRequest(connect, &HSTRING::from(method), &HSTRING::from(path), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), flags)
        };
        let result = (|| {
            if request.is_null() {
                return Err(fail("connect"));
            }
            let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
            let extra: Vec<u16> = extra.encode_utf16().collect();
            let data = body.unwrap_or(&[]);
            WinHttpSendRequest(
                request,
                (!extra.is_empty()).then_some(extra.as_slice()),
                (!data.is_empty()).then_some(data.as_ptr() as *const std::ffi::c_void),
                data.len() as u32,
                data.len() as u32,
                0,
            )
            .map_err(|e| format!("send: {e}"))?;
            WinHttpReceiveResponse(request, std::ptr::null_mut()).map_err(|e| format!("receive: {e}"))?;
            let mut status = 0u32;
            let mut size = std::mem::size_of::<u32>() as u32;
            let _ = WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut std::ffi::c_void),
                &mut size,
                std::ptr::null_mut(),
            );
            let mut body = Vec::new();
            loop {
                let mut available = 0u32;
                WinHttpQueryDataAvailable(request, &mut available).map_err(|e| format!("read: {e}"))?;
                if available == 0 {
                    break;
                }
                let start = body.len();
                body.resize(start + available as usize, 0u8);
                let mut read = 0u32;
                WinHttpReadData(request, body[start..].as_mut_ptr() as *mut std::ffi::c_void, available, &mut read)
                    .map_err(|e| format!("read: {e}"))?;
                body.truncate(start + read as usize);
            }
            Ok((status, body))
        })();
        for h in [request, connect, session] {
            if !h.is_null() {
                let _ = WinHttpCloseHandle(h);
            }
        }
        result
    }
}

/// Percent-encodes a query value.
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
