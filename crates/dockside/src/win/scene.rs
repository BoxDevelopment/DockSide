//! What's behind a glass pane, rebuilt from other apps' windows.
//!
//! Glass can't capture the monitor without seeing itself, and hiding our
//! windows from capture hides them from screenshots and recordings too. So
//! instead each window under some glass is captured on its own with
//! Windows.Graphics.Capture, and the windows are drawn back to front, from the
//! top-most one that hides everything under it. Explorer's desktop (Progman:
//! wallpaper and icons) is a window like any other, at the bottom. Our own
//! windows are simply left out, so DockSide stays visible to every capture.
//!
//! Some things never make it in: what DWM draws itself (window shadows,
//! open / minimize animations), and windows we may not capture (an elevated
//! app while we aren't), which are drawn as a solid fill instead.

use std::time::{Duration, Instant};

use windows::core::{s, Interface, Result};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureDirtyRegionMode, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::glass;

/// Windows under one pane, at most; the lowest ones are dropped past this.
const MAX_LAYERS: usize = 8;
/// A window's rounded corners: these squares at each corner of a window
/// (unless maximized) may show what's behind.
const CORNER: i32 = 12;
/// Uncovered pieces tracked per pane; past this, windows stop covering any
/// more (so a few extra get drawn) rather than splitting it further.
const MAX_OPEN: usize = 64;
/// How long a capture no pane needs keeps running, so a window that only
/// briefly stops being under glass (a drag across the Dock) needn't restart.
const GRACE: Duration = Duration::from_secs(5);
/// Drawn for a window that can't be captured.
const FILL: [f32; 4] = [0.16, 0.16, 0.17, 1.0];

const SHADER: &str = r#"
Texture2D src : register(t0);
SamplerState smp : register(s0);

cbuffer B : register(b0) {
    float4 dst;     // x, y, w, h in target px
    float2 target;  // target size, px
    float2 uvMax;   // content size / texture size
    float4 color;   // fill, premultiplied
};

struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

V vs(uint id : SV_VertexID) {
    float2 t = float2(id & 1, id >> 1);
    V o;
    o.uv = t * uvMax;
    o.pos = float4((dst.xy + t * dst.zw) / target * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}

float4 ps(V i) : SV_Target { return src.SampleLevel(smp, i.uv, 0); }
float4 fill(V i) : SV_Target { return color; }
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Constants {
    dst: [f32; 4],
    target: [f32; 2],
    uv_max: [f32; 2],
    color: [f32; 4],
}

/// A window under some glass: its handle and visible frame in screen pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layer {
    pub hwnd: isize,
    pub rect: RECT,
}

/// A window that could be under glass.
pub struct Window {
    layer: Layer,
    /// Hides what's behind it wherever it covers (no transparency to show through).
    opaque: bool,
    maximized: bool,
    /// One of ours: never drawn, only marks where a pane sits in the stack.
    ours: bool,
    /// The desktop (Progman), which holds the wallpaper, live or not.
    desktop: bool,
}

fn intersects(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

struct Enum {
    pid: u32,
    out: Vec<Window>,
}

unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
    let e = &mut *(lp.0 as *mut Enum);
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return TRUE;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == e.pid {
        e.out.push(Window { layer: Layer { hwnd: hwnd.0 as isize, rect: RECT::default() }, opaque: false, maximized: false, ours: true, desktop: false });
        return TRUE;
    }
    // Other virtual desktops, suspended Store apps, windows still opening.
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4);
    if cloaked != 0 {
        return TRUE;
    }
    let mut rect = RECT::default();
    if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut rect as *mut _ as _, std::mem::size_of::<RECT>() as u32).is_err() {
        let _ = GetWindowRect(hwnd, &mut rect);
    }
    if rect.right - rect.left < 2 || rect.bottom - rect.top < 2 {
        return TRUE;
    }
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let see_through = ex & (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0) != 0;
    e.out.push(Window { layer: Layer { hwnd: hwnd.0 as isize, rect }, opaque: !see_through, maximized: IsZoomed(hwnd).as_bool(), ours: false, desktop: crate::win::class_name(hwnd) == "Progman" });
    TRUE
}

