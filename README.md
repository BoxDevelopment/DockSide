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
- Drag icons to reorder them, or drag one up out of the Dock to remove it (both are saved to config)
- Auto-hide (`autohide = true`): the Dock slides away and returns when the pointer touches the bottom edge
- Right-click the separator or an empty part of the Dock: Turn Hiding On/Off, Turn Magnification On/Off
- Config changes apply in place: editing pinned apps updates the Dock without redrawing anything else
- Store apps (`shell:AppsFolder\...`), `.lnk` shortcuts and custom PNG/ICO icons

**Finder**
- Opens from the Dock's Finder icon (`path = "macfinder:finder"`) or with `macfinder --finder`
- Sidebar with Favorites (Home, Desktop, Documents, Downloads, Pictures, Music, Movies) and drives
- Icon, list (sortable Name / Date Modified / Size / Kind) and column views with a preview pane
- Background icon and thumbnail loading; folders refresh when their contents change on disk
- Back/forward, resizable window, zoom, inline rename, New Folder, Move to Trash (Recycle Bin)
- Context menus: Open, Rename, Copy Path, Show in File Explorer, New Terminal at Folder, view and sort options
- Shortcuts (Ctrl stands in for ⌘): Ctrl+1/2/3 views, Ctrl+N new window, Ctrl+Shift+N new folder,
  Ctrl+↑ parent, Alt+←/→ back/forward, Enter open, F2 rename, Delete trash, Ctrl+Shift+. hidden files,
  type to select

**Platform**
- True rounded acrylic and blur via Windows.UI.Composition (a host backdrop brush with any corner radius), per-monitor DPI and dark/light mode that follows Windows
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

## Settings

Open **System Settings** from the logo menu (**MacFinder Settings…**), from **Dock Settings…** when
you right-click the Dock, or by launching MacFinder again while it's running.

- **General:** Start MacFinder at login, hide the Windows taskbar, Mac- or Windows-style shortcut hints
- **Appearance:** Auto / Light / Dark, accent color, font
- **Menu Bar:** background, logo, clock format, reserved space, status items
- **Dock:** size, magnification and magnified size, auto-hide, icon style, background,
  running apps, Trash, pinned apps (add with a file picker, remove with −)

"Start at login" registers the exe you're running (`HKCU\...\Run`). Use a release build
(`cargo build --release`) for that, not `target\debug`.

## Configuration

Everything Settings changes is stored in `%APPDATA%\MacFinder\config.toml`, which is created on
first run with comments. Editing it by hand works too: changes apply as soon as you save, and only
the affected part of the shell redraws.

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
├── finder/       Finder window (views, input), filesystem helpers, background icon loader
├── settings.rs   System Settings window
├── chrome.rs     shared window chrome: shadow, body, traffic lights
├── surface.rs    backdrop (DWM acrylic) window + layered content window pair
├── gfx/          Direct2D canvas/painter, DirectWrite text, icon loading (shell + WIC)
├── anim.rs       DwmFlush-paced frame clock
├── config.rs     TOML config: defaults merged under the user file, hot reload, toml_edit writes
├── theme.rs      light/dark palette
└── win/          Win32: AppBar, taskbar, window enumeration, system actions, accent policy
```

## Roadmap

- [x] Finder: sidebar, icon / list / column views, rename, trash, context menus
- [ ] Finder: drag and drop, Quick Look (Space), tabs, search, sidebar vibrancy
- [ ] Widgets: a desktop widget layer (clock, weather, calendar, battery, notes)
- [x] True rounded acrylic via `Windows.UI.Composition` host backdrop
- [x] Dock: drag to reorder, drag out to remove, auto-hide
- [ ] Dock: minimized windows, a downloads stack, multi-monitor
- [ ] Real per-app menus via UI Automation, where apps expose them
- [ ] Spotlight-style launcher, Mission Control / App Exposé, Launchpad
- [x] A System Settings-style GUI for the config, plus Start at login

## Notes

- Blurred backdrops need Windows 11 with **Transparency effects** turned on (Settings → Personalization → Colors). If Composition isn't available, MacFinder falls back to a DWM accent with 8px corners.
