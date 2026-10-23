//! Direct2D/DirectWrite rendering into per-pixel-alpha layered windows.
//!
//! Every surface owns a [`Canvas`]: a 32bpp premultiplied DIB that a D2D DC
//! render target draws into, pushed to the screen with `UpdateLayeredWindow`.
//! Render targets use the monitor DPI, so all drawing is in logical points.

pub mod finder_icon;
pub mod icons;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::{w, Interface, Result, HSTRING};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::{CLSID_WICImagingFactory, IWICImagingFactory};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, UpdateLayeredWindowIndirect, UPDATELAYEREDWINDOWINFO, ULW_ALPHA};

use crate::geom::{Color, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Align {
    Leading,
    Center,
    Trailing,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Weight {
    Regular,
    Bold,
}

impl Weight {
    fn dwrite(self) -> DWRITE_FONT_WEIGHT {
        match self {
            Weight::Regular => DWRITE_FONT_WEIGHT_NORMAL,
            Weight::Bold => DWRITE_FONT_WEIGHT_BOLD,
        }
    }
}

type FormatKey = (String, u32, Weight, Align, bool);
/// Text, its format (by identity: formats are cached) and the box size.
type LayoutKey = (String, usize, u32, u32);

/// Shaped text kept between frames. Shaping (glyphs, kerning, line breaks) is
/// the most expensive part of drawing a frame, and while something scrolls or a
/// highlight glides the text doesn't change, so it's shaped once and reused.
/// Two generations: entries unused for a whole generation are dropped.
#[derive(Default)]
struct TextCache {
    layouts: HashMap<LayoutKey, IDWriteTextLayout>,
    old_layouts: HashMap<LayoutKey, IDWriteTextLayout>,
    widths: HashMap<(String, usize), f32>,
    old_widths: HashMap<(String, usize), f32>,
}

const TEXT_CACHE_GENERATION: usize = 4096;

impl TextCache {
    fn layout(&mut self, key: LayoutKey) -> std::result::Result<IDWriteTextLayout, LayoutKey> {
        if let Some(l) = self.layouts.get(&key) {
            return Ok(l.clone());
        }
        match self.old_layouts.remove(&key) {
            Some(l) => {
                self.keep_layout(key, l.clone());
                Ok(l)
            }
            None => Err(key),
        }
    }

    fn keep_layout(&mut self, key: LayoutKey, l: IDWriteTextLayout) {
        if self.layouts.len() >= TEXT_CACHE_GENERATION {
            self.old_layouts = std::mem::take(&mut self.layouts);
        }
        self.layouts.insert(key, l);
    }

    fn width(&mut self, key: &(String, usize)) -> Option<f32> {
        if let Some(w) = self.widths.get(key) {
            return Some(*w);
        }
        let w = self.old_widths.remove(key)?;
        self.keep_width(key.clone(), w);
        Some(w)
    }

    fn keep_width(&mut self, key: (String, usize), w: f32) {
        if self.widths.len() >= TEXT_CACHE_GENERATION {
            self.old_widths = std::mem::take(&mut self.widths);
        }
        self.widths.insert(key, w);
    }
}

pub struct Gfx {
    pub d2d: ID2D1Factory,
    pub dwrite: IDWriteFactory,
    pub wic: IWICImagingFactory,
    pub round_stroke: ID2D1StrokeStyle,
    formats: RefCell<HashMap<FormatKey, IDWriteTextFormat>>,
    text: RefCell<TextCache>,
}

impl Gfx {
    pub fn new() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let round_stroke = d2d.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 10.0,
                    dashStyle: D2D1_DASH_STYLE_SOLID,
                    dashOffset: 0.0,
                },
                None,
            )?;
            Ok(Self { d2d, dwrite, wic, round_stroke, formats: RefCell::new(HashMap::new()), text: RefCell::default() })
        }
    }

    pub fn font_exists(&self, family: &str) -> bool {
        unsafe {
            let mut coll: Option<IDWriteFontCollection> = None;
            if self.dwrite.GetSystemFontCollection(&mut coll, false).is_err() {
                return false;
            }
            let Some(coll) = coll else { return false };
            let mut index = 0u32;
            let mut exists = BOOL(0);
            coll.FindFamilyName(&HSTRING::from(family), &mut index, &mut exists).is_ok() && exists.as_bool()
        }
    }

    /// Cached single-line text format, vertically centered, trimmed with an ellipsis.
    pub fn format(&self, family: &str, size: f32, weight: Weight, align: Align) -> IDWriteTextFormat {
        self.format_ex(family, size, weight, align, false)
    }

    /// Centered, top-aligned text that wraps onto multiple lines (icon labels).
    pub fn format_wrapped(&self, family: &str, size: f32) -> IDWriteTextFormat {
        self.format_ex(family, size, Weight::Regular, Align::Center, true)
    }

    /// Top-aligned text that wraps, with the given alignment (notes, long titles).
    pub fn format_block(&self, family: &str, size: f32, weight: Weight, align: Align) -> IDWriteTextFormat {
        self.format_ex(family, size, weight, align, true)
    }

    fn format_ex(&self, family: &str, size: f32, weight: Weight, align: Align, wrap: bool) -> IDWriteTextFormat {
        let key = (family.to_string(), (size * 100.0) as u32, weight, align, wrap);
        if let Some(f) = self.formats.borrow().get(&key) {
            return f.clone();
        }
        let f = self.create_format(family, size, weight, align, wrap).expect("CreateTextFormat");
        self.formats.borrow_mut().insert(key, f.clone());
        f
    }

    fn create_format(&self, family: &str, size: f32, weight: Weight, align: Align, wrap: bool) -> Result<IDWriteTextFormat> {
        unsafe {
            let f = self.dwrite.CreateTextFormat(
                &HSTRING::from(family),
                None,
                weight.dwrite(),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("en-us"),
            )?;
            f.SetWordWrapping(if wrap { DWRITE_WORD_WRAPPING_EMERGENCY_BREAK } else { DWRITE_WORD_WRAPPING_NO_WRAP })?;
            f.SetParagraphAlignment(if wrap { DWRITE_PARAGRAPH_ALIGNMENT_NEAR } else { DWRITE_PARAGRAPH_ALIGNMENT_CENTER })?;
            f.SetTextAlignment(match align {
                Align::Leading => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Trailing => DWRITE_TEXT_ALIGNMENT_TRAILING,
            })?;
            let sign = self.dwrite.CreateEllipsisTrimmingSign(&f)?;
            let trimming =
                DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            f.SetTrimming(&trimming, &sign)?;
            Ok(f)
        }
    }

    pub fn layout(&self, text: &str, format: &IDWriteTextFormat, w: f32, h: f32) -> Option<IDWriteTextLayout> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe { self.dwrite.CreateTextLayout(&wide, format, w.max(0.0), h.max(0.0)).ok() }
    }

    /// A shaped layout for drawing `text` in a `w` x `h` box, reused across frames.
    /// Don't modify it (it's shared); use [`Self::layout`] for one of your own.
    pub fn shared_layout(&self, text: &str, format: &IDWriteTextFormat, w: f32, h: f32) -> Option<IDWriteTextLayout> {
        let key = (text.to_string(), format.as_raw() as usize, w.max(0.0).to_bits(), h.max(0.0).to_bits());
        let key = match self.text.borrow_mut().layout(key) {
            Ok(l) => return Some(l),
            Err(key) => key,
        };
        let l = self.layout(text, format, w, h)?;
        self.text.borrow_mut().keep_layout(key, l.clone());
        Some(l)
    }

    /// Width of `text` in points.
    pub fn measure(&self, text: &str, format: &IDWriteTextFormat) -> f32 {
        let key = (text.to_string(), format.as_raw() as usize);
        if let Some(w) = self.text.borrow_mut().width(&key) {
            return w;
        }
        let Some(layout) = self.layout(text, format, 100_000.0, 100.0) else { return 0.0 };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            if layout.GetMetrics(&mut m).is_err() {
                return 0.0;
            }
        }
        let w = m.widthIncludingTrailingWhitespace;
        self.text.borrow_mut().keep_width(key, w);
        w
    }

    /// A path geometry built by `f`.
    pub fn path(&self, f: impl FnOnce(&ID2D1GeometrySink)) -> Option<ID2D1PathGeometry> {
        unsafe {
            let geom = self.d2d.CreatePathGeometry().ok()?;
            let sink = geom.Open().ok()?;
            f(&sink);
            sink.Close().ok()?;
            Some(geom)
        }
    }
}

