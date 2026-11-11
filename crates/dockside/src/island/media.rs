//! Now Playing, read from the system media transport controls: the same
//! sessions that drive Windows' own media flyout (Spotify, browsers, Media
//! Player, ...).
//!
//! The session manager lives in a helper process (`dockside --media-helper`):
//! Windows.Media.MediaControl.dll has crashed its host from its own worker
//! threads (a fail-fast nothing in-process can catch), so it never loads into
//! DockSide itself. The helper sends snapshots over its stdout; DockSide sends
//! commands down its stdin and restarts it if it dies.
//!
//! In the helper, a worker thread owns the session manager. WinRT events (on
//! arbitrary threads) only nudge it through a channel; it then rebuilds a
//! snapshot, decodes album art, and writes it to the pipe. In DockSide, a
//! supervisor thread reads snapshots and posts a message to the UI thread. The
//! island and the Now Playing widget share one helper (see [`Media::shared`]).

use std::cell::RefCell;
use std::io::{BufWriter, Read, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Runs the helper process instead of DockSide.
pub const HELPER_ARG: &str = "--media-helper";

/// The helper's exit code when Now Playing isn't available on this system
/// (no point restarting it).
const UNAVAILABLE: i32 = 3;

/// What the supervisor thread waits on.
enum Msg {
    Cmd(Cmd),
    /// The helper with this generation closed its pipe (exited or crashed).
    Exited(u64),
}

struct Handle {
    tx: Sender<Msg>,
    shared: Shared,
}

impl Drop for Handle {
    fn drop(&mut self) {
        // The reader threads hold senders too, so the channel never closes on its own.
        let _ = self.tx.send(Msg::Cmd(Cmd::Quit));
    }
}

thread_local! {
    static WORKER: RefCell<Weak<Handle>> = const { RefCell::new(Weak::new()) };
}

/// A handle on the Now Playing worker. Clones share it; it stops with the last one.
#[derive(Clone)]
pub struct Media(Rc<Handle>);

impl Media {
    /// The worker everything showing Now Playing shares (the island and the
    /// widget), started on first use. It posts `WM_APP_MEDIA` to `hwnd` whenever
    /// Now Playing changes.
    pub fn shared(hwnd: isize) -> Self {
        if let Some(w) = WORKER.with(|w| w.borrow().upgrade()) {
            return Self(w);
        }
        let worker = Rc::new(Self::start(hwnd, crate::win::WM_APP_MEDIA));
        WORKER.with(|w| *w.borrow_mut() = Rc::downgrade(&worker));
        Self(worker)
    }

    fn start(hwnd: isize, msg: u32) -> Handle {
        let (tx, rx) = channel();
        let shared: Shared = Arc::default();
        let (tx2, shared2) = (tx.clone(), shared.clone());
        let spawned = std::thread::Builder::new()
            .name("media".into())
            .spawn(move || supervise(rx, tx2, shared2, hwnd, msg));
        if let Err(e) = spawned {
            log!("media thread failed: {e}");
        }
        Handle { tx, shared }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.0.tx.send(Msg::Cmd(cmd));
    }

    pub fn snapshot(&self) -> Option<NowPlaying> {
        self.0.shared.lock().ok().and_then(|s| s.clone())
    }
}

// ---- DockSide side: run the helper, restart it when it dies ----

/// Starts helpers one after another until DockSide quits. A helper that ran a
/// while restarts at once; one that keeps dying backs off up to a minute.
fn supervise(rx: Receiver<Msg>, tx: Sender<Msg>, shared: Shared, hwnd: isize, msg: u32) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return log!("now playing unavailable: {e}"),
    };
    let mut quick_deaths = 0u32;
    for generation in 0u64.. {
        let started = Instant::now();
        let Some(mut helper) = spawn_helper(&exe, generation, &tx, &shared, hwnd, msg) else { return };
        let mut stdin = helper.stdin.take();

        // Pass commands along until the helper goes away (or we quit).
        loop {
            match rx.recv() {
                Ok(Msg::Cmd(Cmd::Quit)) | Err(_) => return stop(helper),
                Ok(Msg::Cmd(cmd)) => {
                    // A failed write means it's dying: its Exited follows.
                    if let Some(w) = stdin.as_mut() {
                        if write_cmd(w, &cmd).is_err() {
                            stdin = None;
                        }
                    }
                }
                Ok(Msg::Exited(g)) if g == generation => break,
                Ok(Msg::Exited(_)) => {}
            }
        }
        drop(stdin);
        let code = helper.wait().ok().and_then(|s| s.code());
        if code == Some(UNAVAILABLE) {
            // Logged by the helper. Nothing to show; wait to be told to quit.
            set(&shared, None, hwnd, msg);
            while !matches!(rx.recv(), Ok(Msg::Cmd(Cmd::Quit)) | Err(_)) {}
            return;
        }
        quick_deaths = if started.elapsed() > Duration::from_secs(60) { 0 } else { quick_deaths + 1 };
        let delay = Duration::from_millis(250 << quick_deaths.min(8)).min(Duration::from_secs(60));
        log!("now playing helper exited ({}); restarting in {delay:?}", describe_exit(code));
        if quick_deaths >= 3 {
            // Don't leave a stale track up while it keeps failing.
            set(&shared, None, hwnd, msg);
        }
        let until = Instant::now() + delay;
        loop {
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(Msg::Cmd(Cmd::Quit)) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => break,
                // Commands for a track we can't reach; the new helper refreshes anyway.
                Ok(_) => {}
            }
        }
    }
}

