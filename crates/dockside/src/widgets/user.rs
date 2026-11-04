//! Who's signed in, for the User widget: the account's name and picture as
//! Windows shows them (Microsoft account or local), this PC's name and uptime.

use std::sync::{Arc, Mutex};

use windows::core::{Interface, HSTRING};
use windows::Foundation::IReference;
use windows::System::{KnownUserProperties, User, UserPictureSize};

use crate::island::media::Art;
use crate::win::post;

#[derive(Clone, Default)]
pub struct UserInfo {
    /// "Ollie Smith" (falls back to the account name).
    pub name: String,
    /// "Ollie", for the greeting.
    pub first: String,
    pub picture: Option<Arc<Art>>,
    /// This PC's name.
    pub pc: String,
}

pub type SharedUser = Arc<Mutex<Option<UserInfo>>>;

/// The options a User widget can show.
pub const OPTIONS: [(&str, &str); 3] = [("greeting", "Greeting"), ("buttons", "Lock, Sign Out and Sleep"), ("device", "PC Name and Uptime")];

/// Does `item` show `option`? (All of them, until some are turned off.)
pub fn shows(item: &crate::config::WidgetItem, option: &str) -> bool {
    item.options.as_ref().is_none_or(|o| o.iter().any(|x| x.eq_ignore_ascii_case(option)))
}

/// Looks the account up off the UI thread (WinRT answers slowly), then posts `msg`.
pub fn load(shared: SharedUser, hwnd: isize, msg: u32) {
    let spawned = std::thread::Builder::new().name("user widget".into()).spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let info = read();
        if let Ok(mut s) = shared.lock() {
            *s = Some(info);
        }
        post(hwnd, msg);
    });
    if let Err(e) = spawned {
        log!("user widget thread failed: {e}");
    }
}

fn string_prop(user: &User, prop: windows::core::Result<HSTRING>) -> Option<String> {
    let v = user.GetPropertyAsync(&prop.ok()?).ok()?.get().ok()?;
    let s = v.cast::<IReference<HSTRING>>().ok()?.Value().ok()?.to_string();
    (!s.trim().is_empty()).then_some(s)
}

fn read() -> UserInfo {
    let user = User::FindAllAsync().ok().and_then(|op| op.get().ok()).and_then(|list| list.GetAt(0).ok());
    let account = account_name();
    let (mut name, mut first, mut picture) = (None, None, None);
    if let Some(u) = &user {
        first = string_prop(u, KnownUserProperties::FirstName());
        let last = string_prop(u, KnownUserProperties::LastName());
        name = string_prop(u, KnownUserProperties::DisplayName()).or_else(|| first.as_ref().map(|f| match &last {
            Some(l) => format!("{f} {l}"),
            None => f.clone(),
        }));
        picture = crate::island::media::wic().and_then(|wic| {
            let pic = u.GetPictureAsync(UserPictureSize::Size424x424).ok()?.get().ok()?;
            crate::island::media::art_from(&wic, &pic, 256, 0x5553_4552).map(Arc::new)
        });
    }
    let name = name.unwrap_or_else(|| account.clone());
    let first = first.unwrap_or_else(|| name.split_whitespace().next().unwrap_or(&name).to_string());
    UserInfo { name, first, picture, pc: std::env::var("COMPUTERNAME").unwrap_or_default() }
}

/// The Windows account name ("ollie").
fn account_name() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "User".into())
}

/// "Good morning" / "Good afternoon" / "Good evening" for `hour` (0-23).
pub fn greeting(hour: u32) -> &'static str {
    match hour {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

/// How long since Windows started: "3 days, 4 hrs", "2 hrs, 5 mins", "12 mins".
pub fn uptime() -> String {
    let secs = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() } / 1000;
    let (d, h, m) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    let unit = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (d, h) {
        (0, 0) => unit(m, "min", "mins"),
        (0, _) => format!("{}, {}", unit(h, "hr", "hrs"), unit(m, "min", "mins")),
        _ => format!("{}, {}", unit(d, "day", "days"), unit(h, "hr", "hrs")),
    }
}

/// "OS" from "Ollie Smith": up to two initials, for when there's no picture.
pub fn initials(name: &str) -> String {
    name.split_whitespace().filter_map(|w| w.chars().next()).take(2).flat_map(char::to_uppercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        assert_eq!(greeting(8), "Good morning");
        assert_eq!(greeting(13), "Good afternoon");
        assert_eq!(greeting(23), "Good evening");
        assert_eq!(initials("ollie smith"), "OS");
        assert_eq!(initials("Box"), "B");
    }

    #[test]
    #[ignore]
    fn live_user() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let u = read();
        println!("{} / {} / {} / picture: {}", u.name, u.first, u.pc, u.picture.is_some());
        println!("up {}", uptime());
    }
}
