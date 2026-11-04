//! Widgets that move: Photo (a picture, a looping GIF or a folder slideshow),
//! Video (a file looping, muted) and Music Wave (a spectrum of what the
//! speakers are playing).
//!
//! Each runs on its own worker thread and hands the UI thread its latest
//! frame through a [`Feed`], posting the widgets' data message only when the
//! last frame has been taken (frames never queue up). While a widget is
//! covered by windows its feed is paused: the worker sleeps.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, HSTRING, PROPVARIANT};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::win::post;

/// How long each picture of a folder stays up.
const SLIDE: Duration = Duration::from_secs(30);
/// Video frames shown per second, at most.
const VIDEO_FPS: f64 = 30.0;
/// Spectrum bands worked out (views draw fewer by pairing them).
pub const BANDS: usize = 32;

/// Premultiplied BGRA, top-down.
#[derive(Clone)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

/// One worker's output and the switches it obeys.
#[derive(Default)]
pub struct Feed {
    frame: Mutex<Option<Frame>>,
    fresh: AtomicBool,
    paused: AtomicBool,
    stopped: AtomicBool,
    /// Why there's nothing to show ("Can't play this file").
    pub error: Mutex<Option<String>>,
}

impl Feed {
    fn publish(&self, f: Frame, hwnd: isize, msg: u32) {
        if let Ok(mut slot) = self.frame.lock() {
            *slot = Some(f);
        }
        if !self.fresh.swap(true, Ordering::AcqRel) {
            post(hwnd, msg);
        }
    }

    fn fail(&self, why: &str, hwnd: isize, msg: u32) {
        if let Ok(mut e) = self.error.lock() {
            *e = Some(why.to_string());
        }
        self.fresh.store(true, Ordering::Release);
        post(hwnd, msg);
    }

    /// The newest frame, if one came since the last call.
    pub fn take(&self) -> Option<Frame> {
        if self.fresh.swap(false, Ordering::AcqRel) {
            self.frame.lock().ok()?.take()
        } else {
            None
        }
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().ok()?.clone()
    }

    pub fn set_paused(&self, on: bool) {
        self.paused.store(on, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Sleeps `d` in short slices; false once stopped. Paused time doesn't count.
    fn sleep(&self, d: Duration) -> bool {
        let until = Instant::now() + d;
        loop {
            if self.stopped() {
                return false;
            }
            if self.paused.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            let now = Instant::now();
            if now >= until {
                return true;
            }
            std::thread::sleep((until - now).min(Duration::from_millis(100)));
        }
    }

    /// Waits while paused; true if it was (the caller re-times itself), None once stopped.
    fn hold(&self) -> Option<bool> {
        let mut held = false;
        while self.paused.load(Ordering::Relaxed) {
            if self.stopped() {
                return None;
            }
            held = true;
            std::thread::sleep(Duration::from_millis(200));
        }
        (!self.stopped()).then_some(held)
    }
}

pub type SharedFeed = Arc<Feed>;

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        f()
    }) {
        log!("{name} thread failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// Covered
// ---------------------------------------------------------------------------

/// Is `rect` (screen pixels) hidden behind an app window? Then its widget
/// needn't move. Transparent overlays, tool windows and cloaked windows (other
/// virtual desktops, suspended apps) don't count.
pub fn covered(rect: windows::Win32::Foundation::RECT) -> bool {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::UI::WindowsAndMessaging::*;
    struct Search {
        target: RECT,
        pid: u32,
        hit: bool,
    }
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
        let s = &mut *(lp.0 as *mut Search);
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return true.into();
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & (WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
            return true.into();
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == s.pid {
            return true.into();
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
        if cloaked != 0 {
            return true.into();
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_ok()
            && r.left <= s.target.left
            && r.top <= s.target.top
            && r.right >= s.target.right
            && r.bottom >= s.target.bottom
        {
            // The desktop itself (Progman / WorkerW) is below the widgets.
            let mut class = [0u16; 32];
            let n = GetClassNameW(hwnd, &mut class) as usize;
            let class = String::from_utf16_lossy(&class[..n]);
            if class != "Progman" && class != "WorkerW" {
                s.hit = true;
                return false.into();
            }
        }
        true.into()
    }
    let mut s = Search { target: rect, pid: unsafe { windows::Win32::System::Threading::GetCurrentProcessId() }, hit: false };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut s as *mut _ as isize));
    }
    s.hit
}

// ---------------------------------------------------------------------------
// Photo
// ---------------------------------------------------------------------------

const PICTURES: [&str; 11] = ["jpg", "jpeg", "png", "gif", "bmp", "webp", "heic", "heif", "tif", "tiff", "jxr"];

fn has_ext(p: &Path, list: &[&str]) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| list.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// The pictures a Photo widget shows: the file, or a folder's pictures in name order.
fn pictures(path: &Path) -> Vec<PathBuf> {
    if !path.is_dir() {
        return vec![path.to_path_buf()];
    }
    let mut v: Vec<PathBuf> =
        std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).filter(|p| has_ext(p, &PICTURES)).collect()).unwrap_or_default();
    v.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    v
}

