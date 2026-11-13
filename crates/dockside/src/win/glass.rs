//! Liquid Glass refraction.
//!
//! DWM's host backdrop is a fixed, heavy blur, so it can't bend light. For
//! real glass we rebuild what's behind each pane from captures of the other
//! apps' windows under it (see [`scene`]), into a mipmapped texture, and run a
//! pixel shader into a composition swap chain: a convex bezel that bends and
//! magnifies what's behind near the edges with a little chromatic dispersion,
//! a light frost and saturation boost, and specular highlights that catch
//! light from the top-left. Our own windows never go into that picture, so
//! nothing needs hiding from capture, and the glass shows in screenshots and
//! recordings as it does on screen.
//!
//! Panes that only ever sit over the desktop (the menu bar and Dock with
//! reserved space, desktop widgets) refract the wallpaper image instead (see
//! [`Source`]), which needs no capture at all.
//!
//! Everything runs on the UI thread: frame pools are created with `Create`
//! (not free-threaded), so frames arrive through its DispatcherQueue.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use windows::core::{s, Interface, Result, PCSTR};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::GraphicsCaptureSession;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::System::{DispatcherQueue, DispatcherQueueTimer};
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Dwm::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsIconic, IsWindowVisible};

use crate::geom::Color;
use crate::win::scene::{self, timespan, Layer, Scene};
use crate::win::wallpaper;

/// Pixels kept around each pane, for the frost to sample past its edges.
const MARGIN: i32 = 16;
/// Shortest time between renders of the screen panes (about 60 Hz): a busy
/// window (video, scrolling) sends a frame every refresh, up to 144+ a second.
const MIN_INTERVAL: Duration = Duration::from_millis(16);
/// The same in battery saver and Energy Saver (about 30 Hz).
const LOW_POWER_INTERVAL: Duration = Duration::from_millis(33);
/// How often to look for windows that moved, opened or closed under glass
/// without telling us (the foreground window's moves come right away).
const POLL: Duration = Duration::from_millis(250);
/// How often to check whether a live wallpaper app started or stopped (they
/// don't announce it).
const LIVE_POLL: Duration = Duration::from_secs(2);

fn min_interval() -> Duration {
    if crate::win::controls::low_power_mode() { LOW_POWER_INTERVAL } else { MIN_INTERVAL }
}

/// How assertive a pane's glass is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    /// Dock, menus, Spotlight: clear glass with a strong lens at the bezel.
    Bold,
    /// Window sidebars: frosted and mostly tint, with a faint lens and rim.
    Subtle,
}
const MIPS: u32 = 5;

/// What a pane refracts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Whatever is on screen behind it, rebuilt from the windows there.
    Screen,
    /// The desktop wallpaper, drawn directly, for panes that only sit over the
    /// desktop. Falls back to `Screen` under a live wallpaper.
    Wallpaper,
}

const SHADER: &str = r#"
Texture2D src : register(t0);
SamplerState smp : register(s0);

cbuffer C : register(b0) {
    float2 size;     // pane, px
    float2 origin;   // pane (0,0) in source texels
    float2 texSize;  // source texture, px
    float radius;
    float scale;     // DPI scale
    float4 tint;     // straight alpha
    float dark;
    float bezel;     // px; 0 = flat glass (no bending, no rim)
    float refract;   // max bend, px
    float frost;     // blur tap radius, px
    float strength;  // 1 = bold (Dock, menus); lower = subtle, frostier (sidebars)
    float lod;       // extra blur, in mip levels
    float count;     // shapes in the pane
    float pad;
    float4 shapes[5]; // x, y, w, h in pane px
    float4 melt[5];   // x: how far (px) a shape reaches to merge with those before it
    float4 coat;      // painted over the glass, straight alpha
    float4 border;    // a hairline around its edge, straight alpha
};

struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