fn describe_exit(code: Option<i32>) -> String {
    match code {
        Some(c) if c < 0 => format!("code {:#010x}", c as u32),
        Some(c) => format!("code {c}"),
        None => "no exit code".into(),
    }
}

fn spawn_helper(
    exe: &std::path::Path,
    generation: u64,
    tx: &Sender<Msg>,
    shared: &Shared,
    hwnd: isize,
    msg: u32,
) -> Option<Child> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let spawned = Command::new(exe)
        .arg(HELPER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let mut helper = match spawned {
        Ok(c) => c,
        Err(e) => {
            log!("now playing helper didn't start: {e}");
            return None;
        }
    };
    let stdout = helper.stdout.take()?;
    let (tx, shared) = (tx.clone(), shared.clone());
    let reader = std::thread::Builder::new().name("media-read".into()).spawn(move || {
        let mut r = std::io::BufReader::new(stdout);
        let mut art = None;
        while let Ok(snap) = read_snapshot(&mut r, &mut art) {
            set(&shared, snap, hwnd, msg);
        }
        let _ = tx.send(Msg::Exited(generation));
    });
    if let Err(e) = reader {
        log!("media reader failed: {e}");
        stop(helper);
        return None;
    }
    Some(helper)
}

fn set(shared: &Shared, snap: Option<NowPlaying>, hwnd: isize, msg: u32) {
    if let Ok(mut s) = shared.lock() {
        *s = snap;
    }
    post(hwnd, msg);
}

/// Closes the helper's stdin (it exits on its own) and makes sure it's gone.
fn stop(mut helper: Child) {
    drop(helper.stdin.take());
    let _ = helper.kill();
    let _ = helper.wait();
}

// ---- Helper side ----

