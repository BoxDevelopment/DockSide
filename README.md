<p align="center"><img src="assets/logo-256.png" width="128" alt="DockSide logo"></p>

# DockSide

A fast, open-source macOS-style desktop shell for Windows 11, written in Rust.
Pretty much 99% vibe-code. Couldn't find a good alternative so I made my own.
Feel free to contribute.

Discord: boxeth

It runs **on top of Explorer** (nothing is replaced): the Windows taskbar is
hidden while DockSide runs and is always restored when it exits.

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
- Opens from the Dock's Finder icon (`path = "dockside:finder"`) or with `dockside --finder`
- Sidebar with Favorites (Home, Desktop, Documents, Downloads, Pictures, Music, Movies) and drives,
  with macOS-style vibrancy (the desktop blurs through it; `[finder] sidebar_vibrancy`)
- Icon, list (sortable Name / Date Modified / Size / Kind) and column views with a preview pane;
  the view, sort order and hidden-files choice are remembered for new windows
- Tabs: Ctrl+T, the tab bar's +, "Open in New Tab" or middle-click a folder; Ctrl+Tab / Ctrl+Shift+Tab
  (or Ctrl+Shift+[ / ]) switch, Ctrl+W or middle-click closes; drop files on a tab to move them there
- Search the current folder (Ctrl+F), Quick Look (Space) with text, image and video previews
- Drag and drop: drag items out to other apps, drop files onto folders, the sidebar or the view.
  Like macOS, items move on the same drive and copy across drives (Ctrl forces a copy, Shift a move)
- Background icon and thumbnail loading; folders refresh when their contents change on disk
- Back/forward, resizable window, zoom, inline rename, New Folder, Move to Trash (Recycle Bin)
- Context menus: Open, Rename, Copy Path, Show in File Explorer, New Terminal at Folder, view and sort options
- Shortcuts (Ctrl stands in for ⌘): Ctrl+1/2/3 views, Ctrl+N new window, Ctrl+Shift+N new folder,
  Ctrl+↑ parent, Alt+←/→ back/forward, Enter open, F2 rename, Delete trash, Ctrl+Shift+. hidden files,
  type to select

**Dynamic Island**
- An always-present black pill at the top center of the menu bar that morphs between activities on
  spring physics (bouncy growth, a press squish, hover lift, cross-fades). With two activities running,
  the second splits off into a bubble beside it, like the iPhone
- Click the idle pill for the time, your next meeting and one-click timers; right-click for timers,
  the shelf and settings
- **Now Playing** from anything that reports to Windows' media controls (Spotify, browsers, Media Player):
  artwork, a waveform tinted from it, and an expanded player with scrubbing and transport controls
- **Volume and brightness HUD** in place of the Windows popups (volume keys, or scroll over the island);
  drag the bar to set the volume
- **Timer and stopwatch**: from the home panel, the right-click menu, or Spotlight ("timer 10m", "stopwatch")
- **File shelf**: drop files on the island to hold them, then drag them out where you need them
- **Upcoming meetings** from the Windows calendar, a few minutes before they start
- **Mic and camera indicators**: an orange / green dot while an app is using them, with the app's name
- **Alerts**: notifications (as banners from the island), Bluetooth devices connecting with their battery,
  screenshots (click to open, drag to use), finished downloads (with a live download activity while they run),
  charging / low battery, Caps Lock and Do Not Disturb
- Alerts queue up and wait while you hover them; every source can be switched off in `[island]`

**Spotlight**
- Alt+Space (configurable in Settings → General, or `[spotlight] hotkey`) or the menu bar's magnifier
- Finds every app (desktop and Store) plus files and folders in Desktop, Documents, Downloads,
  Pictures, Music and Movies (indexed in the background)
- Ranks exact and prefix matches first, then word starts and initials ("vsc" → Visual Studio Code)
- Inline calculator ("12*4.5+3" → 57; Enter copies), plus a web-search fallback
- Enter opens; folders open in Finder; Ctrl+Enter reveals the item in Finder; Esc clears, then closes

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

