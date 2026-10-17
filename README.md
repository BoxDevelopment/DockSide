# MacFinder

A fast, open-source macOS-style desktop shell for Windows 11, written in Rust.

It runs **on top of Explorer** (nothing is replaced): the Windows taskbar is
hidden while MacFinder runs and is always restored when it exits.

## Features (milestone 1)

**Menu bar**
- Logo menu: About, System Settings, App Store, Force Quit, Sleep / Restart / Shut Down, Lock, Log Out
- Bold name of the focused app, plus File / Edit / View / Window / Help menus that drive the
  focused app (Copy, Paste, New Window, Minimize, Zoom, tiling, a window list, and so on)
- Status items: battery, Wi-Fi, Search, Control Center and a clock, mapped to the Windows
  equivalents (Win+A, Win+S, Win+N)
- Slide across titles with a menu open, or use the arrow keys, Enter and Esc

**Dock**
- Pinned apps plus running apps, the Trash, a separator and running indicators
- macOS-style magnification (cosine falloff, cursor-anchored), a launch bounce and tooltips
- Click to launch or activate; clicking an app that's already frontmost cycles its windows
- Context menu: the app's windows, Keep in Dock, Show in Explorer, Open / Quit, Empty Trash
- Store apps (`shell:AppsFolder\...`), `.lnk` shortcuts and custom PNG/ICO icons

**Platform**
- Acrylic / blur backdrops, per-monitor DPI and dark/light mode that follows Windows
- Menu bar and Dock reserve screen space (Win32 AppBars), so maximized windows fit between them
- Hides itself for fullscreen apps
- Rendering uses Direct2D/DirectWrite into layered windows; animations are vsync-paced and the
  app idles at ~0% CPU

## Build & run

Requirements: Rust (stable, MSVC toolchain) and the Visual Studio C++ Build Tools.

```bash
cargo run --release
```

Quit from the logo menu (**Quit MacFinder**). If it's ever killed and the taskbar stays hidden:

```bash
cargo run --release -- --restore-taskbar
```

## Configuration

`%APPDATA%\MacFinder\config.toml` is created on first run, with comments. Changes apply as soon
as you save the file. Open it from the logo menu: **MacFinder Settings…**.

For the most faithful look, install **SF Pro Text** (or Inter) and set `font`. Give dock apps
macOS-style icons with `icon = "C:\\path\\to\\icon.png"`.

Logs: `%APPDATA%\MacFinder\macfinder.log`.

## Architecture

```
crates/macfinder/src
├── app.rs        event loop, message routing, actions (queued, run outside borrows)
├── menubar.rs    menu bar layout, vector status glyphs
├── dock.rs       dock items, magnification layout (unit-tested), bounce, context menus
├── menu.rs       popup menus + the standard menu contents
├── surface.rs    backdrop (DWM acrylic) window + layered content window pair
├── gfx/          Direct2D canvas/painter, DirectWrite text, icon loading (shell + WIC)
├── anim.rs       DwmFlush-paced frame clock
├── config.rs     TOML config: defaults merged under the user file, hot reload, toml_edit writes
├── theme.rs      light/dark palette
└── win/          Win32: AppBar, taskbar, window enumeration, system actions, accent policy
```

## Roadmap

- [ ] Finder: a file manager window (sidebar, icon/list/column views, Quick Look)
- [ ] Widgets: a desktop widget layer (clock, weather, calendar, battery, notes)
- [ ] True rounded acrylic via `Windows.UI.Composition` host backdrop (DWM's native corners are 8px)
- [ ] Dock: drag to reorder, auto-hide, minimized windows, a downloads stack, multi-monitor
- [ ] Real per-app menus via UI Automation, where apps expose them
- [ ] Spotlight-style launcher, Mission Control / App Exposé, Launchpad
- [ ] A System Settings-style GUI for the config

## Known issues

- `backdrop = "blur"` renders an opaque black shelf; use `"acrylic"` (the default) or `"none"`.
- Corner radius of blurred surfaces is limited to Windows 11's native 8px rounding.
