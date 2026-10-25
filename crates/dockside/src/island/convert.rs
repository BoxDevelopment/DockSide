//! "Convert to" for shelf items: images through WIC (plus our own WebP, ICO
//! and PDF writers), text and table formats in plain Rust, and audio / video
//! through the Windows media transcoder.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{w, GUID, HSTRING, PROPVARIANT, PWSTR, VARIANT};
use windows::Win32::Foundation::{GENERIC_READ, HGLOBAL};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream, PROPBAG2};
use windows::Win32::System::Com::{CoCreateInstance, IStream, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

use super::encode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Bmp,
    Gif,
    Tiff,
    WebP,
    Heic,
    Jxr,
    Ico,
    Pdf,
    Txt,
    Md,
    Html,
    Rtf,
    Csv,
    Tsv,
    Json,
    Mp3,
    M4a,
    Wav,
    Wma,
    Flac,
    Mp4,
    Wmv,
}

impl Format {
    pub fn ext(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::Bmp => "bmp",
            Format::Gif => "gif",
            Format::Tiff => "tiff",
            Format::WebP => "webp",
            Format::Heic => "heic",
            Format::Jxr => "jxr",
            Format::Ico => "ico",
            Format::Pdf => "pdf",
            Format::Txt => "txt",
            Format::Md => "md",
            Format::Html => "html",
            Format::Rtf => "rtf",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
            Format::Mp3 => "mp3",
            Format::M4a => "m4a",
            Format::Wav => "wav",
            Format::Wma => "wma",
            Format::Flac => "flac",
            Format::Mp4 => "mp4",
            Format::Wmv => "wmv",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Png => "PNG",
            Format::Jpeg => "JPEG",
            Format::Bmp => "BMP",
            Format::Gif => "GIF",
            Format::Tiff => "TIFF",
            Format::WebP => "WebP",
            Format::Heic => "HEIC",
            Format::Jxr => "JPEG XR",
            Format::Ico => "ICO (Icon)",
            Format::Pdf => "PDF",
            Format::Txt => "Plain Text",
            Format::Md => "Markdown",
            Format::Html => "HTML",
            Format::Rtf => "Rich Text (RTF)",
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
            Format::Json => "JSON",
            Format::Mp3 => "MP3",
            Format::M4a => "AAC (M4A)",
            Format::Wav => "WAV",
            Format::Wma => "WMA",
            Format::Flac => "FLAC",
            Format::Mp4 => "MP4 (H.264)",
            Format::Wmv => "WMV",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Image,
    Text,
    Markdown,
    Table(u8),
    Json,
    Html,
    Audio,
    Video,
}

fn kind(path: &Path) -> Option<(Kind, String)> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    let k = match ext.as_str() {
        "png" | "jpg" | "jpeg" | "jpe" | "jfif" | "bmp" | "dib" | "gif" | "tif" | "tiff" | "webp" | "heic" | "heif" | "avif"
        | "jxr" | "wdp" | "hdp" | "ico" => Kind::Image,
        "txt" | "text" | "log" | "ini" | "cfg" | "conf" | "nfo" => Kind::Text,
        "md" | "markdown" => Kind::Markdown,
        "csv" => Kind::Table(b','),
        "tsv" | "tab" => Kind::Table(b'\t'),
        "json" => Kind::Json,
        "html" | "htm" | "xhtml" => Kind::Html,
        "mp3" | "m4a" | "aac" | "wav" | "wma" | "flac" | "alac" | "aiff" | "aif" | "amr" | "ogg" | "opus" => Kind::Audio,
        "mp4" | "m4v" | "mov" | "wmv" | "avi" | "mkv" | "3gp" | "3g2" | "webm" | "mpg" | "mpeg" | "ts" | "mts" | "m2ts" => {
            Kind::Video
        }
        _ => return None,
    };
    Some((k, ext))
}

