//! What Quick Look shows inside its panel: pictures (GIFs play), videos and
//! sound (with play/pause and a scrubber), PDFs page by page, Markdown
//! formatted, source code coloured, other text as is, Office documents and
//! anything else with a shell preview handler, and otherwise a big thumbnail
//! or icon.
//!
//! The slow parts (decoding, playing, rendering pages, preview handlers) run
//! on their own threads and post `WM_APP_QUICKLOOK`; [`QuickLook::poll`]
//! collects what came, and the next frame repaints just the content area.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap, ID2D1SolidColorBrush, D2D1_BITMAP_PROPERTIES, D2D1_DRAW_TEXT_OPTIONS_NONE};
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

use super::fs::Entry as FileEntry;
use super::loader::IconLoader;
use super::media::{format_time, Player};
use super::pdf::Pdf;
use super::prevhost::{self, PreviewHost};
use super::rich::{self, Block, Doc, Role};
use super::{Hit, QlStyle};
use crate::anim::Scroll;
use crate::geom::{Color, Rect};
use crate::gfx::{pt, Painter};
use crate::island::glyphs;
use crate::widgets::live::{self, Feed, Frame};

/// Pictures WIC decodes itself (others fall back to the shell's thumbnail).
const PICTURES: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "webp", "heic", "heif", "avif", "jxl", "tif", "tiff", "ico", "jxr", "wdp",
    "dng", "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2",
];
const VIDEO: &[&str] = &["mp4", "m4v", "mov", "mkv", "avi", "wmv", "webm", "mpg", "mpeg", "3gp", "3g2", "mts", "m2ts", "asf"];
const AUDIO: &[&str] = &["mp3", "m4a", "aac", "flac", "wav", "wma", "ogg", "opus", "aiff", "aif", "amr", "ac3", "m4b"];
/// Text read for a preview, at most (bigger files show their start).
const TEXT_MAX: u64 = 192 * 1024;
const MONO: &str = "Consolas";

pub enum Content {
    /// Text, Markdown or code, and its layout for the width last drawn at.
    Text { doc: Doc, truncated: bool, view: Option<TextView> },
    /// A picture or GIF: decoding starts once the panel's size is known.
    Picture { feed: Option<Arc<Feed>>, asked: (u32, u32), frame: Option<Frame>, bmp: Option<ID2D1Bitmap>, failed: bool },
    Media { player: Option<Player>, frame: Option<Frame>, bmp: Option<ID2D1Bitmap>, video: bool, seek_at: Instant },
    Pdf { pdf: Pdf, pages: HashMap<usize, (u32, ID2D1Bitmap)>, fresh: Vec<(usize, u32, Frame)>, asked: Vec<(usize, u32)> },
    Handler { host: Option<PreviewHost>, clsid: windows::core::GUID },
    /// The big thumbnail or icon.
    Fallback,
}

/// A laid-out styled text and the brushes its runs are coloured with.
pub struct TextView {
    layout: IDWriteTextLayout,
    width: f32,
    dark: bool,
    height: f32,
    _brushes: Vec<ID2D1SolidColorBrush>,
}

pub struct QuickLook {
    pub path: PathBuf,
    pub content: Content,
    pub scroll: Scroll,
    pub max_scroll: f32,
    /// The content area last drawn (window points): what news repaints.
    pub area: Option<Rect>,
    /// The scrubber's track, for turning a drag into a time.
    pub scrub: Option<Rect>,
    /// Nothing more will come (closing): media stopped, workers gone.
    frozen: bool,
}

impl Default for QuickLook {
    fn default() -> Self {
        QuickLook { path: PathBuf::new(), content: Content::Fallback, scroll: Scroll::default(), max_scroll: 0.0, area: None, scrub: None, frozen: false }
    }
}

fn has(list: &[&str], ext: &str) -> bool {
    list.contains(&ext)
}