V vs(uint id : SV_VertexID) {
    V o;
    float2 t = float2((id << 1) & 2, id & 2);
    o.uv = t;
    o.pos = float4(t * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}

float sd(float2 p, float2 b, float r) {
    float2 q = abs(p) - b + r;
    return length(max(q, 0)) + min(max(q.x, q.y), 0) - r;
}

// Distance to the pane's shapes (rounded to at least `minR`), as one body:
// shapes that near each other flow together like drops (a smooth union).
float field(float2 p, float minR) {
    float d = 1e6;
    [loop] for (int i = 0; i < (int)count; i++) {
        float2 b = shapes[i].zw * 0.5;
        float r = min(max(radius, minR), min(b.x, b.y));
        float di = sd(p - shapes[i].xy - b, b, r);
        float k = melt[i].x;
        if (i > 0 && k > 0) {
            float m = max(k - abs(d - di), 0) / k;
            d = min(d, di) - m * m * k * 0.25;
        } else {
            d = min(d, di);
        }
    }
    return d;
}

float3 tap(float2 px, float level) {
    float2 inv = 1.0 / texSize;
    return src.SampleLevel(smp, clamp(px * inv, 0.5 * inv, 1 - 0.5 * inv), level).rgb;
}

// Nine taps: a sharper middle (returned in `mid`) and a ring a mip blurrier.
float3 frosted(float2 px, float lv, out float3 mid) {
    static const float2 ring[8] = {
        float2(1, 0), float2(0.707, 0.707), float2(0, 1), float2(-0.707, 0.707),
        float2(-1, 0), float2(-0.707, -0.707), float2(0, -1), float2(0.707, -0.707)
    };
    mid = tap(px, 1.0 + lv);
    float3 c = mid * 2;
    // The ring reaches at least three quarters of a texel of the level it
    // reads: any closer and the taps all land in the same few texels, which
    // shows that level's grid (blocky frost that shimmers as things move
    // behind it) where the blur is strong.
    float reach = max(frost, 0.75 * exp2(1.5 + lv));
    [unroll] for (int i = 0; i < 8; i++) {
        c += tap(px + ring[i] * reach, 1.5 + lv);
    }
    return c / 10;
}

float4 ps(V i) : SV_Target {
    float2 p = i.uv * size;
    float d = field(p, 0);
    float alpha = saturate(0.5 - d);
    if (alpha <= 0) return 0;

    // The lens normal comes from the shape rounded to at least the bezel's
    // width, so it turns smoothly through tight corners instead of snapping
    // between the two edges along the diagonal.
    float e = 0.5;
    float2 n = float2(field(p + float2(e, 0), bezel) - field(p - float2(e, 0), bezel),
                      field(p + float2(0, e), bezel) - field(p - float2(0, e), bezel));
    n = n / max(length(n), 1e-4);
    float depth = -d;

    // Convex bezel with a circular profile: what's behind is pulled in from
    // further inside near the edge (magnified, steeply at the rim), easing to
    // flat glass in the middle.
    float t = bezel > 0 ? saturate(depth / bezel) : 1;
    float u = 1 - t;
    float bend = refract * (1 - sqrt(saturate(1 - u * u)));
    float2 q = p - n * bend + origin;

    // Toward the rim the bend outruns the pixels, so each one spans several
    // texels of what's behind; sample a mip that covers them, or moving
    // content shimmers there.
    float slope = bezel > 0 ? refract * u / (bezel * sqrt(max(1 - u * u, 1e-3))) : 0;
    float lv = lod + clamp(log2(max(abs(1 - slope), 1)), 0, 3);

    // Dispersion: red and blue shift apart along the normal, as a correction
    // on top of the frost rather than a frost per channel.
    float3 mid;
    float3 c = frosted(q, lv, mid);
    if (bend > 0.4) {
        float ca = bend * 0.14;
        c.r += 0.6 * (tap(q + n * ca, 1.0 + lv).r - mid.r);
        c.b += 0.6 * (tap(q - n * ca, 1.0 + lv).b - mid.b);
    }

    // Vibrancy, then the body of the glass.
    // Light glass gets a gentler boost: saturated colour behind dark text is
    // what makes it hard to read.
    float l = dot(c, float3(0.2126, 0.7152, 0.0722));
    c = lerp(l.xxx, c, 1 + (dark > 0.5 ? 0.4 : 0.2) * strength);
    // Adaptive, like macOS: dark glass dims further over bright content and
    // light glass lifts over dark content, so what's written on it stays
    // readable. Judged from the coarsest mip, so it eases across the pane
    // instead of following every detail behind it.
    float back = dot(tap(q, 4.0), float3(0.2126, 0.7152, 0.0722));
    c = dark > 0.5 ? c * lerp(0.86, 0.5, smoothstep(0.55, 0.95, back))
                   : lerp(c, 1, lerp(0.2, 0.6, 1 - smoothstep(0.2, 0.85, back)));
    c = lerp(c, tint.rgb, tint.a);
    if (dark < 0.5) {
        // A floor under dark text: whatever's behind, light glass stays light
        // enough to read on (a grey lift, so it keeps its colour). Subtle
        // glass is mostly tint already and needs less.
        float floorL = lerp(0.66, 0.74, strength);
        c += max(floorL - dot(c, float3(0.2126, 0.7152, 0.0722)), 0);
    }

    if (bezel > 0) {
        // Light from the top-left: a bright glint on the facing rim, a softer
        // one opposite, and the curved bezel catching some of it.
        float2 L = normalize(float2(-0.55, -0.85));
        float facing = dot(n, L);
        float glint = pow(saturate(facing), 3) + 0.5 * pow(saturate(-facing), 3);
        float k = (dark > 0.5 ? 0.65 : 1.0) * (0.35 + 0.65 * strength);
        float rim = saturate(1 - depth / (1.3 * scale));
        c += k * rim * (0.16 + 0.7 * glint);
        c += k * pow(1 - t, 3) * (0.05 + 0.18 * glint);
        c *= 1 - 0.08 * pow(1 - t, 2) * saturate(-facing);
    }
    // What a surface would otherwise paint over its glass: here it follows
    // the glass exactly, through every merge, and never a frame behind it.
    c = lerp(saturate(c), coat.rgb, coat.a);
    c = lerp(c, border.rgb, border.a * saturate(0.5 * scale - depth + 0.5));
    return float4(saturate(c) * alpha, alpha);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Constants {
    size: [f32; 2],
    origin: [f32; 2],
    tex_size: [f32; 2],
    radius: f32,
    scale: f32,
    tint: [f32; 4],
    dark: f32,
    bezel: f32,
    refract: f32,
    frost: f32,
    strength: f32,
    lod: f32,
    count: f32,
    pad: f32,
    shapes: [[f32; 4]; MAX_SHAPES],
    melt: [[f32; 4]; MAX_SHAPES],
    coat: [f32; 4],
    border: [f32; 4],
}

/// The most shapes one pane can hold (see `Pane::set_shapes`).
pub const MAX_SHAPES: usize = 5;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
    static FAILED: Cell<bool> = const { Cell::new(false) };
    static STATS: RefCell<Stats> = RefCell::new(Stats::new());
}

/// Whether new glass surfaces refract (`[general] glass_refraction`).
pub fn set_enabled(on: bool) {
    ENABLED.with(|e| e.set(on));
}

pub fn enabled() -> bool {
    ENABLED.with(|e| e.get()) && !FAILED.with(|f| f.get())
}

/// A monitor's wallpaper, as `Source::Wallpaper` panes see it.
struct Wallpaper {
    handle: HMONITOR,
    rect: RECT,
    /// With its mips, made once when it loads: every pane over the desktop
    /// reads this one texture. None while it loads, and when it couldn't be
    /// loaded (not retried until the wallpaper changes).
    tex: Option<ID3D11ShaderResourceView>,
    loading: bool,
}

/// Whether `hwnd` shows any of itself: visible, not minimized, and not sized to
/// nothing or moved off its monitor (the Dock slides off the screen to auto-hide).
fn on_screen(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_err() {
            return false;
        }
        let m = monitor_rect(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST));
        wr.left.max(m.left) < wr.right.min(m.right) && wr.top.max(m.top) < wr.bottom.min(m.bottom)
    }
}