/// Every format `path` can be converted to (never its own).
pub fn targets(path: &Path) -> Vec<Format> {
    let Some((k, ext)) = kind(path) else { return Vec::new() };
    if path.is_dir() {
        return Vec::new();
    }
    use Format::*;
    let list: Vec<Format> = match k {
        Kind::Image => {
            let mut v = vec![Png, Jpeg, Pdf, WebP, Gif, Tiff, Bmp, Ico, Jxr];
            if heic_encoder() {
                v.insert(4, Heic);
            }
            v
        }
        Kind::Text => vec![Txt, Md, Html, Rtf, Pdf],
        Kind::Markdown => vec![Html, Txt, Rtf, Pdf],
        Kind::Table(_) => vec![Csv, Tsv, Json, Html, Md],
        Kind::Json => vec![Csv, Tsv, Txt],
        Kind::Html => vec![Txt, Pdf],
        Kind::Audio => vec![Mp3, M4a, Wav, Flac, Wma],
        Kind::Video => vec![Mp4, Wmv, Mp3, M4a],
    };
    let same = |f: &Format| match f {
        Jpeg => matches!(ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif"),
        Tiff => matches!(ext.as_str(), "tif" | "tiff"),
        Heic => matches!(ext.as_str(), "heic" | "heif"),
        Jxr => matches!(ext.as_str(), "jxr" | "wdp" | "hdp"),
        Md => matches!(ext.as_str(), "md" | "markdown"),
        Html => matches!(ext.as_str(), "html" | "htm" | "xhtml"),
        Tsv => matches!(ext.as_str(), "tsv" | "tab"),
        Txt => ext == "txt",
        f => f.ext() == ext,
    };
    list.into_iter().filter(|f| !same(f)).collect()
}

/// Writes `src` converted to `to` at `dst`.
pub fn convert(src: &Path, to: Format, dst: &Path) -> Result<()> {
    let (k, _) = kind(src).ok_or_else(|| anyhow!("This kind of file can't be converted."))?;
    match k {
        Kind::Image => image(src, to, dst),
        Kind::Audio | Kind::Video => media(src, to, dst),
        _ => {
            let text = read_text(src)?;
            let out = text_convert(k, &text, to)?;
            std::fs::write(dst, out).context("Couldn't write the converted file")
        }
    }
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

fn factory() -> Result<IWICImagingFactory> {
    Ok(unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? })
}

thread_local! {
    static HEIC: std::cell::OnceCell<bool> = const { std::cell::OnceCell::new() };
}

/// Whether HEIC can be written: the HEIF extension has an encoder, but it
/// also needs the HEVC codec, so try a small image.
fn heic_encoder() -> bool {
    HEIC.with(|c| {
        *c.get_or_init(|| {
            let px = Pixels { w: 64, h: 64, bgra: vec![128; 64 * 64 * 4], dpi: (96.0, 96.0) };
            factory().is_ok_and(|f| encode_wic(&f, &px, Format::Heic).is_ok_and(|b| !b.is_empty()))
        })
    })
}

/// Decoded pixels: straight-alpha BGRA, upright, with the source's DPI.
struct Pixels {
    w: u32,
    h: u32,
    bgra: Vec<u8>,
    dpi: (f64, f64),
}

impl Pixels {
    fn has_alpha(&self) -> bool {
        self.bgra.chunks_exact(4).any(|p| p[3] != 255)
    }

    /// Composited onto white, for formats without transparency.
    fn flattened(&self) -> Vec<u8> {
        let mut out = self.bgra.clone();
        for p in out.chunks_exact_mut(4) {
            let a = p[3] as u32;
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
            }
            p[3] = 255;
        }
        out
    }
}