impl QuickLook {
    /// Decides how `e` is shown. Cheap except for text, which is read here.
    pub fn new(e: &FileEntry) -> QuickLook {
        let content = if e.is_dir {
            Content::Fallback
        } else if has(VIDEO, &e.ext) || has(AUDIO, &e.ext) {
            Content::Media { player: None, frame: None, bmp: None, video: has(VIDEO, &e.ext), seek_at: Instant::now() }
        } else if has(PICTURES, &e.ext) {
            Content::Picture { feed: None, asked: (0, 0), frame: None, bmp: None, failed: false }
        } else if e.ext == "pdf" {
            Content::Pdf { pdf: Pdf::open(&e.path, 0, 0), pages: HashMap::new(), fresh: Vec::new(), asked: Vec::new() }
        } else if let Some(c) = text_content(e) {
            c
        } else if let Some(clsid) = prevhost::handler_for(&e.ext) {
            Content::Handler { host: None, clsid }
        } else {
            Content::Fallback
        };
        QuickLook { path: e.path.clone(), content, ..Default::default() }
    }

    /// Starts what needs the window (the PDF worker posts to `notify`).
    pub fn start(&mut self, notify: isize) {
        if let Content::Pdf { pdf, .. } = &mut self.content {
            *pdf = Pdf::open(&self.path, notify, crate::win::WM_APP_QUICKLOOK);
        }
    }

    /// Collects what the workers sent; true if there's something new to draw.
    pub fn poll(&mut self) -> bool {
        match &mut self.content {
            Content::Picture { feed: Some(feed), frame, failed, .. } => {
                if let Some(f) = feed.take() {
                    *frame = Some(f);
                    return true;
                }
                if feed.error().is_some() && !*failed {
                    *failed = true;
                    return true;
                }
                false
            }
            Content::Media { player: Some(player), frame, .. } => {
                let before = (player.state.playing, player.state.ready, player.state.position, player.state.error.is_some());
                if let Some(f) = player.poll() {
                    *frame = Some(f);
                    return true;
                }
                let s = &player.state;
                before != (s.playing, s.ready, s.position, s.error.is_some())
            }
            Content::Pdf { pdf, fresh, .. } => {
                let got = pdf.take();
                let any = !got.is_empty();
                fresh.extend(got);
                // The page list arriving is news too.
                any || pdf.pages().is_some()
            }
            Content::Handler { host: Some(_), .. } => true,
            _ => false,
        }
    }

    /// Quick Look is closing: sound stops now, the last picture stays for the zoom away.
    pub fn freeze(&mut self) {
        self.frozen = true;
        match &mut self.content {
            Content::Media { player, .. } => *player = None,
            Content::Picture { feed, .. } => {
                if let Some(f) = feed.take() {
                    f.stop();
                }
            }
            Content::Handler { host, .. } => *host = None,
            _ => {}
        }
    }

    /// The Finder window was minimized or hidden: pause sound and video.
    pub fn pause(&mut self) {
        if let Content::Media { player: Some(p), .. } = &mut self.content {
            p.pause();
        }
    }

    pub fn toggle_play(&mut self) {
        if let Content::Media { player: Some(p), .. } = &mut self.content {
            p.toggle();
        }
    }

    /// Seeks to where window x falls on the scrubber (throttled while dragging).
    pub fn scrub_to(&mut self, x: f32, force: bool) {
        let Some(track) = self.scrub else { return };
        if let Content::Media { player: Some(p), seek_at, .. } = &mut self.content {
            let d = p.state.duration;
            if d <= 0.0 || !force && seek_at.elapsed() < Duration::from_millis(33) {
                return;
            }
            *seek_at = Instant::now();
            let k = ((x - track.x) / track.w.max(1.0)).clamp(0.0, 1.0) as f64;
            p.seek(k * d);
        }
    }

    /// Where the preview handler's window goes (device pixels from the window's
    /// corner), or None to hide it. Call after drawing.
    pub fn place_host(&mut self, at: Option<RECT>) {
        if let Content::Handler { host: Some(h), .. } = &mut self.content {
            h.place(at.filter(|_| h.status() == prevhost::READY));
        }
    }
}

impl Drop for QuickLook {
    fn drop(&mut self) {
        if let Content::Picture { feed: Some(f), .. } = &self.content {
            f.stop();
        }
    }
}

// ---------------------------------------------------------------------------
// Text