fn monitor_rect(handle: HMONITOR) -> RECT {
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(handle, &mut info);
    }
    info.rcMonitor
}

/// A texture size for `need`: `have` while it holds `need` and isn't more
/// than twice as big, else `need` rounded up to `STEP`, at most `max`.
fn fit(have: Option<(u32, u32)>, need: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    const STEP: u32 = 64;
    let ok = |have: u32, need: u32, max: u32| have >= need.min(max) && have <= max && have <= need * 2 + STEP;
    let up = |need: u32, max: u32| (need.div_ceil(STEP) * STEP).min(max).max(1);
    match have {
        Some(h) if ok(h.0, need.0, max.0) && ok(h.1, need.1, max.1) => h,
        _ => (up(need.0, max.0), up(need.1, max.1)),
    }
}

/// How often the display refreshes, from DWM (60 Hz if it can't say).
fn refresh_period() -> Duration {
    let mut info = DWM_TIMING_INFO { cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32, ..Default::default() };
    let r = unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut info) };
    let (n, d) = (info.rateRefresh.uiNumerator, info.rateRefresh.uiDenominator);
    if r.is_ok() && n > 0 && d > 0 {
        Duration::from_secs_f64(d as f64 / n as f64).clamp(Duration::from_millis(2), Duration::from_millis(34))
    } else {
        Duration::from_micros(16_667)
    }
}

fn intersects(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// A timer on this thread's DispatcherQueue that runs `f` on the engine.
fn engine_timer(repeating: bool, f: fn(&mut Engine)) -> Option<DispatcherQueueTimer> {
    let timer = (|| -> Result<DispatcherQueueTimer> {
        let t = DispatcherQueue::GetForCurrentThread()?.CreateTimer()?;
        t.SetIsRepeating(repeating)?;
        t.Tick(&TypedEventHandler::new(move |_, _| {
            with_running(f);
            Ok(())
        }))?;
        Ok(t)
    })();
    timer.map_err(|e| log!("glass timer failed: {e}")).ok()
}

struct Engine {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    factory: IDXGIFactory2,
    winrt: IDirect3DDevice,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    scene: Scene,
    wallpapers: Vec<Wallpaper>,
    /// Bumped when the wallpaper changes, so a load still on its way for the
    /// old one is dropped.
    wallpaper_gen: u64,
    panes: Vec<Weak<RefCell<PaneState>>>,
    /// Shortest time between renders (see `min_interval`).
    interval: Duration,
    /// When screen panes last rendered for new frames.
    last: Option<Instant>,
    /// Renders what changed too soon after the last render, once it's due.
    timer: Option<DispatcherQueueTimer>,
    /// Looks for windows that changed under glass (see `POLL`).
    poll: Option<DispatcherQueueTimer>,
    /// The display's refresh period: moved panes render at most this often.
    refresh: Duration,
    /// Renders panes that moved too soon after their last render (see `request`).
    moved: Option<DispatcherQueueTimer>,
    /// Windows may have moved, opened or closed since panes last rendered.
    restack: bool,
    /// See `windows()`.
    windows: Option<(Vec<scene::Window>, Instant)>,
    /// An app draws a live wallpaper (see `wallpaper::is_live`): panes that
    /// would refract the wallpaper image refract the desktop window instead,
    /// which holds the live wallpaper's frames.
    live: bool,
    /// Keeps `live` current.
    live_poll: Option<DispatcherQueueTimer>,
}

/// Compiles one entry point of an HLSL source.
pub(crate) fn compile(source: &str, name: PCSTR, entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    let mut code = None;
    let mut errors = None;
    let r = unsafe {
        D3DCompile(
            source.as_ptr() as _,
            source.len(),
            name,
            None,
            None,
            entry,
            target,
            1 << 15, // D3DCOMPILE_OPTIMIZATION_LEVEL3
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = r {
        if let Some(b) = errors {
            let msg = unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) };
            log!("glass shader: {}", String::from_utf8_lossy(msg));
        }
        return Err(e);
    }
    let b = code.unwrap();
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec())
}

