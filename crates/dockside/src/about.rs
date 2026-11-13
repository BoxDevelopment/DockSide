//! About This PC: macOS's "About This Mac" window, filled in from this PC's
//! firmware (SMBIOS), registry and DXGI. Screenshot Mode swaps the serial
//! number and the startup disk's name for demo ones.

use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::Motion;
use crate::chrome::{self, WindowAnim, SHADOW};
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconPixels;
use crate::gfx::{Align, Canvas, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::settings::{Anim, Style};
use crate::theme::Theme;
use crate::win::{self, Monitor};

/// Dock path for this window, which shows in the Dock like an app.
pub const ABOUT_PATH: &str = "dockside:about";
pub const TITLE: &str = "About This PC";

const W: f32 = 324.0;
const IMAGE_Y: f32 = 40.0;
const IMAGE_H: f32 = 118.0;
const ROWS_Y: f32 = 232.0;
const LABEL_W: f32 = 120.0;
const ROW_GAP: f32 = 5.0;
const FOOTER_H: f32 = 52.0;
const ROW_SIZE: f32 = 11.5;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Form {
    Laptop,
    Desktop,
}

/// What the window shows, read once when it opens.
struct Info {
    model: String,
    subtitle: String,
    form: Form,
    /// "Chip" (Arm) or "Processor".
    cpu_label: &'static str,
    cpu: String,
    graphics: Vec<String>,
    memory: String,
    disk_label: String,
    disk_letter: String,
    serial: Option<String>,
    /// "11 Home 25H2" and "Build 26200.7171".
    windows: String,
    build: String,
}

/// Clickable parts of the window.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Target {
    Close,
    MoreInfo,
    /// The Windows version, which shows the build when clicked (like macOS).
    Version,
}

pub enum AboutEvent {
    None,
    Close,
    Action(Action),
}

pub struct About {
    pub hwnd: HWND,
    canvas: Canvas,
    scale: f32,
    h: f32,
    info: Info,
    /// The desktop picture, small, for the computer's screen.
    wallpaper: Option<IconPixels>,
    show_build: bool,
    hits: Vec<(Rect, Target)>,
    hover: Option<Target>,
    pressed: Option<Target>,
    shown: bool,
    window: WindowAnim,
    motion: Motion<Anim>,
    pub tracking: bool,
    pub dirty: bool,
}

