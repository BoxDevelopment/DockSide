//! The desktop wallpaper as a monitor's worth of pixels, laid out the way
//! Windows lays it out (fill, fit, stretch, center, tile or span). Glass over
//! the desktop refracts this instead of a screen capture, so it never has to
//! hide from capture and shows up in screenshots and recordings.

use windows::core::{w, Result, HSTRING, PWSTR};
use windows::Win32::Foundation::{BOOL, GENERIC_READ, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Threading::GetCurrentProcessId;
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
        let decoder = wic.CreateDecoderFromFilename(&HSTRING::from(path), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
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