impl Engine {
    fn new() -> Result<Self> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL));
        }
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let (device, context) = (device.unwrap(), context.unwrap());
        let dxgi: IDXGIDevice = device.cast()?;
        let factory: IDXGIFactory2 = unsafe { dxgi.GetAdapter()?.GetParent()? };
        let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;

        let vs_code = compile(SHADER, s!("glass.hlsl"), s!("vs"), s!("vs_4_0"))?;
        let ps_code = compile(SHADER, s!("glass.hlsl"), s!("ps"), s!("ps_4_0"))?;
        let (mut vs, mut ps, mut sampler, mut constants) = (None, None, None, None);
        unsafe {
            device.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps))?;
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            device.CreateSamplerState(&desc, Some(&mut sampler))?;
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Constants>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            device.CreateBuffer(&desc, None, Some(&mut constants))?;
        }
        let scene = Scene::new(&device)?;
        Ok(Self {
            device,
            context,
            factory,
            winrt,
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            sampler: sampler.unwrap(),
            constants: constants.unwrap(),
            scene,
            wallpapers: Vec::new(),
            wallpaper_gen: 0,
            panes: Vec::new(),
            interval: min_interval(),
            last: None,
            timer: None,
            poll: None,
            refresh: refresh_period(),
            moved: None,
            restack: false,
            windows: None,
            live: wallpaper::is_live(),
            live_poll: None,
        })
    }

    /// A monitor's wallpaper texture with its screen origin and size, loaded on
    /// first use and again when the monitor's bounds change. None while it
    /// loads: panes refract the screen until then.
    fn wallpaper(&mut self, handle: HMONITOR) -> Option<(ID3D11ShaderResourceView, (i32, i32), (i32, i32))> {
        let rect = monitor_rect(handle);
        let Some(found) = self.wallpapers.iter().find(|w| w.handle == handle && w.rect == rect) else {
            self.wallpapers.retain(|w| w.handle != handle);
            self.wallpapers.push(Wallpaper { handle, rect, tex: None, loading: true });
            self.load_wallpaper(handle, rect);
            return None;
        };
        let tex = found.tex.clone()?;
        Some((tex, (rect.left, rect.top), (rect.right - rect.left, rect.bottom - rect.top)))
    }

    /// Renders the wallpaper's pixels on a thread of their own and hands them
    /// back to this one. Off DockSide's UI thread because it asks Explorer
    /// (out of process) and decodes the whole picture: seconds at login.
    fn load_wallpaper(&self, handle: HMONITOR, rect: RECT) {
        let queue = match DispatcherQueue::GetForCurrentThread() {
            Ok(q) => q,
            Err(e) => return log!("glass wallpaper failed: {e}"),
        };
        let (generation, monitor) = (self.wallpaper_gen, handle.0 as isize);
        let spawned = std::thread::Builder::new().name("glass wallpaper".into()).spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            }
            let started = Instant::now();
            let mut pixels = Some(wallpaper::render(rect));
            let took = started.elapsed();
            if took > Duration::from_millis(200) {
                log!("glass wallpaper: rendered in {took:?}");
            }
            let handler = windows::System::DispatcherQueueHandler::new(move || {
                if let Some(px) = pixels.take() {
                    with_running(|e| e.wallpaper_loaded(monitor, rect, generation, px));
                }
                Ok(())
            });
            if !queue.TryEnqueue(&handler).unwrap_or(false) {
                log!("glass wallpaper: couldn't hand the picture back");
            }
        });
        if let Err(e) = spawned {
            log!("glass wallpaper thread: {e}");
        }
    }

    /// A wallpaper's pixels arrived: made into its texture, and the panes that
    /// want it switch from the screen to it.
    fn wallpaper_loaded(&mut self, monitor: isize, rect: RECT, generation: u64, pixels: Result<Vec<u8>>) {
        if generation != self.wallpaper_gen {
            return;
        }
        let waiting = |w: &Wallpaper| w.handle.0 as isize == monitor && w.rect == rect && w.loading;
        if !self.wallpapers.iter().any(waiting) {
            return;
        }
        let tex = pixels.and_then(|px| self.upload_wallpaper(rect, &px)).map_err(|e| log!("glass wallpaper failed: {e}")).ok();
        if let Some(w) = self.wallpapers.iter_mut().find(|w| waiting(w)) {
            w.loading = false;
            w.tex = tex;
        }
        self.resource_all();
    }

    fn upload_wallpaper(&self, rect: RECT, pixels: &[u8]) -> Result<ID3D11ShaderResourceView> {
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        // The mips are the frost: made here, once, not on every render.
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w as u32,
            Height: h as u32,
            MipLevels: MIPS,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
        };
        let (mut tex, mut srv) = (None, None);
        unsafe {
            self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.as_ref().unwrap();
            self.context.UpdateSubresource(tex, 0, None, pixels.as_ptr() as _, (w * 4) as u32, 0);
            self.device.CreateShaderResourceView(tex, None, Some(&mut srv))?;
            self.context.GenerateMips(srv.as_ref().unwrap());
        }
        Ok(srv.unwrap())
    }

    /// What a pane asking for `want` refracts: the wallpaper image only while
    /// it's a still picture that loaded.
    fn resolve(&mut self, want: Source, monitor: HMONITOR) -> Source {
        match want {
            Source::Wallpaper if self.live || self.wallpaper(monitor).is_none() => Source::Screen,
            s => s,
        }
    }

    fn start_live_poll(&mut self) {
        if self.live_poll.is_none() {
            self.live_poll = engine_timer(true, Engine::check_live);
            if let Some(t) = &self.live_poll {
                let _ = t.SetInterval(timespan(LIVE_POLL));
                let _ = t.Start();
            }
        }
    }

    /// Notices a live wallpaper starting or stopping, and redraws every pane for it.
    fn check_live(&mut self) {
        let live = wallpaper::is_live();
        if live != self.live {
            log!("live wallpaper {}", if live { "started: glass refracts the desktop window" } else { "stopped" });
            self.live = live;
            self.resource_all();
        }
    }

    /// Re-picks every pane's source and redraws it.
    fn resource_all(&mut self) {
        self.windows = None;
        let panes: Vec<_> = self.panes.iter().filter_map(|p| p.upgrade()).collect();
        for pane in panes {
            let Ok(mut state) = pane.try_borrow_mut() else { continue };
            let monitor = state.monitor;
            state.source = self.resolve(state.want, monitor);
            if state.source != Source::Screen {
                state.layers.clear();
            }
            state.stale = true;
            if let Err(e) = self.render(&mut state) {
                log!("glass render failed: {e}");
            }
        }
        self.prune();
    }

    fn screen_panes(&mut self) -> Vec<Rc<RefCell<PaneState>>> {
        self.panes.retain(|p| p.strong_count() > 0);
        self.panes
            .iter()
            .filter_map(|p| p.upgrade())
            .filter(|p| p.try_borrow().is_ok_and(|s| s.source == Source::Screen))
            .collect()
    }

    /// Takes a window's newest frame and marks the panes it shows through
    /// where it changed.
    fn window_frame(&mut self, hwnd: isize) {
        let panes = self.screen_panes();
        let rect = panes.iter().find_map(|p| p.try_borrow().ok()?.layers.iter().find(|l| l.hwnd == hwnd).map(|l| l.rect));
        let Some(dirty) = self.scene.take(&self.device, &self.winrt, hwnd, rect) else { return };
        count(|s| s.frames += 1);
        let mut any = false;
        for pane in &panes {
            let Ok(mut s) = pane.try_borrow_mut() else { continue };
            let changed = dirty.as_ref().map_or(true, |d| d.iter().any(|r| intersects(r, &s.region)));
            if changed && s.layers.iter().any(|l| l.hwnd == hwnd) {
                s.stale = true;
                any = true;
            }
        }
        if any {
            self.schedule();
        }
    }

    /// Renders stale panes now, or once `interval` has passed since the last render.
    fn schedule(&mut self) {
        if let Some(wait) = self.last.and_then(|t| self.interval.checked_sub(t.elapsed())) {
            if self.timer.is_none() {
                self.timer = engine_timer(false, Engine::flush);
            }
            if let Some(t) = &self.timer {
                // Already waiting: that render takes everything changed until then.
                if t.IsRunning().unwrap_or(false) {
                    return;
                }
                let _ = t.SetInterval(timespan(wait));
                if t.Start().is_ok() {
                    return;
                }
            }
        }
        self.flush();
    }

    /// Other apps' windows, as last listed. Listing asks DWM about every
    /// window, so a pane that only moves or reshapes (the Dock magnifying under
    /// the pointer) reuses the list; it's made afresh when windows change,
    /// and at least every `POLL`.
    fn windows(&mut self) -> &[scene::Window] {
        if self.windows.as_ref().map_or(true, |(_, at)| at.elapsed() >= POLL) {
            self.windows = Some((scene::windows(), Instant::now()));
        }
        &self.windows.as_ref().unwrap().0
    }

    /// Re-renders the screen panes whose windows changed.
    fn flush(&mut self) {
        let panes = self.screen_panes();
        if std::mem::take(&mut self.restack) {
            self.windows = None;
            let windows = self.windows();
            for pane in &panes {
                let Ok(mut s) = pane.try_borrow_mut() else { continue };
                if on_screen(s.hwnd) && (!s.shown || layers_for(s.want, windows, &s.region, s.hwnd) != s.layers) {
                    s.stale = true;
                }
            }
        }
        let mut rendered = 0;
        let started = Instant::now();
        for pane in &panes {
            let Ok(mut s) = pane.try_borrow_mut() else { continue };
            if s.stale {
                rendered += 1;
                if let Err(e) = self.render(&mut s) {
                    log!("glass render failed: {e}");
                }
            }
        }
        if rendered > 0 {
            crate::cpu_watch::glass_render(rendered, started.elapsed());
            self.last = Some(Instant::now());
        }
        self.prune();
    }

    /// Stops unneeded captures (see `Scene::prune`). Direct3D only frees what
    /// was released once the context is flushed, and with no glass on screen
    /// nothing else would flush it: a maximized window's capture would stay
    /// allocated (~29 MB) until the next render.
    fn prune(&mut self) {
        if self.scene.prune() {
            unsafe { self.context.Flush() };
        }
    }

    /// Looks for changed windows, while any screen pane is on screen.
    fn poll(&mut self) {
        let panes = self.screen_panes();
        if !panes.iter().any(|p| p.try_borrow().is_ok_and(|s| !s.layers.is_empty())) {
            // Keeps going until the last capture has been let go (see `scene::GRACE`).
            self.prune();
            if self.scene.hwnds().is_empty() {
                if let Some(t) = &self.poll {
                    let _ = t.Stop();
                }
            }
            return;
        }
        // A frame whose arrival we missed (the engine was busy) would otherwise
        // sit in its pool and stall that capture.
        for hwnd in self.scene.hwnds() {
            self.window_frame(hwnd);
        }
        self.restack = true;
        self.schedule();
    }

    /// Renders a pane that moved or reshaped, at most once per display
    /// refresh: a widget dragged with a fast mouse moves several times a
    /// refresh, and each render presents a frame no one would see.
    fn request(&mut self, pane: &mut PaneState) {
        // Some slack: a surface animating at the refresh rate shouldn't be held back by jitter.
        let min = self.refresh.mul_f32(0.75);
        // Resized buffers show nothing until drawn into, so those can't wait.
        let resized = pane.drawn.as_ref().map_or(true, |k| k.1 != pane.size);
        if let Some(wait) = pane.presented.filter(|_| !resized).and_then(|t| min.checked_sub(t.elapsed())) {
            pane.moved = true;
            if self.moved.is_none() {
                self.moved = engine_timer(false, Engine::render_moved);
            }
            if let Some(t) = &self.moved {
                // Already waiting: that render picks this pane up too.
                if t.IsRunning().unwrap_or(false) {
                    return;
                }
                let _ = t.SetInterval(timespan(wait));
                if t.Start().is_ok() {
                    return;
                }
            }
            pane.moved = false;
        }
        if let Err(e) = self.render(pane) {
            log!("glass render failed: {e}");
        }
    }

    fn render_moved(&mut self) {
        let panes: Vec<_> = self.panes.iter().filter_map(|p| p.upgrade()).collect();
        for pane in panes {
            let Ok(mut s) = pane.try_borrow_mut() else { continue };
            if std::mem::take(&mut s.moved) {
                if let Err(e) = self.render(&mut s) {
                    log!("glass render failed: {e}");
                }
            }
        }
    }

    fn start_poll(&mut self) {
        if self.poll.is_none() {
            self.poll = engine_timer(true, Engine::poll);
            if let Some(t) = &self.poll {
                let _ = t.SetInterval(timespan(POLL));
            }
        }
        if let Some(t) = &self.poll {
            if !t.IsRunning().unwrap_or(true) {
                let _ = t.Start();
            }
        }
    }

    fn render(&mut self, pane: &mut PaneState) -> Result<()> {
        let stale = std::mem::take(&mut pane.stale);
        if pane.size.0 == 0 || pane.size.1 == 0 {
            return Ok(());
        }
        // Hidden, minimized or slid away: nothing to show, and no windows to keep capturing.
        if pane.source == Source::Screen && !on_screen(pane.hwnd) {
            pane.layers.clear();
            pane.drawn = None;
            return Ok(());
        }
        let monitor = unsafe { MonitorFromWindow(pane.hwnd, MONITOR_DEFAULTTONEAREST) };
        pane.monitor = monitor;

        // Region behind the pane, plus a margin, in screen pixels, kept to its monitor.
        let mut wr = RECT::default();
        unsafe { GetWindowRect(pane.hwnd, &mut wr)? };
        // A surface often moves and reshapes in separate calls within one
        // frame; once it's drawn for where it ends up, the other is a no-op.
        let t = pane.tint;
        let mut shape_bits: Vec<u32> = pane.shapes.iter().flat_map(|(r, k)| r.iter().chain(std::iter::once(k)).map(|v| v.to_bits())).collect();
        shape_bits.extend([pane.coat, pane.border].iter().flat_map(|c| [c.r, c.g, c.b, c.a]).map(f32::to_bits));
        let key = (wr, pane.size, pane.radius.to_bits(), [t.r, t.g, t.b, t.a].map(f32::to_bits), shape_bits);
        if !stale && pane.drawn.as_ref() == Some(&key) {
            return Ok(());
        }
        let (w, h) = (pane.size.0 as i32, pane.size.1 as i32);
        let m = monitor_rect(monitor);
        let need = RECT {
            left: (wr.left - MARGIN).clamp(m.left, m.right),
            top: (wr.top - MARGIN).clamp(m.top, m.bottom),
            right: (wr.left + w + MARGIN).clamp(m.left, m.right),
            bottom: (wr.top + h + MARGIN).clamp(m.top, m.bottom),
        };
        if need.right - need.left < 1 || need.bottom - need.top < 1 {
            return Ok(());
        }
        let (srv, region, src_size) = match pane.source {
            Source::Screen => {
                // The texture is kept a size up from what's needed, so a pane that
                // grows a little each frame (Dock magnification) doesn't reallocate it
                // every frame. It holds more of the screen to fill it, slid back
                // inside the monitor at its edges.
                let need_size = ((need.right - need.left) as u32, (need.bottom - need.top) as u32);
                let mon_size = ((m.right - m.left) as u32, (m.bottom - m.top) as u32);
                let src_size = fit(pane.src.as_ref().map(|s| s.3), need_size, mon_size);
                let left = need.left.min(m.right - src_size.0 as i32).max(m.left);
                let top = need.top.min(m.bottom - src_size.1 as i32).max(m.top);
                let region = RECT { left, top, right: left + src_size.0 as i32, bottom: top + src_size.1 as i32 };
                if pane.src.as_ref().map(|s| s.3) != Some(src_size) {
                    let desc = D3D11_TEXTURE2D_DESC {
                        Width: src_size.0,
                        Height: src_size.1,
                        MipLevels: MIPS,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
                    };
                    let (mut tex, mut srv, mut rtv) = (None, None, None);
                    unsafe {
                        self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                        let tex = tex.as_ref().unwrap();
                        self.device.CreateShaderResourceView(tex, None, Some(&mut srv))?;
                        // Mip 0 only: GenerateMips makes the rest for the frost.
                        let rdesc = D3D11_RENDER_TARGET_VIEW_DESC {
                            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
                        };
                        self.device.CreateRenderTargetView(tex, Some(&rdesc), Some(&mut rtv))?;
                    }
                    pane.src = Some((tex.unwrap(), srv.unwrap(), rtv.unwrap(), src_size));
                }

                let layers = layers_for(pane.want, self.windows(), &region, pane.hwnd);
                // Heavy frost hides motion: what's behind it needn't come as often.
                self.scene.capture(&layers, &self.winrt, self.interval, pane.style == Style::Subtle);
                let (_, srv, rtv, _) = pane.src.as_ref().unwrap();
                self.scene.draw(&self.context, rtv, (region.left, region.top), src_size, &layers);
                pane.layers = layers;
                self.start_poll();
                // The frost reads its mips.
                unsafe { self.context.GenerateMips(srv) };
                (srv.clone(), region, src_size)
            }
            Source::Wallpaper => {
                // The monitor's one wallpaper texture, as it is: nothing to
                // copy or blur for this pane, however often it moves.
                let Some((srv, origin, size)) = self.wallpaper(monitor) else { return Ok(()) };
                pane.src = None;
                let region = RECT { left: origin.0, top: origin.1, right: origin.0 + size.0, bottom: origin.1 + size.1 };
                (srv, region, (size.0 as u32, size.1 as u32))
            }
        };
        pane.region = region;

        let dpi = unsafe { GetDpiForWindow(pane.hwnd) }.max(96) as f32;
        let scale = dpi / 96.0;
        let r = pane.radius.min(w as f32 / 2.0).min(h as f32 / 2.0);
        let flat = r < 0.5;
        let bezel = if flat { 0.0 } else { (h.min(w) as f32 * 0.45).min(20.0 * scale).max(r * 0.6) };
        let tint = pane.tint;
        let subtle = pane.style == Style::Subtle;
        // No shapes given: the pane is one, filling it.
        let (mut shapes, mut melt) = ([[0.0; 4]; MAX_SHAPES], [[0.0; 4]; MAX_SHAPES]);
        shapes[0] = [0.0, 0.0, w as f32, h as f32];
        for (i, (r, k)) in pane.shapes.iter().enumerate() {
            shapes[i] = *r;
            melt[i][0] = *k;
        }
        let consts = Constants {
            size: [w as f32, h as f32],
            origin: [(wr.left - region.left) as f32, (wr.top - region.top) as f32],
            tex_size: [src_size.0 as f32, src_size.1 as f32],
            radius: r,
            scale,
            tint: [tint.r, tint.g, tint.b, tint.a],
            dark: (tint.r + tint.g + tint.b < 1.5) as u8 as f32,
            bezel,
            refract: bezel * if subtle { 0.3 } else { 1.1 },
            frost: if subtle { 5.0 } else { 2.5 } * scale,
            strength: if subtle { 0.25 } else { 1.0 },
            lod: if subtle { 1.5 } else { 0.0 },
            count: pane.shapes.len().max(1) as f32,
            pad: 0.0,
            shapes,
            melt,
            coat: [pane.coat.r, pane.coat.g, pane.coat.b, pane.coat.a],
            border: [pane.border.r, pane.border.g, pane.border.b, pane.border.a],
        };

        unsafe {
            self.context.UpdateSubresource(&self.constants, 0, None, &consts as *const _ as _, 0, 0);

            // Flip model: buffer 0 is always the one to draw into next, so its view lasts.
            if pane.rtv.is_none() {
                let back: ID3D11Texture2D = pane.swapchain.GetBuffer(0)?;
                self.device.CreateRenderTargetView(&back, None, Some(&mut pane.rtv))?;
            }
            let c = &self.context;
            c.OMSetRenderTargets(Some(&[pane.rtv.clone()]), None);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: w as f32,
                Height: h as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.IASetInputLayout(None);
            c.VSSetShader(&self.vs, None);
            c.PSSetShader(&self.ps, None);
            c.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            c.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            c.Draw(3, 0);
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetRenderTargets(None, None);
            pane.swapchain.Present(0, DXGI_PRESENT(0)).ok()?;
        }
        pane.drawn = Some(key);
        pane.presented = Some(Instant::now());
        pane.shown = true;
        count(|s| s.renders += 1);
        Ok(())
    }
}

