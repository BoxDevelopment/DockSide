//! Icon loading: shell icons (exe, lnk, folders, Store apps) and image files,
//! decoded once to premultiplied BGRA and uploaded per render target.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use windows::core::{GUID, HSTRING};
use windows::Win32::Foundation::{GENERIC_READ, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap, ID2D1RenderTarget, D2D1_BITMAP_PROPERTIES};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
};

pub const ICON_PX: i32 = 256;
pub const RECYCLE_BIN: &str = "::{645FF040-5081-101B-9F08-00AA002F954E}";

pub struct IconPixels {
    pub width: u32,
    pub height: u32,
    /// Premultiplied BGRA, top-down.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for IconPixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IconPixels({}x{})", self.width, self.height)
    }
}

impl IconPixels {
    pub fn to_bitmap(&self, rt: &ID2D1RenderTarget) -> Option<ID2D1Bitmap> {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        unsafe {
            rt.CreateBitmap(
                D2D_SIZE_U { width: self.width, height: self.height },
                Some(self.data.as_ptr() as *const c_void),
                self.width * 4,
                &props,
            )
            .ok()
        }
    }
}

/// Process-wide pixel cache, keyed by source (survives surface rebuilds).
#[derive(Default)]
pub struct IconStore {
    cache: HashMap<String, Option<Rc<IconPixels>>>,
    /// The longest side an image file is kept at (see `set_max_px`); 0 for `ICON_PX`.
    max_px: u32,
    /// Background loading for `request` (the Dock), started on first use.
    worker: Option<Worker>,
    /// Window and message the worker pings when results are ready.
    notify: Option<(isize, u32)>,
    pending: HashSet<String>,
    /// Bumped when cached sizes go stale, so loads already queued are dropped.
    generation: u64,
}

struct Job {
    key: String,
    parse_name: String,
    custom: Option<String>,
    max: u32,
    generation: u64,
}

struct Worker {
    tx: Sender<Job>,
    rx: Receiver<(String, u64, Option<IconPixels>)>,
    pinged: Arc<AtomicBool>,
}

impl IconStore {
    /// `custom` (an image file) wins over the shell icon of `parse_name`.
    /// Loaded here and now if it isn't cached yet; see `request` for the
    /// background way.
    pub fn get(&mut self, wic: &IWICImagingFactory, parse_name: &str, custom: Option<&str>) -> Option<Rc<IconPixels>> {
        let key = custom.unwrap_or(parse_name).to_string();
        let max = self.max();
        self.pending.remove(&key);
        self.cache.entry(key).or_insert_with(|| load(wic, parse_name, custom, max).map(Rc::new)).clone()
    }

    /// Where results of `request` are announced: `msg` posted to `hwnd`.
    pub fn set_notify(&mut self, hwnd: isize, msg: u32) {
        self.notify = Some((hwnd, msg));
    }

    /// Like `get`, but never waits: an icon that isn't cached is loaded on a
    /// worker thread (from the disk cache when it can be) and None comes back
    /// until it arrives (see `is_pending` and `drain`). At login, decoding the
    /// Dock's icons and asking the shell for them took seconds.
    pub fn request(&mut self, key: &str, parse_name: &str, custom: Option<&str>) -> Option<Rc<IconPixels>> {
        if let Some(px) = self.cache.get(key) {
            return px.clone();
        }
        let notify = self.notify?;
        if self.pending.contains(key) {
            return None;
        }
        let job = Job {
            key: key.to_string(),
            parse_name: parse_name.to_string(),
            custom: custom.map(str::to_string),
            max: self.max(),
            generation: self.generation,
        };
        let worker = self.worker.get_or_insert_with(|| Worker::start(notify));
        if worker.tx.send(job).is_ok() {
            self.pending.insert(key.to_string());
        }
        None
    }

    /// Whether `key` was requested and hasn't arrived yet.
    pub fn is_pending(&self, key: &str) -> bool {
        self.pending.contains(key)
    }

    /// Takes in what the worker finished; true if anything arrived.
    pub fn drain(&mut self) -> bool {
        let Some(w) = &self.worker else { return false };
        w.pinged.store(false, Ordering::Release);
        let mut any = false;
        while let Ok((key, generation, px)) = w.rx.try_recv() {
            if generation != self.generation || !self.pending.remove(&key) {
                continue;
            }
            self.cache.insert(key, px.map(Rc::new));
            any = true;
        }
        any
    }

    fn max(&self) -> u32 {
        if self.max_px == 0 { ICON_PX as u32 } else { self.max_px }
    }
}

