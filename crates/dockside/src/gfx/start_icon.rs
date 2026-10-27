//! The Start item's Dock icon, drawn in code: the Windows logo's four blue
//! panes on a pale macOS-style rounded tile.

use super::icons::{IconPixels, ICON_PX};
use super::{Canvas, Gfx};
use crate::geom::{Color, Rect};

/// Icon-store key for the drawn icon.
pub const KEY: &str = "dockside:start-icon";

pub fn render(gfx: &Gfx) -> Option<IconPixels> {
    let size = ICON_PX;
    let mut canvas = Canvas::new(gfx, size, size, 1.0).ok()?;
    let s = size as f32;
    // The same tile inset and rounding the Dock uses for its squircle tiles.
    let tile = Rect::new(0.0, 0.0, s, s).inset(s * 0.06, s * 0.06);
    let (x0, y0, t) = (tile.x, tile.y, tile.w);
    let r = t * 0.225;

    canvas
        .draw(gfx, |painter| {
            painter.fill_round(tile.offset(0.0, s * 0.01), r, Color::gray(0, 0.14));
            painter.fill_round_gradient(tile, r, Color::rgba(252, 253, 255, 1.0), Color::rgba(219, 231, 245, 1.0));

            // The logo: 2×2 panes, lighter at the top like Windows 11's.
            let (pane, gap) = (t * 0.25, t * 0.025);
            let left = x0 + (t - 2.0 * pane - gap) / 2.0;
            let top = y0 + (t - 2.0 * pane - gap) / 2.0;
            for (col, row) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                let rect = Rect::new(left + col * (pane + gap), top + row * (pane + gap), pane, pane);
                let (from, to) = if row == 0.0 {
                    (Color::rgba(56, 182, 255, 1.0), Color::rgba(18, 140, 240, 1.0))
                } else {
                    (Color::rgba(14, 132, 236, 1.0), Color::rgba(0, 96, 210, 1.0))
                };
                painter.fill_round_gradient(rect, t * 0.012, from, to);
            }
        })
        .ok()?;
    Some(IconPixels { width: size as u32, height: size as u32, data: canvas.pixels() })
}

#[cfg(test)]
mod tests {
    /// Writes the icon to `DOCKSIDE_ICON_OUT` for a look:
    /// `cargo test start_icon -- --ignored`.
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