/// Stops every window capture and releases the GPU device. Called on quit,
/// while the UI thread still runs: left to process exit, open capture
/// sessions can hang the process in the graphics driver for good.
pub fn shutdown() {
    // Nothing may start the engine again.
    FAILED.with(|f| f.set(true));
    let Some(mut engine) = ENGINE.with(|e| e.try_borrow_mut().ok().and_then(|mut e| e.take())) else { return };
    for t in [&engine.timer, &engine.poll, &engine.moved, &engine.live_poll].into_iter().flatten() {
        let _ = t.Stop();
    }
    engine.scene.close();
    unsafe {
        engine.context.ClearState();
        engine.context.Flush();
    }
}

/// Runs `f` on the engine if it's already running (never starts it).
/// Where the glass engine's time goes, logged every 10 s while
/// `%APPDATA%\DockSide\glass-stats` exists (create it, then restart).
struct Stats {
    on: bool,
    since: Instant,
    /// Engine calls, the time spent in them, and the slowest one.
    calls: u32,
    busy: Duration,
    worst: Duration,
    /// Captured frames taken, and panes presented.
    frames: u32,
    renders: u32,
}

impl Stats {
    fn new() -> Self {
        let on = crate::config::dir().join("glass-stats").exists();
        Self { on, since: Instant::now(), calls: 0, busy: Duration::ZERO, worst: Duration::ZERO, frames: 0, renders: 0 }
    }
}

