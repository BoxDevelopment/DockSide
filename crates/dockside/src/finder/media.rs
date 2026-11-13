//! Quick Look's player for videos and audio files: Media Foundation's Media
//! Engine in frame-server mode.
//!
//! The engine decodes (on the GPU where it can), keeps sound and picture in
//! step and seeks. A worker thread owns it: when a new video frame is due it
//! has the engine scale it on the GPU to the size Quick Look shows it at,
//! reads that back and hands it to the UI thread, posting a message only once
//! the last frame was taken (frames never queue up). Dropping the [`Player`]
//! stops the sound at once and shuts the engine down.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, BSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

pub use crate::widgets::live::Frame;

/// What the scrubber and buttons show.
#[derive(Clone, Default, Debug)]
pub struct State {
    /// Seconds (0 until the file's length is known; infinite for live streams).
    pub duration: f64,
    pub position: f64,
    pub playing: bool,
    pub ended: bool,
    /// The video's own size in pixels (None for sound only, or until known).
    pub video: Option<(u32, u32)>,
    /// The file's length and streams are known.
    pub ready: bool,
    pub error: Option<String>,
}

enum Msg {
    Play,
    Pause,
    Seek(f64),
    /// The most pixels (device) the video is shown at.
    Size(u32, u32),
    Event(u32),
    Stop,
}

struct Shared {
    frame: Mutex<Option<Frame>>,
    fresh: AtomicBool,
    state: Mutex<State>,
    /// A message is on its way that the UI thread hasn't handled.
    pinged: AtomicBool,
}

pub struct Player {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    /// The state as last read, with what the UI asked for applied at once
    /// (the button shows Pause as soon as Play is clicked).
    pub state: State,
}

impl Player {
    /// Starts playing `path`; `hwnd` gets `msg` (wParam `id`) when there's news.
    pub fn open(path: &Path, max_px: (u32, u32), hwnd: isize, msg: u32, id: u32) -> Player {
        let (tx, rx) = channel::<Msg>();
        let shared = Arc::new(Shared { frame: Mutex::new(None), fresh: AtomicBool::new(false), state: Mutex::default(), pinged: AtomicBool::new(false) });
        let (worker, events) = (shared.clone(), tx.clone());
        let file = path.to_path_buf();
        let _ = tx.send(Msg::Size(max_px.0, max_px.1));
        let spawned = std::thread::Builder::new().name("quick look media".into()).spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if MFStartup(MF_VERSION, MFSTARTUP_LITE).is_err() {
                worker.fail("Can't play this file", hwnd, msg, id);
                return;
            }
            let ping = || {
                if !worker.pinged.swap(true, Ordering::AcqRel) {
                    crate::win::post_wp(hwnd, msg, id as usize);
                }
            };
            match Engine::new(&file, events) {
                Some(engine) => engine.run(&rx, &worker, &ping),
                None => worker.fail("Can't play this file", hwnd, msg, id),
            }
            let _ = MFShutdown();
        });
        if let Err(e) = spawned {
            log!("quick look media thread failed: {e}");
        }
        Player { tx, shared, state: State::default() }
    }

    /// The newest frame, if one came since the last call; also refreshes [`Self::state`].
    pub fn poll(&mut self) -> Option<Frame> {
        self.shared.pinged.store(false, Ordering::Release);
        if let Ok(s) = self.shared.state.lock() {
            self.state = s.clone();
        }
        if self.shared.fresh.swap(false, Ordering::AcqRel) {
            self.shared.frame.lock().ok()?.take()
        } else {
            None
        }
    }

    pub fn toggle(&mut self) {
        if self.state.playing {
            self.state.playing = false;
            let _ = self.tx.send(Msg::Pause);
        } else {
            // Ended: play again from the start, like macOS.
            if self.state.ended {
                self.seek(0.0);
            }
            self.state.playing = true;
            self.state.ended = false;
            let _ = self.tx.send(Msg::Play);
        }
    }

    pub fn pause(&mut self) {
        if self.state.playing {
            self.toggle();
        }
    }

    pub fn seek(&mut self, t: f64) {
        self.state.position = t;
        let _ = self.tx.send(Msg::Seek(t));
    }

    pub fn set_size(&self, w: u32, h: u32) {
        let _ = self.tx.send(Msg::Size(w, h));
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

impl Shared {
    fn fail(&self, why: &str, hwnd: isize, msg: u32, id: u32) {
        if let Ok(mut s) = self.state.lock() {
            s.error = Some(why.to_string());
            s.playing = false;
        }
        crate::win::post_wp(hwnd, msg, id as usize);
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        if let Ok(mut s) = self.state.lock() {
            f(&mut s);
        }
    }
}

/// Passes the engine's events to the worker thread.
#[implement(IMFMediaEngineNotify)]
struct Notify {
    tx: Mutex<Sender<Msg>>,
}

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, event: u32, _param1: usize, _param2: u32) -> windows::core::Result<()> {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(Msg::Event(event));
        }
        Ok(())
    }
}