pub fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}

pub struct Canvas {
    rt: ID2D1DCRenderTarget,
    ctx: Option<ID2D1DeviceContext>,
    brush: ID2D1SolidColorBrush,
    /// Static artwork (soft shadows) drawn once and stamped every frame.
    layers: RefCell<HashMap<u64, ID2D1Bitmap>>,
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    pub width: i32,
    pub height: i32,
    scale: f32,
}

impl Canvas {
    pub fn new(gfx: &Gfx, width: i32, height: i32, scale: f32) -> Result<Self> {
        unsafe {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                // Drawn on the CPU: layered windows need the pixels in system
                // memory, and copying them back from the GPU each frame stalls it
                // (measured: big windows managed ~92 fps on a 165 Hz display that
                // way, a steady 165+ drawn here).
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0 * scale,
                dpiY: 96.0 * scale,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = gfx.d2d.CreateDCRenderTarget(&props)?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let ctx = rt.cast::<ID2D1DeviceContext>().ok();
            let brush = rt.CreateSolidColorBrush(&Color::BLACK.d2d(), None)?;
            let dc = CreateCompatibleDC(None);
            let mut canvas = Canvas {
                rt,
                ctx,
                brush,
                layers: RefCell::default(),
                dc,
                bitmap: HBITMAP::default(),
                old: HGDIOBJ::default(),
                width: 0,
                height: 0,
                scale,
            };
            canvas.resize(width, height)?;
            Ok(canvas)
        }
    }