/// What a screen pane draws behind it. One made for the wallpaper (it only
/// sits over the desktop) refracts just the desktop window, holding a live
/// wallpaper: windows passing over or under it never tint it.
fn layers_for(want: Source, windows: &[scene::Window], region: &RECT, pane: HWND) -> Vec<Layer> {
    match want {
        Source::Wallpaper => scene::desktop(windows),
        Source::Screen => scene::layers(windows, region, pane.0 as isize),
    }
}

/// Times one engine call (see `Stats`).
fn timed<R>(f: impl FnOnce() -> R) -> R {
    if !STATS.with(|s| s.borrow().on) {
        return f();
    }
    let start = Instant::now();
    let r = f();
    let took = start.elapsed();
    STATS.with(|s| {
        let mut s = s.borrow_mut();
        s.calls += 1;
        s.busy += took;
        s.worst = s.worst.max(took);
        let span = s.since.elapsed();
        if span >= Duration::from_secs(10) {
            log!(
                "glass stats: {:.1}% of a core, {} calls (worst {:.1} ms), {:.1} frames/s, {:.1} renders/s",
                s.busy.as_secs_f64() / span.as_secs_f64() * 100.0,
                s.calls,
                s.worst.as_secs_f64() * 1000.0,
                s.frames as f64 / span.as_secs_f64(),
                s.renders as f64 / span.as_secs_f64()
            );
            *s = Stats::new();
        }
    });
    r
}