fn decode(f: &IWICImagingFactory, src: &Path) -> Result<Pixels> {
    unsafe {
        let decoder = f
            .CreateDecoderFromFilename(&HSTRING::from(src.as_os_str()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .context("Windows can't read this image (its codec may not be installed)")?;
        let mut frame = decoder.GetFrame(0)?;
        // Icons hold several sizes: take the largest.
        if decoder.GetContainerFormat().is_ok_and(|g| g == GUID_ContainerFormatIco) {
            let area = |f: &IWICBitmapFrameDecode| {
                let (mut w, mut h) = (0u32, 0u32);
                let _ = f.GetSize(&mut w, &mut h);
                w as u64 * h as u64
            };
            for i in 1..decoder.GetFrameCount().unwrap_or(1) {
                if let Ok(f) = decoder.GetFrame(i) {
                    if area(&f) > area(&frame) {
                        frame = f;
                    }
                }
            }
        }
        let conv = f.CreateFormatConverter()?;
        conv.Initialize(&frame, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let (mut w, mut h) = (0, 0);
        conv.GetSize(&mut w, &mut h)?;
        let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut bgra)?;
        let (mut dx, mut dy) = (96.0, 96.0);
        let _ = frame.GetResolution(&mut dx, &mut dy);
        if !(dx > 1.0 && dy > 1.0) {
            (dx, dy) = (96.0, 96.0);
        }
        let mut px = Pixels { w, h, bgra, dpi: (dx, dy) };
        if let Some(o) = orientation(&frame) {
            px = orient(px, o);
        }
        Ok(px)
    }
}

/// EXIF orientation (1-8), from JPEG, TIFF or HEIF metadata.
fn orientation(frame: &IWICBitmapFrameDecode) -> Option<u16> {
    unsafe {
        let reader = frame.GetMetadataQueryReader().ok()?;
        for q in [w!("/app1/ifd/{ushort=274}"), w!("/ifd/{ushort=274}"), w!("System.Photo.Orientation")] {
            let mut v = PROPVARIANT::default();
            if reader.GetMetadataByName(q, &mut v).is_ok() {
                if let Ok(o) = u16::try_from(&v) {
                    return Some(o);
                }
            }
        }
        None
    }
}

/// Turns pixels stored at EXIF orientation `o` upright.
fn orient(px: Pixels, o: u16) -> Pixels {
    if !(2..=8).contains(&o) {
        return px;
    }
    let (w, h) = (px.w as usize, px.h as usize);
    let swap = o >= 5;
    let (nw, nh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u8; px.bgra.len()];
    for y in 0..nh {
        for x in 0..nw {
            // Where the upright pixel (x, y) is stored.
            let (sx, sy) = match o {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (y, h - 1 - x),
                7 => (w - 1 - y, h - 1 - x),
                _ => (w - 1 - y, x),
            };
            let s = (sy * w + sx) * 4;
            let d = (y * nw + x) * 4;
            out[d..d + 4].copy_from_slice(&px.bgra[s..s + 4]);
        }
    }
    let dpi = if swap { (px.dpi.1, px.dpi.0) } else { px.dpi };
    Pixels { w: nw as u32, h: nh as u32, bgra: out, dpi }
}

fn image(src: &Path, to: Format, dst: &Path) -> Result<()> {
    let f = factory()?;
    let px = decode(&f, src)?;
    let bytes = match to {
        Format::WebP => encode::webp(&px.bgra, px.w, px.h)
            .ok_or_else(|| anyhow!("The image is too large for WebP ({} px at most per side).", encode::WEBP_MAX))?,
        Format::Ico => ico(&f, &px)?,
        Format::Pdf => {
            let jpeg = encode_wic(&f, &px, Format::Jpeg)?;
            let (w_pt, h_pt) = (px.w as f64 * 72.0 / px.dpi.0, px.h as f64 * 72.0 / px.dpi.1);
            encode::pdf_jpeg(&jpeg, px.w, px.h, w_pt as f32, h_pt as f32)
        }
        _ => encode_wic(&f, &px, to)?,
    };
    std::fs::write(dst, bytes).context("Couldn't write the converted file")
}

fn container(to: Format) -> Option<GUID> {
    Some(match to {
        Format::Png => GUID_ContainerFormatPng,
        Format::Jpeg => GUID_ContainerFormatJpeg,
        Format::Bmp => GUID_ContainerFormatBmp,
        Format::Gif => GUID_ContainerFormatGif,
        Format::Tiff => GUID_ContainerFormatTiff,
        Format::Heic => GUID_ContainerFormatHeif,
        Format::Jxr => GUID_ContainerFormatWmp,
        _ => return None,
    })
}

/// Encodes with a WIC encoder into memory.
fn encode_wic(f: &IWICImagingFactory, px: &Pixels, to: Format) -> Result<Vec<u8>> {
    let guid = container(to).ok_or_else(|| anyhow!("unsupported format"))?;
    let keeps_alpha = matches!(to, Format::Png | Format::Tiff | Format::Gif | Format::Jxr);
    let data = if keeps_alpha || !px.has_alpha() { px.bgra.clone() } else { px.flattened() };
    unsafe {
        let stream: IStream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let enc = f.CreateEncoder(&guid, std::ptr::null()).context("This format's encoder isn't installed")?;
        enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        let mut props = None;
        enc.CreateNewFrame(&mut frame, &mut props)?;
        let frame = frame.ok_or_else(|| anyhow!("no frame"))?;
        if let (Some(bag), true) = (&props, matches!(to, Format::Jpeg | Format::Heic | Format::Jxr)) {
            let mut name: Vec<u16> = "ImageQuality\0".encode_utf16().collect();
            let opt = PROPBAG2 { pstrName: PWSTR(name.as_mut_ptr()), ..Default::default() };
            let _ = bag.Write(1, &opt, &VARIANT::from(0.92f32));
        }
        frame.Initialize(props.as_ref())?;
        frame.SetSize(px.w, px.h)?;
        let _ = frame.SetResolution(px.dpi.0, px.dpi.1);
        let bmp = f.CreateBitmapFromMemory(px.w, px.h, &GUID_WICPixelFormat32bppBGRA, px.w * 4, &data)?;
        if to == Format::Gif {
            gif_frame(f, &frame, &bmp, px.has_alpha())?;
        } else {
            let mut fmt = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut fmt)?;
            // WriteSource converts to whatever the encoder settled on.
            frame.WriteSource(&bmp, std::ptr::null())?;
        }
        frame.Commit()?;
        enc.Commit()?;
        stream_bytes(&stream)
    }
}

/// GIF wants 256 colors: an optimized palette, dithered, with a transparent entry when needed.
unsafe fn gif_frame(f: &IWICImagingFactory, frame: &IWICBitmapFrameEncode, bmp: &IWICBitmap, alpha: bool) -> Result<()> {
    let palette = f.CreatePalette()?;
    palette.InitializeFromBitmap(bmp, 256, alpha)?;
    let mut fmt = GUID_WICPixelFormat8bppIndexed;
    frame.SetPixelFormat(&mut fmt)?;
    frame.SetPalette(&palette)?;
    let conv = f.CreateFormatConverter()?;
    conv.Initialize(bmp, &GUID_WICPixelFormat8bppIndexed, WICBitmapDitherTypeErrorDiffusion, &palette, 0.0, WICBitmapPaletteTypeCustom)?;
    if alpha {
        let mut colors = [0u32; 256];
        let mut n = 0;
        palette.GetColors(&mut colors, &mut n)?;
        if let Some(i) = colors[..n as usize].iter().position(|c| c >> 24 == 0) {
            if let Ok(meta) = frame.GetMetadataQueryWriter() {
                let _ = meta.SetMetadataByName(w!("/grctlext/TransparencyFlag"), &PROPVARIANT::from(true));
                let _ = meta.SetMetadataByName(w!("/grctlext/TransparentColorIndex"), &PROPVARIANT::from(i as u8));
            }
        }
    }
    frame.WriteSource(&conv, std::ptr::null())?;
    Ok(())
}

/// An icon with the usual sizes up to 256, each fitted into a transparent square.
fn ico(f: &IWICImagingFactory, px: &Pixels) -> Result<Vec<u8>> {
    let biggest = px.w.max(px.h);
    let mut images = Vec::new();
    for side in [16u32, 24, 32, 48, 64, 128, 256] {
        if side > biggest && !images.is_empty() {
            break;
        }
        let square = fit_square(f, px, side)?;
        images.push((side, encode_wic(f, &square, Format::Png)?));
    }
    Ok(encode::ico(&images))
}

fn fit_square(f: &IWICImagingFactory, px: &Pixels, side: u32) -> Result<Pixels> {
    let k = side as f64 / px.w.max(px.h) as f64;
    let (w, h) = (((px.w as f64 * k).round() as u32).max(1), ((px.h as f64 * k).round() as u32).max(1));
    let mut scaled = vec![0u8; (w * h * 4) as usize];
    unsafe {
        let bmp = f.CreateBitmapFromMemory(px.w, px.h, &GUID_WICPixelFormat32bppBGRA, px.w * 4, &px.bgra)?;
        let scaler = f.CreateBitmapScaler()?;
        scaler.Initialize(&bmp, w, h, WICBitmapInterpolationModeHighQualityCubic)?;
        scaler.CopyPixels(std::ptr::null(), w * 4, &mut scaled)?;
    }
    let mut out = vec![0u8; (side * side * 4) as usize];
    let (ox, oy) = ((side - w) / 2, (side - h) / 2);
    for y in 0..h {
        let s = (y * w * 4) as usize;
        let d = (((y + oy) * side + ox) * 4) as usize;
        out[d..d + (w * 4) as usize].copy_from_slice(&scaled[s..s + (w * 4) as usize]);
    }
    Ok(Pixels { w: side, h: side, bgra: out, dpi: (96.0, 96.0) })
}

unsafe fn stream_bytes(stream: &IStream) -> Result<Vec<u8>> {
    let h = GetHGlobalFromStream(stream)?;
    let size = GlobalSize(h);
    let ptr = GlobalLock(h) as *const u8;
    if ptr.is_null() {
        bail!("couldn't read the encoded image");
    }
    // The HGLOBAL may be larger than what was written; the stream knows the real size.
    let mut stat = Default::default();
    let len = match stream.Stat(&mut stat, windows::Win32::System::Com::STATFLAG_NONAME) {
        Ok(()) => (stat.cbSize as usize).min(size),
        Err(_) => size,
    };
    let out = std::slice::from_raw_parts(ptr, len).to_vec();
    let _ = GlobalUnlock(h);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Audio and video
// ---------------------------------------------------------------------------

fn media(src: &Path, to: Format, dst: &Path) -> Result<()> {
    use windows::Media::MediaProperties::{AudioEncodingQuality, MediaEncodingProfile, VideoEncodingQuality};
    use windows::Media::Transcoding::MediaTranscoder;
    use windows::Storage::StorageFile;

    let profile = match to {
        Format::Mp3 => MediaEncodingProfile::CreateMp3(AudioEncodingQuality::High)?,
        Format::M4a => MediaEncodingProfile::CreateM4a(AudioEncodingQuality::High)?,
        Format::Wav => MediaEncodingProfile::CreateWav(AudioEncodingQuality::High)?,
        Format::Wma => MediaEncodingProfile::CreateWma(AudioEncodingQuality::High)?,
        Format::Flac => MediaEncodingProfile::CreateFlac(AudioEncodingQuality::High)?,
        Format::Mp4 => MediaEncodingProfile::CreateMp4(VideoEncodingQuality::Auto)?,
        Format::Wmv => MediaEncodingProfile::CreateWmv(VideoEncodingQuality::Auto)?,
        _ => bail!("unsupported format"),
    };
    if !dst.exists() {
        std::fs::write(dst, [])?;
    }
    let source = StorageFile::GetFileFromPathAsync(&HSTRING::from(src.as_os_str()))?.get()?;
    let dest = StorageFile::GetFileFromPathAsync(&HSTRING::from(dst.as_os_str()))?.get()?;
    let transcoder = MediaTranscoder::new()?;
    let prepared = transcoder.PrepareFileTranscodeAsync(&source, &dest, &profile)?.get()?;
    if !prepared.CanTranscode()? {
        let why = match prepared.FailureReason()?.0 {
            2 => "Windows doesn't have a codec for this file.",
            3 => "The file isn't in a format Windows can read.",
            _ => "Windows couldn't convert this file.",
        };
        bail!("{why}");
    }
    prepared.TranscodeAsync()?.get()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

fn read_text(src: &Path) -> Result<String> {
    let bytes = std::fs::read(src).context("Couldn't read the file")?;
    Ok(decode_text(&bytes))
}

/// UTF-8 (with or without a BOM) or UTF-16 with a BOM.
fn decode_text(bytes: &[u8]) -> String {
    let utf16 = |be: bool| {
        let units: Vec<u16> =
            bytes[2..].chunks_exact(2).map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, ..] => utf16(false),
        [0xFE, 0xFF, ..] => utf16(true),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn text_convert(k: Kind, text: &str, to: Format) -> Result<Vec<u8>> {
    let s = match (k, to) {
        (_, Format::Pdf) => {
            let plain = if k == Kind::Html { html_to_text(text) } else { text.to_string() };
            return Ok(encode::pdf_text(&plain));
        }
        (Kind::Html, Format::Txt) => html_to_text(text),
        (Kind::Markdown, Format::Html) => html_page(&markdown_to_html(text)),
        (Kind::Text, Format::Html) => html_page(&format!("<pre>{}</pre>", escape_html(text))),
        (Kind::Text | Kind::Markdown, Format::Txt | Format::Md) => text.to_string(),
        (Kind::Text | Kind::Markdown, Format::Rtf) => rtf(text),
        (Kind::Table(sep), _) => {
            let rows = parse_delimited(text, sep);
            write_table(&rows, to)?
        }
        (Kind::Json, Format::Txt) => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(v) => serde_json::to_string_pretty(&v)?,
            Err(_) => text.to_string(),
        },
        (Kind::Json, _) => {
            let v: serde_json::Value = serde_json::from_str(text).context("The file isn't valid JSON")?;
            write_table(&json_rows(&v)?, to)?
        }
        _ => bail!("unsupported conversion"),
    };
    Ok(s.into_bytes())
}

fn html_page(body: &str) -> String {
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<style>body{{font:15px/1.5 -apple-system,'Segoe UI',sans-serif;max-width:46em;margin:2em auto;padding:0 1em}}pre,code{{font-family:Consolas,monospace}}pre{{white-space:pre-wrap}}table{{border-collapse:collapse}}td,th{{border:1px solid #ccc;padding:4px 8px}}</style>\n</head>\n<body>\n{body}\n</body>\n</html>\n"
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn rtf(text: &str) -> String {
    let mut out = String::from("{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Calibri;}}\\f0\\fs22\n");
    for c in text.replace("\r\n", "\n").chars() {
        match c {
            '\\' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\par\n"),
            '\t' => out.push_str("\\tab "),
            c if (c as u32) < 128 => out.push(c),
            c => {
                // RTF \u takes signed 16-bit units, with '?' as the fallback character.
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{}?", *u as i16));
                }
            }
        }
    }
    out.push('}');
    out
}

/// Text from HTML: tags dropped (scripts and styles with their contents),
/// blocks on their own lines, common entities decoded.
fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let tag_end = rest[i..].find('>').map(|j| i + j + 1).unwrap_or(rest.len());
        let tag = rest[i + 1..tag_end.saturating_sub(1).max(i + 1)].trim().to_lowercase();
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        rest = &rest[tag_end..];
        if (name == "script" || name == "style") && !tag.starts_with('/') {
            let close = format!("</{name}");
            let end = rest.to_lowercase().find(&close).unwrap_or(rest.len());
            rest = &rest[end..];
            continue;
        }
        match name.as_str() {
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "blockquote" | "table" | "ul"
            | "ol" | "section" | "article" | "header" | "footer" => out.push('\n'),
            "td" | "th" => out.push('\t'),
            _ => {}
        }
        if name == "li" && !tag.starts_with('/') {
            out.push_str("• ");
        }
    }
    out.push_str(rest);
    let decoded = decode_entities(&out);
    // Collapse the blank lines left by nested blocks.
    let mut lines: Vec<&str> = Vec::new();
    for l in decoded.lines().map(str::trim_end) {
        if !(l.trim().is_empty() && lines.last().is_none_or(|p| p.trim().is_empty())) {
            lines.push(l);
        }
    }
    lines.join("\n").trim().to_string() + "\n"
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[..rest.len().min(12)].find(';');
        let decoded = end.and_then(|e| {
            let ent = &rest[1..e];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "copy" => Some('©'),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
                _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A small Markdown renderer: headings, paragraphs, lists, quotes, code
/// blocks, rules, and inline code / bold / italic / links.
fn markdown_to_html(md: &str) -> String {
    let mut out = String::new();
    let mut para: Vec<String> = Vec::new();
    let mut list: Option<&str> = None;
    let mut code: Option<String> = None;
    let flush_para = |out: &mut String, para: &mut Vec<String>| {
        if !para.is_empty() {
            out.push_str(&format!("<p>{}</p>\n", inline_md(&para.join(" "))));
            para.clear();
        }
    };
    for line in md.replace("\r\n", "\n").lines() {
        if let Some(buf) = code.as_mut() {
            if line.trim_start().starts_with("```") {
                out.push_str(&format!("<pre><code>{}</code></pre>\n", escape_html(buf)));
                code = None;
            } else {
                buf.push_str(line);
                buf.push('\n');
            }
            continue;
        }
        let t = line.trim();
        let bullet = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| t.strip_prefix("+ "));
        let numbered = t.split_once(". ").filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).map(|(_, r)| r);
        let want = if bullet.is_some() { Some("ul") } else if numbered.is_some() { Some("ol") } else { None };
        if list.is_some() && want != list {
            out.push_str(&format!("</{}>\n", list.unwrap_or("ul")));
            list = None;
        }
        if t.starts_with("```") {
            flush_para(&mut out, &mut para);
            code = Some(String::new());
        } else if t.is_empty() {
            flush_para(&mut out, &mut para);
        } else if let Some(item) = bullet.or(numbered) {
            flush_para(&mut out, &mut para);
            if list.is_none() {
                list = want;
                out.push_str(&format!("<{}>\n", want.unwrap_or("ul")));
            }
            out.push_str(&format!("<li>{}</li>\n", inline_md(item)));
        } else if t.starts_with('#') && t.trim_start_matches('#').starts_with(' ') {
            flush_para(&mut out, &mut para);
            let level = t.chars().take_while(|&c| c == '#').count().min(6);
            out.push_str(&format!("<h{level}>{}</h{level}>\n", inline_md(t[level..].trim())));
        } else if let Some(q) = t.strip_prefix('>') {
            flush_para(&mut out, &mut para);
            out.push_str(&format!("<blockquote>{}</blockquote>\n", inline_md(q.trim())));
        } else if t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*')) {
            flush_para(&mut out, &mut para);
            out.push_str("<hr>\n");
        } else {
            para.push(t.to_string());
        }
    }
    flush_para(&mut out, &mut para);
    if let Some(l) = list {
        out.push_str(&format!("</{l}>\n"));
    }
    if let Some(buf) = code {
        out.push_str(&format!("<pre><code>{}</code></pre>\n", escape_html(&buf)));
    }
    out
}

fn inline_md(s: &str) -> String {
    let s = escape_html(s);
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let find = |from: usize, pat: &str| -> Option<usize> {
        let rest: String = chars[from..].iter().collect();
        rest.find(pat).map(|b| from + rest[..b].chars().count())
    };
    let (mut bold, mut italic) = (false, false);
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '`' {
            if let Some(end) = find(i + 1, "`") {
                out.push_str("<code>");
                out.extend(&chars[i + 1..end]);
                out.push_str("</code>");
                i = end + 1;
                continue;
            }
        }
        if c == '[' {
            if let Some(close) = find(i + 1, "](") {
                if let Some(end) = find(close + 2, ")") {
                    let text: String = chars[i + 1..close].iter().collect();
                    let href: String = chars[close + 2..end].iter().collect();
                    out.push_str(&format!("<a href=\"{href}\">{}</a>", inline_md_plain(&text)));
                    i = end + 1;
                    continue;
                }
            }
        }
        if (c == '*' || c == '_') && next == Some(c) {
            out.push_str(if bold { "</strong>" } else { "<strong>" });
            bold = !bold;
            i += 2;
            continue;
        }
        if c == '*' || (c == '_' && (i == 0 || !chars[i - 1].is_alphanumeric())) || (c == '_' && italic) {
            out.push_str(if italic { "</em>" } else { "<em>" });
            italic = !italic;
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    if italic {
        out.push_str("</em>");
    }
    if bold {
        out.push_str("</strong>");
    }
    out
}

/// Link text, already escaped: only emphasis inside.
fn inline_md_plain(s: &str) -> String {
    s.replace("**", "")
}

/// Rows of a CSV / TSV, honouring quoted fields (with "" escapes and line breaks).
fn parse_delimited(text: &str, sep: u8) -> Vec<Vec<String>> {
    let sep = sep as char;
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                c => field.push(c),
            }
        } else if c == '"' && field.is_empty() {
            quoted = true;
        } else if c == sep {
            row.push(std::mem::take(&mut field));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            row.push(std::mem::take(&mut field));
            rows.push(std::mem::take(&mut row));
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// A JSON array of objects (columns from every key, in order of appearance) or of arrays.
fn json_rows(v: &serde_json::Value) -> Result<Vec<Vec<String>>> {
    use serde_json::Value;
    let cell = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    };
    let items = match v {
        Value::Array(a) => a.as_slice(),
        Value::Object(_) => std::slice::from_ref(v),
        _ => bail!("Only a JSON list (of objects or arrays) can become a table."),
    };
    if items.iter().all(Value::is_object) {
        let mut cols: Vec<String> = Vec::new();
        for o in items.iter().filter_map(Value::as_object) {
            for k in o.keys() {
                if !cols.contains(k) {
                    cols.push(k.clone());
                }
            }
        }
        let mut rows = vec![cols.clone()];
        for o in items.iter().filter_map(Value::as_object) {
            rows.push(cols.iter().map(|c| o.get(c).map(cell).unwrap_or_default()).collect());
        }
        Ok(rows)
    } else {
        Ok(items.iter().map(|r| match r {
            Value::Array(a) => a.iter().map(cell).collect(),
            v => vec![cell(v)],
        })
        .collect())
    }
}

fn write_table(rows: &[Vec<String>], to: Format) -> Result<String> {
    Ok(match to {
        Format::Csv => rows.iter().map(|r| r.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",")).collect::<Vec<_>>().join("\r\n") + "\r\n",
        Format::Tsv => {
            rows.iter().map(|r| r.iter().map(|f| f.replace(['\t', '\n', '\r'], " ")).collect::<Vec<_>>().join("\t")).collect::<Vec<_>>().join("\n")
                + "\n"
        }
        Format::Json => {
            let Some((head, body)) = rows.split_first() else { return Ok("[]\n".into()) };
            let list: Vec<serde_json::Value> = body
                .iter()
                .map(|r| {
                    let mut m = serde_json::Map::new();
                    for (i, h) in head.iter().enumerate() {
                        m.insert(h.clone(), serde_json::Value::String(r.get(i).cloned().unwrap_or_default()));
                    }
                    serde_json::Value::Object(m)
                })
                .collect();
            serde_json::to_string_pretty(&list)? + "\n"
        }
        Format::Html => {
            let mut t = String::from("<table>\n");
            for (i, r) in rows.iter().enumerate() {
                let tag = if i == 0 { "th" } else { "td" };
                t.push_str("<tr>");
                for f in r {
                    t.push_str(&format!("<{tag}>{}</{tag}>", escape_html(f)));
                }
                t.push_str("</tr>\n");
            }
            t.push_str("</table>");
            html_page(&t)
        }
        Format::Md => {
            let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
            let line = |r: &[String]| {
                let cells: Vec<String> = (0..cols).map(|i| r.get(i).map(|f| f.replace('|', "\\|").replace('\n', " ")).unwrap_or_default()).collect();
                format!("| {} |", cells.join(" | "))
            };
            let mut out = Vec::new();
            if let Some((head, body)) = rows.split_first() {
                out.push(line(head));
                out.push(format!("|{}|", vec![" --- "; cols].join("|")));
                out.extend(body.iter().map(|r| line(r)));
            }
            out.join("\n") + "\n"
        }
        _ => bail!("unsupported conversion"),
    })
}

fn csv_field(f: &str) -> String {
    if f.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", f.replace('"', "\"\""))
    } else {
        f.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn offers_every_other_format() {
        let png = targets(Path::new("a.PNG"));
        for f in [Format::Jpeg, Format::Pdf, Format::Bmp, Format::Gif, Format::Tiff, Format::WebP, Format::Ico] {
            assert!(png.contains(&f), "{f:?}");
        }
        assert!(!png.contains(&Format::Png));
        assert!(!targets(Path::new("b.jpeg")).contains(&Format::Jpeg));
        assert_eq!(targets(Path::new("t.csv")), vec![Format::Tsv, Format::Json, Format::Html, Format::Md]);
        assert!(targets(Path::new("song.mp3")).contains(&Format::Flac));
        assert!(targets(Path::new("x.exe")).is_empty());
        assert!(targets(&PathBuf::from("noext")).is_empty());
    }

    #[test]
    fn tables_round_trip() {
        let rows = parse_delimited("name,note\r\n\"Doe, J\",\"said \"\"hi\"\"\"\nX,\"two\nlines\"\n", b',');
        assert_eq!(rows, vec![vec!["name", "note"], vec!["Doe, J", "said \"hi\""], vec!["X", "two\nlines"]]);
        let csv = write_table(&rows, Format::Csv).unwrap();
        assert_eq!(parse_delimited(&csv, b','), rows);
        let json = write_table(&rows, Format::Json).unwrap();
        let back = json_rows(&serde_json::from_str(&json).unwrap()).unwrap();
        assert_eq!(back, rows);
    }

    #[test]
    fn markdown_basics() {
        let html = markdown_to_html("# Title\n\nSome **bold** and *it* `code`.\n\n- a\n- [b](https://x.y)\n\n```\n<x>\n```\n");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<em>it</em>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<ul>\n<li>a</li>\n<li><a href=\"https://x.y\">b</a></li>\n</ul>"));
        assert!(html.contains("<pre><code>&lt;x&gt;\n</code></pre>"));
        assert!(markdown_to_html("snake_case_name").contains("snake_case_name"));
    }

    #[test]
    fn html_text() {
        let t = html_to_text("<html><style>p{}</style><body><h1>Hi &amp; bye</h1><p>One<br>Two</p><ul><li>x</li></ul><script>a<b</script></body></html>");
        assert_eq!(t, "Hi & bye\n\nOne\nTwo\n\n• x\n");
    }

    #[test]
    fn rtf_escapes() {
        assert_eq!(rtf("a{b}\\\né"), "{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Calibri;}}\\f0\\fs22\na\\{b\\}\\\\\\par\n\\u233?}");
    }

    #[test]
    fn utf16_text() {
        assert_eq!(decode_text(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
        assert_eq!(decode_text(&[0xEF, 0xBB, 0xBF, b'o', b'k']), "ok");
    }

    #[test]
    fn orientations() {
        // 2x1 image: red, green. Orientation 6 (stored rotated 90° counter-clockwise) becomes 1x2.
        let px = Pixels { w: 2, h: 1, bgra: vec![0, 0, 255, 255, 0, 255, 0, 255], dpi: (96.0, 72.0) };
        let up = orient(px, 6);
        assert_eq!((up.w, up.h), (1, 2));
        assert_eq!(up.bgra, vec![0, 0, 255, 255, 0, 255, 0, 255]);
        assert_eq!(up.dpi, (72.0, 96.0));
        let px = Pixels { w: 2, h: 1, bgra: vec![1, 1, 1, 1, 2, 2, 2, 2], dpi: (96.0, 96.0) };
        assert_eq!(orient(px, 3).bgra, vec![2, 2, 2, 2, 1, 1, 1, 1]);
    }

    /// A second of WAV through the Windows transcoder to each audio format.
    #[test]
    #[ignore]
    fn live_audio() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let dir = std::env::temp_dir().join(format!("dockside-audio-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rate = 44100u32;
        let samples: Vec<u8> = (0..rate).flat_map(|i| (((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0) as i16).to_le_bytes()).collect();
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        for v in [16u32, 1 | (1 << 16), rate, rate * 2, 2 | (16 << 16)] {
            wav.extend_from_slice(&v.to_le_bytes());
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(&samples);
        let src = dir.join("tone.wav");
        std::fs::write(&src, wav).unwrap();
        for to in targets(&src) {
            let dst = dir.join(format!("out.{}", to.ext()));
            convert(&src, to, &dst).unwrap_or_else(|e| panic!("{to:?}: {e:#}"));
            assert!(std::fs::metadata(&dst).unwrap().len() > 1000, "{to:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Encodes a test image to every image format and reads each back through WIC.
    #[test]
    fn images_round_trip() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let dir = std::env::temp_dir().join(format!("dockside-convert-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h) = (37u32, 23u32);
        let mut bgra = Vec::new();
        for y in 0..h {
            for x in 0..w {
                bgra.extend_from_slice(&[(x * 7) as u8, (y * 11) as u8, ((x + y) * 3) as u8, if x < 4 { 0 } else { 255 }]);
            }
        }
        let f = factory().unwrap();
        let src = dir.join("src.png");
        std::fs::write(&src, encode_wic(&f, &Pixels { w, h, bgra: bgra.clone(), dpi: (96.0, 96.0) }, Format::Png).unwrap()).unwrap();
        for to in targets(&src) {
            let dst = dir.join(format!("out.{}", to.ext()));
            convert(&src, to, &dst).unwrap_or_else(|e| panic!("{to:?}: {e:#}"));
            assert!(std::fs::metadata(&dst).unwrap().len() > 0, "{to:?}");
            if to == Format::Pdf {
                assert!(std::fs::read(&dst).unwrap().starts_with(b"%PDF"));
                continue;
            }
            // WebP needs the (usually preinstalled) WebP Image Extensions to read back.
            let Ok(back) = decode(&f, &dst) else {
                assert_eq!(to, Format::WebP, "couldn't read back {to:?}");
                eprintln!("no WebP decoder: skipped reading it back");
                continue;
            };
            if to == Format::Ico {
                assert_eq!((back.w, back.h), (32, 32));
                continue;
            }
            assert_eq!((back.w, back.h), (w, h), "{to:?}");
            if matches!(to, Format::WebP | Format::Tiff | Format::Bmp) {
                // Lossless (BMP is flattened onto white where transparent).
                for (i, (a, b)) in back.bgra.chunks_exact(4).zip(bgra.chunks_exact(4)).enumerate() {
                    if b[3] == 255 {
                        assert_eq!(a, b, "{to:?} pixel {i}");
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