/// Every other app's window on screen, top-most first, with markers for ours.
pub fn windows() -> Vec<Window> {
    let mut e = Enum { pid: std::process::id(), out: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut e as *mut _ as isize));
    }
    e.out
}

/// Cuts `cut` out of a set of rectangles.
fn subtract(rects: &mut Vec<RECT>, cut: &RECT) {
    let mut out = Vec::with_capacity(rects.len() + 4);
    for r in rects.drain(..) {
        if !intersects(&r, cut) {
            out.push(r);
            continue;
        }
        if r.top < cut.top {
            out.push(RECT { bottom: cut.top, ..r });
        }
        if cut.bottom < r.bottom {
            out.push(RECT { top: cut.bottom, ..r });
        }
        let (top, bottom) = (r.top.max(cut.top), r.bottom.min(cut.bottom));
        if r.left < cut.left {
            out.push(RECT { right: cut.left, top, bottom, ..r });
        }
        if cut.right < r.right {
            out.push(RECT { left: cut.right, top, bottom, ..r });
        }
    }
    *rects = out;
}

/// Just the desktop window, for panes that only ever sit over the desktop
/// (desktop widgets): they show the wallpaper and nothing else, whatever
/// passes over or under them.
pub fn desktop(windows: &[Window]) -> Vec<Layer> {
    windows.iter().filter(|w| w.desktop).map(|w| w.layer).take(1).collect()
}

/// The windows that show in `region`, bottom-most first. Top-most first, each
/// window that some still-uncovered part of the region shows is kept, and its
/// opaque area covers that part for the windows below; a window only hidden
/// by those above is skipped. (Maximized windows all overlap a pane at the top
/// of the screen, but only the front one shows under it.)
///
/// Only windows below `pane` count: one in front of it (the Alt+Tab switcher
/// over a desktop widget) isn't behind the glass.
pub fn layers(windows: &[Window], region: &RECT, pane: isize) -> Vec<Layer> {
    let below = windows.iter().position(|w| w.ours && w.layer.hwnd == pane).map_or(0, |i| i + 1);
    let mut open = vec![*region];
    let mut out = Vec::new();
    for w in &windows[below..] {
        if w.ours || !open.iter().any(|r| intersects(r, &w.layer.rect)) {
            continue;
        }
        out.push(w.layer);
        if w.opaque && open.len() < MAX_OPEN {
            let r = w.layer.rect;
            if w.maximized {
                subtract(&mut open, &r);
            } else {
                // Everything but the four rounded corners.
                subtract(&mut open, &RECT { left: r.left + CORNER, right: r.right - CORNER, ..r });
                subtract(&mut open, &RECT { top: r.top + CORNER, bottom: r.bottom - CORNER, ..r });
            }
        }
        if open.is_empty() || out.len() == MAX_LAYERS {
            break;
        }
    }
    out.reverse();
    out
}

/// One window's capture.
struct Capture {
    hwnd: isize,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
    size: SizeInt32,
    /// Latest frame, held until a newer one arrives, with a view to draw it
    /// from and the size of its content.
    frame: Option<(Direct3D11CaptureFrame, ID3D11ShaderResourceView, SizeInt32, (u32, u32))>,
    reports_dirty: bool,
    /// The desktop window (see `pace`).
    desktop: bool,
    /// Only frosted panes show it (see `pace`).
    frosted: bool,
    /// Last needed by a pane.
    used: Instant,
}

