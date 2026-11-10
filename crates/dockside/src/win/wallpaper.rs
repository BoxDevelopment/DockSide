//! The desktop wallpaper as a monitor's worth of pixels, laid out the way
//! Windows lays it out (fill, fit, stretch, center, tile or span). Glass over
//! the desktop refracts this instead of a screen capture, so it never has to
//! hide from capture and shows up in screenshots and recordings.

use windows::core::{w, Result, HSTRING, PWSTR};
use windows::Win32::Foundation::{BOOL, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, GENERIC_READ, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Registry::{RegEnumValueW, RegNotifyChangeKeyValue, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, KEY_READ, REG_NOTIFY_CHANGE_LAST_SET};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, FindWindowExW, FindWindowW, GetWindowThreadProcessId, IsWindowVisible};

/// Takes a COM-allocated string, freeing it.
unsafe fn take(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

/// Premultiplied BGRA pixels (top-down) of the wallpaper behind `monitor`, a
/// screen rect. A solid-color background, or a missing image, gives that color.
pub fn render(monitor: RECT) -> Result<Vec<u8>> {
    let (mw, mh) = (monitor.right - monitor.left, monitor.bottom - monitor.top);
    let dw: IDesktopWallpaper = unsafe { CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)? };
    // COLORREF is 0x00BBGGRR.
    let bg = unsafe { dw.GetBackgroundColor()? }.0;
    let mut px = vec![0u8; (mw * mh * 4) as usize];
    for p in px.chunks_exact_mut(4) {
        p.copy_from_slice(&[(bg >> 16) as u8, (bg >> 8) as u8, bg as u8, 255]);
    }

    let mut path = String::new();
    let mut span = monitor;
    unsafe {
        for i in 0..dw.GetMonitorDevicePathCount()? {
            let id = dw.GetMonitorDevicePathAt(i)?;
            if let Ok(r) = dw.GetMonitorRECT(id) {
                span = RECT {
                    left: span.left.min(r.left),
                    top: span.top.min(r.top),
                    right: span.right.max(r.right),
                    bottom: span.bottom.max(r.bottom),
                };
                if r == monitor {
                    path = dw.GetWallpaper(id).map(|p| take(p)).unwrap_or_default();
                }
            }
            take(id);
        }
    }
    if path.is_empty() {
        return Ok(px);
    }
    let position = unsafe { dw.GetPosition()? };

    let wic: IWICImagingFactory = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
    let image: IWICBitmapSource = unsafe {
        let open = |p: &str| wic.CreateDecoderFromFilename(&HSTRING::from(p), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand);
        // The picture may since have been moved or deleted (or be one Windows
        // Spotlight already cleaned up); Explorer keeps its own copy of what it shows.
        let decoder = open(&path).or_else(|e| {
            let copy = std::env::var("APPDATA").map(|a| format!("{a}\\Microsoft\\Windows\\Themes\\TranscodedWallpaper")).map_err(|_| e.clone())?;
            open(&copy).map_err(|_| e)
        })?;
        let converter = wic.CreateFormatConverter()?;
        converter.Initialize(&decoder.GetFrame(0)?, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        converter.into()
    };
    let (mut iw, mut ih) = (0u32, 0u32);
    unsafe { image.GetSize(&mut iw, &mut ih)? };
    let (iw, ih) = (iw as i32, ih as i32);
    if iw == 0 || ih == 0 {
        return Ok(px);
    }

    if position == DWPOS_TILE {
        for y in (0..mh).step_by(ih as usize) {
            for x in (0..mw).step_by(iw as usize) {
                blit(&wic, &image, (iw, ih), (iw, ih), (x, y), (mw, mh), &mut px)?;
            }
        }
        return Ok(px);
    }
    // Spanned wallpapers fill the bounds of every monitor together.
    let area = if position == DWPOS_SPAN { span } else { monitor };
    let (aw, ah) = ((area.right - area.left) as f32, (area.bottom - area.top) as f32);
    let (fw, fh) = (iw as f32, ih as f32);
    let (sw, sh) = match position {
        DWPOS_STRETCH => (aw, ah),
        DWPOS_CENTER => (fw, fh),
        DWPOS_FIT => {
            let s = (aw / fw).min(ah / fh);
            (fw * s, fh * s)
        }
        // Fill and span.
        _ => {
            let s = (aw / fw).max(ah / fh);
            (fw * s, fh * s)
        }
    };
    let (sw, sh) = (sw.round().max(1.0) as i32, sh.round().max(1.0) as i32);
    let x = area.left + (aw as i32 - sw) / 2 - monitor.left;
    let y = area.top + (ah as i32 - sh) / 2 - monitor.top;
    blit(&wic, &image, (iw, ih), (sw, sh), (x, y), (mw, mh), &mut px)?;
    Ok(px)
}

/// Copies `image`, scaled to `size` with its top-left at `at`, into the part
/// of `px` (`dst` pixels) it covers.
fn blit(wic: &IWICImagingFactory, image: &IWICBitmapSource, natural: (i32, i32), size: (i32, i32), at: (i32, i32), dst: (i32, i32), px: &mut [u8]) -> Result<()> {
    let (x0, y0) = (at.0.max(0), at.1.max(0));
    let (x1, y1) = ((at.0 + size.0).min(dst.0), (at.1 + size.1).min(dst.1));
    if x1 <= x0 || y1 <= y0 {
        return Ok(());
    }
    let source: IWICBitmapSource = if size == natural {
        image.clone()
    } else {
        unsafe {
            let scaler = wic.CreateBitmapScaler()?;
            scaler.Initialize(image, size.0 as u32, size.1 as u32, WICBitmapInterpolationModeHighQualityCubic)?;
            scaler.into()
        }
    };
    let rect = WICRect { X: x0 - at.0, Y: y0 - at.1, Width: x1 - x0, Height: y1 - y0 };
    let offset = ((y0 * dst.0 + x0) * 4) as usize;
    unsafe { source.CopyPixels(&rect, (dst.0 * 4) as u32, &mut px[offset..]) }
}

/// The registry values that say which picture the desktop shows (the path, its
/// layout, and the per-monitor transcoded copies Explorer makes of it).
fn signature(key: HKEY) -> Vec<u8> {
    let mut sig = Vec::new();
    let mut name = [0u16; 256];
    let mut data = vec![0u8; 4096];
    for i in 0.. {
        let (mut n, mut len) = (name.len() as u32, data.len() as u32);
        let r = unsafe { RegEnumValueW(key, i, PWSTR(name.as_mut_ptr()), &mut n, None, None, Some(data.as_mut_ptr()), Some(&mut len)) };
        if r == ERROR_NO_MORE_ITEMS {
            break;
        }
        // Too big for the buffer: its name still counts.
        let len = if r.is_ok() { len as usize } else if r == ERROR_MORE_DATA { 0 } else { continue };
        let name = String::from_utf16_lossy(&name[..n as usize]);
        if name.starts_with("Wallpaper") || name.starts_with("TileWallpaper") || name.starts_with("TranscodedImageCache") {
            sig.extend_from_slice(name.as_bytes());
            sig.extend_from_slice(&data[..len.min(data.len())]);
        }
    }
    sig
}

/// Posts `msg` to `hwnd` whenever the desktop wallpaper changes, however it
/// was changed: Settings, a slideshow or Windows Spotlight moving on, or
/// another app. (Not every one of them broadcasts `SPI_SETDESKWALLPAPER`.)
pub fn watch(hwnd: HWND, msg: u32) {
    let hwnd = hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, w!("Control Panel\\Desktop"), 0, KEY_READ | KEY_NOTIFY, &mut key).is_err() {
            log!("wallpaper watch unavailable");
            return;
        }
        let Ok(event) = CreateEventW(None, false, false, None) else { return };
        let mut last = signature(key);
        loop {
            if RegNotifyChangeKeyValue(key, false, REG_NOTIFY_CHANGE_LAST_SET, event, true).is_err() {
                log!("wallpaper watch stopped");
                return;
            }
            WaitForSingleObject(event, INFINITE);
            let now = signature(key);
            if now != last {
                last = now;
                crate::win::post(hwnd, msg);
            }
        }
    });
}

