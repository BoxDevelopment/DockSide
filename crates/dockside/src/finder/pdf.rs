//! Quick Look's PDF pages, rendered by Windows.Data.Pdf on a worker thread.
//!
//! The document opens once; pages are rendered only when they scroll into
//! view, at the width they're shown at, and the UI keeps just the few around
//! what's visible.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use windows::core::HSTRING;
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapEncoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode, ExifOrientationMode};
use windows::Storage::StorageFile;
use windows::Storage::Streams::InMemoryRandomAccessStream;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub use crate::widgets::live::Frame;

#[derive(Default)]
struct Shared {
    /// Each page's size in points, once the document is open (or why it can't be).
    pages: Mutex<Option<Result<Vec<(f32, f32)>, String>>>,
    /// Rendered pages not yet taken: (page, width asked for, pixels).
    done: Mutex<Vec<(usize, u32, Frame)>>,
    pinged: AtomicBool,
}

pub struct Pdf {
    tx: Sender<Vec<(usize, u32)>>,
    shared: Arc<Shared>,
}

impl Pdf {
    /// Opens `path`; `hwnd` gets `msg` when there's news.
    pub fn open(path: &Path, hwnd: isize, msg: u32) -> Pdf {
        let (tx, rx) = channel::<Vec<(usize, u32)>>();
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        let file = path.to_path_buf();
        let spawned = std::thread::Builder::new().name("quick look pdf".into()).spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let ping = || {
                if !worker.pinged.swap(true, Ordering::AcqRel) {
                    crate::win::post(hwnd, msg);
                }
            };
            let doc = match load(&file) {
                Ok(d) => d,
                Err(why) => {
                    *worker.pages.lock().unwrap() = Some(Err(why));
                    return ping();
                }
            };
            let sizes: Vec<(f32, f32)> = (0..doc.PageCount().unwrap_or(0))
                .map(|i| doc.GetPage(i).and_then(|p| p.Size()).map(|s| (s.Width, s.Height)).unwrap_or((612.0, 792.0)))
                .collect();
            *worker.pages.lock().unwrap() = Some(Ok(sizes));
            ping();
            // The UI sends the pages it wants; only the newest list counts.
            let mut next: Option<Vec<(usize, u32)>> = None;
            while let Some(mut want) = next.take().or_else(|| rx.recv().ok()) {
                while let Ok(newer) = rx.try_recv() {
                    want = newer;
                }
                for (page, w) in want {
                    // Something newer asked for: that wins.
                    if let Ok(newer) = rx.try_recv() {
                        next = Some(newer);
                        break;
                    }
                    match render(&doc, page, w) {
                        Some(f) => {
                            worker.done.lock().unwrap().push((page, w, f));
                            ping();
                        }
                        None => log!("quick look: couldn't render page {page}"),
                    }
                }
            }
        });
        if let Err(e) = spawned {
            log!("quick look pdf thread failed: {e}");
        }
        Pdf { tx, shared }
    }

    /// Page sizes in points: None while opening, Err if it can't be opened.
    pub fn pages(&self) -> Option<Result<Vec<(f32, f32)>, String>> {
        self.shared.pages.lock().ok()?.clone()
    }

    /// Asks for these pages at these pixel widths (replacing any earlier ask).
    pub fn want(&self, pages: Vec<(usize, u32)>) {
        let _ = self.tx.send(pages);
    }

    /// Pages rendered since the last call.
    pub fn take(&self) -> Vec<(usize, u32, Frame)> {
        self.shared.pinged.store(false, Ordering::Release);
        self.shared.done.lock().map(|mut d| std::mem::take(&mut *d)).unwrap_or_default()
    }
}

fn load(path: &Path) -> Result<PdfDocument, String> {
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str())).and_then(|op| op.get()).map_err(|_| "Can't open this PDF".to_string())?;
    PdfDocument::LoadFromFileAsync(&file).and_then(|op| op.get()).map_err(|e| {
        // A locked PDF fails to load without its password.
        if e.code().0 as u32 == 0x8007052B {
            "This PDF is locked with a password".to_string()
        } else {
            "Can't open this PDF".to_string()
        }
    })
}

/// Page `i` rendered `w` pixels wide.
fn render(doc: &PdfDocument, i: usize, w: u32) -> Option<Frame> {
    let page = doc.GetPage(i as u32).ok()?;
    let stream = InMemoryRandomAccessStream::new().ok()?;
    let opts = PdfPageRenderOptions::new().ok()?;
    opts.SetDestinationWidth(w).ok()?;
    // BMP: quick to write and read back (PNG compression is wasted here).
    if let Ok(id) = BitmapEncoder::BmpEncoderId() {
        let _ = opts.SetBitmapEncoderId(id);
    }
    page.RenderWithOptionsToStreamAsync(&stream, &opts).ok()?.get().ok()?;
    stream.Seek(0).ok()?;
    let decoder = BitmapDecoder::CreateAsync(&stream).ok()?.get().ok()?;
    let (pw, ph) = (decoder.PixelWidth().ok()?, decoder.PixelHeight().ok()?);
    let px = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &BitmapTransform::new().ok()?,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )
        .ok()?
        .get()
        .ok()?;
    let data = px.DetachPixelData().ok()?.to_vec();
    (data.len() == (pw * ph * 4) as usize).then_some(Frame { w: pw, h: ph, data })
}