fn text_content(e: &FileEntry) -> Option<Content> {
    let named = matches!(e.name.to_lowercase().as_str(), "makefile" | "dockerfile" | "license" | "readme" | "changelog" | "authors" | "copying");
    let lang = rich::lang_for(&e.ext, &e.name);
    let known = super::fs::TEXT_EXT.contains(&e.ext.as_str()) || named || lang.is_some();
    // Unknown types are looked at: plain text in them still shows.
    let sniff = !known && !IconLoader::key_for(e).1 && prevhost::handler_for(&e.ext).is_none() && e.size < 64 << 20;
    if !known && !sniff {
        return None;
    }
    let (text, truncated) = read_text(e, !known)?;
    let doc = match (e.ext.as_str(), lang) {
        ("md" | "markdown" | "mdown" | "mkd", _) => rich::markdown(&text),
        (_, Some(lang)) => rich::code(&text, lang),
        _ => rich::plain(&text),
    };
    Some(Content::Text { doc, truncated, view: None })
}

/// The start of a text file (UTF-8, or UTF-16 with a BOM), tabs expanded.
/// `strict`: an unknown type, which must look like text throughout.
fn read_text(e: &FileEntry, strict: bool) -> Option<(String, bool)> {
    let mut buf = Vec::new();
    let max = if strict { 8 * 1024 } else { TEXT_MAX };
    std::fs::File::open(&e.path).ok()?.take(max).read_to_end(&mut buf).ok()?;
    let truncated = (buf.len() as u64) < e.size;
    let text = if buf.starts_with(&[0xFF, 0xFE]) || buf.starts_with(&[0xFE, 0xFF]) {
        let le = buf[0] == 0xFF;
        let units: Vec<u16> = buf[2..].chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        String::from_utf16_lossy(&units)
    } else {
        if buf.contains(&0) {
            return None;
        }
        // A cut in the middle of a character isn't a reason to give up.
        let valid = match std::str::from_utf8(&buf) {
            Ok(_) => true,
            Err(err) => err.error_len().is_none(),
        };
        if strict && !valid {
            return None;
        }
        String::from_utf8_lossy(&buf).into_owned()
    };
    if strict {
        let control = text.chars().filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{c}')).count();
        if text.trim().is_empty() || control * 50 > text.len() {
            return None;
        }
        // Small enough to read whole now it's known to be text.
        if truncated {
            return read_text(e, false);
        }
    }
    Some((text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\t', "    "), truncated))
}

fn role_color(role: Role, s: &QlStyle) -> Color {
    let d = s.dark;
    let hex = |l: u32, k: u32| {
        let v = if d { k } else { l };
        Color::rgba((v >> 16) as u8, (v >> 8) as u8, v as u8, 1.0)
    };
    match role {
        Role::Text => s.theme.text,
        Role::Secondary => s.theme.text_secondary,
        Role::Accent => s.theme.accent,
        // Xcode's default colours.
        Role::Keyword => hex(0x9B2393, 0xFC5FA3),
        Role::Str => hex(0xC41A16, 0xFC6A5D),
        Role::Comment => hex(0x5D6C79, 0x7F8C98),
        Role::Number => hex(0x1C00CF, 0xD0BF69),
        Role::Type => hex(0x0B4F79, 0x5DD8FF),
        Role::Attr => hex(0x643820, 0xFD8F3F),
    }
}

fn range(at: u32, len: u32) -> DWRITE_TEXT_RANGE {
    DWRITE_TEXT_RANGE { startPosition: at, length: len }
}

fn build_view(p: &Painter, doc: &Doc, width: f32, s: &QlStyle) -> Option<TextView> {
    let format = if doc.prose { p.gfx.format_block(&s.theme.font, 13.0, crate::gfx::Weight::Regular, crate::gfx::Align::Leading) } else { s.mono.clone() };
    let layout = p.gfx.layout(&doc.text, &format, width, 1_000_000.0)?;
    let mut brushes: Vec<(Role, ID2D1SolidColorBrush)> = Vec::new();
    unsafe {
        let _ = layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
        let _ = layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        let _ = layout.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
        if doc.prose {
            let _ = layout.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_DEFAULT, 0.0, 0.0);
        }
        let mono = HSTRING::from(MONO);
        for &(at, len, st) in &doc.runs {
            let r = range(at, len);
            if st.bold {
                let _ = layout.SetFontWeight(if doc.prose && st.size > 0.0 { DWRITE_FONT_WEIGHT_SEMI_BOLD } else { DWRITE_FONT_WEIGHT_BOLD }, r);
            }
            if st.italic {
                let _ = layout.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, r);
            }
            if st.strike {
                let _ = layout.SetStrikethrough(true, r);
            }
            if st.mono && doc.prose {
                let _ = layout.SetFontFamilyName(&mono, r);
                let _ = layout.SetFontSize(12.0, r);
            }
            if st.size > 0.0 {
                let _ = layout.SetFontSize(st.size, r);
            }
            if st.role != Role::Text {
                let brush = match brushes.iter().find(|(role, _)| *role == st.role) {
                    Some((_, b)) => b.clone(),
                    None => {
                        let Ok(b) = p.rt.CreateSolidColorBrush(&role_color(st.role, s).d2d(), None) else { continue };
                        brushes.push((st.role, b.clone()));
                        b
                    }
                };
                if let Ok(unk) = brush.cast::<windows::core::IUnknown>() {
                    let _ = layout.SetDrawingEffect(&unk, r);
                }
            }
        }
        let mut m = DWRITE_TEXT_METRICS::default();
        let _ = layout.GetMetrics(&mut m);
        Some(TextView { layout, width, dark: s.dark, height: m.height, _brushes: brushes.into_iter().map(|(_, b)| b).collect() })
    }
}