/// Shows `path` (a picture, GIF or folder) into `feed`, at about `px` pixels
/// (the widget's size: pictures are decoded just big enough to cover it).
pub fn play_photo(path: PathBuf, px: (u32, u32), feed: SharedFeed, hwnd: isize, msg: u32) {
    spawn("photo widget", move || {
        let Some(wic) = crate::island::media::wic() else { return feed.fail("Pictures can't be opened", hwnd, msg) };
        let files = pictures(&path);
        if files.is_empty() {
            return feed.fail(if path.is_dir() { "No pictures in this folder" } else { "Picture not found" }, hwnd, msg);
        }
        let slideshow = files.len() > 1;
        let mut shown = 0usize;
        for file in files.iter().cycle() {
            if feed.stopped() {
                return;
            }
            let until = Instant::now() + SLIDE;
            match show_picture(&wic, file, px, &feed, hwnd, msg, slideshow.then_some(until)) {
                Some(()) => shown += 1,
                None => log!("photo widget: couldn't show {}", file.display()),
            }
            // Nothing in the folder opens: say so rather than spin.
            if shown == 0 && file == files.last().unwrap_or(file) {
                return feed.fail("These pictures can't be opened", hwnd, msg);
            }
            if !slideshow {
                // One still picture: shown, then nothing more to do.
                while feed.sleep(Duration::from_secs(3600)) {}
                return;
            }
            let left = until.saturating_duration_since(Instant::now());
            if !feed.sleep(left) {
                return;
            }
        }
    });
}

/// Shows one picture (a GIF plays until `until`, or for ever without one).
fn show_picture(
    wic: &IWICImagingFactory,
    file: &Path,
    px: (u32, u32),
    feed: &Feed,
    hwnd: isize,
    msg: u32,
    until: Option<Instant>,
) -> Option<()> {
    unsafe {
        let decoder = wic
            .CreateDecoderFromFilename(&HSTRING::from(file.as_os_str()), None, windows::Win32::Foundation::GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frames = decoder.GetFrameCount().ok()?;
        let gif = decoder.GetContainerFormat().ok() == Some(GUID_ContainerFormatGif);
        if gif && frames > 1 {
            return play_gif(wic, &decoder, frames, feed, hwnd, msg, until);
        }
        let frame = decoder.GetFrame(0).ok()?;
        let source: IWICBitmapSource = upright(wic, &frame).unwrap_or_else(|| frame.cast().unwrap());
        let (mut w, mut h) = (0, 0);
        source.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 {
            return None;
        }
        let k = (px.0 as f64 / w as f64).max(px.1 as f64 / h as f64).min(1.0);
        let (ow, oh) = (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1));
        let scaler = wic.CreateBitmapScaler().ok()?;
        scaler.Initialize(&source, ow, oh, WICBitmapInterpolationModeHighQualityCubic).ok()?;
        let data = pbgra(wic, &scaler.cast().ok()?, ow, oh)?;
        feed.publish(Frame { w: ow, h: oh, data }, hwnd, msg);
        Some(())
    }
}