    pub fn target(&self) -> &ID2D1RenderTarget {
        &self.rt
    }

    pub fn resize(&mut self, width: i32, height: i32) -> Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        if width == self.width && height == self.height {
            return Ok(());
        }
        unsafe {
            self.free_bitmap();
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(self.dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            self.old = SelectObject(self.dc, bitmap);
            self.bitmap = bitmap;
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// The drawn pixels (premultiplied BGRA, top-down).
    pub fn pixels(&self) -> Vec<u8> {
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.width,
                biHeight: -self.height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (self.width * self.height * 4) as usize];
        unsafe {
            // GetDIBits wants the bitmap deselected.
            SelectObject(self.dc, self.old);
            GetDIBits(self.dc, self.bitmap, 0, self.height as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bmi, DIB_RGB_COLORS);
            SelectObject(self.dc, self.bitmap);
        }
        buf
    }

    unsafe fn free_bitmap(&mut self) {
        if !self.bitmap.is_invalid() {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap);
            self.bitmap = HBITMAP::default();
        }
    }

    pub fn draw(&mut self, gfx: &Gfx, f: impl FnOnce(&Painter)) -> Result<()> {
        self.draw_part(gfx, None, f)
    }

    /// The device-pixel rect covering `r` (in points), padded a pixel for
    /// antialiasing and clipped to the canvas; `None` if that's nothing.
    pub fn part_px(&self, r: Rect) -> Option<RECT> {
        let s = self.scale;
        let rc = RECT {
            left: ((r.x * s).floor() as i32 - 1).max(0),
            top: ((r.y * s).floor() as i32 - 1).max(0),
            right: ((r.right() * s).ceil() as i32 + 1).min(self.width),
            bottom: ((r.bottom() * s).ceil() as i32 + 1).min(self.height),
        };
        (rc.right > rc.left && rc.bottom > rc.top).then_some(rc)
    }