impl Worker {
    fn start((hwnd, msg): (isize, u32)) -> Self {
        let (tx, jobs) = channel::<Job>();
        let (done, rx) = channel();
        let pinged = Arc::new(AtomicBool::new(false));
        let flag = pinged.clone();
        let spawned = std::thread::Builder::new().name("icons".into()).spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            let wic: Option<IWICImagingFactory> = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() };
            let Some(wic) = wic else {
                return log!("icons: no WIC factory");
            };
            disk::prune();
            while let Ok(job) = jobs.recv() {
                let px = load(&wic, &job.parse_name, job.custom.as_deref(), job.max);
                if done.send((job.key, job.generation, px)).is_err() {
                    break;
                }
                if !flag.swap(true, Ordering::AcqRel) {
                    crate::win::post(hwnd, msg);
                }
            }
        });
        if let Err(e) = spawned {
            log!("icons: worker didn't start: {e}");
        }
        Self { tx, rx, pinged }
    }
}

/// An icon's pixels: from the disk cache when it has them, otherwise
/// `custom` (an image file, fitted to `max`) or the shell icon of `parse_name`,
/// then saved to the disk cache.
fn load(wic: &IWICImagingFactory, parse_name: &str, custom: Option<&str>, max: u32) -> Option<IconPixels> {
    let cached = disk::path(parse_name, custom, max);
    if let Some(px) = cached.as_deref().and_then(disk::read) {
        return Some(px);
    }
    let px = match custom {
        Some(file) => load_image_fit(wic, file, max).or_else(|| load_shell_icon(parse_name)),
        None => load_shell_icon(parse_name),
    };
    match (&px, cached) {
        (None, _) => log!("no icon for {}", custom.unwrap_or(parse_name)),
        (Some(px), Some(path)) => disk::write(&path, px),
        _ => {}
    }
    px
}

/// Icons as they were last loaded, already fitted, so a cold start (login)
/// reads one small file each instead of decoding a 1024 px picture or asking
/// the shell. Keyed by the source file's path, size and modified time, so a
/// changed icon or an updated app is loaded afresh.
mod disk {
    use super::IconPixels;
    use std::path::{Path, PathBuf};

    const VERSION: u32 = 1;
    /// More files than this and the cache is emptied (old icons pile up as apps update).
    const MAX_FILES: usize = 400;

    fn dir() -> PathBuf {
        crate::config::dir().join("cache").join("icons")
    }

    /// The cache file for an icon, or None for one that doesn't come from a
    /// file (a shell namespace item, say: nothing says when it changed).
    pub fn path(parse_name: &str, custom: Option<&str>, max: u32) -> Option<PathBuf> {
        let source = custom.unwrap_or(parse_name);
        let meta = std::fs::metadata(source).ok()?;
        let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
        let id = format!("{VERSION}|{parse_name}|{}|{max}|{}|{modified}", custom.unwrap_or(""), meta.len());
        let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
        Some(dir().join(format!("{hash:016x}.bgra")))
    }

    /// Width and height (little-endian u32s), then premultiplied BGRA rows.
    pub fn read(path: &Path) -> Option<IconPixels> {
        let bytes = std::fs::read(path).ok()?;
        let word = |i: usize| -> Option<u32> { Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?)) };
        let (width, height) = (word(0)?, word(4)?);
        let size = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
        if width == 0 || height == 0 || bytes.len() != 8 + size {
            return None;
        }
        Some(IconPixels { width, height, data: bytes[8..].to_vec() })
    }

    pub fn write(path: &Path, px: &IconPixels) {
        let mut bytes = Vec::with_capacity(8 + px.data.len());
        bytes.extend_from_slice(&px.width.to_le_bytes());
        bytes.extend_from_slice(&px.height.to_le_bytes());
        bytes.extend_from_slice(&px.data);
        // Written beside and renamed into place, so a half-written file is never read.
        let tmp = path.with_extension("tmp");
        let r = std::fs::create_dir_all(dir()).and_then(|_| std::fs::write(&tmp, &bytes)).and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = r {
            log!("icon cache: {e}");
        }
    }

    pub fn prune() {
        let Ok(entries) = std::fs::read_dir(dir()) else { return };
        let files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        if files.len() > MAX_FILES {
            files.iter().for_each(|f| drop(std::fs::remove_file(f)));
        }
    }
}

impl IconStore {
    /// The biggest any icon is drawn, in pixels. Image files are kept no
    /// bigger than that: an icon pack's 1024 px PNG is 4 MB decoded, and
    /// again as each bitmap made from it, to be drawn 50-odd pixels wide.
    pub fn set_max_px(&mut self, px: u32) {
        let px = px.next_power_of_two().clamp(ICON_PX as u32, 1024);
        // Bigger than before: what's cached was fitted too small.
        if px > self.max_px.max(ICON_PX as u32) {
            self.cache.clear();
            self.pending.clear();
            self.generation += 1;
        }
        self.max_px = px;
    }