impl Capture {
    fn close(&mut self) {
        if let Some((f, ..)) = self.frame.take() {
            let _ = f.Close();
        }
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// What changed in a window's newest frames, in screen pixels; None for all of it.
pub type Dirty = Option<Vec<RECT>>;

pub struct Scene {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    fill: ID3D11PixelShader,
    blend: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    captures: Vec<Capture>,
    /// Windows that couldn't be captured; not retried while they exist.
    failed: Vec<isize>,
}

impl Scene {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let vs_code = glass::compile(SHADER, s!("scene.hlsl"), s!("vs"), s!("vs_4_0"))?;
        let ps_code = glass::compile(SHADER, s!("scene.hlsl"), s!("ps"), s!("ps_4_0"))?;
        let fill_code = glass::compile(SHADER, s!("scene.hlsl"), s!("fill"), s!("ps_4_0"))?;
        let (mut vs, mut ps, mut fill, mut blend, mut sampler, mut constants) = (None, None, None, None, None, None);
        unsafe {
            device.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps))?;
            device.CreatePixelShader(&fill_code, None, Some(&mut fill))?;
            // Captured windows are premultiplied (their rounded corners fade out).
            let mut desc = D3D11_BLEND_DESC::default();
            desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            device.CreateBlendState(&desc, Some(&mut blend))?;
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
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
        Ok(Self {
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            fill: fill.unwrap(),
            blend: blend.unwrap(),
            sampler: sampler.unwrap(),
            constants: constants.unwrap(),
            captures: Vec::new(),
            failed: Vec::new(),
        })
    }

    /// Starts capturing any of `layers` not captured yet, and marks them all in
    /// use. `frosted`: the pane asking blurs them heavily (see `pace`).
    pub fn capture(&mut self, layers: &[Layer], winrt: &IDirect3DDevice, interval: Duration, frosted: bool) {
        let now = Instant::now();
        for l in layers {
            if let Some(c) = self.captures.iter_mut().find(|c| c.hwnd == l.hwnd) {
                c.used = now;
                // A clear pane shows it now: back to full pace.
                if c.frosted && !frosted {
                    c.frosted = false;
                    let _ = c.session.SetMinUpdateInterval(timespan(pace(c.desktop, interval)));
                }
                continue;
            }
            if self.failed.contains(&l.hwnd) {
                continue;
            }
            match start(l.hwnd, winrt, interval, frosted) {
                Ok(c) => self.captures.push(c),
                Err(e) => {
                    log!("glass can't capture window {:#x}: {e}", l.hwnd);
                    self.failed.push(l.hwnd);
                }
            }
        }
    }

