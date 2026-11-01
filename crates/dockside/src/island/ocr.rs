//! "Copy Text" for files dropped on the island: Windows' own OCR
//! (`Windows.Media.Ocr`, in the languages installed for the user) reads the
//! text in pictures on a worker thread and puts it on the clipboard.

use std::path::{Path, PathBuf};

use windows::core::{Result, HSTRING};
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
    ExifOrientationMode, SoftwareBitmap,
};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::{FileAccessMode, StorageFile};

use super::holding::{self, Done};

/// Reads the text in the pictures among `paths`, copies it, and reports
/// `Done::Copied` (empty when there was none).
pub fn copy_text(paths: Vec<PathBuf>) {
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let text = paths
            .iter()
            .filter(|p| super::convert::is_image(p))
            .filter_map(|p| match recognize(p) {
                Ok(t) => Some(t),
                Err(e) => {
                    log!("ocr: {}: {e}", p.display());
                    None
                }
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            crate::finder::fs::copy_text(&text);
        }
        holding::finish(Done::Copied(text));
    });
}

fn recognize(path: &Path) -> Result<String> {
    // Fails when no OCR language is installed (Settings > Time & language > Language).
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()?;
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?.get()?;
    let stream = file.OpenAsync(FileAccessMode::Read)?.get()?;
    let decoder = BitmapDecoder::CreateAsync(&stream)?.get()?;
    let bitmap = decode(&decoder, OcrEngine::MaxImageDimension()?)?;
    let lines = engine.RecognizeAsync(&bitmap)?.get()?.Lines()?;
    let text: Vec<String> = lines.into_iter().filter_map(|l| l.Text().ok()).map(|t| t.to_string()).collect();
    Ok(text.join("\n"))
}

/// The picture as BGRA, scaled down to fit the OCR engine's largest side.
fn decode(decoder: &BitmapDecoder, max: u32) -> Result<SoftwareBitmap> {
    let (w, h) = (decoder.OrientedPixelWidth()?, decoder.OrientedPixelHeight()?);
    let transform = BitmapTransform::new()?;
    let longest = w.max(h).max(1);
    if longest > max {
        let k = max as f64 / longest as f64;
        transform.SetScaledWidth(((decoder.PixelWidth()? as f64 * k) as u32).max(1))?;
        transform.SetScaledHeight(((decoder.PixelHeight()? as f64 * k) as u32).max(1))?;
        transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    }
    decoder
        .GetSoftwareBitmapTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )?
        .get()
}