fn count(f: impl FnOnce(&mut Stats)) {
    STATS.with(|s| {
        let mut s = s.borrow_mut();
        if s.on {
            f(&mut s);
        }
    });
}

fn with_running(f: impl FnOnce(&mut Engine)) {
    ENGINE.with(|e| {
        if let Ok(mut e) = e.try_borrow_mut() {
            if let Some(engine) = e.as_mut() {
                timed(|| f(engine));
            }
        }
    });
}

/// Re-paces the window captures after battery saver or Energy Saver turned on or off.
pub fn power_changed() {
    with_running(|engine| {
        engine.interval = min_interval();
        engine.scene.set_interval(engine.interval);
    });
}

/// Some window may have moved, opened, closed or come forward: screen panes
/// check what's behind them (at most once per `interval`).
pub fn windows_changed() {
    with_running(|engine| {
        if engine.poll.as_ref().is_some_and(|t| t.IsRunning().unwrap_or(false)) {
            engine.restack = true;
            engine.schedule();
        }
    });
}

/// Reloads the wallpaper under `Source::Wallpaper` panes (after the user
/// changed it, or a slideshow moved on).
pub fn wallpaper_changed() {
    with_running(|engine| {
        engine.wallpapers.clear();
        engine.wallpaper_gen += 1;
        engine.live = wallpaper::is_live();
        engine.resource_all();
    });
}

fn with_engine<R>(f: impl FnOnce(&mut Engine) -> R) -> Option<R> {
    ENGINE.with(|e| {
        let mut e = e.try_borrow_mut().ok()?;
        if e.is_none() {
            if FAILED.with(|f| f.get()) {
                return None;
            }
            match Engine::new() {
                Ok(engine) => *e = Some(engine),
                Err(err) => {
                    log!("glass refraction unavailable: {err}");
                    FAILED.with(|f| f.set(true));
                    return None;
                }
            }
        }
        let engine = e.as_mut().unwrap();
        Some(timed(|| f(engine)))
    })
}