    /// Draws a frame. With `part` (device pixels), only that region is repainted
    /// and the rest keeps the last frame's pixels: `f` still issues the whole
    /// frame, but nothing outside `part` is rasterized, which is where the time
    /// goes. Use while just one area animates (a list scrolling, a highlight gliding).
    pub fn draw_part(&mut self, gfx: &Gfx, part: Option<RECT>, f: impl FnOnce(&Painter)) -> Result<()> {
        unsafe {
            let rc = part.unwrap_or(RECT { left: 0, top: 0, right: self.width, bottom: self.height });
            self.rt.BindDC(self.dc, &rc)?;
            self.rt.BeginDraw();
            // Drawing stays in canvas coordinates; the bound region starts at its corner.
            self.rt.SetTransform(&Matrix3x2::translation(-rc.left as f32 / self.scale, -rc.top as f32 / self.scale));
            self.rt.Clear(Some(&Color::TRANSPARENT.d2d()));
            let painter = Painter { rt: &self.rt, ctx: self.ctx.as_ref(), brush: &self.brush, gfx, layers: &self.layers };
            f(&painter);
            let r = self.rt.EndDraw(None, None);
            self.rt.SetTransform(&Matrix3x2::identity());
            r
        }
    }

    /// Presents only `part` (device pixels) of the canvas, which must be where
    /// the window already is: the compositor re-uploads just that region.
    pub fn present_part(&self, hwnd: HWND, x: i32, y: i32, alpha: f32, part: Option<RECT>) -> Result<()> {
        let Some(dirty) = part else { return self.present_alpha(hwnd, x, y, alpha) };
        unsafe {
            let screen = GetDC(None);
            let blend = blend(alpha);
            let (dst, size, src) = (POINT { x, y }, SIZE { cx: self.width, cy: self.height }, POINT::default());
            let info = UPDATELAYEREDWINDOWINFO {
                cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
                hdcDst: screen,
                pptDst: &dst,
                psize: &size,
                hdcSrc: self.dc,
                pptSrc: &src,
                crKey: COLORREF(0),
                pblend: &blend,
                dwFlags: ULW_ALPHA,
                prcDirty: &dirty,
            };
            let ok = UpdateLayeredWindowIndirect(hwnd, &info).as_bool();
            ReleaseDC(None, screen);
            if ok {
                Ok(())
            } else {
                Err(windows::core::Error::from_win32())
            }
        }
    }

    /// Presents with the whole window faded to `alpha` (0..1).
    pub fn present_alpha(&self, hwnd: HWND, x: i32, y: i32, alpha: f32) -> Result<()> {
        unsafe {
            let screen = GetDC(None);
            let blend = blend(alpha);
            let r = UpdateLayeredWindow(
                hwnd,
                screen,
                Some(&POINT { x, y }),
                Some(&SIZE { cx: self.width, cy: self.height }),
                self.dc,
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            ReleaseDC(None, screen);
            r
        }
    }
}

fn blend(alpha: f32) -> BLENDFUNCTION {
    BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
        AlphaFormat: AC_SRC_ALPHA as u8,
    }
}

