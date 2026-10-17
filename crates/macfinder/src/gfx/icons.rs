//! Icon loading: shell icons (exe, lnk, folders, Store apps) and image files,
//! decoded once to premultiplied BGRA and uploaded per render target.

use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;

use windows::core::{GUID, HSTRING};
use windows::Win32::Foundation::{GENERIC_READ, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap, ID2D1RenderTarget, D2D1_BITMAP_PROPERTIES};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
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
}

impl IconStore {
    /// `custom` (an image file) wins over the shell icon of `parse_name`.
    pub fn get(&mut self, wic: &IWICImagingFactory, parse_name: &str, custom: Option<&str>) -> Option<Rc<IconPixels>> {
        let key = custom.unwrap_or(parse_name).to_string();
        self.cache
            .entry(key)
            .or_insert_with(|| {
                let px = match custom {
                    Some(file) => load_image_file(wic, file).or_else(|| load_shell_icon(parse_name)),
                    None => load_shell_icon(parse_name),
                };
                if px.is_none() {
                    log!("no icon for {}", custom.unwrap_or(parse_name));
                }
                px.map(Rc::new)
            })
            .clone()
    }
}

pub fn load_shell_icon(parse_name: &str) -> Option<IconPixels> {
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(parse_name), None).ok()?;
        let hbmp = factory.GetImage(SIZE { cx: ICON_PX, cy: ICON_PX }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let px = read_hbitmap(hbmp);
        let _ = DeleteObject(hbmp);
        px
    }
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
        let mut data = vec![0u8; (w * h * 4) as usize];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut data).ok()?;
        Some(IconPixels { width: w, height: h, data })
    }
}
