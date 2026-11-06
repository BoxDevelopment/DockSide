//! The Launchpad item's Dock icon, drawn in code: a 3 × 3 grid of coloured
//! rounded squares on a pale tile, like macOS's Launchpad and Apps icons.

use super::icons::{IconPixels, ICON_PX};
use super::{Canvas, Gfx};
use crate::geom::{Color, Rect};

/// Icon-store key for the drawn icon.
pub const KEY: &str = "dockside:launchpad-icon";

/// The squares' colours, top-left to bottom-right: (top, bottom) of each gradient.
const SQUARES: [((u8, u8, u8), (u8, u8, u8)); 9] = [
    ((255, 94, 87), (235, 52, 64)),
    ((255, 176, 46), (250, 128, 22)),
    ((255, 214, 10), (247, 179, 0)),
    ((102, 222, 104), (40, 186, 74)),
    ((70, 214, 214), (24, 172, 196)),
    ((64, 160, 255), (10, 110, 240)),
    ((126, 112, 255), (88, 74, 230)),
    ((222, 108, 255), (186, 64, 230)),
    ((255, 106, 170), (240, 62, 130)),
];

pub fn render(gfx: &Gfx) -> Option<IconPixels> {
    let size = ICON_PX;
    let mut canvas = Canvas::new(gfx, size, size, 1.0).ok()?;
    let s = size as f32;
    // The same tile inset and rounding the Dock uses for its squircle tiles.
    let tile = Rect::new(0.0, 0.0, s, s).inset(s * 0.06, s * 0.06);
    let t = tile.w;
    let r = t * 0.225;
    canvas
        .draw(gfx, |p| {
            p.fill_round(tile.offset(0.0, s * 0.01), r, Color::gray(0, 0.14));
            p.fill_round_gradient(tile, r, Color::rgba(250, 250, 252, 1.0), Color::rgba(218, 222, 230, 1.0));
            let (sq, gap) = (t * 0.2, t * 0.07);
            let start = tile.x + (t - 3.0 * sq - 2.0 * gap) / 2.0;
            let top = tile.y + (t - 3.0 * sq - 2.0 * gap) / 2.0;
            for (i, (a, b)) in SQUARES.iter().enumerate() {
                let (c, row) = ((i % 3) as f32, (i / 3) as f32);
                let rect = Rect::new(start + c * (sq + gap), top + row * (sq + gap), sq, sq);
                p.fill_round_gradient(rect, sq * 0.28, Color::rgba(a.0, a.1, a.2, 1.0), Color::rgba(b.0, b.1, b.2, 1.0));
            }
        })
        .ok()?;
    Some(IconPixels { width: size as u32, height: size as u32, data: canvas.pixels() })
}

#[cfg(test)]
mod tests {
    /// Writes the icon to `DOCKSIDE_ICON_OUT` for a look:
    /// `cargo test launchpad_icon -- --ignored`.
    #[test]
    #[ignore]
    fn writes_png() {
        let Ok(out) = std::env::var("DOCKSIDE_ICON_OUT") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let gfx = crate::gfx::Gfx::new().unwrap();
        let px = super::render(&gfx).unwrap();
        crate::gfx::write_png(&gfx, px.width, px.height, &px.data, &out);
    }
}