/// A photo turned the way its camera says (EXIF orientation).
unsafe fn upright(wic: &IWICImagingFactory, frame: &IWICBitmapFrameDecode) -> Option<IWICBitmapSource> {
    let reader = frame.GetMetadataQueryReader().ok()?;
    let orientation = ["/app1/ifd/{ushort=274}", "/ifd/{ushort=274}"].iter().find_map(|q| {
        let mut v = PROPVARIANT::default();
        reader.GetMetadataByName(&HSTRING::from(*q), &mut v).ok()?;
        u16::try_from(&v).ok()
    })?;
    let turn = match orientation {
        3 => WICBitmapTransformRotate180,
        6 => WICBitmapTransformRotate90,
        8 => WICBitmapTransformRotate270,
        _ => return None,
    };
    let rot = wic.CreateBitmapFlipRotator().ok()?;
    rot.Initialize(frame, turn).ok()?;
    rot.cast().ok()
}

/// `src` as premultiplied BGRA.
unsafe fn pbgra(wic: &IWICImagingFactory, src: &IWICBitmapSource, w: u32, h: u32) -> Option<Vec<u8>> {
    let conv = wic.CreateFormatConverter().ok()?;
    conv.Initialize(src, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
    let mut data = vec![0u8; (w * h * 4) as usize];
    conv.CopyPixels(std::ptr::null(), w * 4, &mut data).ok()?;
    Some(data)
}

unsafe fn meta_u16(reader: &IWICMetadataQueryReader, name: &str) -> Option<u16> {
    let mut v = PROPVARIANT::default();
    reader.GetMetadataByName(&HSTRING::from(name), &mut v).ok()?;
    u16::try_from(&v).ok()
}

/// Plays a GIF's frames onto a canvas of its logical screen size, honouring
/// each frame's offset, delay and disposal.
unsafe fn play_gif(
    wic: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    frames: u32,
    feed: &Feed,
    hwnd: isize,
    msg: u32,
    until: Option<Instant>,
) -> Option<()> {
    let first = decoder.GetFrame(0).ok()?;
    let (mut fw, mut fh) = (0, 0);
    first.GetSize(&mut fw, &mut fh).ok()?;
    let (cw, ch) = match decoder.GetMetadataQueryReader() {
        Ok(r) => (meta_u16(&r, "/logscrdesc/Width").map_or(fw, u32::from), meta_u16(&r, "/logscrdesc/Height").map_or(fh, u32::from)),
        Err(_) => (fw, fh),
    };
    if cw == 0 || ch == 0 {
        return None;
    }
    let mut canvas = vec![0u8; (cw * ch * 4) as usize];
    let mut saved: Option<Vec<u8>> = None;
    // What the last frame asked to happen after it: (disposal, rect).
    let mut dispose: Option<(u16, (u32, u32, u32, u32))> = None;
    loop {
        for i in 0..frames {
            let started = Instant::now();
            match dispose.take() {
                Some((2, (x, y, w, h))) => clear(&mut canvas, cw, ch, x, y, w, h),
                Some((3, _)) => {
                    if let Some(s) = saved.take() {
                        canvas = s;
                    }
                }
                _ => {}
            }
            let Ok(frame) = decoder.GetFrame(i) else { continue };
            let (mut w, mut h) = (0, 0);
            if frame.GetSize(&mut w, &mut h).is_err() {
                continue;
            }
            let reader = frame.GetMetadataQueryReader().ok();
            let get = |n: &str| reader.as_ref().and_then(|r| meta_u16(r, n));
            let (x, y) = (get("/imgdesc/Left").unwrap_or(0) as u32, get("/imgdesc/Top").unwrap_or(0) as u32);
            let delay = get("/grctlext/Delay").unwrap_or(10).max(2) as u64 * 10;
            let disposal = get("/grctlext/Disposal").unwrap_or(0);
            if disposal == 3 {
                saved = Some(canvas.clone());
            }
            if let Some(src) = frame.cast::<IWICBitmapSource>().ok().and_then(|s| pbgra(wic, &s, w, h)) {
                over(&mut canvas, cw, ch, &src, w, h, x, y);
            }
            dispose = Some((disposal, (x, y, w, h)));
            feed.publish(Frame { w: cw, h: ch, data: canvas.clone() }, hwnd, msg);
            let wait = Duration::from_millis(delay).saturating_sub(started.elapsed());
            if !feed.sleep(wait) {
                return Some(());
            }
            if until.is_some_and(|u| Instant::now() >= u) {
                return Some(());
            }
        }
        // The canvas starts afresh each loop.
        canvas.iter_mut().for_each(|b| *b = 0);
        saved = None;
        dispose = None;
    }
}

fn clear(canvas: &mut [u8], cw: u32, ch: u32, x: u32, y: u32, w: u32, h: u32) {
    for row in y..(y + h).min(ch) {
        let a = ((row * cw + x.min(cw)) * 4) as usize;
        let b = ((row * cw + (x + w).min(cw)) * 4) as usize;
        canvas[a..b].iter_mut().for_each(|v| *v = 0);
    }
}

/// Premultiplied `src` (w × h) drawn over `canvas` at (x, y).
#[allow(clippy::too_many_arguments)]
fn over(canvas: &mut [u8], cw: u32, ch: u32, src: &[u8], w: u32, h: u32, x: u32, y: u32) {
    for row in 0..h {
        let cy = y + row;
        if cy >= ch {
            break;
        }
        for col in 0..w {
            let cx = x + col;
            if cx >= cw {
                break;
            }
            let s = ((row * w + col) * 4) as usize;
            let a = src[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = ((cy * cw + cx) * 4) as usize;
            if a == 255 {
                canvas[d..d + 4].copy_from_slice(&src[s..s + 4]);
            } else {
                for k in 0..4 {
                    canvas[d + k] = (src[s + k] as u32 + canvas[d + k] as u32 * (255 - a) / 255).min(255) as u8;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------

/// Plays `path` muted and looping into `feed`, scaled to about `px`.
pub fn play_video(path: PathBuf, px: (u32, u32), feed: SharedFeed, hwnd: isize, msg: u32) {
    use windows::Win32::Media::MediaFoundation::*;
    spawn("video widget", move || unsafe {
        if MFStartup(MF_VERSION, MFSTARTUP_LITE).is_err() {
            return feed.fail("Video playback isn't available", hwnd, msg);
        }
        if !path.is_file() {
            feed.fail("Video not found", hwnd, msg);
        } else {
            // The GPU decodes when it can; plain software decoding otherwise.
            let played = video_loop(&path, px, &feed, hwnd, msg, true).or_else(|| video_loop(&path, px, &feed, hwnd, msg, false));
            if played.is_none() {
                feed.fail("Can't play this video", hwnd, msg);
            }
        }
        let _ = MFShutdown();
    });
}

/// A D3D device for the decoder, wrapped for Media Foundation.
unsafe fn video_device() -> Option<windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager> {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Media::MediaFoundation::*;
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
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    MFCreateDXGIDeviceManager(&mut token, &mut manager).ok()?;
    let manager = manager?;
    manager.ResetDevice(&device, token).ok()?;
    Some(manager)
}

/// Plays until stopped (Some) or fails to start (None).
unsafe fn video_loop(path: &Path, px: (u32, u32), feed: &Feed, hwnd: isize, msg: u32, gpu: bool) -> Option<()> {
    use windows::Win32::Media::MediaFoundation::*;
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 3).ok()?;
    let attrs = attrs?;
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1).ok()?;
    if gpu {
        let manager = video_device()?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1).ok()?;
        attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager).ok()?;
    }
    let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs).ok()?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).ok()?;
    reader.SetStreamSelection(stream, true).ok()?;
    // Decode to RGB just big enough to cover the widget, keeping the shape.
    let native = reader.GetNativeMediaType(stream, 0).ok()?;
    let size = native.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
    let (nw, nh) = ((size >> 32) as u32, size as u32);
    if nw == 0 || nh == 0 {
        return None;
    }
    let k = (px.0 as f64 / nw as f64).max(px.1 as f64 / nh as f64).min(1.0);
    let even = |v: f64| ((v.round() as u32).max(2) + 1) & !1;
    let (ow, oh) = (even(nw as f64 * k), even(nh as f64 * k));
    let out = MFCreateMediaType().ok()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ok()?;
    out.SetUINT64(&MF_MT_FRAME_SIZE, ((ow as u64) << 32) | oh as u64).ok()?;
    if reader.SetCurrentMediaType(stream, None, &out).is_err() {
        // Not scaled after all: RGB at its own size.
        out.DeleteItem(&MF_MT_FRAME_SIZE).ok()?;
        reader.SetCurrentMediaType(stream, None, &out).ok()?;
    }
    let cur = reader.GetCurrentMediaType(stream).ok()?;
    let size = cur.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
    let (w, h) = ((size >> 32) as u32, size as u32);
    log!("video widget: {}x{} -> {w}x{h} ({})", nw, nh, if gpu { "GPU" } else { "software" });

    let step = Duration::from_secs_f64(1.0 / VIDEO_FPS);
    // When the clip's time 0 is (re-set at each loop and after a pause).
    let mut base: Option<Instant> = None;
    let mut last_shown: Option<i64> = None;
    let mut frames = 0u64;
    loop {
        match feed.hold() {
            None => return Some(()),
            Some(true) => base = None,
            Some(false) => {}
        }
        let (mut flags, mut ts) = (0u32, 0i64);
        let mut sample: Option<IMFSample> = None;
        if reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)).is_err() {
            return (frames > 0).then_some(());
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            if frames == 0 {
                return None;
            }
            reader.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from(0i64)).ok()?;
            base = None;
            last_shown = None;
            continue;
        }
        let Some(sample) = sample else { continue };
        // Frames closer together than the cap are skipped (decoded, not shown).
        if last_shown.is_some_and(|l| ((ts - l) as f64) < 1e7 / VIDEO_FPS * 0.9) {
            continue;
        }
        let at = Duration::from_nanos(ts.max(0) as u64 * 100);
        let start = *base.get_or_insert_with(|| Instant::now() - at);
        let due = start + at;
        let now = Instant::now();
        if now > due + Duration::from_millis(500) {
            // Far behind (a stall): carry on from here.
            base = Some(now - at);
        } else if due > now {
            if !feed.sleep(due - now) {
                return Some(());
            }
        } else if now > due + step {
            continue;
        }
        if let Some(data) = sample_pixels(&sample, w, h) {
            feed.publish(Frame { w, h, data }, hwnd, msg);
            last_shown = Some(ts);
            frames += 1;
        }
    }
}

