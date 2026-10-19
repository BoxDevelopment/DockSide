//! Notification banners: new toasts in Windows' notification center, read with
//! `UserNotificationListener`. Unpackaged apps can't subscribe to change
//! events, so a worker polls the (small) list every couple of seconds.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use windows::core::Interface;

use windows::Foundation::Size;
use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds, UserNotification};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::media::{art_from, wic, Art};
use super::sources::{Inbox, Signal, Stop};

const POLL: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Note {
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
                // The first pass only records what's already there.
                if let Some(old) = &seen {
                    for n in notes.iter().filter(|n| n.Id().is_ok_and(|id| !old.contains(&id))) {
                        next_icon += 1;
                        if let Some(note) = read(n, wic.as_ref(), next_icon) {
                            inbox.push(Signal::Notification(note));
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
    Some(Note { app, app_id, title, body, icon })
}
