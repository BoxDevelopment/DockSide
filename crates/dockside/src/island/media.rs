//! Now Playing, read from the system media transport controls: the same
//! sessions that drive Windows' own media flyout (Spotify, browsers, Media
//! Player, ...).
//!
//! A worker thread owns the session manager. WinRT events (on arbitrary
//! threads) only nudge it through a channel; it then rebuilds a snapshot,
//! decodes album art, and posts a message to the UI thread.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::core::{GUID, Result};
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IStream, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::CreateStreamOverRandomAccessStream;

use crate::geom::Color;
use crate::win::post;

/// Album art, decoded to premultiplied BGRA, cropped square.
pub struct Art {
    pub id: u64,
    pub size: u32,
    pub data: Vec<u8>,
    /// A bright accent taken from the artwork (for the waveform).
    pub tint: Color,
}

#[derive(Clone)]
pub struct NowPlaying {
    pub app_id: String,
    pub title: String,
    pub artist: String,
    pub playing: bool,
    /// Seconds into the track at `at`.
    pub position: f32,
    pub duration: f32,
    pub at: Instant,
    pub can_prev: bool,
    pub can_next: bool,
    pub can_seek: bool,
    pub art: Option<Arc<Art>>,
}

impl NowPlaying {
    /// Position extrapolated to now while playing.
    pub fn position_now(&self) -> f32 {
        let p = if self.playing { self.position + self.at.elapsed().as_secs_f32() } else { self.position };
        if self.duration > 0.0 {
            p.clamp(0.0, self.duration)
        } else {
            p.max(0.0)
        }
    }

    /// Same track (ignores playback state and position).
    pub fn same_track(&self, other: &NowPlaying) -> bool {
        self.app_id == other.app_id && self.title == other.title && self.artist == other.artist
    }
}

#[derive(Debug)]
pub enum Cmd {
    Refresh,
    TogglePlay,
    Next,
    Prev,
    /// Seconds from the start of the track.
    Seek(f32),
    Quit,
}

pub type Shared = Arc<Mutex<Option<NowPlaying>>>;

pub struct Media {
    tx: Sender<Cmd>,
    shared: Shared,
}

impl Media {
    /// Starts the worker; it posts `msg` to `hwnd` whenever Now Playing changes.
    pub fn start(hwnd: isize, msg: u32) -> Self {
        let (tx, rx) = channel();
        let shared: Shared = Arc::default();
        let (tx2, shared2) = (tx.clone(), shared.clone());
        let spawned = std::thread::Builder::new().name("media".into()).spawn(move || {
            if let Err(e) = run(rx, tx2, shared2, hwnd, msg) {
                log!("now playing unavailable: {e}");
            }
        });
        if let Err(e) = spawned {
            log!("media thread failed: {e}");
        }
        Self { tx, shared }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn snapshot(&self) -> Option<NowPlaying> {
        self.shared.lock().ok().and_then(|s| s.clone())
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        // Event handlers hold senders too, so the channel never closes on its own.
        let _ = self.tx.send(Cmd::Quit);
    }
}

struct Sub {
    session: Session,
    tokens: [EventRegistrationToken; 3],
}

impl Sub {
    fn new(session: Session, tx: &Sender<Cmd>) -> Result<Self> {
        let tokens = [
            session.MediaPropertiesChanged(&TypedEventHandler::new(nudge(tx)))?,
            session.PlaybackInfoChanged(&TypedEventHandler::new(nudge(tx)))?,
            session.TimelinePropertiesChanged(&TypedEventHandler::new(nudge(tx)))?,
        ];
        Ok(Self { session, tokens })
    }
}

impl Drop for Sub {
    fn drop(&mut self) {
        let [a, b, c] = self.tokens;
        let _ = self.session.RemoveMediaPropertiesChanged(a);
        let _ = self.session.RemovePlaybackInfoChanged(b);
        let _ = self.session.RemoveTimelinePropertiesChanged(c);
    }
}

fn nudge<A, B>(tx: &Sender<Cmd>) -> impl FnMut(&A, &B) -> Result<()> + Send + 'static {
    let tx = tx.clone();
    move |_, _| {
        let _ = tx.send(Cmd::Refresh);
        Ok(())
    }
}

#[derive(Default)]
struct Worker {
    subs: Vec<Sub>,
    /// The session being shown, by app id.
    chosen: Option<String>,
    art_key: String,
    art_tries: u32,
    art: Option<Arc<Art>>,
    next_art_id: u64,
    wic: Option<IWICImagingFactory>,
}