    /// An icon DockSide draws itself, made by `make` on first use.
    pub fn builtin(&mut self, key: &str, make: impl FnOnce() -> Option<IconPixels>) -> Option<Rc<IconPixels>> {
        self.cache.entry(key.to_string()).or_insert_with(|| make().map(Rc::new)).clone()
    }
}

pub fn load_shell_icon(parse_name: &str) -> Option<IconPixels> {
    load_shell_image(parse_name, ICON_PX, false)
}

/// Shell image for `parse_name` at `size` px. With `thumbnail`, images, videos
/// and documents return a preview of their content instead of a type icon.
pub fn load_shell_image(parse_name: &str, size: i32, thumbnail: bool) -> Option<IconPixels> {
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(parse_name), None).ok()?;
        let flags = if thumbnail { SIIGBF_BIGGERSIZEOK } else { SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK };
        let hbmp = factory.GetImage(SIZE { cx: size, cy: size }, flags).ok()?;
        let px = read_hbitmap(hbmp);
        let _ = DeleteObject(hbmp);
        px
    }
}

/// Pixels of a bitmap someone else owns (e.g. a shell context menu item's).
pub fn hbitmap_pixels(hbmp: HBITMAP) -> Option<IconPixels> {
    unsafe { read_hbitmap(hbmp) }
}

unsafe fn read_hbitmap(hbmp: HBITMAP) -> Option<IconPixels> {
    let mut bm = BITMAP::default();
    if GetObjectW(hbmp, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut data = vec![0u8; (w * h * 4) as usize];
    let dc = CreateCompatibleDC(None);
    let lines = GetDIBits(dc, hbmp, 0, h as u32, Some(data.as_mut_ptr() as *mut c_void), &mut bmi, DIB_RGB_COLORS);
    let _ = DeleteDC(dc);
    if lines == 0 {
        return None;
    }
    normalize_alpha(&mut data);
    Some(IconPixels { width: w as u32, height: h as u32, data })
}

/// Shell bitmaps are usually premultiplied, but legacy icons may have no alpha
/// at all or straight alpha. Fix both cases up.
fn normalize_alpha(data: &mut [u8]) {
    if data.chunks_exact(4).all(|p| p[3] == 0) {
        data.chunks_exact_mut(4).for_each(|p| p[3] = 255);
        return;
    }
    let straight = data.chunks_exact(4).any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]);
    if straight {
        for p in data.chunks_exact_mut(4) {
            let a = p[3] as u32;
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

pub fn load_image_file(wic: &IWICImagingFactory, path: &str) -> Option<IconPixels> {
    load_image_fit(wic, path, u32::MAX)
}

/// An image file's pixels, scaled down (never up) so neither side is over `max`.
pub fn load_image_fit(wic: &IWICImagingFactory, path: &str, max: u32) -> Option<IconPixels> {
    unsafe {
        let decoder = wic
            .CreateDecoderFromFilename(&HSTRING::from(path), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .ok()?;
        // .ico files hold several frames; take the largest.
        let count = decoder.GetFrameCount().ok()?;
        let mut best: Option<(IWICBitmapFrameDecode, u32)> = None;
        for i in 0..count {
            let frame = decoder.GetFrame(i).ok()?;
            let (mut w, mut h) = (0, 0);
            frame.GetSize(&mut w, &mut h).ok()?;
            if best.as_ref().map(|(_, s)| w * h > *s).unwrap_or(true) {
                best = Some((frame, w * h));
            }
        }
        let (frame, _) = best?;
        let converter = wic.CreateFormatConverter().ok()?;
        let format: GUID = GUID_WICPixelFormat32bppPBGRA;
        converter
            .Initialize(&frame, &format, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .ok()?;
        let (mut w, mut h) = (0, 0);
        converter.GetSize(&mut w, &mut h).ok()?;
        if w.max(h) > max {
            // Premultiplied already, so edges don't fringe as they're averaged.
            let k = max as f32 / w.max(h) as f32;
            let (sw, sh) = (((w as f32 * k).round() as u32).max(1), ((h as f32 * k).round() as u32).max(1));
            let scaler = wic.CreateBitmapScaler().ok()?;
            scaler.Initialize(&converter, sw, sh, WICBitmapInterpolationModeFant).ok()?;
            let mut data = vec![0u8; (sw * sh * 4) as usize];
            scaler.CopyPixels(std::ptr::null(), sw * 4, &mut data).ok()?;
            return Some(IconPixels { width: sw, height: sh, data });
        }
        let mut data = vec![0u8; (w * h * 4) as usize];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut data).ok()?;
        Some(IconPixels { width: w, height: h, data })
    }
}