/// The y extent (layout coordinates) of a range of text.
fn span_y(layout: &IDWriteTextLayout, at: u32, len: u32) -> Option<(f32, f32)> {
    let mut n = 0u32;
    unsafe {
        let _ = layout.HitTestTextRange(at, len.max(1), 0.0, 0.0, None, &mut n);
        let mut v = vec![DWRITE_HIT_TEST_METRICS::default(); n.max(1) as usize];
        layout.HitTestTextRange(at, len.max(1), 0.0, 0.0, Some(&mut v), &mut n).ok()?;
        let v = &v[..n as usize];
        let top = v.iter().map(|m| m.top).fold(f32::MAX, f32::min);
        let bottom = v.iter().map(|m| m.top + m.height).fold(f32::MIN, f32::max);
        (top <= bottom).then_some((top, bottom))
    }
}

fn draw_text(p: &Painter, area: Rect, ql: &mut QuickLook, s: &QlStyle) {
    let Content::Text { doc, truncated, view } = &mut ql.content else { return };
    let paper = if s.dark { Color::rgba(30, 30, 30, 1.0) } else { Color::WHITE };
    p.fill_round(area, 6.0, paper);
    let inner = if doc.prose { area.inset(28.0, 20.0) } else { area.inset(14.0, 12.0) };
    if view.as_ref().is_none_or(|v| (v.width - inner.w).abs() > 0.5 || v.dark != s.dark) {
        *view = build_view(p, doc, inner.w, s);
    }
    let Some(v) = view.as_ref() else { return };
    let footer = if *truncated { 28.0 } else { 0.0 };
    ql.max_scroll = (v.height + footer - inner.h).max(0.0);
    ql.scroll.clamp(ql.max_scroll);
    let top = inner.y - ql.scroll.offset();
    let ink = if s.dark { 255 } else { 0 };
    p.clipped(area.inset(1.0, 1.0), || {
        for b in &doc.blocks {
            match *b {
                Block::Code(at, len) => {
                    if let Some((y0, y1)) = span_y(&v.layout, at, len) {
                        let r = Rect::new(inner.x - 10.0, top + y0 - 6.0, inner.w + 20.0, y1 - y0 + 12.0);
                        if r.bottom() > area.y && r.y < area.bottom() {
                            p.fill_round(r, 6.0, Color::gray(ink, 0.05));
                        }
                    }
                }
                Block::Quote(at, len) => {
                    if let Some((y0, y1)) = span_y(&v.layout, at, len) {
                        p.fill_round(Rect::new(inner.x - 14.0, top + y0, 3.0, y1 - y0), 1.5, Color::gray(ink, 0.18));
                    }
                }
                Block::Rule(at) => {
                    if let Some((y0, y1)) = span_y(&v.layout, at, 1) {
                        let y = top + (y0 + y1) / 2.0;
                        p.line(inner.x, y, inner.right(), y, Color::gray(ink, 0.12), 1.0);
                    }
                }
            }
        }
        unsafe {
            if let Ok(brush) = p.rt.CreateSolidColorBrush(&s.theme.text.d2d(), None) {
                p.rt.DrawTextLayout(pt(inner.x, top), &v.layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
        }
        if *truncated {
            let r = Rect::new(inner.x, top + v.height + 8.0, inner.w, 20.0);
            p.text("Only the start of this file is shown.", s.center, r, s.theme.text_secondary);
        }
    });
}

// ---------------------------------------------------------------------------
// Pictures, video, sound

fn upload(p: &Painter, f: &Frame, into: &mut Option<ID2D1Bitmap>) {
    unsafe {
        if let Some(b) = into.as_ref() {
            let sz = b.GetPixelSize();
            if (sz.width, sz.height) == (f.w, f.h) && b.CopyFromMemory(None, f.data.as_ptr() as *const _, f.w * 4).is_ok() {
                return;
            }
        }
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        *into = p.rt.CreateBitmap(D2D_SIZE_U { width: f.w, height: f.h }, Some(f.data.as_ptr() as *const _), f.w * 4, &props).ok();
    }
}

/// `bmp` (`px` device pixels) fitted in `area`, centred, never enlarged past its pixels.
fn fit(bmp: &ID2D1Bitmap, area: Rect, scale: f32, grow: bool) -> Rect {
    let sz = unsafe { bmp.GetPixelSize() };
    let (bw, bh) = (sz.width.max(1) as f32 / scale, sz.height.max(1) as f32 / scale);
    let mut k = (area.w / bw).min(area.h / bh);
    if !grow {
        k = k.min(1.0);
    }
    let (w, h) = (bw * k, bh * k);
    Rect::new(area.cx() - w / 2.0, area.cy() - h / 2.0, w, h)
}

fn px(area: Rect, scale: f32) -> (u32, u32) {
    ((area.w * scale).round().max(1.0) as u32, (area.h * scale).round().max(1.0) as u32)
}

/// Returns false if the picture can't be shown (the caller draws the thumbnail).
fn draw_picture(p: &Painter, area: Rect, ql: &mut QuickLook, s: &QlStyle) -> bool {
    let frozen = ql.frozen;
    let Content::Picture { feed, asked, frame, bmp, failed } = &mut ql.content else { return false };
    if *failed {
        return false;
    }
    let want = px(area, s.scale);
    // Decoded again only when the panel grows well past what was decoded.
    let grew = want.0 as f32 > asked.0 as f32 * 1.15 || want.1 as f32 > asked.1 as f32 * 1.15;
    if !frozen && (feed.is_none() || grew && bmp.is_some()) {
        if let Some(f) = feed.take() {
            f.stop();
        }
        let f = Arc::new(Feed::default());
        live::play_fitted(ql.path.clone(), want, f.clone(), s.notify, crate::win::WM_APP_QUICKLOOK);
        *feed = Some(f);
        *asked = want;
    }
    if let Some(f) = frame.take() {
        upload(p, &f, bmp);
    }
    let Some(b) = bmp.as_ref() else { return false };
    p.bitmap(b, fit(b, area, s.scale, false), 1.0);
    true
}

fn draw_media(p: &Painter, area: Rect, ql: &mut QuickLook, e: &FileEntry, image: Option<&ID2D1Bitmap>, info: &str, s: &QlStyle, hover: &dyn Fn(Hit) -> f32, hits: &mut Vec<(Rect, Hit)>) {
    let frozen = ql.frozen;
    let path = ql.path.clone();
    let Content::Media { player, frame, bmp, video, .. } = &mut ql.content else { return };
    let want = px(area, s.scale);
    if player.is_none() && !frozen {
        *player = Some(Player::open(&path, want, s.notify, crate::win::WM_APP_QUICKLOOK, 0));
    }
    if let Some(pl) = player.as_ref() {
        pl.set_size(want.0, want.1);
    }
    if let Some(f) = frame.take() {
        upload(p, &f, bmp);
    }
    let state = player.as_ref().map(|p| p.state.clone()).unwrap_or_default();
    let ink = if s.dark { 255 } else { 0 };
    if let Some(err) = &state.error {
        big_icon(p, area, e, image, err, s);
        return;
    }
    if *video {
        let Some(b) = bmp.as_ref() else {
            // Until the first frame: the thumbnail, dimmed a little.
            if let Some(img) = image {
                p.bitmap(img, fit(img, area, 1.0, false), 0.6);
            }
            return;
        };
        let r = fit(b, area, s.scale, true);
        p.fill_rect(r, Color::gray(0, 1.0));
        p.bitmap(b, r, 1.0);
        hits.push((r, Hit::QlMedia));
        // The controls float over the bottom of the picture while the pointer's
        // over it, and stay while paused.
        let over = hover(Hit::QlMedia).max(hover(Hit::QlPlay)).max(hover(Hit::QlScrub));
        let t = if state.playing { over } else { 1.0 };
        if t > 0.01 && !frozen {
            let w = (r.w - 32.0).clamp(160.0, 520.0);
            let bar = Rect::new(r.cx() - w / 2.0, r.bottom() - 16.0 - 34.0, w, 34.0);
            controls(p, bar, &state, t, true, &mut ql.scrub, s, hover, hits);
        }
        return;
    }
    // Sound: the artwork (or icon), its name, and the controls under them.
    let side = 200.0_f32.min(area.h - 150.0).max(48.0);
    let top = area.cy() - (side + 110.0) / 2.0;
    if let Some(img) = image {
        let sz = unsafe { img.GetSize() };
        let k = (side / sz.width.max(1.0)).min(side / sz.height.max(1.0));
        let (iw, ih) = (sz.width * k, sz.height * k);
        p.bitmap(img, Rect::new(area.cx() - iw / 2.0, top + side - ih, iw, ih), 1.0);
    }
    p.text(&e.name, s.preview_title, Rect::new(area.x, top + side + 12.0, area.w, 24.0), s.theme.text);
    p.text(info, s.center, Rect::new(area.x, top + side + 36.0, area.w, 20.0), s.theme.text_secondary);
    let w = area.w.min(380.0);
    let bar = Rect::new(area.cx() - w / 2.0, top + side + 68.0, w, 34.0);
    p.fill_round(bar, 9.0, Color::gray(ink, 0.06));
    controls(p, bar, &state, 1.0, false, &mut ql.scrub, s, hover, hits);
}

/// Play/pause, elapsed time, the scrubber and the time left, in `bar`.
/// `hud`: over video (light on a dark glass), else in the panel's colours.
#[allow(clippy::too_many_arguments)]
fn controls(p: &Painter, bar: Rect, st: &super::media::State, t: f32, hud: bool, scrub: &mut Option<Rect>, s: &QlStyle, hover: &dyn Fn(Hit) -> f32, hits: &mut Vec<(Rect, Hit)>) {
    let fg = if hud { Color::gray(255, 0.92 * t) } else { s.theme.text.alpha(t) };
    let dim = if hud { Color::gray(255, 0.35 * t) } else { s.theme.text_secondary.alpha(0.5 * t) };
    if hud {
        p.fill_round(bar, 10.0, Color::gray(20, 0.62 * t));
        p.stroke_round(bar, 10.0, Color::gray(255, 0.10 * t), 1.0);
    }
    let btn = Rect::new(bar.x + 6.0, bar.cy() - 13.0, 26.0, 26.0);
    let hv = hover(Hit::QlPlay);
    if hv > 0.0 {
        p.fill_round(btn, 6.0, if hud { Color::gray(255, 0.14 * hv * t) } else { Color::gray(if s.dark { 255 } else { 0 }, 0.08 * hv * t) });
    }
    if st.playing {
        glyphs::pause(p, btn.cx(), btn.cy(), 12.0, fg);
    } else {
        glyphs::play(p, btn.cx(), btn.cy(), 13.0, fg);
    }
    hits.push((btn, Hit::QlPlay));
    let small = s.time;
    let elapsed = Rect::new(btn.right() + 4.0, bar.y, 46.0, bar.h);
    p.text(&format_time(st.position), small, elapsed, fg);
    let left = Rect::new(bar.right() - 54.0, bar.y, 48.0, bar.h);
    let remaining = if st.duration > 0.0 { format!("-{}", format_time(st.duration - st.position)) } else { "--:--".into() };
    p.text(&remaining, small, left, fg);
    let track = Rect::new(elapsed.right() + 6.0, bar.cy() - 2.0, (left.x - 6.0) - (elapsed.right() + 6.0), 4.0);
    if track.w < 20.0 {
        return;
    }
    let k = if st.duration > 0.0 { (st.position / st.duration).clamp(0.0, 1.0) as f32 } else { 0.0 };
    p.fill_round(track, 2.0, dim);
    p.fill_round(Rect::new(track.x, track.y, track.w * k, track.h), 2.0, fg);
    let hs = hover(Hit::QlScrub);
    if hs > 0.0 {
        p.fill_circle(track.x + track.w * k, track.cy(), 5.5 * hs, fg);
    }
    *scrub = Some(track);
    hits.push((Rect::new(track.x - 4.0, bar.y, track.w + 8.0, bar.h), Hit::QlScrub));
}

fn big_icon(p: &Painter, area: Rect, e: &FileEntry, image: Option<&ID2D1Bitmap>, info: &str, s: &QlStyle) {
    let side = 192.0_f32.min(area.h - 90.0).max(48.0);
    let top = area.cy() - (side + 70.0) / 2.0;
    if let Some(b) = image {
        let size = unsafe { b.GetSize() };
        let k = (side / size.width.max(1.0)).min(side / size.height.max(1.0));
        let (iw, ih) = (size.width * k, size.height * k);
        p.bitmap(b, Rect::new(area.cx() - iw / 2.0, top + side - ih, iw, ih), 1.0);
    }
    p.text(&e.name, s.preview_title, Rect::new(area.x, top + side + 16.0, area.w, 24.0), s.theme.text);
    p.text(info, s.center, Rect::new(area.x, top + side + 42.0, area.w, 20.0), s.theme.text_secondary);
}

// ---------------------------------------------------------------------------
// PDF

const PAGE_GAP: f32 = 14.0;

fn draw_pdf(p: &Painter, area: Rect, ql: &mut QuickLook, e: &FileEntry, image: Option<&ID2D1Bitmap>, info: &str, s: &QlStyle) {
    let Content::Pdf { pdf, pages, fresh, asked } = &mut ql.content else { return };
    let sizes = match pdf.pages() {
        Some(Ok(v)) if !v.is_empty() => v,
        Some(Err(why)) => return big_icon(p, area, e, image, &why, s),
        Some(Ok(_)) => return big_icon(p, area, e, image, info, s),
        // Opening: the first page's thumbnail stands in.
        None => {
            if let Some(b) = image {
                p.bitmap(b, fit(b, area, 1.0, false), 1.0);
            }
            return;
        }
    };
    for (i, w, f) in fresh.drain(..) {
        let mut slot = None;
        upload(p, &f, &mut slot);
        if let Some(b) = slot {
            pages.insert(i, (w, b));
        }
    }
    let ink = if s.dark { 255 } else { 0 };
    p.fill_round(area, 6.0, Color::gray(ink, if s.dark { 0.04 } else { 0.05 }));
    let page_w = (area.w - 48.0).min(1000.0);
    let heights: Vec<f32> = sizes.iter().map(|(w, h)| page_w * h / w.max(1.0)).collect();
    let total: f32 = heights.iter().sum::<f32>() + PAGE_GAP * (heights.len() as f32 + 1.0);
    ql.max_scroll = (total - area.h).max(0.0);
    ql.scroll.clamp(ql.max_scroll);
    // Widths rendered at go up in steps, so a small resize doesn't redo pages.
    let want_px = (((page_w * s.scale) / 128.0).ceil() * 128.0) as u32;
    let mut y = area.y + PAGE_GAP - ql.scroll.offset();
    let mut visible = Vec::new();
    p.clipped(area, || {
        for (i, h) in heights.iter().enumerate() {
            let r = Rect::new(area.cx() - page_w / 2.0, y, page_w, *h);
            y += h + PAGE_GAP;
            // One page either side is kept ready.
            let near = r.bottom() > area.y - area.h && r.y < area.bottom() + area.h;
            if !near {
                continue;
            }
            visible.push(i);
            if r.bottom() < area.y || r.y > area.bottom() {
                continue;
            }
            for k in 1..=3 {
                let d = k as f32;
                p.fill_round(r.inset(-d, -d).offset(0.0, d * 0.5), d + 1.0, Color::gray(0, 0.035));
            }
            p.fill_rect(r, Color::WHITE);
            if let Some((_, b)) = pages.get(&i) {
                p.bitmap(b, r, 1.0);
            }
        }
    });
    pages.retain(|i, _| visible.contains(i));
    let missing: Vec<(usize, u32)> = visible.iter().filter(|i| pages.get(i).is_none_or(|(w, _)| *w != want_px)).map(|i| (*i, want_px)).collect();
    if missing != *asked && !ql.frozen {
        pdf.want(missing.clone());
        *asked = missing;
    }
}

// ---------------------------------------------------------------------------

/// Draws the content area for `ql`.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    p: &Painter,
    area: Rect,
    ql: &mut QuickLook,
    e: &FileEntry,
    image: Option<ID2D1Bitmap>,
    info: &str,
    s: &QlStyle,
    hover: &dyn Fn(Hit) -> f32,
    hits: &mut Vec<(Rect, Hit)>,
) {
    ql.area = Some(area);
    ql.scrub = None;
    if !matches!(ql.content, Content::Text { .. } | Content::Pdf { .. }) {
        ql.max_scroll = 0.0;
    }
    match &ql.content {
        Content::Text { .. } => return draw_text(p, area, ql, s),
        Content::Pdf { .. } => return draw_pdf(p, area, ql, e, image.as_ref(), info, s),
        Content::Media { .. } => return draw_media(p, area, ql, e, image.as_ref(), info, s, hover, hits),
        Content::Picture { .. } => {
            if draw_picture(p, area, ql, s) {
                return;
            }
        }
        Content::Handler { host, clsid } => {
            if host.is_none() && !ql.frozen {
                let clsid = *clsid;
                let bg = if s.dark { Color::rgba(44, 44, 46, 1.0) } else { Color::rgba(246, 246, 246, 1.0) };
                let cr = |c: Color| {
                    let d = c.d2d();
                    COLORREF((d.r * 255.0) as u32 | ((d.g * 255.0) as u32) << 8 | ((d.b * 255.0) as u32) << 16)
                };
                let h = PreviewHost::open(&ql.path, clsid, HWND(s.owner as _), cr(bg), cr(s.theme.text), s.notify, crate::win::WM_APP_QUICKLOOK);
                ql.content = Content::Handler { host: Some(h), clsid };
            }
            // Under the preview window (or until it's up): the thumbnail. If the
            // handler failed, the usual big icon.
            if let Content::Handler { host: Some(h), .. } = &ql.content {
                if h.status() != prevhost::FAILED {
                    if let Some(b) = image.as_ref().filter(|_| IconLoader::key_for(e).1) {
                        p.bitmap(b, fit(b, area, 1.0, false), 1.0);
                        return;
                    }
                }
            }
        }
        Content::Fallback => {}
    }
    // The big thumbnail, or the icon with the name and details under it.
    let Some(b) = image else { return };
    if IconLoader::key_for(e).1 {
        p.bitmap(&b, fit(&b, area, 1.0, false), 1.0);
    } else {
        big_icon(p, area, e, Some(&b), info, s);
    }
}