Quit from the logo menu (**Quit DockSide**). If it's ever killed and the taskbar stays hidden:

```bash
cargo run --release -- --restore-taskbar
```

## Settings

Open **System Settings** from the logo menu (**DockSide Settings…**), from **Dock Settings…** when
you right-click the Dock, or by launching DockSide again while it's running.

- **General:** Start DockSide at login, hide the Windows taskbar, Mac- or Windows-style shortcut hints
- **Appearance:** Auto / Light / Dark, accent color, font
- **Menu Bar:** background, logo, clock format, reserved space, status items
- **Dock:** size, magnification and magnified size, auto-hide, icon style, background,
  running apps, Trash, pinned apps (add with a file picker, remove with −)

"Start at login" registers the exe you're running (`HKCU\...\Run`). Use a release build
(`cargo build --release`) for that, not `target\debug`.

## Configuration

Everything Settings changes is stored in `%APPDATA%\DockSide\config.toml`, which is created on
first run with comments. Editing it by hand works too: changes apply as soon as you save, and only
the affected part of the shell redraws.

For the most faithful look, install **SF Pro Text** (or Inter) and set `font`. Give dock apps
macOS-style icons with `icon = "C:\\path\\to\\icon.png"`.

Logs: `%APPDATA%\DockSide\dockside.log`.

## Architecture

```
crates/dockside/src
├── app.rs        event loop, message routing, actions (queued, run outside borrows)
├── menubar.rs    menu bar layout, vector status glyphs
├── dock.rs       dock items, magnification layout (unit-tested), bounce, context menus
├── menu.rs       popup menus + the standard menu contents
├── finder/       Finder window (views, input), filesystem helpers, background icon loader
├── settings.rs   System Settings window
├── island/       Dynamic Island: spring-driven pill, views, glyphs; media, calendar, notifications,
│                 bluetooth and system sources feed it from background threads
├── chrome.rs     shared window chrome: shadow, body, traffic lights
├── surface.rs    backdrop (DWM acrylic) window + layered content window pair
├── gfx/          Direct2D canvas/painter, DirectWrite text, icon loading (shell + WIC)
├── anim.rs       DwmFlush-paced frame clock, damped springs
├── config.rs     TOML config: defaults merged under the user file, hot reload, toml_edit writes
├── theme.rs      light/dark palette
└── win/          Win32: AppBar, taskbar, window enumeration, system actions, accent policy
```

## Roadmap

- [x] Finder: sidebar, icon / list / column views, rename, trash, context menus
- [x] Finder: drag and drop, Quick Look (Space), tabs, search, sidebar vibrancy
- [ ] Widgets: a desktop widget layer (clock, weather, calendar, battery, notes)
- [x] True rounded acrylic via `Windows.UI.Composition` host backdrop
- [x] Dock: drag to reorder, drag out to remove, auto-hide
- [ ] Dock: minimized windows, a downloads stack, multi-monitor
- [ ] Real per-app menus via UI Automation, where apps expose them
- [x] Spotlight
- [ ] Mission Control / App Exposé, Launchpad
- [x] Dynamic Island: Now Playing, HUDs, timers, shelf, calendar, notifications, device and status alerts
- [ ] Dynamic Island: reply to notifications, Live Activities from other apps
- [x] A System Settings-style GUI for the config, plus Start at login
- [ ] Top Tray thing: Add a tray item that windows has (showing all tray'ed apps)
- [ ] Latex list in spotlight for notion
- [ ] WIndows commands in Spotlight e.g brightness, low power mode, or just opening every page in the settings app.
## Notes

- Blurred backdrops need Windows 11 with **Transparency effects** turned on (Settings → Personalization → Colors). If Composition isn't available, DockSide falls back to a DWM accent with 8px corners.
