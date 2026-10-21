//! The Finder's Dock icon, drawn in code: a macOS-style two-tone face on a
//! rounded tile (a blue left half, a pale profile on the right whose nose
//! juts into it, two eyes and a smile).

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::ID2D1GeometrySink;

use super::icons::{IconPixels, ICON_PX};
use super::{pt, Canvas, Gfx};
use crate::geom::{Color, Rect};

/// Icon-store key for the drawn icon.
pub const KEY: &str = "dockside:finder-icon";

/// Quarter-circle bezier handle length, as a fraction of the radius.
const K: f32 = 0.5523;

const NAVY: Color = Color::rgba(27, 42, 73, 1.0);

pub fn render(gfx: &Gfx) -> Option<IconPixels> {
    let size = ICON_PX;
    let mut canvas = Canvas::new(gfx, size, size, 1.0).ok()?;
    let s = size as f32;
    // The same tile inset and rounding the Dock uses for its squircle tiles.
    let tile = Rect::new(0.0, 0.0, s, s).inset(s * 0.06, s * 0.06);
    let (x0, y0, t) = (tile.x, tile.y, tile.w);
    let r = t * 0.225;
    // Tile-local fractions to canvas points.
    let p = |fx: f32, fy: f32| pt(x0 + fx * t, y0 + fy * t);
    let bez = |sink: &ID2D1GeometrySink, c1: (f32, f32), c2: (f32, f32), end: (f32, f32)| unsafe {
        sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: p(c1.0, c1.1), point2: p(c2.0, c2.1), point3: p(end.0, end.1) });
    };
    let rf = r / t;

    canvas
        .draw(gfx, |painter| {
            painter.fill_round(tile.offset(0.0, s * 0.01), r, Color::gray(0, 0.14));
            // Left half: the whole tile in blue; the right half is drawn over it.
            painter.fill_round_gradient(tile, r, Color::rgba(98, 205, 250, 1.0), Color::rgba(25, 116, 236, 1.0));

            // The pale right half, bounded on the left by the face's profile.
            let profile = gfx.path(|sink| unsafe {
                sink.BeginFigure(p(0.555, 0.0), D2D1_FIGURE_BEGIN_FILLED);
                sink.AddLine(p(0.535, 0.40));
                bez(sink, (0.53, 0.47), (0.43, 0.54), (0.425, 0.60));
                bez(sink, (0.43, 0.635), (0.49, 0.645), (0.53, 0.645));
                sink.AddLine(p(0.555, 1.0));
                sink.AddLine(p(1.0 - rf, 1.0));
                bez(sink, (1.0 - rf + K * rf, 1.0), (1.0, 1.0 - rf + K * rf), (1.0, 1.0 - rf));
                sink.AddLine(p(1.0, rf));
                bez(sink, (1.0, rf - K * rf), (1.0 - rf + K * rf, 0.0), (1.0 - rf, 0.0));
                sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            });
            if let Some(g) = profile {
                painter.fill_geometry(&g, Color::rgba(236, 245, 253, 1.0));
            }

            // Eyes: one on each half.
            let (ew, eh) = (t * 0.066, t * 0.155);
            for cx in [0.315, 0.71] {
                let eye = Rect::new(x0 + cx * t - ew / 2.0, y0 + 0.255 * t, ew, eh);
                painter.fill_round(eye, ew / 2.0, NAVY);
            }

            // The smile, across both halves.
            let smile = gfx.path(|sink| unsafe {
                sink.BeginFigure(p(0.22, 0.695), D2D1_FIGURE_BEGIN_HOLLOW);
                bez(sink, (0.38, 0.835), (0.63, 0.835), (0.79, 0.695));
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
            });
            if let Some(g) = smile {
                painter.stroke_geometry(&g, NAVY, t * 0.034);
                // Round off the stroke's square ends.
                for (fx, fy) in [(0.22, 0.695), (0.79, 0.695)] {
                    let c = p(fx, fy);
                    painter.fill_circle(c.x, c.y, t * 0.017, NAVY);
                }
            }
        })
        .ok()?;
    Some(IconPixels { width: size as u32, height: size as u32, data: canvas.pixels() })
}

#[cfg(test)]
mod tests {
    /// Writes the icon to `DOCKSIDE_ICON_OUT` for a look:
    /// `cargo test finder_icon -- --ignored`.
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