/// Re-fades a layered window's last presented pixels without redrawing them:
/// the cheap way to animate a whole window's opacity.
pub fn set_window_alpha(hwnd: HWND, alpha: f32) {
    unsafe {
        let _ = UpdateLayeredWindow(hwnd, None, None, None, None, None, COLORREF(0), Some(&blend(alpha)), ULW_ALPHA);
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            self.free_bitmap();
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Immediate-mode drawing helpers over a render target, in points.
pub struct Painter<'a> {
    pub rt: &'a ID2D1RenderTarget,
    ctx: Option<&'a ID2D1DeviceContext>,
    brush: &'a ID2D1SolidColorBrush,
    pub gfx: &'a Gfx,
    layers: &'a RefCell<HashMap<u64, ID2D1Bitmap>>,
}

impl Painter<'_> {
    fn color(&self, c: Color) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&c.d2d()) };
        self.brush
    }

    pub fn fill_rect(&self, r: Rect, c: Color) {
        unsafe { self.rt.FillRectangle(&r.d2d(), self.color(c)) }
    }

    /// Runs `f` with drawing clipped to `r`.
    pub fn clipped(&self, r: Rect, f: impl FnOnce()) {
        unsafe {
            self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            f();
            self.rt.PopAxisAlignedClip();
        }
    }

    /// Clips drawing to `r` until the matching [`Self::pop_clip`].
    pub fn push_clip(&self, r: Rect) {
        unsafe { self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) }
    }

    pub fn pop_clip(&self) {
        unsafe { self.rt.PopAxisAlignedClip() }
    }

    /// Resets `r` to fully transparent.
    pub fn clear_rect(&self, r: Rect) {
        unsafe {
            self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_ALIASED);
            self.rt.Clear(Some(&Color::TRANSPARENT.d2d()));
            self.rt.PopAxisAlignedClip();
        }
    }

    /// Resets a rounded rect to fully transparent (antialiased edges). Without a
    /// device context this falls back to clearing the rect inside the corners.
    pub fn clear_round(&self, r: Rect, radius: f32) {
        let Some(ctx) = self.ctx else {
            return self.clear_rect(r.inset(radius * 0.3, radius * 0.3));
        };
        unsafe {
            ctx.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_COPY);
            self.fill_round(r, radius, Color::TRANSPARENT);
            ctx.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_SOURCE_OVER);
        }
    }

    pub fn fill_round(&self, r: Rect, radius: f32, c: Color) {
        let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
        unsafe { self.rt.FillRoundedRectangle(&rr, self.color(c)) }
    }

    /// A rounded rect filled with a top-to-bottom gradient.
    pub fn fill_round_gradient(&self, r: Rect, radius: f32, top: Color, bottom: Color) {
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: top.d2d() },
            D2D1_GRADIENT_STOP { position: 1.0, color: bottom.d2d() },
        ];
        unsafe {
            let Ok(coll) = self.rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else {
                return self.fill_round(r, radius, top);
            };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: pt(r.x, r.y), endPoint: pt(r.x, r.bottom()) };
            if let Ok(brush) = self.rt.CreateLinearGradientBrush(&props, None, &coll) {
                let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
                self.rt.FillRoundedRectangle(&rr, &brush);
            }
        }
    }

    pub fn stroke_round(&self, r: Rect, radius: f32, c: Color, width: f32) {
        let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
        unsafe { self.rt.DrawRoundedRectangle(&rr, self.color(c), width, None) }
    }

    pub fn fill_circle(&self, cx: f32, cy: f32, r: f32, c: Color) {
        let e = D2D1_ELLIPSE { point: pt(cx, cy), radiusX: r, radiusY: r };
        unsafe { self.rt.FillEllipse(&e, self.color(c)) }
    }

    pub fn stroke_circle(&self, cx: f32, cy: f32, r: f32, c: Color, width: f32) {
        let e = D2D1_ELLIPSE { point: pt(cx, cy), radiusX: r, radiusY: r };
        unsafe { self.rt.DrawEllipse(&e, self.color(c), width, None) }
    }

    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, c: Color, width: f32) {
        unsafe { self.rt.DrawLine(pt(x0, y0), pt(x1, y1), self.color(c), width, &self.gfx.round_stroke) }
    }

    pub fn fill_geometry(&self, g: &ID2D1PathGeometry, c: Color) {
        unsafe { self.rt.FillGeometry(g, self.color(c), None) }
    }

    pub fn stroke_geometry(&self, g: &ID2D1PathGeometry, c: Color, width: f32) {
        unsafe { self.rt.DrawGeometry(g, self.color(c), width, &self.gfx.round_stroke) }
    }

    /// Draws `text` inside `r` using the format's alignment (vertically centered).
    pub fn text(&self, text: &str, format: &IDWriteTextFormat, r: Rect, c: Color) {
        if let Some(layout) = self.gfx.shared_layout(text, format, r.w, r.h) {
            unsafe {
                self.rt.DrawTextLayout(pt(r.x, r.y), &layout, self.color(c), D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT)
            }
        }
    }

    /// Draws a prepared text layout with its top-left at (x, y).
    pub fn text_layout(&self, layout: &IDWriteTextLayout, x: f32, y: f32, c: Color) {
        unsafe { self.rt.DrawTextLayout(pt(x, y), layout, self.color(c), D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT) }
    }

    /// High-quality scaled bitmap draw (cubic when the device context is available).
    pub fn bitmap(&self, bmp: &ID2D1Bitmap, r: Rect, opacity: f32) {
        unsafe {
            match self.ctx {
                Some(ctx) => ctx.DrawBitmap(
                    bmp,
                    Some(&r.d2d()),
                    opacity,
                    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                    None,
                    None,
                ),
                None => self.rt.DrawBitmap(bmp, Some(&r.d2d()), opacity, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None),
            }
        }
    }

    /// A bitmap stretched over `r` with rounded corners (album art).
    pub fn bitmap_round(&self, bmp: &ID2D1Bitmap, r: Rect, radius: f32, opacity: f32) {
        unsafe {
            let size = bmp.GetSize();
            if size.width <= 0.0 || size.height <= 0.0 {
                return;
            }
            let m = Matrix3x2 {
                M11: r.w / size.width,
                M12: 0.0,
                M21: 0.0,
                M22: r.h / size.height,
                M31: r.x,
                M32: r.y,
            };
            let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
            let brush_props = D2D1_BRUSH_PROPERTIES { opacity, transform: m };
            let brush: Option<ID2D1Brush> = match self.ctx {
                Some(ctx) => {
                    let props = D2D1_BITMAP_BRUSH_PROPERTIES1 {
                        extendModeX: D2D1_EXTEND_MODE_CLAMP,
                        extendModeY: D2D1_EXTEND_MODE_CLAMP,
                        interpolationMode: D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                    };
                    ctx.CreateBitmapBrush(bmp, Some(&props), Some(&brush_props)).ok().and_then(|b| b.cast().ok())
                }
                None => {
                    let props = D2D1_BITMAP_BRUSH_PROPERTIES {
                        extendModeX: D2D1_EXTEND_MODE_CLAMP,
                        extendModeY: D2D1_EXTEND_MODE_CLAMP,
                        interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    };
                    self.rt.CreateBitmapBrush(bmp, Some(&props), Some(&brush_props)).ok().and_then(|b| b.cast().ok())
                }
            };
            if let Some(brush) = brush {
                self.rt.FillRoundedRectangle(&rr, &brush);
            }
        }
    }

    /// Like [`Painter::text`], with fixed-width digits so changing numbers don't jiggle.
    pub fn text_tabular(&self, text: &str, format: &IDWriteTextFormat, r: Rect, c: Color) {
        let Some(layout) = self.gfx.layout(text, format, r.w, r.h) else { return };
        unsafe {
            if let Ok(typo) = self.gfx.dwrite.CreateTypography() {
                let feature = DWRITE_FONT_FEATURE { nameTag: DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES, parameter: 1 };
                if typo.AddFontFeature(feature).is_ok() {
                    let _ = layout.SetTypography(&typo, DWRITE_TEXT_RANGE { startPosition: 0, length: u32::MAX });
                }
            }
            self.rt.DrawTextLayout(pt(r.x, r.y), &layout, self.color(c), D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT)
        }
    }

    /// Fills `c` through the bitmap's alpha (used to darken a pressed icon).
    pub fn tint_bitmap(&self, bmp: &ID2D1Bitmap, r: Rect, c: Color) {
        unsafe {
            self.rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
            self.rt.FillOpacityMask(bmp, self.color(c), D2D1_OPACITY_MASK_CONTENT_GRAPHICS, Some(&r.d2d()), None);
            self.rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        }
    }

    /// Draws `f` (which covers `bounds`) once into a bitmap kept under `key`, then
    /// just stamps that bitmap: for artwork that's costly and rarely changes. The
    /// key must capture everything `f` depends on except its position.
    pub fn cached(&self, key: u64, bounds: Rect, f: impl FnOnce(&Painter)) {
        let bmp = self.layers.borrow().get(&key).cloned();
        let bmp = match bmp {
            Some(b) => b,
            None => {
                let made = unsafe {
                    (|| -> Result<ID2D1Bitmap> {
                        let size = D2D_SIZE_F { width: bounds.w.max(1.0), height: bounds.h.max(1.0) };
                        let brt = self.rt.CreateCompatibleRenderTarget(Some(&size), None, None, D2D1_COMPATIBLE_RENDER_TARGET_OPTIONS_NONE)?;
                        let brush = brt.CreateSolidColorBrush(&Color::BLACK.d2d(), None)?;
                        let ctx = brt.cast::<ID2D1DeviceContext>().ok();
                        brt.BeginDraw();
                        brt.Clear(Some(&Color::TRANSPARENT.d2d()));
                        brt.SetTransform(&Matrix3x2::translation(-bounds.x, -bounds.y));
                        let p = Painter { rt: &brt, ctx: ctx.as_ref(), brush: &brush, gfx: self.gfx, layers: self.layers };
                        f(&p);
                        brt.EndDraw(None, None)?;
                        brt.GetBitmap()
                    })()
                };
                let Ok(b) = made else { return };
                let mut layers = self.layers.borrow_mut();
                // Resizing a window mints a key per size: keep the cache small.
                if layers.len() >= 16 {
                    layers.clear();
                }
                layers.insert(key, b.clone());
                b
            }
        };
        unsafe {
            self.rt.DrawBitmap(&bmp, Some(&bounds.d2d()), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
        }
    }

    /// A soft drop shadow under a rounded `body`: `steps` rounded rects growing
    /// `step` points each, fading out from `alpha`, offset `dy` down. Cached.
    #[allow(clippy::too_many_arguments)]
    pub fn soft_shadow(&self, body: Rect, radius: f32, steps: u32, step: f32, alpha: f32, dy: f32, c: Color) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        ("soft_shadow", body.w.to_bits(), body.h.to_bits(), radius.to_bits(), steps, step.to_bits(), alpha.to_bits(), dy.to_bits()).hash(&mut h);
        (c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()).hash(&mut h);
        let reach = steps as f32 * step + 1.0;
        let bounds = Rect::new(body.x - reach, body.y - reach + dy, body.w + 2.0 * reach, body.h + 2.0 * reach);
        self.cached(h.finish(), bounds, |p| {
            for i in 0..steps {
                let grow = i as f32 * step;
                let a = alpha * (steps - i) as f32 / steps as f32;
                p.fill_round(body.inset(-grow, -grow).offset(0.0, dy), radius + grow, c.alpha(a));
            }
        });
    }

    /// Draws `f` at `opacity` as one group (overlaps don't double up). Fully
    /// opaque or fully clear skips the offscreen layer.
    pub fn faded(&self, opacity: f32, f: impl FnOnce()) {
        if opacity >= 0.999 {
            return f();
        }
        if opacity <= 0.001 {
            return;
        }
        unsafe {
            let Ok(layer) = self.rt.CreateLayer(None) else { return f() };
            let params = D2D1_LAYER_PARAMETERS {
                contentBounds: D2D_RECT_F { left: -f32::MAX, top: -f32::MAX, right: f32::MAX, bottom: f32::MAX },
                geometricMask: std::mem::ManuallyDrop::new(None),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: Matrix3x2::identity(),
                opacity,
                opacityBrush: std::mem::ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS_NONE,
            };
            self.rt.PushLayer(&params, &layer);
            f();
            self.rt.PopLayer();
        }
    }

    /// Draws `f` scaled by `scale` about the point (`cx`, `cy`).
    pub fn scaled_about(&self, cx: f32, cy: f32, scale: f32, f: impl FnOnce()) {
        if (scale - 1.0).abs() < 0.0005 {
            return f();
        }
        self.transformed(cx * (1.0 - scale), cy * (1.0 - scale), scale, f)
    }

    /// Runs `f` with a translate + uniform scale transform applied.
    /// Draws `f` translated then scaled, on top of any transform already in effect.
    pub fn transformed(&self, x: f32, y: f32, scale: f32, f: impl FnOnce()) {
        unsafe {
            let mut outer = Matrix3x2::default();
            self.rt.GetTransform(&mut outer);
            let m = Matrix3x2 {
                M11: scale * outer.M11,
                M12: scale * outer.M12,
                M21: scale * outer.M21,
                M22: scale * outer.M22,
                M31: x * outer.M11 + y * outer.M21 + outer.M31,
                M32: x * outer.M12 + y * outer.M22 + outer.M32,
            };
            self.rt.SetTransform(&m);
            f();
            self.rt.SetTransform(&outer);
        }
    }
}

