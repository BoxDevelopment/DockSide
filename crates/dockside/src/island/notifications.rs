//! Notification banners: new toasts in Windows' notification center, read with
//! `UserNotificationListener`. Unpackaged apps can't subscribe to change
//! events, so a worker polls the (small) list every couple of seconds. The
//! island also keeps them as a stack, in step with the notification center:
//! what's cleared there goes from the island, and the reverse.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::Interface;

use windows::Foundation::Size;
use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds, UserNotification};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::media::{art_from, wic, Art};
use super::sources::{Inbox, Signal, Stop};

const POLL: Duration = Duration::from_secs(2);
/// Notifications already waiting when DockSide starts that join the stack.
const HISTORY: usize = 20;

#[derive(Clone)]
pub struct Note {
    /// The notification center's id for it.
    pub id: u32,
    /// When the app sent it.
    pub at: SystemTime,
    pub app: String,
    /// AppUserModelID, to open the app on click.
    pub app_id: String,
    pub title: String,
    pub body: String,
    pub icon: Option<Arc<Art>>,
}

pub fn watch(inbox: Inbox, stop: Stop) {
    let spawned = std::thread::Builder::new().name("notifications".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let listener = match UserNotificationListener::Current() {
            Ok(l) => l,
            Err(e) => return log!("notifications unavailable: {e}"),
        };
        let access = listener.RequestAccessAsync().and_then(|op| op.get());
        if access != Ok(UserNotificationListenerAccessStatus::Allowed) {
            return log!("notification access not granted ({access:?})");
        }
        let wic = wic();
        let mut seen: Option<HashSet<u32>> = None;
        let mut next_icon = 1u64 << 40;
        while !stop.stopped() {
            if let Ok(list) = listener.GetNotificationsAsync(NotificationKinds::Toast).and_then(|op| op.get()) {
                let notes: Vec<UserNotification> = list.into_iter().collect();
                let ids: HashSet<u32> = notes.iter().filter_map(|n| n.Id().ok()).collect();
                match &seen {
                    // The first pass: what's already there joins the stack without a banner.
                    None => {
                        let mut history: Vec<Note> = notes
                            .iter()
                            .rev()
                            .take(HISTORY)
                            .filter_map(|n| {
                                next_icon += 1;
                                read(n, wic.as_ref(), next_icon)
                            })
                            .collect();
                        history.sort_by_key(|n| std::cmp::Reverse(n.at));
                        if !history.is_empty() {
                            inbox.push(Signal::NoteHistory(history));
                        }
                    }
                    Some(old) => {
                        for n in notes.iter().filter(|n| n.Id().is_ok_and(|id| !old.contains(&id))) {
                            next_icon += 1;
                            if let Some(note) = read(n, wic.as_ref(), next_icon) {
                                inbox.push(Signal::Notification(note));
                            }
                        }
                        let gone: Vec<u32> = old.difference(&ids).copied().collect();
                        if !gone.is_empty() {
                            inbox.push(Signal::NotesGone(gone));
                        }
                    }
                }
                seen = Some(ids);
            }
            std::thread::sleep(POLL);
        }
    });
    if let Err(e) = spawned {
        log!("notification thread failed: {e}");
    }
}

fn read(n: &UserNotification, wic: Option<&windows::Win32::Graphics::Imaging::IWICImagingFactory>, id: u64) -> Option<Note> {
    let info = n.AppInfo().ok()?;
    let display = info.DisplayInfo().ok()?;
    let app = display.DisplayName().map(|s| s.to_string()).unwrap_or_default();
    let app_id = info.AppUserModelId().map(|s| s.to_string()).unwrap_or_default();
    let binding = n.Notification().ok()?.Visual().ok()?.GetBinding(&KnownNotificationBindings::ToastGeneric().ok()?).ok()?;
    let texts: Vec<String> = binding
        .GetTextElements()
        .ok()?
        .into_iter()
        .filter_map(|t| t.Text().ok().map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
        .collect();
    let title = texts.first().cloned().unwrap_or_else(|| app.clone());
    let body = texts.get(1..).map(|r| r.join(" · ")).unwrap_or_default();
    let icon = wic
        .and_then(|w| display.GetLogo(Size { Width: 64.0, Height: 64.0 }).ok().and_then(|logo| logo.cast().ok().and_then(|r| art_from(w, &r, 64, id))))
        .map(Arc::new);
    // Windows file time: 100 ns ticks since 1601.
    const UNIX_TICKS: i64 = 116_444_736_000_000_000;
    let at = n
        .CreationTime()
        .ok()
        .and_then(|t| u64::try_from(t.UniversalTime - UNIX_TICKS).ok())
        .map_or_else(SystemTime::now, |t| UNIX_EPOCH + Duration::from_nanos(t * 100));
    Some(Note { id: n.Id().ok()?, at, app, app_id, title, body, icon })
}

/// Whether Windows' Notifications switch is on (off, Windows keeps none for the island to read).
pub fn windows_enabled() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let (mut on, mut size) = (1u32, 4u32);
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\PushNotifications"),
            w!("ToastEnabled"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut on as *mut u32 as *mut _),
            Some(&mut size),
        );
    }
    on != 0
}

/// Takes notifications out of the notification center (`None`: all of them).
/// On its own thread: the listener is slow to answer.
pub fn remove(ids: Option<Vec<u32>>) {
    let spawned = std::thread::Builder::new().name("notifications-remove".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let Ok(listener) = UserNotificationListener::Current() else { return };
        match ids {
            Some(ids) => {
                for id in ids {
                    let _ = listener.RemoveNotification(id);
                }
            }
            None => {
                let _ = listener.ClearNotifications();
            }
        }
    });
    if let Err(e) = spawned {
        log!("notification thread failed: {e}");
    }
}

/// "now", "5m", "2h", "Mon".
pub fn age(at: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(at).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => {
            let local: chrono::DateTime<chrono::Local> = at.into();
            local.format("%a").to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        let now = SystemTime::now();
        assert_eq!(age(now, now), "now");
        assert_eq!(age(now - Duration::from_secs(300), now), "5m");
        assert_eq!(age(now - Duration::from_secs(7300), now), "2h");
        assert_eq!(age(now + Duration::from_secs(5), now), "now");
        assert_eq!(age(now - Duration::from_secs(3 * 86_400), now).len(), 3);
    }
}