fn run(rx: Receiver<Cmd>, tx: Sender<Cmd>, shared: Shared, hwnd: isize, msg: u32) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let manager = Manager::RequestAsync()?.get()?;
    let t1 = manager.SessionsChanged(&TypedEventHandler::new(nudge(&tx)))?;
    let t2 = manager.CurrentSessionChanged(&TypedEventHandler::new(nudge(&tx)))?;
    let mut w = Worker {
        wic: wic(),
        ..Default::default()
    };
    let _ = tx.send(Cmd::Refresh);

    while let Ok(cmd) = rx.recv() {
        if matches!(cmd, Cmd::Quit) {
            break;
        }
        let r = match cmd {
            Cmd::Refresh => w.refresh(&manager, &tx).map(|snap| {
                if let Ok(mut s) = shared.lock() {
                    *s = snap;
                }
                post(hwnd, msg);
            }),
            other => w.command(other),
        };
        if let Err(e) = r {
            log!("now playing: {e}");
        }
    }
    w.subs.clear();
    let _ = manager.RemoveSessionsChanged(t1);
    let _ = manager.RemoveCurrentSessionChanged(t2);
    Ok(())
}

fn app_id(s: &Session) -> String {
    s.SourceAppUserModelId().map(|h| h.to_string()).unwrap_or_default()
}

fn is_playing(s: &Session) -> bool {
    s.GetPlaybackInfo().and_then(|i| i.PlaybackStatus()).is_ok_and(|st| st == Status::Playing)
}

impl Worker {
    fn session(&self) -> Option<&Session> {
        let id = self.chosen.as_ref()?;
        self.subs.iter().map(|s| &s.session).find(|s| app_id(s) == *id)
    }

    fn command(&mut self, cmd: Cmd) -> Result<()> {
        let Some(s) = self.session() else { return Ok(()) };
        match cmd {
            Cmd::TogglePlay => s.TryTogglePlayPauseAsync()?.get().map(drop),
            Cmd::Next => s.TrySkipNextAsync()?.get().map(drop),
            Cmd::Prev => s.TrySkipPreviousAsync()?.get().map(drop),
            Cmd::Seek(secs) => {
                let start = s.GetTimelineProperties()?.StartTime()?.Duration;
                s.TryChangePlaybackPositionAsync(start + (secs as f64 * 1e7) as i64)?.get().map(drop)
            }
            Cmd::Refresh | Cmd::Quit => Ok(()),
        }
    }

    /// Re-subscribes when sessions come and go, picks the session to show and snapshots it.
    fn refresh(&mut self, manager: &Manager, tx: &Sender<Cmd>) -> Result<Option<NowPlaying>> {
        let sessions: Vec<Session> = manager.GetSessions()?.into_iter().collect();
        let ids: Vec<String> = sessions.iter().map(app_id).collect();
        let subscribed: Vec<String> = self.subs.iter().map(|s| app_id(&s.session)).collect();
        if ids != subscribed {
            self.subs.clear();
            self.subs = sessions.iter().filter_map(|s| Sub::new(s.clone(), tx).ok()).collect();
        }

        // Prefer whatever is playing, sticking with the current pick; a paused
        // session stays shown until something else starts.
        let current = manager.GetCurrentSession().ok().map(|s| app_id(&s));
        let playing = |id: &String| sessions.iter().any(|s| app_id(s) == *id && is_playing(s));
        let chosen = self
            .chosen
            .clone()
            .filter(playing)
            .or_else(|| current.clone().filter(playing))
            .or_else(|| ids.iter().find(|id| playing(id)).cloned())
            .or_else(|| self.chosen.clone().filter(|id| ids.contains(id)))
            .or(current)
            .filter(|id| ids.contains(id));
        self.chosen = chosen;
        let Some(session) = self.session().cloned() else {
            self.art = None;
            self.art_key.clear();
            return Ok(None);
        };
        self.snapshot(&session).map(Some)
    }

    fn snapshot(&mut self, s: &Session) -> Result<NowPlaying> {
        let app_id = app_id(s);
        let props = s.TryGetMediaPropertiesAsync()?.get()?;
        let title = props.Title()?.to_string();
        let artist = props.Artist().map(|a| a.to_string()).unwrap_or_default();
        let album = props.AlbumTitle().map(|a| a.to_string()).unwrap_or_default();

        let info = s.GetPlaybackInfo()?;
        let playing = info.PlaybackStatus()? == Status::Playing;
        let controls = info.Controls()?;

        let tl = s.GetTimelineProperties()?;
        let start = tl.StartTime()?.Duration;
        let end = tl.EndTime()?.Duration;
        let mut pos = tl.Position()?.Duration - start;
        let updated = tl.LastUpdatedTime()?.UniversalTime;
        // Positions are stamped when the app reported them; bring them up to now.
        let since = now_universal() - updated;
        if playing && updated > 0 && (0..86_400 * 10_000_000).contains(&since) {
            pos += since;
        }
        let duration = ((end - start) as f64 / 1e7) as f32;

        let key = format!("{app_id}\n{title}\n{artist}\n{album}");
        if key != self.art_key {
            self.art_key = key;
            self.art_tries = 0;
            self.art = None;
        }
        // Apps often publish the title first and the artwork a moment later.
        if self.art.is_none() && self.art_tries < 4 {
            self.art_tries += 1;
            self.art = self.load_art(&props).map(Arc::new);
        }

        Ok(NowPlaying {
            app_id,
            title,
            artist,
            playing,
            position: (pos as f64 / 1e7) as f32,
            duration: duration.max(0.0),
            at: Instant::now(),
            can_prev: controls.IsPreviousEnabled().unwrap_or(false),
            can_next: controls.IsNextEnabled().unwrap_or(false),
            can_seek: controls.IsPlaybackPositionEnabled().unwrap_or(false) && duration > 0.0,
            art: self.art.clone(),
        })
    }