/// A captured window sent a new frame.
pub(crate) fn on_window_frame(hwnd: isize) {
    crate::cpu_watch::glass_frame();
    with_running(|engine| engine.window_frame(hwnd));
}

struct PaneState {
    hwnd: HWND,
    swapchain: IDXGISwapChain1,
    size: (u32, u32),
    radius: f32,
    tint: Color,
    style: Style,
    /// What it was made to refract, and what it does now (see `Engine::resolve`).
    want: Source,
    source: Source,
    monitor: HMONITOR,
    /// A view of its back buffer, made once per resize.
    rtv: Option<ID3D11RenderTargetView>,
    /// What's behind it, with views to draw it and read it, and its size.
    src: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, ID3D11RenderTargetView, (u32, u32))>,
    /// What it last sampled, in screen pixels.
    region: RECT,
    /// The windows behind it when it last rendered, bottom-most first.
    layers: Vec<Layer>,
    /// Something behind it changed since it last rendered.
    stale: bool,
    /// What it was last drawn for: window rect, size, radius, tint and shapes.
    drawn: Option<(RECT, (u32, u32), u32, [u32; 4], Vec<u32>)>,
    /// The shapes in it (x, y, w, h in its pixels) and how far each reaches
    /// to merge with those before it; empty for one shape filling the pane.
    shapes: Vec<([f32; 4], f32)>,
    /// A colour painted over the glass and a hairline around it (see `Pane::set_coat`).
    coat: Color,
    border: Color,
    /// When it last presented a frame.
    presented: Option<Instant>,
    /// Moved while too recently presented; renders on `Engine::moved`.
    moved: bool,
    /// Has presented at least once.
    shown: bool,
}

/// One refracting glass pane: a swap chain the caller puts in its visual tree.
pub struct Pane(Rc<RefCell<PaneState>>);

impl Pane {
    /// Returns the pane with its swap chain.
    pub fn new(hwnd: HWND, tint: Color, style: Style, source: Source) -> Option<(Self, IDXGISwapChain1)> {
        let swapchain = with_engine(|engine| -> Result<IDXGISwapChain1> {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: 1,
                Height: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            unsafe { engine.factory.CreateSwapChainForComposition(&engine.device, &desc, None) }
        })?;
        let swapchain = swapchain.map_err(|e| log!("glass swap chain failed: {e}")).ok()?;
        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        let want = source;
        let source = with_engine(|e| {
            e.start_live_poll();
            e.resolve(want, monitor)
        })
        .unwrap_or(Source::Screen);
        let state = Rc::new(RefCell::new(PaneState {
            hwnd,
            swapchain: swapchain.clone(),
            size: (0, 0),
            radius: 0.0,
            tint,
            style,
            want,
            source,
            monitor,
            rtv: None,
            src: None,
            region: RECT::default(),
            layers: Vec::new(),
            stale: false,
            drawn: None,
            shapes: Vec::new(),
            coat: Color::TRANSPARENT,
            border: Color::TRANSPARENT,
            presented: None,
            moved: false,
            shown: false,
        }));
        with_engine(|engine| engine.panes.push(Rc::downgrade(&state)));
        Some((Self(state), swapchain))
    }

    /// Size and corner radius in physical pixels.
    pub fn set_shape(&self, w: f32, h: f32, radius: f32) {
        let mut s = self.0.borrow_mut();
        let size = (w.round().max(0.0) as u32, h.round().max(0.0) as u32);
        s.radius = radius;
        if size == s.size || size.0 == 0 || size.1 == 0 {
            return;
        }
        s.size = size;
        // Exactly the pane's size: Composition shows a swap chain's whole
        // buffer (it ignores SetSourceSize), stretched over the visual, so
        // spare room in it would shrink the glass out of its shape.
        // The view holds a buffer, which must be let go first.
        s.rtv = None;
        unsafe {
            if let Err(e) = s.swapchain.ResizeBuffers(2, size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0)) {
                log!("glass resize failed: {e}");
            }
        }
    }

    /// Draws several shapes in the pane instead of one filling it, each
    /// rounded like the pane: `(x, y, w, h)` in its pixels, and how far (px)
    /// the shape reaches to flow together with those before it. Returns
    /// whether they changed (the caller then renders).
    pub fn set_shapes(&self, shapes: &[([f32; 4], f32)]) -> bool {
        let mut s = self.0.borrow_mut();
        let shapes = &shapes[..shapes.len().min(MAX_SHAPES)];
        if s.shapes == shapes {
            return false;
        }
        s.shapes = shapes.to_vec();
        true
    }

    /// Has the glass paint `coat` over itself and a hairline `border` around
    /// its edge, in place of the surface painting them on top: they then
    /// follow the glass's own outline. Shows from the next render.
    pub fn set_coat(&self, coat: Color, border: Color) {
        let mut s = self.0.borrow_mut();
        s.coat = coat;
        s.border = border;
    }

    pub fn set_tint(&self, tint: Color) {
        self.0.borrow_mut().tint = tint;
        self.render();
    }

    /// Whether it has drawn a frame yet.
    pub fn shown(&self) -> bool {
        self.0.try_borrow().is_ok_and(|s| s.shown)
    }

    /// Re-renders now (after the window moved or reshaped).
    pub fn render(&self) {
        let Ok(mut s) = self.0.try_borrow_mut() else { return };
        with_engine(|engine| engine.request(&mut s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_compiles() {
        compile(SHADER, s!("glass.hlsl"), s!("vs"), s!("vs_4_0")).unwrap();
        compile(SHADER, s!("glass.hlsl"), s!("ps"), s!("ps_4_0")).unwrap();
    }
}