/// A decoded RGB32 sample's pixels as opaque BGRA.
unsafe fn sample_pixels(sample: &windows::Win32::Media::MediaFoundation::IMFSample, w: u32, h: u32) -> Option<Vec<u8>> {
    use windows::Win32::Media::MediaFoundation::*;
    let buf = sample.ConvertToContiguousBuffer().ok()?;
    let row = (w * 4) as usize;
    let mut out = vec![0u8; row * h as usize];
    if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
        let (mut p, mut pitch) = (std::ptr::null_mut::<u8>(), 0i32);
        b2.Lock2D(&mut p, &mut pitch).ok()?;
        for y in 0..h as usize {
            let src = p.offset(pitch as isize * y as isize);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(y * row), row);
        }
        let _ = b2.Unlock2D();
    } else {
        let (mut p, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
        buf.Lock(&mut p, None, Some(&mut len)).ok()?;
        let n = (len as usize).min(out.len());
        std::ptr::copy_nonoverlapping(p, out.as_mut_ptr(), n);
        let _ = buf.Unlock();
    }
    for px in out.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Music Wave
// ---------------------------------------------------------------------------

/// Spectrum levels, 0..1 per band, low to high.
#[derive(Default)]
pub struct Bands {
    levels: Mutex<Vec<f32>>,
    fresh: AtomicBool,
    paused: AtomicBool,
    stopped: AtomicBool,
}

pub type SharedBands = Arc<Bands>;

impl Bands {
    pub fn take(&self) -> Option<Vec<f32>> {
        if self.fresh.swap(false, Ordering::AcqRel) {
            self.levels.lock().ok().map(|l| l.clone())
        } else {
            None
        }
    }
    pub fn set_paused(&self, on: bool) {
        self.paused.store(on, Ordering::Relaxed);
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

const FFT: usize = 1024;

/// Listens to the default speakers (loopback) and keeps `bands` up to date,
/// about 30 times a second while there's sound; silent, it stays quiet.
pub fn listen(bands: SharedBands, hwnd: isize, msg: u32) {
    spawn("music wave", move || {
        while !bands.stopped.load(Ordering::Relaxed) {
            if let Err(e) = unsafe { capture(&bands, hwnd, msg) } {
                log!("music wave: {e}");
            }
            // The device went away (headphones unplugged...): try again shortly.
            for _ in 0..10 {
                if bands.stopped.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
}

unsafe fn capture(bands: &Bands, hwnd: isize, msg: u32) -> windows::core::Result<()> {
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
    let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let device = devices.GetDefaultAudioEndpoint(eRender, eConsole)?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let fmt = client.GetMixFormat()?;
    let (channels, rate, bits) = ((*fmt).nChannels.max(1) as usize, (*fmt).nSamplesPerSec as f32, (*fmt).wBitsPerSample);
    let tag = (*fmt).wFormatTag as u32;
    // Extensible formats carry their real type in a sub-format; 32-bit mix formats are float.
    const WAVE_FORMAT_IEEE_FLOAT: u32 = 3;
    let float = tag == WAVE_FORMAT_IEEE_FLOAT || (tag == 0xFFFE && bits == 32);
    let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, fmt, None);
    CoTaskMemFree(Some(fmt as *const _));
    init?;
    let cap: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
    let mut running = true;
    let mut ring = vec![0f32; FFT];
    let mut at = 0usize;
    let mut levels = vec![0f32; BANDS];
    let mut last = Instant::now();
    let mut quiet = true;
    let window: Vec<f32> = (0..FFT).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT as f32).cos()).collect();
    loop {
        if bands.stopped.load(Ordering::Relaxed) {
            let _ = client.Stop();
            return Ok(());
        }
        if bands.paused.load(Ordering::Relaxed) {
            if running {
                let _ = client.Stop();
                running = false;
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        if !running {
            client.Start()?;
            running = true;
        }
        std::thread::sleep(Duration::from_millis(15));
        let mut got = false;
        while cap.GetNextPacketSize()? > 0 {
            let (mut data, mut n, mut flags) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
            cap.GetBuffer(&mut data, &mut n, &mut flags, None, None)?;
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            for f in 0..n as usize {
                let mut v = 0.0;
                if !silent {
                    for ch in 0..channels {
                        let i = f * channels + ch;
                        v += if float {
                            *(data as *const f32).add(i)
                        } else if bits == 16 {
                            *(data as *const i16).add(i) as f32 / 32768.0
                        } else {
                            0.0
                        };
                    }
                }
                ring[at] = v / channels as f32;
                at = (at + 1) % FFT;
            }
            cap.ReleaseBuffer(n)?;
            got = true;
        }
        if last.elapsed() < Duration::from_millis(33) {
            continue;
        }
        last = Instant::now();
        // No packets at all: the speakers are silent; the bars fall.
        let fresh = if got { spectrum(&ring, at, &window, rate) } else { vec![0.0; BANDS] };
        let mut moving = false;
        for (l, f) in levels.iter_mut().zip(fresh) {
            // Quick to rise, slow to fall, like a real meter.
            let next = if f > *l { *l + (f - *l) * 0.7 } else { *l * 0.82 };
            let next = if next < 0.01 { 0.0 } else { next };
            moving |= (next - *l).abs() > 0.002 || next > 0.0;
            *l = next;
        }
        if moving || !quiet {
            if let Ok(mut s) = bands.levels.lock() {
                s.clone_from(&levels);
            }
            if !bands.fresh.swap(true, Ordering::AcqRel) {
                post(hwnd, msg);
            }
        }
        quiet = !moving;
    }
}

/// Band levels (0..1) of the newest `FFT` samples in `ring` (oldest at `at`).
fn spectrum(ring: &[f32], at: usize, window: &[f32], rate: f32) -> Vec<f32> {
    let mut re: Vec<f32> = (0..FFT).map(|i| ring[(at + i) % FFT] * window[i]).collect();
    let mut im = vec![0f32; FFT];
    fft(&mut re, &mut im);
    let bin_hz = rate / FFT as f32;
    let (lo, hi) = (40.0f32, 16_000.0f32.min(rate / 2.0));
    (0..BANDS)
        .map(|b| {
            // Log-spaced, as hearing is.
            let f0 = lo * (hi / lo).powf(b as f32 / BANDS as f32);
            let f1 = lo * (hi / lo).powf((b + 1) as f32 / BANDS as f32);
            let (i0, i1) = ((f0 / bin_hz) as usize, ((f1 / bin_hz) as usize).max((f0 / bin_hz) as usize + 1));
            let peak = (i0..i1.min(FFT / 2)).map(|i| (re[i] * re[i] + im[i] * im[i]).sqrt()).fold(0.0f32, f32::max);
            // About -60 dB .. 0 dB; highs are tilted up (music has less energy there).
            let db = 20.0 * (peak / (FFT as f32 / 4.0)).max(1e-6).log10() + 6.0 * (b as f32 / BANDS as f32) * 2.0;
            ((db + 60.0) / 60.0).clamp(0.0, 1.0)
        })
        .collect()
}

/// In-place radix-2 FFT.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f32::consts::TAU / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                (cr, ci) = (cr * wr - ci * wi, cr * wi + ci * wr);
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_a_tone() {
        let rate = 48_000.0;
        let ring: Vec<f32> = (0..FFT).map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / rate).sin()).collect();
        let window: Vec<f32> = (0..FFT).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT as f32).cos()).collect();
        let s = spectrum(&ring, 0, &window, rate);
        let loudest = s.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        // 1 kHz sits about 46% of the way along 40 Hz..16 kHz on a log scale.
        assert!((13..=17).contains(&loudest), "loudest band {loudest}: {s:?}");
        assert!(s[0] < s[loudest] * 0.6);
    }

    #[test]
    fn gif_compositing() {
        let mut canvas = vec![0u8; 4 * 4 * 4];
        let src = vec![255u8; 2 * 2 * 4];
        over(&mut canvas, 4, 4, &src, 2, 2, 1, 1);
        assert_eq!(canvas[((1 * 4 + 1) * 4) as usize], 255);
        assert_eq!(canvas[0], 0);
        clear(&mut canvas, 4, 4, 1, 1, 2, 2);
        assert!(canvas.iter().all(|&b| b == 0));
    }

    /// This process's CPU time so far, in ms.
    fn cpu_ms() -> f64 {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut a, mut b, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe {
            let _ = GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u);
        }
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 10_000.0;
        t(k) + t(u)
    }

    /// Plays Windows' own sample GIF and video for a few seconds, counting
    /// frames: `cargo test live_players -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_players() {
        let media = PathBuf::from(r"C:\Windows\SystemApps\Microsoft.Windows.CloudExperienceHost_cw5n1h2txyewy\media");
        let mut clips = vec![(media.join("CortanaAnimation.gif"), false), (media.join("oobe-intro.mp4"), true)];
        // Another video to measure: DOCKSIDE_VIDEO=path.
        if let Ok(p) = std::env::var("DOCKSIDE_VIDEO") {
            clips.push((PathBuf::from(p), true));
        }
        for (path, video) in clips {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let feed = SharedFeed::default();
            if video {
                play_video(path, (364, 364), feed.clone(), 0, 0);
            } else {
                play_photo(path, (364, 364), feed.clone(), 0, 0);
            }
            // Measured from the first frame: start-up isn't the steady cost.
            let first = Instant::now();
            while feed.take().is_none() && first.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let (cpu0, started) = (cpu_ms(), Instant::now());
            let (mut frames, mut size) = (0, (0, 0));
            while started.elapsed() < Duration::from_secs(4) {
                if let Some(f) = feed.take() {
                    frames += 1;
                    size = (f.w, f.h);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let cpu = cpu_ms() - cpu0;
            feed.stop();
            println!("{name}: {frames} frames in 4 s at {size:?}, {:.1}% of one core, error {:?}", cpu / 40.0, feed.error());
            assert!(frames > 5, "{name} barely played");
        }
    }

    /// Listens to the speakers for 4 s: `cargo test live_listen -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_listen() {
        let bands = SharedBands::default();
        listen(bands.clone(), 0, 0);
        std::thread::sleep(Duration::from_millis(500));
        let (cpu0, started) = (cpu_ms(), Instant::now());
        let (mut updates, mut loudest) = (0, 0.0f32);
        while started.elapsed() < Duration::from_secs(4) {
            if let Some(l) = bands.take() {
                updates += 1;
                loudest = l.iter().copied().fold(loudest, f32::max);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        bands.stop();
        println!("{updates} updates in 4 s, loudest band {loudest:.2}, {:.1}% of one core", (cpu_ms() - cpu0) / 40.0);
    }

    #[test]
    fn folders_list_pictures() {
        let dir = std::env::temp_dir().join(format!("dockside-photos-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["b.JPG", "a.png", "notes.txt"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        let names: Vec<String> = pictures(&dir).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a.png", "b.JPG"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