    /// Takes `hwnd`'s newest frame. Returns what changed, or None when no new frame came.
    pub fn take(&mut self, device: &ID3D11Device, winrt: &IDirect3DDevice, hwnd: isize, rect: Option<RECT>) -> Option<Dirty> {
        let c = self.captures.iter_mut().find(|c| c.hwnd == hwnd)?;
        let mut frame = c.pool.TryGetNextFrame().ok()?;
        let mut dirty: Dirty = Some(Vec::new());
        let mut add = |f: &Direct3D11CaptureFrame| {
            let rects = c.reports_dirty.then(|| f.DirtyRegions().ok()).flatten();
            match (dirty.as_mut(), rects, rect) {
                (Some(d), Some(rects), Some(at)) => d.extend(rects.into_iter().map(|r| RECT {
                    left: at.left + r.X,
                    top: at.top + r.Y,
                    right: at.left + r.X + r.Width,
                    bottom: at.top + r.Y + r.Height,
                })),
                _ => dirty = None,
            }
        };
        add(&frame);
        while let Ok(newer) = c.pool.TryGetNextFrame() {
            let _ = frame.Close();
            frame = newer;
            add(&frame);
        }
        let view = (|| -> Result<(ID3D11ShaderResourceView, SizeInt32, (u32, u32))> {
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut desc) };
            let mut srv = None;
            unsafe { device.CreateShaderResourceView(&tex, None, Some(&mut srv))? };
            Ok((srv.unwrap(), frame.ContentSize()?, (desc.Width, desc.Height)))
        })();
        let Ok((srv, size, tex_size)) = view else {
            let _ = frame.Close();
            return Some(None);
        };
        if let Some((old, ..)) = c.frame.replace((frame, srv, size, tex_size)) {
            let _ = old.Close();
        }
        if size.Width != c.size.Width || size.Height != c.size.Height {
            // The window resized: later frames come at the new size.
            c.size = size;
            dirty = None;
            if let Err(e) = c.pool.Recreate(winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size) {
                log!("glass window capture resize failed: {e}");
            }
        }
        Some(dirty)
    }

    /// Draws `layers` into `target` (`size` px, showing the screen from `origin`).
    pub fn draw(&self, context: &ID3D11DeviceContext, target: &ID3D11RenderTargetView, origin: (i32, i32), size: (u32, u32), layers: &[Layer]) {
        let c = context;
        unsafe {
            c.ClearRenderTargetView(target, &FILL);
            c.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            c.OMSetBlendState(&self.blend, None, u32::MAX);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: size.0 as f32, Height: size.1 as f32, MaxDepth: 1.0, ..Default::default() }]));
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            c.IASetInputLayout(None);
            c.VSSetShader(&self.vs, None);
            c.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            c.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        }
        for l in layers {
            let (x, y) = ((l.rect.left - origin.0) as f32, (l.rect.top - origin.1) as f32);
            let mut k = Constants { dst: [0.0; 4], target: [size.0 as f32, size.1 as f32], uv_max: [1.0, 1.0], color: FILL };
            if let Some((_, srv, content, tex)) = self.captures.iter().find(|c| c.hwnd == l.hwnd).and_then(|c| c.frame.as_ref()) {
                k.dst = [x, y, content.Width as f32, content.Height as f32];
                k.uv_max = [content.Width as f32 / tex.0 as f32, content.Height as f32 / tex.1 as f32];
                unsafe {
                    c.UpdateSubresource(&self.constants, 0, None, &k as *const _ as _, 0, 0);
                    c.PSSetShader(&self.ps, None);
                    c.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
                    c.Draw(4, 0);
                }
            } else if self.failed.contains(&l.hwnd) {
                k.dst = [x, y, (l.rect.right - l.rect.left) as f32, (l.rect.bottom - l.rect.top) as f32];
                unsafe {
                    c.UpdateSubresource(&self.constants, 0, None, &k as *const _ as _, 0, 0);
                    c.PSSetShader(&self.fill, None);
                    c.Draw(4, 0);
                }
            }
            // Otherwise its first frame hasn't come yet; it draws once it does.
        }
        unsafe {
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetBlendState(None, None, u32::MAX);
            c.OMSetRenderTargets(None, None);
        }
    }

    /// Stops captures no pane has needed for a while, and those of closed
    /// windows. True when any stopped: their buffers are only freed once the
    /// device context is next flushed.
    pub fn prune(&mut self) -> bool {
        let now = Instant::now();
        self.failed.retain(|&h| unsafe { IsWindow(HWND(h as _)) }.as_bool());
        let before = self.captures.len();
        self.captures.retain_mut(|c| {
            let keep = unsafe { IsWindow(HWND(c.hwnd as _)) }.as_bool() && now.duration_since(c.used) < GRACE;
            if !keep {
                c.close();
            }
            keep
        });
        self.captures.len() != before
    }

    /// Every window being captured.
    pub fn hwnds(&self) -> Vec<isize> {
        self.captures.iter().map(|c| c.hwnd).collect()
    }

    /// Re-paces every capture (battery saver turned on or off).
    pub fn set_interval(&self, interval: Duration) {
        for c in &self.captures {
            let _ = c.session.SetMinUpdateInterval(timespan(pace(c.desktop || c.frosted, interval)));
        }
    }

    pub fn close(&mut self) {
        for c in &mut self.captures {
            c.close();
        }
        self.captures.clear();
    }
}

pub fn timespan(d: Duration) -> TimeSpan {
    TimeSpan { Duration: (d.as_nanos() / 100) as i64 }
}

/// How often a capture may send frames. The desktop window holds a live
/// wallpaper's animation (Wallpaper Engine, Lively), redrawn in full every
/// frame: glass follows it at up to 30 fps (15 in battery saver), smooth
/// through the bend and frost, so a 60 fps wallpaper costs no more than a 30
/// fps one. Every frame re-renders each pane over the desktop.
///
/// A window only frosted panes show (window sidebars) is paced the same way:
/// through that much blur 30 fps reads as smoothly as 60, and each frame is a
/// copy of the whole window.
fn pace(half: bool, interval: Duration) -> Duration {
    if half { (interval * 2).max(Duration::from_millis(33)) } else { interval }
}