/// Where frames land: on the GPU (and read back), or straight into a WIC bitmap
/// when there's no hardware device.
enum Target {
    Gpu { tex: ID3D11Texture2D, staging: ID3D11Texture2D, w: u32, h: u32 },
    Cpu { bmp: windows::Win32::Graphics::Imaging::IWICBitmap, w: u32, h: u32 },
}

impl Target {
    fn size(&self) -> (u32, u32) {
        match self {
            Target::Gpu { w, h, .. } | Target::Cpu { w, h, .. } => (*w, *h),
        }
    }
}

struct Engine {
    engine: IMFMediaEngine,
    device: Option<ID3D11Device>,
}

impl Engine {
    unsafe fn new(path: &Path, events: Sender<Msg>) -> Option<Engine> {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 3).ok()?;
        let attrs = attrs?;
        let notify: IMFMediaEngineNotify = Notify { tx: Mutex::new(events) }.into();
        attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify).ok()?;
        attrs.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32).ok()?;
        let device = gpu_device().and_then(|(device, manager)| attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager).ok().map(|_| device));
        let factory: IMFMediaEngineClassFactory = CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let engine = factory.CreateInstance(0, &attrs).ok()?;
        engine.SetSource(&BSTR::from(path.to_string_lossy().as_ref())).ok()?;
        // Tests play silently.
        #[cfg(test)]
        let _ = engine.SetMuted(true);
        engine.Play().ok()?;
        Some(Engine { engine, device })
    }

    unsafe fn run(self, rx: &std::sync::mpsc::Receiver<Msg>, shared: &Shared, ping: &dyn Fn()) {
        let e = &self.engine;
        let mut max_px = (0u32, 0u32);
        let mut target: Option<Target> = None;
        let (mut has_video, mut playing) = (false, true);
        let mut last_pts: Option<i64> = None;
        // Frames are looked for a while after a seek or resize too, paused or not.
        let mut look_until = Instant::now() + Duration::from_secs(2);
        let mut time_at = Instant::now();
        shared.update(|s| s.playing = true);
        loop {
            let busy = has_video && (playing || Instant::now() < look_until);
            let wait = if busy {
                Duration::from_millis(6)
            } else if playing {
                Duration::from_millis(250)
            } else {
                Duration::from_millis(1000)
            };
            let first = match rx.recv_timeout(wait) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let mut stop = false;
            for m in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
                match m {
                    Msg::Stop => stop = true,
                    Msg::Play => {
                        let _ = e.Play();
                        playing = true;
                        shared.update(|s| {
                            s.playing = true;
                            s.ended = false;
                        });
                    }
                    Msg::Pause => {
                        let _ = e.Pause();
                        playing = false;
                        shared.update(|s| s.playing = false);
                    }
                    Msg::Seek(t) => {
                        let _ = e.SetCurrentTime(t);
                        look_until = Instant::now() + Duration::from_secs(1);
                        shared.update(|s| {
                            s.position = t;
                            s.ended = false;
                        });
                    }
                    Msg::Size(w, h) => {
                        if (w, h) != max_px {
                            max_px = (w, h);
                            target = None;
                            last_pts = None;
                            look_until = Instant::now() + Duration::from_secs(1);
                        }
                    }
                    Msg::Event(ev) => {
                        let ev = MF_MEDIA_ENGINE_EVENT(ev as i32);
                        if ev == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA {
                            has_video = e.HasVideo().as_bool();
                            let (mut w, mut h) = (0u32, 0u32);
                            if has_video {
                                let _ = e.GetNativeVideoSize(Some(&mut w), Some(&mut h));
                            }
                            let d = e.GetDuration();
                            shared.update(|s| {
                                s.ready = true;
                                s.duration = if d.is_finite() { d } else { 0.0 };
                                s.video = (has_video && w > 0 && h > 0).then_some((w, h));
                            });
                            look_until = Instant::now() + Duration::from_secs(2);
                            ping();
                        } else if ev == MF_MEDIA_ENGINE_EVENT_ENDED {
                            playing = false;
                            shared.update(|s| {
                                s.playing = false;
                                s.ended = true;
                                s.position = s.duration;
                            });
                            ping();
                        } else if ev == MF_MEDIA_ENGINE_EVENT_ERROR {
                            playing = false;
                            shared.update(|s| {
                                s.playing = false;
                                s.error = Some("Can't play this file".into());
                            });
                            ping();
                        } else if ev == MF_MEDIA_ENGINE_EVENT_FIRSTFRAMEREADY {
                            look_until = Instant::now() + Duration::from_secs(1);
                        }
                    }
                }
            }
            if stop {
                break;
            }
            let mut news = false;
            if has_video && max_px.0 > 0 && max_px.1 > 0 {
                if let Ok(pts) = e.OnVideoStreamTick() {
                    if last_pts != Some(pts) {
                        if let Some(f) = self.frame(&mut target, max_px) {
                            last_pts = Some(pts);
                            if let Ok(mut slot) = shared.frame.lock() {
                                *slot = Some(f);
                            }
                            shared.fresh.store(true, Ordering::Release);
                            news = true;
                        }
                    }
                }
            }
            // The scrubber moves on its own a few times a second.
            if playing && time_at.elapsed() >= Duration::from_millis(250) || news {
                time_at = Instant::now();
                let t = e.GetCurrentTime();
                shared.update(|s| s.position = t);
                news = true;
            }
            if news {
                ping();
            }
        }
        let _ = e.Pause();
        let _ = e.Shutdown();
    }

    /// The current frame at the size it's shown at (fitted to `max_px`, never enlarged).
    unsafe fn frame(&self, target: &mut Option<Target>, max_px: (u32, u32)) -> Option<Frame> {
        let e = &self.engine;
        let (mut nw, mut nh) = (0u32, 0u32);
        e.GetNativeVideoSize(Some(&mut nw), Some(&mut nh)).ok()?;
        if nw == 0 || nh == 0 {
            return None;
        }
        // Non-square pixels (anamorphic DVDs): shown at their display shape.
        let (mut ax, mut ay) = (0u32, 0u32);
        let aspect = if e.GetVideoAspectRatio(Some(&mut ax), Some(&mut ay)).is_ok() && ax > 0 && ay > 0 {
            ax as f64 / ay as f64
        } else {
            nw as f64 / nh as f64
        };
        let (dw, dh) = (nh as f64 * aspect, nh as f64);
        let k = (max_px.0 as f64 / dw).min(max_px.1 as f64 / dh).min(1.0);
        let (w, h) = (((dw * k).round() as u32).max(2) & !1, ((dh * k).round() as u32).max(2) & !1);
        if target.as_ref().map(|t| t.size()) != Some((w, h)) {
            *target = self.make_target(w, h);
        }
        let dst = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        let mut data = vec![0u8; (w * h * 4) as usize];
        match target.as_ref()? {
            Target::Gpu { tex, staging, .. } => {
                e.TransferVideoFrame(tex, None, &dst, None).ok()?;
                let device = self.device.as_ref()?;
                let ctx = device.GetImmediateContext().ok()?;
                ctx.CopyResource(staging, tex);
                let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut map)).ok()?;
                let src = std::slice::from_raw_parts(map.pData as *const u8, (map.RowPitch * h) as usize);
                for y in 0..h as usize {
                    let row = &src[y * map.RowPitch as usize..][..w as usize * 4];
                    data[y * w as usize * 4..][..w as usize * 4].copy_from_slice(row);
                }
                ctx.Unmap(staging, 0);
            }
            Target::Cpu { bmp, .. } => {
                e.TransferVideoFrame(bmp, None, &dst, None).ok()?;
                bmp.CopyPixels(std::ptr::null(), w * 4, &mut data).ok()?;
            }
        }
        // Video is opaque, whatever the decoder left in alpha.
        for px in data.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Some(Frame { w, h, data })
    }

    unsafe fn make_target(&self, w: u32, h: u32) -> Option<Target> {
        if let Some(device) = &self.device {
            let mut desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex)).ok()?;
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            device.CreateTexture2D(&desc, None, Some(&mut staging)).ok()?;
            return Some(Target::Gpu { tex: tex?, staging: staging?, w, h });
        }
        use windows::Win32::Graphics::Imaging::*;
        let wic = crate::island::media::wic()?;
        let bmp = wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad).ok()?;
        Some(Target::Cpu { bmp, w, h })
    }
}

/// A hardware D3D device the engine decodes with, wrapped for Media Foundation.
unsafe fn gpu_device() -> Option<(ID3D11Device, IMFDXGIDeviceManager)> {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    let mut device: Option<ID3D11Device> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        None,
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        None,
    )
    .ok()?;
    let device = device?;
    // The engine uses it from its own threads while this one reads frames back.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    MFCreateDXGIDeviceManager(&mut token, &mut manager).ok()?;
    let manager = manager?;
    manager.ResetDevice(&device, token).ok()?;
    Some((device, manager))
}

/// "1:05" / "1:02:05".
pub fn format_time(t: f64) -> String {
    let s = if t.is_finite() { t.max(0.0) as u64 } else { 0 };
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}