/// `dockside --media-helper`: reads Now Playing and writes snapshots to stdout
/// until stdin closes (DockSide quit, restarted it, or crashed).
pub fn helper_main() {
    let (tx, rx) = channel();
    let tx2 = tx.clone();
    let _ = std::thread::Builder::new().name("media-cmds".into()).spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Ok(cmd) = read_cmd(&mut stdin) {
            if tx2.send(cmd).is_err() {
                break;
            }
        }
        // DockSide is gone: don't wait on a WinRT call that may never return.
        std::process::exit(0);
    });
    let mut out = BufWriter::new(std::io::stdout().lock());
    if let Err(e) = run(rx, tx, &mut out) {
        log!("now playing unavailable: {e}");
        std::process::exit(UNAVAILABLE);
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

/// How long after a track change its artwork may still be replaced, how often
/// it's re-read in that time, and how many reads at most.
const ART_SETTLE: Duration = Duration::from_secs(20);
const ART_RECHECK_GAP: Duration = Duration::from_millis(700);
const ART_RECHECKS: u32 = 16;

#[derive(Default)]
struct Worker {
    subs: Vec<Sub>,
    /// The session being shown, by app id.
    chosen: Option<String>,
    art_key: String,
    art_tries: u32,
    /// When the track changed, and when its artwork was last read.
    art_since: Option<Instant>,
    art_checked: Option<Instant>,
    art: Option<Arc<Art>>,
    next_art_id: u64,
    wic: Option<IWICImagingFactory>,
}

/// The helper's worker. Errs only when Now Playing can't start at all.
fn run(rx: Receiver<Cmd>, tx: Sender<Cmd>, out: &mut impl Write) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let manager = Manager::RequestAsync()?.get()?;
    let t1 = manager.SessionsChanged(&TypedEventHandler::new(nudge(&tx)))?;
    let t2 = manager.CurrentSessionChanged(&TypedEventHandler::new(nudge(&tx)))?;
    let mut w = Worker {
        wic: wic(),
        ..Default::default()
    };
    let _ = tx.send(Cmd::Refresh);

    // The art last written in full, which later snapshots refer to by id.
    let mut sent_art = None;
    let mut next = None;
    loop {
        let cmd = match next.take() {
            Some(cmd) => cmd,
            None => match rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => break,
            },
        };
        if matches!(cmd, Cmd::Quit) {
            break;
        }
        if matches!(cmd, Cmd::Refresh) {
            // Change events come in bursts (some players report the timeline many times
            // a second): one snapshot covers every refresh already queued.
            while let Ok(c) = rx.try_recv() {
                if !matches!(c, Cmd::Refresh) {
                    next = Some(c);
                    break;
                }
            }
        }
        let r = match cmd {
            Cmd::Refresh => w.refresh(&manager, &tx).map(Some),
            other => w.command(other).map(|_| None),
        };
        match r {
            Ok(Some(snap)) => {
                // DockSide stopped listening: we're done.
                if write_snapshot(out, snap.as_ref(), &mut sent_art).and_then(|_| out.flush()).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => log!("now playing: {e}"),
        }
    }
    w.subs.clear();
    let _ = manager.RemoveSessionsChanged(t1);
    let _ = manager.RemoveCurrentSessionChanged(t2);
    Ok(())
}

// ---- Pipe protocol (little-endian; both ends are the same exe) ----

fn put(w: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(bytes)
}

fn put_str(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    put(w, &(s.len() as u32).to_le_bytes())?;
    put(w, s.as_bytes())
}

fn get<const N: usize>(r: &mut impl Read) -> std::io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn get_f32(r: &mut impl Read) -> std::io::Result<f32> {
    Ok(f32::from_le_bytes(get(r)?))
}