impl About {
    pub fn new(gfx: &Gfx, theme: &Theme, monitor: &Monitor) -> Result<Self> {
        let info = read_info();
        let h = window_height(gfx, theme, &info);
        let s = monitor.scale;
        let (w, hp) = (((W + 2.0 * SHADOW) * s).round() as i32, ((h + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        // A little above the middle, where macOS puts it.
        let top = mon.top + (mon.bottom - mon.top - hp) * 2 / 5;
        let rect = RECT { left, top, right: left + w, bottom: top + hp };
        // The minimize box (though its light is greyed out) is how the genie hook
        // tells our app windows from the Dock and popups: without it there's no
        // open / close effect, and the close the genie was handed never plays.
        let hwnd = win::create_window(WS_EX_LAYERED | WS_EX_APPWINDOW, WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX, rect, HWND::default())?;
        unsafe {
            let _ = SetWindowTextW(hwnd, w!("About This PC"));
        }
        Ok(Self {
            hwnd,
            canvas: Canvas::new(gfx, w, hp, s)?,
            scale: s,
            h,
            info,
            wallpaper: small_wallpaper(&Monitor::primary()),
            show_build: false,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            shown: false,
            window: WindowAnim::opening(),
            motion: Motion::default(),
            tracking: false,
            dirty: true,
        })
    }

    pub fn on_size(&mut self) {
        if self.window.restored(self.hwnd) {
            self.dirty = true;
        }
    }

    pub fn tick(&mut self, dt: f32) -> bool {
        let moving = self.window.tick(dt, self.hwnd) | self.motion.tick(dt);
        self.dirty |= moving;
        moving
    }

    pub fn animating(&self) -> bool {
        self.window.moving() || self.motion.moving()
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn show(&mut self) {
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.on_size();
    }

    fn body(&self) -> Rect {
        Rect::new(SHADOW, SHADOW, W, self.h)
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Target> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, t)| *t)
    }

    /// The whole window drags, apart from its controls.
    pub fn is_caption(&self, x: f32, y: f32) -> bool {
        self.body().contains(x, y) && self.hit_at(x, y).is_none()
    }

    pub fn on_key(&mut self, vk: u16, ctrl: bool) -> AboutEvent {
        const ESC: u16 = 0x1B;
        match vk {
            ESC => AboutEvent::Close,
            0x57 if ctrl => AboutEvent::Close,
            _ => AboutEvent::None,
        }
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        let h = self.hit_at(x, y);
        if h != self.hover {
            self.hover = h;
            self.dirty = true;
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if self.hover.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn on_down(&mut self, x: f32, y: f32) {
        self.pressed = self.hit_at(x, y);
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> AboutEvent {
        let pressed = self.pressed.take();
        self.dirty = true;
        let Some(target) = self.hit_at(x, y).filter(|t| Some(*t) == pressed) else { return AboutEvent::None };
        match target {
            Target::Close => AboutEvent::Close,
            Target::MoreInfo => AboutEvent::Action(Action::Open("ms-settings:about".into())),
            Target::Version => {
                self.show_build = !self.show_build;
                AboutEvent::None
            }
        }
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        if !self.window.begin_render(self.hwnd) {
            return;
        }
        self.motion.sweep();
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        let body = self.body();
        let (zx, zy, zoom, alpha) = self.window.transform(body);
        let mut hits = Vec::new();
        let frame = Frame {
            info: &self.info,
            wallpaper: self.wallpaper.as_ref(),
            show_build: self.show_build,
            hover: self.hover,
            pressed: self.pressed,
            motion: &self.motion,
        };
        let r = self.canvas.draw(gfx, |p| p.scaled_about(zx, zy, zoom, || hits = paint(p, gfx, theme, body, &frame)));
        self.hits = hits;
        let presented = r.and_then(|_| self.canvas.present_alpha(self.hwnd, rc.left, rc.top, alpha));
        if let Err(e) = presented {
            log!("about window render failed: {e}");
        }
        if !self.shown {
            self.shown = true;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOW);
                let _ = SetForegroundWindow(self.hwnd);
            }
        }
    }
}

impl Drop for About {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

struct Frame<'a> {
    info: &'a Info,
    wallpaper: Option<&'a IconPixels>,
    show_build: bool,
    hover: Option<Target>,
    pressed: Option<Target>,
    motion: &'a Motion<Anim>,
}

/// (label, value, clickable as) rows, with Screenshot Mode applied.
fn rows(info: &Info, show_build: bool) -> Vec<(&'static str, String, Option<Target>)> {
    let private = crate::screenshot_mode::on();
    let mut v = vec![(info.cpu_label, info.cpu.clone(), None)];
    if !info.graphics.is_empty() {
        v.push(("Graphics", info.graphics.join("\n"), None));
    }
    v.push(("Memory", info.memory.clone(), None));
    let disk = if private { crate::screenshot_mode::DISK } else { info.disk_label.as_str() };
    v.push(("Startup disk", format!("{disk} ({})", info.disk_letter), None));
    if private {
        v.push(("Serial number", crate::screenshot_mode::SERIAL.into(), None));
    } else if let Some(s) = &info.serial {
        v.push(("Serial number", s.clone(), None));
    }
    let version = if show_build { info.build.clone() } else { info.windows.clone() };
    v.push(("Windows", version, Some(Target::Version)));
    v
}

fn value_w() -> f32 {
    W - LABEL_W - 8.0 - 18.0
}

/// Height of a value that may wrap (several GPUs, a long processor name).
fn value_h(gfx: &Gfx, theme: &Theme, text: &str) -> f32 {
    let font = gfx.format_block(&theme.font, ROW_SIZE, Weight::Regular, Align::Leading);
    let Some(layout) = gfx.layout(text, &font, value_w(), 1000.0) else { return 16.0 };
    let mut m = windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_METRICS::default();
    unsafe {
        let _ = layout.GetMetrics(&mut m);
    }
    m.height.ceil().max(15.0)
}

fn window_height(gfx: &Gfx, theme: &Theme, info: &Info) -> f32 {
    // Room for the longer of the two version texts.
    let rows: f32 = rows(info, false).iter().map(|(_, v, _)| value_h(gfx, theme, v) + ROW_GAP).sum();
    ROWS_Y + rows + 18.0 + 24.0 + 18.0 + FOOTER_H
}

fn paint(p: &Painter, gfx: &Gfx, theme: &Theme, body: Rect, f: &Frame) -> Vec<(Rect, Target)> {
    let style = Style::new(gfx, theme);
    let font = theme.font.as_str();
    let title_font = gfx.format(font, 20.0, Weight::Bold, Align::Center);
    let sub_font = gfx.format(font, 12.0, Weight::Regular, Align::Center);
    let label_font = gfx.format_block(font, ROW_SIZE, Weight::SemiBold, Align::Trailing);
    let value_font = gfx.format_block(font, ROW_SIZE, Weight::Regular, Align::Leading);
    let button_font = gfx.format(font, 12.5, Weight::Regular, Align::Center);
    let footer_font = gfx.format_block(font, 10.0, Weight::Regular, Align::Center);
    let mut hits = Vec::new();

    let content = if theme.dark { Color::rgba(36, 36, 38, 1.0) } else { Color::rgba(246, 246, 246, 1.0) };
    chrome::shadow(p, body, theme);
    p.clear_rect(body.inset(4.0, 4.0));
    p.fill_round(body, chrome::RADIUS, content);
    p.stroke_round(body.inset(0.25, 0.25), chrome::RADIUS, theme.menu_border, 0.5);

    // Only close works, like macOS: minimize and zoom are greyed out.
    let lights_hot = f.motion.fade(Anim::Lights, f.hover == Some(Target::Close));
    for i in 0..3 {
        let (cx, cy) = (body.x + 20.0 + i as f32 * 20.0, body.y + 20.0);
        let c = if i == 0 { Color::rgba(255, 95, 87, 1.0) } else { Color::gray(128, 0.35) };
        p.fill_circle(cx, cy, 6.0, c);
        p.stroke_circle(cx, cy, 6.0, Color::gray(0, 0.12), 0.5);
        if i == 0 {
            if lights_hot > 0.01 {
                let g = Color::gray(0, 0.55 * lights_hot);
                p.line(cx - 2.5, cy - 2.5, cx + 2.5, cy + 2.5, g, 1.1);
                p.line(cx - 2.5, cy + 2.5, cx + 2.5, cy - 2.5, g, 1.1);
            }
            hits.push((Rect::new(cx - 8.0, cy - 8.0, 16.0, 16.0), Target::Close));
        }
    }

    // The computer, with the desktop picture on its screen.
    let wall = f.wallpaper.and_then(|w| w.to_bitmap(p.rt));
    let image = Rect::new(body.x, body.y + IMAGE_Y, body.w, IMAGE_H);
    match f.info.form {
        Form::Laptop => draw_laptop(p, image, wall.as_ref(), theme),
        Form::Desktop => draw_desktop(p, image, wall.as_ref(), theme),
    }

    let info = f.info;
    p.text(&info.model, &title_font, Rect::new(body.x + 16.0, body.y + 170.0, body.w - 32.0, 28.0), theme.text);
    p.text(&info.subtitle, &sub_font, Rect::new(body.x + 16.0, body.y + 198.0, body.w - 32.0, 18.0), theme.text_secondary);

    // Label: value rows, labels right-aligned against the values.
    let mut y = body.y + ROWS_Y;
    let split = body.x + LABEL_W;
    for (label, value, target) in rows(info, f.show_build) {
        let h = value_h(gfx, theme, &value);
        p.text(label, &label_font, Rect::new(body.x + 10.0, y, split - body.x - 10.0, h), theme.text);
        let vr = Rect::new(split + 8.0, y, value_w(), h);
        p.text(&value, &value_font, vr, theme.text_secondary);
        if let Some(t) = target {
            let w = gfx.measure(&value, &value_font).min(vr.w);
            hits.push((Rect::new(vr.x - 2.0, vr.y - 1.0, w + 4.0, h + 2.0), t));
        }
        y += h + ROW_GAP;
    }

    // More Info…
    let label = "More Info\u{2026}";
    let bw = gfx.measure(label, &button_font) + 28.0;
    let button = Rect::new(body.cx() - bw / 2.0, y + 14.0, bw, 24.0);
    let hot = f.motion.fade(Anim::Button(0), f.hover == Some(Target::MoreInfo));
    let raised = if theme.dark { Color::gray(255, 0.16) } else { Color::WHITE };
    p.fill_round(button.offset(0.0, 0.5), 6.0, Color::gray(0, if theme.dark { 0.25 } else { 0.10 }));
    p.fill_round(button, 6.0, raised.mix(style.selected_seg, hot * 0.5));
    p.stroke_round(button.inset(0.25, 0.25), 6.0, style.card_border, 0.5);
    if f.pressed == Some(Target::MoreInfo) {
        p.fill_round(button, 6.0, Color::gray(0, 0.08));
    }
    p.text(label, &button_font, button, theme.text);
    hits.push((button, Target::MoreInfo));

    let year = chrono::Datelike::year(&chrono::Local::now());
    let footer = format!("\u{2122} and \u{00A9} 1985\u{2013}{year} Microsoft Corporation.\nAll Rights Reserved.");
    p.text(&footer, &footer_font, Rect::new(body.x + 16.0, body.bottom() - FOOTER_H + 10.0, body.w - 32.0, 32.0), theme.text_disabled);
    hits
}

/// The screen's picture: the wallpaper, or a soft gradient without one.
fn draw_screen(p: &Painter, r: Rect, wall: Option<&ID2D1Bitmap>, radius: f32, theme: &Theme) {
    match wall {
        Some(b) => p.bitmap_cover(b, r, radius, 1.0),
        None => p.fill_round_gradient(r, radius, theme.accent, Color::rgba(20, 20, 40, 1.0)),
    }
}

fn draw_laptop(p: &Painter, area: Rect, wall: Option<&ID2D1Bitmap>, theme: &Theme) {
    let cx = area.cx();
    let (lid_w, lid_h, base_h) = (150.0, 98.0, 7.0);
    let lid = Rect::new(cx - lid_w / 2.0, area.bottom() - base_h - lid_h, lid_w, lid_h);
    let edge = if theme.dark { Color::gray(150, 1.0) } else { Color::gray(178, 1.0) };
    p.fill_round(lid, 8.0, edge);
    let bezel = lid.inset(1.2, 1.2);
    p.fill_round(bezel, 7.0, Color::rgba(14, 14, 16, 1.0));
    let screen = Rect::new(bezel.x + 5.0, bezel.y + 6.0, bezel.w - 10.0, bezel.h - 10.0);
    draw_screen(p, screen, wall, 2.5, theme);
    p.fill_circle(cx, bezel.y + 3.0, 0.9, Color::gray(70, 1.0));
    // The base: a thin aluminium slab, wider than the lid, with a finger notch.
    let base = Rect::new(cx - 92.0, lid.bottom() - 0.5, 184.0, base_h);
    p.fill_round_gradient(base, 3.5, Color::gray(226, 1.0), Color::gray(150, 1.0));
    p.fill_round(Rect::new(cx - 15.0, base.y, 30.0, 2.6), 1.3, Color::gray(140, 1.0));
    p.line(base.x + 4.0, base.y + 0.5, base.right() - 4.0, base.y + 0.5, Color::gray(255, 0.5), 0.6);
}

fn draw_desktop(p: &Painter, area: Rect, wall: Option<&ID2D1Bitmap>, theme: &Theme) {
    let cx = area.cx();
    let (dw, dh, neck_h, foot_h) = (164.0, 100.0, 13.0, 4.0);
    let display = Rect::new(cx - dw / 2.0, area.bottom() - foot_h - neck_h - dh, dw, dh);
    // Stand first, so the display sits over the top of the neck.
    let neck = Rect::new(cx - 13.0, display.bottom() - 2.0, 26.0, neck_h + 2.0);
    p.fill_round_gradient(neck, 1.5, Color::gray(150, 1.0), Color::gray(205, 1.0));
    p.fill_round_gradient(Rect::new(cx - 30.0, neck.bottom() - 0.5, 60.0, foot_h), 2.0, Color::gray(220, 1.0), Color::gray(160, 1.0));
    let edge = if theme.dark { Color::gray(150, 1.0) } else { Color::gray(178, 1.0) };
    p.fill_round(display, 7.0, edge);
    let bezel = display.inset(1.2, 1.2);
    p.fill_round(bezel, 6.0, Color::rgba(14, 14, 16, 1.0));
    draw_screen(p, bezel.inset(4.0, 4.0), wall, 2.0, theme);
}

// ---------------------------------------------------------------------------
// System information
// ---------------------------------------------------------------------------

fn read_info() -> Info {
    let bios = smbios();
    let cpu_raw = reg_string(HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString").unwrap_or_default();
    let cpu = clean_cpu(&cpu_raw);
    let arm = cpu.contains("Snapdragon") || cfg!(target_arch = "aarch64") || native_arm64();

    let form = match bios.chassis {
        Some(8 | 9 | 10 | 14 | 30 | 31 | 32) => Form::Laptop,
        Some(3 | 4 | 5 | 6 | 7 | 13 | 15 | 16 | 17 | 23 | 24 | 35 | 36) => Form::Desktop,
        _ if crate::win::system::battery().is_some() => Form::Laptop,
        _ => Form::Desktop,
    };
    let kind = match bios.chassis {
        Some(13) => "All-in-One",
        Some(30) => "Tablet",
        Some(31) => "Convertible",
        Some(32) => "Detachable",
        _ if form == Form::Laptop => "Laptop",
        _ => "Desktop",
    };
    let maker = bios.manufacturer.as_deref().map(short_maker);
    let model = model_name(&bios, maker.as_deref()).unwrap_or_else(|| format!("{kind} PC"));
    let subtitle = match (&maker, model_name(&bios, maker.as_deref())) {
        (Some(m), Some(_)) => format!("{m} \u{00B7} {kind}"),
        // A built PC: the motherboard says the most about it.
        _ => match (&bios.board_maker, &bios.board) {
            (Some(m), Some(b)) => format!("{} {b}", short_maker(m)),
            (None, Some(b)) => b.clone(),
            _ => kind.to_string(),
        },
    };

    let graphics = crate::widgets::sensors::gpus().into_iter().map(|g| g.name).collect();

    let mut kb = 0u64;
    let installed = unsafe { windows::Win32::System::SystemInformation::GetPhysicallyInstalledSystemMemory(&mut kb) }.is_ok();
    let gb = if installed && kb > 0 {
        (kb as f64 / (1024.0 * 1024.0)).round() as u64
    } else {
        let mut st = windows::Win32::System::SystemInformation::MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<windows::Win32::System::SystemInformation::MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        let _ = unsafe { windows::Win32::System::SystemInformation::GlobalMemoryStatusEx(&mut st) };
        (st.ullTotalPhys as f64 / (1u64 << 30) as f64).ceil() as u64
    };
    let mut memory = format!("{gb} GB");
    if let Some(speed) = bios.mem_speed {
        memory.push_str(&format!(" {speed} MT/s"));
    }
    if let Some(t) = bios.mem_type {
        memory.push_str(&format!(" {t}"));
    }

    let (disk_label, disk_letter) = startup_disk();

    const CV: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build_no: u32 = reg_string(HKEY_LOCAL_MACHINE, CV, "CurrentBuildNumber").and_then(|b| b.parse().ok()).unwrap_or(0);
    let ubr = reg_dword(HKEY_LOCAL_MACHINE, CV, "UBR");
    let mut product = reg_string(HKEY_LOCAL_MACHINE, CV, "ProductName").unwrap_or_else(|| "Windows".into());
    // Windows 11 still calls itself Windows 10 here.
    if build_no >= 22000 {
        product = product.replace("Windows 10", "Windows 11");
    }
    let mut windows = product.strip_prefix("Windows ").unwrap_or(&product).to_string();
    if let Some(v) = reg_string(HKEY_LOCAL_MACHINE, CV, "DisplayVersion") {
        windows.push(' ');
        windows.push_str(&v);
    }
    let build = match ubr {
        Some(u) => format!("Build {build_no}.{u}"),
        None => format!("Build {build_no}"),
    };

    Info {
        model,
        subtitle,
        form,
        cpu_label: if arm { "Chip" } else { "Processor" },
        cpu: if cpu.is_empty() { "Unknown".into() } else { cpu },
        graphics,
        memory,
        disk_label,
        disk_letter,
        serial: bios.serial,
        windows,
        build,
    }
}

fn native_arm64() -> bool {
    use windows::Win32::System::SystemInformation::{GetNativeSystemInfo, PROCESSOR_ARCHITECTURE_ARM64, SYSTEM_INFO};
    let mut si = SYSTEM_INFO::default();
    unsafe {
        GetNativeSystemInfo(&mut si);
        si.Anonymous.Anonymous.wProcessorArchitecture == PROCESSOR_ARCHITECTURE_ARM64
    }
}

/// "11th Gen Intel(R) Core(TM) i7-1165G7 @ 2.80GHz" -> "11th Gen Intel Core i7-1165G7".
fn clean_cpu(raw: &str) -> String {
    let mut s = raw.replace("(R)", "").replace("(r)", "").replace("(TM)", "").replace("(tm)", "");
    if let Some(i) = s.find(" @ ") {
        s.truncate(i);
    }
    for tail in [" with Radeon Graphics", " with Radeon Vega Mobile Gfx", " CPU"] {
        if let Some(i) = s.find(tail) {
            s.truncate(i);
        }
    }
    // AMD's "... 8-Core Processor".
    let words: Vec<&str> = s.split_whitespace().collect();
    let words: Vec<&str> = match words.as_slice() {
        [rest @ .., c, "Processor"] if c.ends_with("-Core") => rest.to_vec(),
        [rest @ .., "Processor"] => rest.to_vec(),
        w => w.to_vec(),
    };
    words.join(" ")
}

/// "Dell Inc." -> "Dell", "ASUSTeK COMPUTER INC." -> "ASUS".
fn short_maker(m: &str) -> String {
    let l = m.to_lowercase();
    let known = [
        ("asustek", "ASUS"),
        ("micro-star", "MSI"),
        ("hewlett", "HP"),
        ("lenovo", "Lenovo"),
        ("dell", "Dell"),
        ("microsoft", "Microsoft"),
        ("gigabyte", "Gigabyte"),
        ("samsung", "Samsung"),
        ("acer", "Acer"),
        ("razer", "Razer"),
        ("framework", "Framework"),
        ("huawei", "Huawei"),
        ("toshiba", "Toshiba"),
        ("dynabook", "Dynabook"),
        ("fujitsu", "Fujitsu"),
        ("lg electronics", "LG"),
        ("asrock", "ASRock"),
        ("intel", "Intel"),
        ("apple", "Apple"),
        ("google", "Google"),
        ("xiaomi", "Xiaomi"),
    ];
    if let Some((_, short)) = known.iter().find(|(k, _)| l.starts_with(k)) {
        return short.to_string();
    }
    let mut s = m.trim().to_string();
    for suffix in [", Inc.", " Inc.", " INC.", " Corporation", " Corp.", " Co., Ltd.", " Co.,Ltd.", " Ltd.", " LTD", " GmbH", " Limited"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.trim_end_matches(',').to_string();
        }
    }
    s
}

/// The name people know the PC by: Lenovo keeps it in the version field
/// ("ThinkPad X1 Carbon Gen 9"), everyone else in the product name.
fn model_name(b: &Bios, maker: Option<&str>) -> Option<String> {
    let name = if maker == Some("Lenovo") { b.version.clone().or_else(|| b.product.clone()) } else { b.product.clone() }?;
    // "HP Spectre x360" already says who made it; keep it as is.
    Some(name.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The Windows drive's label and letter ("Local Disk", "C:").
fn startup_disk() -> (String, String) {
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
    let mut dir = [0u16; 260];
    let n = unsafe { GetWindowsDirectoryW(Some(&mut dir)) } as usize;
    let letter = String::from_utf16_lossy(&dir[..n.min(2)]);
    let letter = if letter.len() == 2 { letter } else { "C:".into() };
    let root = HSTRING::from(format!("{letter}\\"));
    let mut name = [0u16; 261];
    let ok = unsafe { GetVolumeInformationW(&root, Some(&mut name), None, None, None, None) }.is_ok();
    let label = if ok { win::from_wide(&name) } else { String::new() };
    let label = if label.trim().is_empty() { "Local Disk".to_string() } else { label };
    (label, letter)
}

/// The desktop picture shrunk to a thumbnail (it's shown about 140 points wide).
fn small_wallpaper(monitor: &Monitor) -> Option<IconPixels> {
    let rect = monitor.rect;
    let px = crate::win::wallpaper::render(rect).map_err(|e| log!("about: wallpaper: {e}")).ok()?;
    let (sw, sh) = ((rect.right - rect.left) as usize, (rect.bottom - rect.top) as usize);
    if sw == 0 || sh == 0 || px.len() < sw * sh * 4 {
        return None;
    }
    let dw = 320usize.min(sw);
    let dh = (sh * dw / sw).max(1);
    let mut out = vec![0u8; dw * dh * 4];
    // Box filter: average every source pixel that falls in each target one.
    for y in 0..dh {
        let (y0, y1) = (y * sh / dh, ((y + 1) * sh / dh).max(y * sh / dh + 1));
        for x in 0..dw {
            let (x0, x1) = (x * sw / dw, ((x + 1) * sw / dw).max(x * sw / dw + 1));
            let mut acc = [0u32; 4];
            for sy in y0..y1 {
                let row = sy * sw * 4;
                for sx in x0..x1 {
                    let i = row + sx * 4;
                    for c in 0..4 {
                        acc[c] += px[i + c] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * dw + x) * 4;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    Some(IconPixels { width: dw as u32, height: dh as u32, data: out })
}

// --- registry ------------------------------------------------------------------

fn reg_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (key, value) = (HSTRING::from(key), HSTRING::from(value));
    unsafe {
        let mut size = 0u32;
        RegGetValueW(root, &key, &value, RRF_RT_REG_SZ, None, None, Some(&mut size)).ok().ok()?;
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        RegGetValueW(root, &key, &value, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size)).ok().ok()?;
        let s = win::from_wide(&buf).trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

fn reg_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let (key, value) = (HSTRING::from(key), HSTRING::from(value));
    let mut v = 0u32;
    let mut size = 4u32;
    unsafe { RegGetValueW(root, &key, PCWSTR(value.as_ptr()), RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut size)).ok().ok()? };
    Some(v)
}

// --- SMBIOS --------------------------------------------------------------------

#[derive(Default, Debug)]
struct Bios {
    manufacturer: Option<String>,
    product: Option<String>,
    version: Option<String>,
    serial: Option<String>,
    board_maker: Option<String>,
    board: Option<String>,
    chassis: Option<u8>,
    mem_type: Option<&'static str>,
    mem_speed: Option<u32>,
}

/// Placeholder text firmware ships with when the maker didn't fill a field in.
fn real(s: Option<&String>) -> Option<String> {
    let s = s?.trim();
    let l = s.to_lowercase();
    const JUNK: [&str; 16] = [
        "to be filled by o.e.m.", "to be filled by oem", "system product name", "system manufacturer", "system version",
        "system serial number", "default string", "not applicable", "not specified", "none", "o.e.m.", "oem",
        "invalid", "chassis serial number", "base board serial number", "123456789",
    ];
    let same = s.chars().all(|c| Some(c) == s.chars().next());
    (!s.is_empty() && !JUNK.contains(&l.as_str()) && !same).then(|| s.to_string())
}

fn smbios() -> Bios {
    use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};
    let mut out = Bios::default();
    let n = unsafe { GetSystemFirmwareTable(RSMB, 0, None) } as usize;
    if n < 8 {
        return out;
    }
    let mut buf = vec![0u8; n];
    let got = unsafe { GetSystemFirmwareTable(RSMB, 0, Some(&mut buf)) } as usize;
    if got < 8 || got > buf.len() {
        return out;
    }
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let data = &buf[8..(8 + len).min(got)];
    let mut speeds = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let (ty, l) = (data[i], data[i + 1] as usize);
        if l < 4 || i + l > data.len() {
            break;
        }
        let s = &data[i..i + l];
        // Strings follow the formatted part, each null-terminated, the set ending in a double null.
        let mut end = i + l;
        while end + 1 < data.len() && !(data[end] == 0 && data[end + 1] == 0) {
            end += 1;
        }
        let strings: Vec<String> = data[i + l..end.min(data.len())]
            .split(|&b| b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).trim().to_string())
            .collect();
        let st = |off: usize| s.get(off).and_then(|&k| if k == 0 { None } else { strings.get(k as usize - 1) });
        match ty {
            1 => {
                out.manufacturer = real(st(4));
                out.product = real(st(5));
                out.version = real(st(6));
                out.serial = real(st(7));
            }
            2 if out.board.is_none() => {
                out.board_maker = real(st(4));
                out.board = real(st(5));
            }
            3 if out.chassis.is_none() => out.chassis = s.get(5).map(|c| c & 0x7F),
            17 if l > 0x16 => {
                let size = u16::from_le_bytes([s[0x0C], s[0x0D]]);
                if size != 0 && size != 0xFFFF {
                    if out.mem_type.is_none() {
                        out.mem_type = match s[0x12] {
                            0x12 => Some("DDR"),
                            0x13 => Some("DDR2"),
                            0x18 => Some("DDR3"),
                            0x1A => Some("DDR4"),
                            0x1B => Some("LPDDR"),
                            0x1C => Some("LPDDR2"),
                            0x1D => Some("LPDDR3"),
                            0x1E => Some("LPDDR4"),
                            0x22 => Some("DDR5"),
                            0x23 => Some("LPDDR5"),
                            _ => None,
                        };
                    }
                    let speed = u16::from_le_bytes([s[0x15], s[0x16]]) as u32;
                    let configured = if l > 0x21 { u16::from_le_bytes([s[0x20], s[0x21]]) as u32 } else { 0 };
                    let v = if configured > 0 && configured != 0xFFFF { configured } else { speed };
                    if v > 0 && v != 0xFFFF {
                        speeds.push(v);
                    }
                }
            }
            127 => break,
            _ => {}
        }
        i = end + 2;
    }
    // The speed the slowest stick runs at is the speed all of them run at.
    out.mem_speed = speeds.into_iter().min();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_names() {
        assert_eq!(clean_cpu("11th Gen Intel(R) Core(TM) i7-1165G7 @ 2.80GHz"), "11th Gen Intel Core i7-1165G7");
        assert_eq!(clean_cpu("AMD Ryzen 7 5800H with Radeon Graphics         "), "AMD Ryzen 7 5800H");
        assert_eq!(clean_cpu("AMD Ryzen 9 5900X 12-Core Processor"), "AMD Ryzen 9 5900X");
        assert_eq!(clean_cpu("Intel(R) Core(TM) i5-8250U CPU @ 1.60GHz"), "Intel Core i5-8250U");
    }

    #[test]
    fn makers() {
        assert_eq!(short_maker("Dell Inc."), "Dell");
        assert_eq!(short_maker("ASUSTeK COMPUTER INC."), "ASUS");
        assert_eq!(short_maker("Contoso Ltd."), "Contoso");
    }

    #[test]
    fn junk_fields() {
        assert_eq!(real(Some(&"To Be Filled By O.E.M.".to_string())), None);
        assert_eq!(real(Some(&"0000000".to_string())), None);
        assert_eq!(real(Some(&"XPS 13 9310".to_string())), Some("XPS 13 9310".into()));
    }
}
