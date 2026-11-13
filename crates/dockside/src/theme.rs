//! Resolved light/dark palette, modelled on macOS system colors.

use crate::config::{Appearance, Config};
use crate::geom::Color;
use crate::gfx::Gfx;
use crate::win::system;

const FALLBACK_FONT: &str = "Segoe UI Variable Text";

#[derive(Clone, Debug)]
pub struct Theme {
    pub dark: bool,
    pub font: String,
    pub accent: Color,
    pub text: Color,
    pub text_secondary: Color,
    pub text_disabled: Color,
    pub menubar_tint: Color,
    pub menubar_highlight: Color,
    pub menu_tint: Color,
    pub menu_border: Color,
    pub menu_separator: Color,
    pub dock_tint: Color,
    pub dock_border: Color,
    pub dock_separator: Color,
    pub indicator: Color,
    pub label_bg: Color,
    pub label_text: Color,
    pub shadow: Color,
}

/// Whether the config's appearance resolves to dark right now.
pub fn wants_dark(cfg: &Config) -> bool {
    match cfg.general.appearance {
        Appearance::Auto => system::is_dark_mode(),
        Appearance::Light => false,
        Appearance::Dark => true,
    }
}

impl Theme {
    pub fn new(cfg: &Config, gfx: &Gfx) -> Theme {
        let dark = wants_dark(cfg);
        let font = if gfx.font_exists(&cfg.general.font) {
            cfg.general.font.clone()
        } else {
            log!("font '{}' not installed, using {FALLBACK_FONT}", cfg.general.font);
            FALLBACK_FONT.to_string()
        };
        let accent = Color::parse(&cfg.general.accent).unwrap_or(Color::rgba(0, 122, 255, 1.0));
        let pick = |light: &str, dark_: &str, fallback: Color| {
            Color::parse(if dark { dark_ } else { light }).unwrap_or(fallback)
        };
        let menubar_tint = pick(&cfg.menubar.tint_light, &cfg.menubar.tint_dark, Color::gray(240, 0.7));
        let dock_tint = pick(&cfg.dock.tint_light, &cfg.dock.tint_dark, Color::gray(246, 0.5));

        if dark {
            Theme {
                dark,
                font,
                accent,
                text: Color::gray(255, 0.92),
                text_secondary: Color::gray(255, 0.55),
                text_disabled: Color::gray(255, 0.25),
                menubar_tint,
                menubar_highlight: Color::gray(255, 0.18),
                menu_tint: Color::rgba(40, 40, 42, 0.78),
                menu_border: Color::gray(255, 0.16),
                menu_separator: Color::gray(255, 0.12),
                dock_tint,
                dock_border: Color::gray(255, 0.18),
                dock_separator: Color::gray(255, 0.22),
                indicator: Color::gray(255, 0.75),
                label_bg: Color::rgba(40, 40, 42, 0.92),
                label_text: Color::gray(255, 0.92),
                shadow: Color::gray(0, 1.0),
            }
        } else {
            Theme {
                dark,
                font,
                accent,
                // A touch blue-grey, like macOS's label colours, and firmer
                // than black-at-half so secondary text holds up on glass.
                text: Color::rgba(18, 18, 22, 0.92),
                text_secondary: Color::rgba(28, 28, 34, 0.62),
                text_disabled: Color::rgba(28, 28, 34, 0.32),
                menubar_tint,
                menubar_highlight: Color::gray(0, 0.1),
                menu_tint: Color::rgba(248, 248, 250, 0.86),
                menu_border: Color::gray(0, 0.1),
                menu_separator: Color::gray(0, 0.12),
                dock_tint,
                dock_border: Color::gray(255, 0.6),
                dock_separator: Color::gray(0, 0.2),
                indicator: Color::gray(0, 0.68),
                label_bg: Color::rgba(250, 250, 252, 0.96),
                label_text: Color::rgba(18, 18, 22, 0.9),
                shadow: Color::gray(0, 1.0),
            }
        }
    }
}
