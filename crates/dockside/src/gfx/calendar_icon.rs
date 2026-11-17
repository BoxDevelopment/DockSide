//! The Calendar app's Dock icon, drawn in code like macOS's: a white tile with
//! today's weekday in red over the day of the month.

use chrono::{Datelike, NaiveDate};

use super::icons::{IconPixels, ICON_PX};
use super::{Align, Canvas, Gfx, Weight};
use crate::geom::{Color, Rect};

/// Icon-store keys start with this; the date follows, so each day draws anew.
pub const PREFIX: &str = "dockside:calendar-icon:";

/// The key for `day`'s icon.
pub fn key(day: NaiveDate) -> String {
    format!("{PREFIX}{}", day.format("%Y%m%d"))
}

/// The day a key (from [`key`]) is for.
pub fn day_of(key: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(key.strip_prefix(PREFIX)?, "%Y%m%d").ok()
}

pub fn render(gfx: &Gfx, day: NaiveDate) -> Option<IconPixels> {
    let size = ICON_PX;
    let mut canvas = Canvas::new(gfx, size, size, 1.0).ok()?;
    let s = size as f32;
    // The same tile inset and rounding the Dock uses for its squircle tiles.
    let tile = Rect::new(0.0, 0.0, s, s).inset(s * 0.06, s * 0.06);
    let t = tile.w;
    let r = t * 0.225;
    let weekday = day.format("%a").to_string().to_uppercase();
    let number = day.day().to_string();
    let small = gfx.format("Segoe UI", t * 0.15, Weight::SemiBold, Align::Center);
    let big = gfx.format("Segoe UI Light", t * 0.56, Weight::Regular, Align::Center);

    canvas
        .draw(gfx, |p| {
            p.fill_round(tile.offset(0.0, s * 0.01), r, Color::gray(0, 0.14));
            p.fill_round_gradient(tile, r, Color::rgba(255, 255, 255, 1.0), Color::rgba(240, 240, 242, 1.0));
            p.text(&weekday, &small, Rect::new(tile.x, tile.y + t * 0.07, t, t * 0.2), Color::rgba(255, 59, 48, 1.0));
            p.text(&number, &big, Rect::new(tile.x, tile.y + t * 0.18, t, t * 0.72), Color::rgba(28, 28, 30, 1.0));
        })
        .ok()?;
    Some(IconPixels { width: size as u32, height: size as u32, data: canvas.pixels() })
}

#[cfg(test)]
mod tests {
    #[test]
    fn keys_round_trip() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 11, 17).unwrap();
        assert_eq!(super::day_of(&super::key(d)), Some(d));
    }

    /// Writes today's icon to `DOCKSIDE_ICON_OUT` for a look:
    /// `cargo test calendar_icon -- --ignored`.
    #[test]
    #[ignore]
    fn writes_png() {
        let Ok(out) = std::env::var("DOCKSIDE_ICON_OUT") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let gfx = crate::gfx::Gfx::new().unwrap();
        let px = super::render(&gfx, chrono::Local::now().date_naive()).unwrap();
        crate::gfx::write_png(&gfx, px.width, px.height, &px.data, &out);
    }
}