fn start(hwnd: isize, winrt: &IDirect3DDevice, interval: Duration, frosted: bool) -> Result<Capture> {
    let desktop = crate::win::class_name(HWND(hwnd as _)) == "Progman";
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(HWND(hwnd as _))? };
    let size = item.Size()?;
    let pool = Direct3D11CaptureFramePool::Create(winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    // No yellow capture border (Windows 11).
    let _ = session.SetIsBorderRequired(false);
    // Windows 11 24H2+: Windows paces the frames and reports what changed, so
    // panes over a still part of a window skip the redraw.
    let _ = session.SetMinUpdateInterval(timespan(pace(desktop || frosted, interval)));
    let reports_dirty = session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportOnly).is_ok();
    pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
        glass::on_window_frame(hwnd);
        Ok(())
    }))?;
    session.StartCapture()?;
    Ok(Capture { hwnd, pool, session, _item: item, size, frame: None, reports_dirty, desktop, frosted, used: Instant::now() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: isize, l: i32, t: i32, r: i32, b: i32, opaque: bool) -> Window {
        Window { layer: Layer { hwnd: id, rect: RECT { left: l, top: t, right: r, bottom: b } }, opaque, maximized: false, ours: false, desktop: false }
    }

    #[test]
    fn shader_compiles() {
        for (entry, target) in [(s!("vs"), s!("vs_4_0")), (s!("ps"), s!("ps_4_0")), (s!("fill"), s!("ps_4_0"))] {
            glass::compile(SHADER, s!("scene.hlsl"), entry, target).unwrap();
        }
    }

    #[test]
    fn stops_at_a_covering_window() {
        let region = RECT { left: 100, top: 100, right: 200, bottom: 150 };
        let ws = [
            win(1, 0, 0, 50, 50, true),     // elsewhere
            win(2, 90, 120, 300, 300, true), // overlaps, doesn't cover
            win(3, 0, 0, 400, 400, true),   // covers
            win(4, 0, 0, 500, 500, true),   // hidden under 3
        ];
        let got: Vec<isize> = layers(&ws, &region, 0).iter().map(|l| l.hwnd).collect();
        assert_eq!(got, vec![3, 2]);
    }

    #[test]
    fn sees_through_layered_and_rounded_windows() {
        let region = RECT { left: 100, top: 100, right: 200, bottom: 150 };
        // Covers, but layered; then one that covers only up to its rounded corners.
        let ws = [win(1, 0, 0, 400, 400, false), win(2, 95, 95, 205, 155, true), win(3, 0, 0, 400, 400, true)];
        let got: Vec<isize> = layers(&ws, &region, 0).iter().map(|l| l.hwnd).collect();
        assert_eq!(got, vec![3, 2, 1]);
    }

    #[test]
    fn skips_windows_hidden_by_those_above() {
        // A pane over the top of the screen, above where maximized windows start.
        let region = RECT { left: 1000, top: 0, right: 1500, bottom: 45 };
        let max = |id, opaque| Window { maximized: true, ..win(id, 0, 29, 2560, 1440, opaque) };
        let ws = [max(1, true), max(2, true), max(3, true), win(4, 0, 0, 2560, 1440, true)];
        let got: Vec<isize> = layers(&ws, &region, 0).iter().map(|l| l.hwnd).collect();
        assert_eq!(got, vec![4, 1]);
    }

    #[test]
    fn ignores_windows_in_front_of_the_pane() {
        // The Alt+Tab switcher (1) over a desktop widget (pane 9), above an app window (2).
        let region = RECT { left: 100, top: 100, right: 200, bottom: 150 };
        let pane = Window { ours: true, ..win(9, 0, 0, 0, 0, false) };
        let ws = [win(1, 0, 0, 400, 400, true), pane, win(2, 90, 120, 300, 300, true)];
        let got: Vec<isize> = layers(&ws, &region, 9).iter().map(|l| l.hwnd).collect();
        assert_eq!(got, vec![2]);
        // A pane that isn't listed (hidden) sees everything.
        let got: Vec<isize> = layers(&ws, &region, 5).iter().map(|l| l.hwnd).collect();
        assert_eq!(got, vec![1]);
    }
}