    fn load_art(
        &mut self,
        props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
    ) -> Option<Art> {
        let wic = self.wic.as_ref()?;
        self.next_art_id += 1;
        art_from(wic, &props.Thumbnail().ok()?, 192, self.next_art_id)
    }
}

/// Decodes an image stream (artwork, app logos) into a square [`Art`].
pub(super) fn art_from(
    wic: &IWICImagingFactory,
    image: &windows::Storage::Streams::IRandomAccessStreamReference,
    max: u32,
    id: u64,
) -> Option<Art> {
    let stream = image.OpenReadAsync().ok()?.get().ok()?;
    let stream: IStream = unsafe { CreateStreamOverRandomAccessStream(&stream).ok()? };
    let (size, data) = decode_square(wic, &stream, max)?;
    Some(Art { id, size, tint: accent_of(&data), data })
}

pub(super) fn wic() -> Option<IWICImagingFactory> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() }
}

/// 100ns ticks since 1601 (WinRT `DateTime`).
fn now_universal() -> i64 {
    const EPOCH_DIFF: i64 = 116_444_736_000_000_000;
    let since_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    EPOCH_DIFF + (since_unix.as_nanos() / 100) as i64
}

/// Decodes an image, center-crops it square (video thumbnails are 16:9) and
/// scales it to at most `max` px.
fn decode_square(wic: &IWICImagingFactory, stream: &IStream, max: u32) -> Option<(u32, Vec<u8>)> {
    unsafe {
        let decoder = wic.CreateDecoderFromStream(stream, &GUID::zeroed(), WICDecodeMetadataCacheOnLoad).ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let (mut w, mut h) = (0, 0);
        frame.GetSize(&mut w, &mut h).ok()?;
        let side = w.min(h);
        if side == 0 {
            return None;
        }
        let clipper = wic.CreateBitmapClipper().ok()?;
        let rc = WICRect { X: ((w - side) / 2) as i32, Y: ((h - side) / 2) as i32, Width: side as i32, Height: side as i32 };
        clipper.Initialize(&frame, &rc).ok()?;
        let out = side.min(max);
        let scaler = wic.CreateBitmapScaler().ok()?;
        scaler.Initialize(&clipper, out, out, WICBitmapInterpolationModeHighQualityCubic).ok()?;
        let converter = wic.CreateFormatConverter().ok()?;
        converter
            .Initialize(&scaler, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .ok()?;
        let mut data = vec![0u8; (out * out * 4) as usize];
        converter.CopyPixels(std::ptr::null(), out * 4, &mut data).ok()?;
        Some((out, data))
    }
}

/// A bright, saturated color representative of the artwork, readable on black.
fn accent_of(bgra: &[u8]) -> Color {
    let (mut r, mut g, mut b, mut total) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for px in bgra.chunks_exact(4).step_by(3) {
        let (pb, pg, pr) = (px[0] as f32 / 255.0, px[1] as f32 / 255.0, px[2] as f32 / 255.0);
        let (hi, lo) = (pr.max(pg).max(pb), pr.min(pg).min(pb));
        // Favor colorful, reasonably bright pixels over greys and shadows.
        let weight = (hi - lo).powi(2) * hi + 0.002;
        r += pr * weight;
        g += pg * weight;
        b += pb * weight;
        total += weight;
    }
    if total <= 0.0 {
        return Color::gray(255, 0.9);
    }
    let (r, g, b) = (r / total, g / total, b / total);
    let hi = r.max(g).max(b);
    let lo = r.min(g).min(b);
    if hi < 0.05 || (hi - lo) / hi < 0.18 {
        return Color::gray(235, 1.0);
    }
    // Lift to full brightness, keeping the hue.
    let k = 1.0 / hi;
    let lift = |v: f32| (v * k * 0.92 + 0.08).min(1.0);
    Color { r: lift(r), g: lift(g), b: lift(b), a: 1.0 }
}

#[cfg(test)]
mod tests {
    use super::accent_of;

    #[test]
    fn accent_is_bright_and_keeps_hue() {
        // Dark red artwork (BGRA).
        let px: Vec<u8> = [20u8, 10, 120, 255].repeat(64);
        let c = accent_of(&px);
        assert!(c.r > 0.95 && c.g < 0.4 && c.b < 0.4, "{c:?}");
    }

    #[test]
    fn grey_artwork_gives_white() {
        let px: Vec<u8> = [90u8, 90, 90, 255].repeat(64);
        assert!(accent_of(&px).r > 0.9);
    }
}