struct Search {
    ignore: [u32; 2],
    live: bool,
}

/// Whether an app draws a live (animated or video) wallpaper, as Wallpaper
/// Engine and Lively do: they put a window of their own behind the desktop icons.
pub fn is_live() -> bool {
    unsafe extern "system" fn visit(hwnd: HWND, lp: LPARAM) -> BOOL {
        let search = &mut *(lp.0 as *mut Search);
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if !search.ignore.contains(&pid) && IsWindowVisible(hwnd).as_bool() {
            search.live = true;
            return BOOL(0);
        }
        BOOL(1)
    }
    unsafe {
        let Ok(progman) = FindWindowW(w!("Progman"), None) else { return false };
        let mut explorer = 0;
        GetWindowThreadProcessId(progman, Some(&mut explorer));
        // Desktop widgets may live in the desktop window too.
        let mut search = Search { ignore: [explorer, GetCurrentProcessId()], live: false };
        let mut roots = vec![progman];
        let mut worker = HWND::default();
        while let Ok(w) = FindWindowExW(None, worker, w!("WorkerW"), None) {
            roots.push(w);
            worker = w;
        }
        for root in roots {
            let _ = EnumChildWindows(root, Some(visit), LPARAM(&mut search as *mut Search as isize));
            if search.live {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    /// Needs a real desktop: `cargo test wallpaper -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn renders_the_primary_wallpaper() {
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        unsafe {
            let _ = GetMonitorInfoW(MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY), &mut info);
        }
        let r = info.rcMonitor;
        let px = super::render(r).unwrap();
        let (w, h) = ((r.right - r.left) as usize, (r.bottom - r.top) as usize);
        assert_eq!(px.len(), w * h * 4);
        let mut colors = std::collections::HashSet::new();
        for y in (0..h).step_by(h / 20) {
            for x in (0..w).step_by(w / 20) {
                let i = (y * w + x) * 4;
                colors.insert((px[i], px[i + 1], px[i + 2]));
            }
        }
        println!("{w}x{h}, {} distinct sampled colors, live wallpaper: {}", colors.len(), super::is_live());
    }
}