fn get_bytes(r: &mut impl Read, max: usize) -> std::io::Result<Vec<u8>> {
    let len = u32::from_le_bytes(get(r)?) as usize;
    if len > max {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut v = vec![0u8; len];
    r.read_exact(&mut v)?;
    Ok(v)
}

fn get_str(r: &mut impl Read) -> std::io::Result<String> {
    String::from_utf8(get_bytes(r, 1 << 20)?).map_err(|_| std::io::ErrorKind::InvalidData.into())
}

fn write_cmd(w: &mut impl Write, cmd: &Cmd) -> std::io::Result<()> {
    let (tag, arg) = match *cmd {
        Cmd::Refresh => (1u8, 0.0),
        Cmd::TogglePlay => (2, 0.0),
        Cmd::Next => (3, 0.0),
        Cmd::Prev => (4, 0.0),
        Cmd::Seek(s) => (5, s),
        // Never sent: closing stdin stops the helper.
        Cmd::Quit => return Ok(()),
    };
    put(w, &[tag])?;
    put(w, &arg.to_le_bytes())?;
    w.flush()
}

fn read_cmd(r: &mut impl Read) -> std::io::Result<Cmd> {
    let [tag] = get::<1>(r)?;
    let arg = get_f32(r)?;
    Ok(match tag {
        1 => Cmd::Refresh,
        2 => Cmd::TogglePlay,
        3 => Cmd::Next,
        4 => Cmd::Prev,
        5 => Cmd::Seek(arg),
        _ => return Err(std::io::ErrorKind::InvalidData.into()),
    })
}

/// Art is sent in full once; snapshots after that name it by the helper's id.
const ART_NONE: u8 = 0;
const ART_SAME: u8 = 1;
const ART_FULL: u8 = 2;

fn write_snapshot(w: &mut impl Write, snap: Option<&NowPlaying>, sent_art: &mut Option<u64>) -> std::io::Result<()> {
    let Some(np) = snap else { return put(w, &[0]) };
    put(w, &[1])?;
    put_str(w, &np.app_id)?;
    put_str(w, &np.title)?;
    put_str(w, &np.artist)?;
    let flags = np.playing as u8 | (np.can_prev as u8) << 1 | (np.can_next as u8) << 2 | (np.can_seek as u8) << 3;
    put(w, &[flags])?;
    // Position as of now; the reader stamps it with its own clock.
    put(w, &np.position_now().to_le_bytes())?;
    put(w, &np.duration.to_le_bytes())?;
    match &np.art {
        None => put(w, &[ART_NONE]),
        Some(a) if *sent_art == Some(a.id) => {
            put(w, &[ART_SAME])?;
            put(w, &a.id.to_le_bytes())
        }
        Some(a) => {
            put(w, &[ART_FULL])?;
            put(w, &a.id.to_le_bytes())?;
            put(w, &a.size.to_le_bytes())?;
            for c in [a.tint.r, a.tint.g, a.tint.b, a.tint.a] {
                put(w, &c.to_le_bytes())?;
            }
            put(w, &(a.data.len() as u32).to_le_bytes())?;
            put(w, &a.data)?;
            *sent_art = Some(a.id);
            Ok(())
        }
    }
}

/// Art ids in DockSide: helpers start counting again when they restart.
static NEXT_ART_ID: AtomicU64 = AtomicU64::new(1);

/// `art`: the last full art received, by the helper's id.
fn read_snapshot(r: &mut impl Read, art: &mut Option<(u64, Arc<Art>)>) -> std::io::Result<Option<NowPlaying>> {
    let [some] = get::<1>(r)?;
    if some == 0 {
        return Ok(None);
    }
    let app_id = get_str(r)?;
    let title = get_str(r)?;
    let artist = get_str(r)?;
    let [flags] = get::<1>(r)?;
    let position = get_f32(r)?;
    let duration = get_f32(r)?;
    let [kind] = get::<1>(r)?;
    let shown = match kind {
        ART_NONE => None,
        ART_SAME => {
            let id = u64::from_le_bytes(get(r)?);
            art.as_ref().filter(|(had, _)| *had == id).map(|(_, a)| a.clone())
        }
        ART_FULL => {
            let id = u64::from_le_bytes(get(r)?);
            let size = u32::from_le_bytes(get(r)?);
            let tint = Color { r: get_f32(r)?, g: get_f32(r)?, b: get_f32(r)?, a: get_f32(r)? };
            let data = get_bytes(r, 64 << 20)?;
            if data.len() != (size as usize).pow(2) * 4 {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            let a = Arc::new(Art { id: NEXT_ART_ID.fetch_add(1, Ordering::Relaxed), size, data, tint });
            *art = Some((id, a.clone()));
            Some(a)
        }
        _ => return Err(std::io::ErrorKind::InvalidData.into()),
    };
    Ok(Some(NowPlaying {
        app_id,
        title,
        artist,
        playing: flags & 1 != 0,
        position,
        duration,
        at: Instant::now(),
        can_prev: flags & 2 != 0,
        can_next: flags & 4 != 0,
        can_seek: flags & 8 != 0,
        art: shown,
    }))
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
        let now = Instant::now();
        if key != self.art_key {
            self.art_key = key;
            self.art_tries = 0;
            self.art = None;
            self.art_since = Some(now);
            self.art_checked = None;
        }
        // Apps often publish the title first and the artwork a moment later.
        if self.art.is_none() && self.art_tries < 4 {
            self.art_tries += 1;
            self.art = self.load_art(&props).map(Arc::new);
            self.art_checked = Some(now);
        } else if self.art.is_some()
            && self.art_tries < ART_RECHECKS
            && self.art_since.is_some_and(|t| now.duration_since(t) < ART_SETTLE)
            && self.art_checked.is_none_or(|t| now.duration_since(t) >= ART_RECHECK_GAP)
        {
            // Browsers put their own logo up first and swap in the page's
            // artwork once it has downloaded: keep looking for a while.
            self.art_tries += 1;
            self.art_checked = Some(now);
            if let Some(new) = self.load_art(&props) {
                if self.art.as_ref().is_none_or(|old| old.data != new.data) {
                    self.art = Some(Arc::new(new));
                }
            }
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
pub(crate) fn art_from(
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

pub(crate) fn wic() -> Option<IWICImagingFactory> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() }
}

/// 100ns ticks since 1601 (WinRT `DateTime`).
fn now_universal() -> i64 {
    const EPOCH_DIFF: i64 = 116_444_736_000_000_000;
    let since_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    EPOCH_DIFF + (since_unix.as_nanos() / 100) as i64
}

/// Decodes image file bytes (PNG, JPEG...) into a square BGRA image of at most
/// `max` px (for pictures fetched off the web).
pub(super) fn decode_bytes(data: &[u8], max: u32) -> Option<(u32, Vec<u8>)> {
    let stream = unsafe { windows::Win32::UI::Shell::SHCreateMemStream(Some(data)) }?;
    decode_square(&wic()?, &stream, max)
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
    use super::*;

    fn track(art: Option<Arc<Art>>) -> NowPlaying {
        NowPlaying {
            app_id: "Spotify.exe".into(),
            title: "Título".into(),
            artist: "Artist".into(),
            playing: false,
            position: 12.5,
            duration: 200.0,
            at: Instant::now(),
            can_prev: true,
            can_next: false,
            can_seek: true,
            art,
        }
    }

    #[test]
    fn snapshots_round_trip_and_art_is_sent_once() {
        let art = Arc::new(Art { id: 7, size: 2, data: vec![9; 16], tint: Color { r: 1.0, g: 0.5, b: 0.25, a: 1.0 } });
        let (mut buf, mut sent) = (Vec::new(), None);
        write_snapshot(&mut buf, Some(&track(Some(art.clone()))), &mut sent).unwrap();
        let first = buf.len();
        write_snapshot(&mut buf, Some(&track(Some(art))), &mut sent).unwrap();
        write_snapshot(&mut buf, None, &mut sent).unwrap();
        assert!(buf.len() - first < 64, "art resent");

        let (mut r, mut cache) = (&buf[..], None);
        let a = read_snapshot(&mut r, &mut cache).unwrap().unwrap();
        let b = read_snapshot(&mut r, &mut cache).unwrap().unwrap();
        assert!(read_snapshot(&mut r, &mut cache).unwrap().is_none());
        assert!(r.is_empty());
        assert_eq!((a.title.as_str(), a.position, a.can_prev, a.can_next, a.can_seek), ("Título", 12.5, true, false, true));
        let (aa, ba) = (a.art.unwrap(), b.art.unwrap());
        assert!(Arc::ptr_eq(&aa, &ba));
        assert_eq!((aa.size, aa.data.len(), aa.tint.g), (2, 16, 0.5));
    }

    #[test]
    fn commands_round_trip() {
        let mut buf = Vec::new();
        for c in [Cmd::Refresh, Cmd::TogglePlay, Cmd::Next, Cmd::Prev, Cmd::Seek(42.5)] {
            write_cmd(&mut buf, &c).unwrap();
        }
        let mut r = &buf[..];
        let got: Vec<String> = (0..5).map(|_| format!("{:?}", read_cmd(&mut r).unwrap())).collect();
        assert_eq!(got, ["Refresh", "TogglePlay", "Next", "Prev", "Seek(42.5)"]);
        assert!(read_cmd(&mut r).is_err());
    }

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