/// Saves premultiplied BGRA pixels as a PNG, for tests that want a look.
#[cfg(test)]
pub fn write_png(gfx: &Gfx, width: u32, height: u32, data: &[u8], path: &str) {
    use windows::Win32::Foundation::GENERIC_WRITE;
    use windows::Win32::Graphics::Imaging::*;
    unsafe {
        let stream = gfx.wic.CreateStream().unwrap();
        stream.InitializeFromFilename(&HSTRING::from(path), GENERIC_WRITE.0).unwrap();
        let encoder = gfx.wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()).unwrap();
        encoder.Initialize(&stream, WICBitmapEncoderNoCache).unwrap();
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut()).unwrap();
        let frame = frame.unwrap();
        frame.Initialize(None).unwrap();
        frame.SetSize(width, height).unwrap();
        let mut format = GUID_WICPixelFormat32bppPBGRA;
        frame.SetPixelFormat(&mut format).unwrap();
        frame.WritePixels(height, width * 4, data).unwrap();
        frame.Commit().unwrap();
        encoder.Commit().unwrap();
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use windows::Win32::Graphics::Dwm::DwmFlush;
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// Frames per second a visible layered window of each size sustains when
    /// fully redrawn every frame (text-heavy, like a Finder list). Shows a faint
    /// window in the top-left corner for a few seconds.
    /// `cargo test --profile fast bench_layered -- --ignored --nocapture --test-threads 1`
    #[test]
    #[ignore]
    fn bench_layered() {
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let _ = crate::win::register_class(Some(proc_));
        let gfx = Gfx::new().unwrap();
        let mon = crate::win::Monitor::primary();
        let s = mon.scale;
        let font = gfx.format("Segoe UI", 13.0, Weight::Regular, Align::Leading);
        for (name, wpt, hpt) in [("menu", 280.0, 360.0), ("picker", 700.0, 680.0), ("finder", 1000.0, 640.0)] {
            let (w, h) = ((wpt * s) as i32, (hpt * s) as i32);
            let rc = RECT { left: 0, top: 0, right: w, bottom: h };
            let ex = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_TOPMOST;
            let hwnd = crate::win::create_window(ex, WS_POPUP, rc, HWND::default()).unwrap();
            let mut c = Canvas::new(&gfx, w, h, s).unwrap();
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            let n = 90;
            let mut draw_ms = 0.0;
            let mut present_ms = 0.0;
            let t = std::time::Instant::now();
            for k in 0..n {
                let t0 = std::time::Instant::now();
                c.draw(&gfx, |p| {
                    p.fill_rect(Rect::new(0.0, 0.0, wpt, hpt), Color::gray(128, 0.04));
                    let mut y = 4.0 - (k as f32 * 3.0) % 24.0;
                    while y < hpt {
                        p.text("Document name.txt    12 KB    Today at 4:41 PM", &font, Rect::new(10.0, y, wpt - 20.0, 24.0), Color::gray(0, 0.06));
                        y += 24.0;
                    }
                })
                .unwrap();
                let t1 = std::time::Instant::now();
                c.present_alpha(hwnd, 0, 0, 1.0).unwrap();
                let t2 = std::time::Instant::now();
                draw_ms += (t1 - t0).as_secs_f64() * 1000.0;
                present_ms += (t2 - t1).as_secs_f64() * 1000.0;
                unsafe {
                    let _ = DwmFlush();
                }
            }
            let fps = n as f64 / t.elapsed().as_secs_f64();
            println!(
                "{name:7} {w}x{h}px: {fps:5.1} fps, draw {:.2} ms, present {:.2} ms per frame",
                draw_ms / n as f64,
                present_ms / n as f64
            );
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}
