//! Liquid Glass refraction.
//!
//! DWM's host backdrop is a fixed, heavy blur, so it can't bend light. For
//! real glass we capture each monitor with Windows.Graphics.Capture (our glass
//! windows are excluded from capture, so they never see themselves), copy the
//! region behind each pane into a mipmapped texture, and run a pixel shader
//! into a composition swap chain: a convex bezel that bends and magnifies what's
//! behind near the edges with a little chromatic dispersion, a light frost and
//! saturation boost, and specular highlights that catch light from the top-left.
//!
//! Everything runs on the UI thread: the frame pool is created with
//! `Create` (not free-threaded), so frames arrive through its DispatcherQueue.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use windows::core::{s, Interface, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE};

use crate::geom::Color;

/// Captured pixels kept around each pane, for the frost to sample past its edges.
const MARGIN: i32 = 16;

/// How assertive a pane's glass is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    /// Dock, menus, Spotlight: clear glass with a strong lens at the bezel.
    Bold,
    /// Window sidebars: frosted and mostly tint, with a faint lens and rim.
    Subtle,
}
const MIPS: u32 = 5;

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
    float2 pad;
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

float3 frosted(float2 px) {
    static const float2 ring[8] = {
        float2(1, 0), float2(0.707, 0.707), float2(0, 1), float2(-0.707, 0.707),
        float2(-1, 0), float2(-0.707, -0.707), float2(0, -1), float2(0.707, -0.707)
    };
    float2 inv = 1.0 / texSize;
    float2 lo = 0.5 * inv, hi = 1 - 0.5 * inv;
    float3 c = src.SampleLevel(smp, clamp(px * inv, lo, hi), 1.0 + lod).rgb * 2;
    [unroll] for (int i = 0; i < 8; i++) {
        c += src.SampleLevel(smp, clamp((px + ring[i] * frost) * inv, lo, hi), 1.5 + lod).rgb;
    }
    return c / 10;
}

float4 ps(V i) : SV_Target {
    float2 h = size * 0.5;
    float2 p = i.uv * size - h;
    float r = min(radius, min(h.x, h.y));
    float d = sd(p, h, r);
    float alpha = saturate(0.5 - d);
    if (alpha <= 0) return 0;

    float e = 0.5;
    float2 n = float2(sd(p + float2(e, 0), h, r) - sd(p - float2(e, 0), h, r),
                      sd(p + float2(0, e), h, r) - sd(p - float2(0, e), h, r));
    n = n / max(length(n), 1e-4);
    float depth = -d;

    // Convex bezel with a circular profile: what's behind is pulled in from
    // further inside near the edge (magnified, steeply at the rim), easing to
    // flat glass in the middle.
    float t = bezel > 0 ? saturate(depth / bezel) : 1;
    float u = 1 - t;
    float bend = refract * (1 - sqrt(saturate(1 - u * u)));
    float2 q = p - n * bend + h + origin;
    float3 c;
    if (bend > 0.4) {
        float ca = bend * 0.14;
        c = float3(frosted(q + n * ca).r, frosted(q).g, frosted(q - n * ca).b);
    } else {
        c = frosted(q);
    }

    // Vibrancy, then the body of the glass.
    float l = dot(c, float3(0.2126, 0.7152, 0.0722));
    c = lerp(l.xxx, c, 1 + 0.4 * strength);
    c = dark > 0.5 ? c * 0.86 : lerp(c, 1, 0.06);
    c = lerp(c, tint.rgb, tint.a);

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
    pad: [f32; 2],
}

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
    static FAILED: Cell<bool> = const { Cell::new(false) };
}

/// Whether new glass surfaces refract (`[general] glass_refraction`).
pub fn set_enabled(on: bool) {
    ENABLED.with(|e| e.set(on));
}

pub fn enabled() -> bool {
    ENABLED.with(|e| e.get()) && !FAILED.with(|f| f.get())
}

/// Hides (or shows again) a window from screenshots and screen capture.
pub fn exclude_from_capture(hwnd: HWND, exclude: bool) {
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, if exclude { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE });
    }
}

struct Monitor {
    handle: HMONITOR,
    origin: (i32, i32),
    size: SizeInt32,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
    /// Latest captured frame.
    frame: Option<ID3D11Texture2D>,
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
    monitors: Vec<Monitor>,
    panes: Vec<Weak<RefCell<PaneState>>>,
}

fn compile(entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Result<Vec<u8>> {
    let mut code = None;
    let mut errors = None;
    let r = unsafe {
        D3DCompile(
            SHADER.as_ptr() as _,
            SHADER.len(),
            s!("glass.hlsl"),
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

        let vs_code = compile(s!("vs"), s!("vs_4_0"))?;
        let ps_code = compile(s!("ps"), s!("ps_4_0"))?;
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
        Ok(Self {
            device,
            context,
            factory,
            winrt,
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            sampler: sampler.unwrap(),
            constants: constants.unwrap(),
            monitors: Vec::new(),
            panes: Vec::new(),
        })
    }

    /// The capture for `handle`, started on first use.
    fn monitor(&mut self, handle: HMONITOR) -> Option<usize> {
        if let Some(i) = self.monitors.iter().position(|m| m.handle == handle) {
            return Some(i);
        }
        match self.start_capture(handle) {
            Ok(m) => {
                self.monitors.push(m);
                Some(self.monitors.len() - 1)
            }
            Err(e) => {
                log!("glass capture failed: {e}");
                None
            }
        }
    }

    fn start_capture(&self, handle: HMONITOR) -> Result<Monitor> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(handle)? };
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::Create(&self.winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(false);
        // No yellow capture border (Windows 11).
        let _ = session.SetIsBorderRequired(false);
        let key = handle.0 as isize;
        pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            on_frame(key);
            Ok(())
        }))?;
        session.StartCapture()?;
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        unsafe {
            let _ = GetMonitorInfoW(handle, &mut info);
        }
        Ok(Monitor {
            handle,
            origin: (info.rcMonitor.left, info.rcMonitor.top),
            size,
            pool,
            session,
            _item: item,
            frame: None,
        })
    }

    /// Takes the newest frame for a monitor; true when there was one.
    fn take_frame(&mut self, key: isize) -> bool {
        let Some(m) = self.monitors.iter_mut().find(|m| m.handle.0 as isize == key) else { return false };
        let Ok(frame) = m.pool.TryGetNextFrame() else { return false };
        let result = (|| -> Result<()> {
            let size = frame.ContentSize()?;
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut desc) };
            let fits = m.frame.as_ref().is_some_and(|f| {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                unsafe { f.GetDesc(&mut d) };
                d.Width == desc.Width && d.Height == desc.Height
            });
            if !fits {
                desc.BindFlags = 0;
                desc.MiscFlags = 0;
                desc.CPUAccessFlags = 0;
                desc.Usage = D3D11_USAGE_DEFAULT;
                let mut copy = None;
                unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut copy))? };
                m.frame = copy;
            }
            if let Some(f) = &m.frame {
                unsafe { self.context.CopyResource(f, &tex) };
            }
            if size.Width != m.size.Width || size.Height != m.size.Height {
                // Resolution changed: the next frames come at the new size.
                m.size = size;
                m.pool.Recreate(&self.winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
                let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                unsafe {
                    let _ = GetMonitorInfoW(m.handle, &mut info);
                }
                m.origin = (info.rcMonitor.left, info.rcMonitor.top);
            }
            Ok(())
        })();
        let _ = frame.Close();
        result.is_ok()
    }

    /// Stops captures no live pane needs.
    fn prune(&mut self) {
        self.panes.retain(|p| p.strong_count() > 0);
        let used: Vec<isize> = self
            .panes
            .iter()
            .filter_map(|p| p.upgrade())
            .filter_map(|p| p.try_borrow().ok().map(|s| s.monitor.0 as isize))
            .collect();
        self.monitors.retain(|m| {
            let keep = used.contains(&(m.handle.0 as isize));
            if !keep {
                let _ = m.session.Close();
                let _ = m.pool.Close();
            }
            keep
        });
    }

    fn render(&mut self, pane: &mut PaneState) -> Result<()> {
        if pane.size.0 == 0 || pane.size.1 == 0 {
            return Ok(());
        }
        let monitor = unsafe { MonitorFromWindow(pane.hwnd, MONITOR_DEFAULTTONEAREST) };
        if monitor != pane.monitor {
            pane.monitor = monitor;
            self.prune();
        }
        let Some(mi) = self.monitor(monitor) else { return Ok(()) };
        let m = &self.monitors[mi];
        let Some(frame) = m.frame.clone() else { return Ok(()) };

        // Region behind the pane, plus a margin, in monitor-local texels.
        let mut wr = RECT::default();
        unsafe { GetWindowRect(pane.hwnd, &mut wr)? };
        let (px, py) = (wr.left - m.origin.0, wr.top - m.origin.1);
        let (w, h) = (pane.size.0 as i32, pane.size.1 as i32);
        let x0 = (px - MARGIN).clamp(0, m.size.Width);
        let y0 = (py - MARGIN).clamp(0, m.size.Height);
        let x1 = (px + w + MARGIN).clamp(0, m.size.Width);
        let y1 = (py + h + MARGIN).clamp(0, m.size.Height);
        if x1 - x0 < 1 || y1 - y0 < 1 {
            return Ok(());
        }
        let src_size = ((x1 - x0) as u32, (y1 - y0) as u32);
        if pane.src.as_ref().map(|s| s.2) != Some(src_size) {
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
            let (mut tex, mut srv) = (None, None);
            unsafe {
                self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                self.device.CreateShaderResourceView(tex.as_ref().unwrap(), None, Some(&mut srv))?;
            }
            pane.src = Some((tex.unwrap(), srv.unwrap(), src_size));
        }
        let (tex, srv, _) = pane.src.as_ref().unwrap();

        let dpi = unsafe { GetDpiForWindow(pane.hwnd) }.max(96) as f32;
        let scale = dpi / 96.0;
        let r = pane.radius.min(w as f32 / 2.0).min(h as f32 / 2.0);
        let flat = r < 0.5;
        let bezel = if flat { 0.0 } else { (h.min(w) as f32 * 0.45).min(20.0 * scale).max(r * 0.6) };
        let tint = pane.tint;
        let subtle = pane.style == Style::Subtle;
        let consts = Constants {
            size: [w as f32, h as f32],
            origin: [(px - x0) as f32, (py - y0) as f32],
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
            pad: [0.0; 2],
        };

        unsafe {
            let bx = D3D11_BOX { left: x0 as u32, top: y0 as u32, front: 0, right: x1 as u32, bottom: y1 as u32, back: 1 };
            self.context.CopySubresourceRegion(tex, 0, 0, 0, 0, &frame, 0, Some(&bx));
            self.context.GenerateMips(srv);
            self.context.UpdateSubresource(&self.constants, 0, None, &consts as *const _ as _, 0, 0);

            let back: ID3D11Texture2D = pane.swapchain.GetBuffer(0)?;
            let mut rtv = None;
            self.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            let c = &self.context;
            c.OMSetRenderTargets(Some(&[rtv.clone()]), None);
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
        pane.shown = true;
        Ok(())
    }
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
        Some(f(e.as_mut().unwrap()))
    })
}

fn on_frame(key: isize) {
    with_engine(|engine| {
        engine.prune();
        if !engine.take_frame(key) {
            return;
        }
        let panes: Vec<_> = engine.panes.iter().filter_map(|p| p.upgrade()).collect();
        for pane in panes {
            let Ok(mut state) = pane.try_borrow_mut() else { continue };
            if state.monitor.0 as isize == key || !state.shown {
                if let Err(e) = engine.render(&mut state) {
                    log!("glass render failed: {e}");
                }
            }
        }
    });
}

struct PaneState {
    hwnd: HWND,
    swapchain: IDXGISwapChain1,
    size: (u32, u32),
    radius: f32,
    tint: Color,
    style: Style,
    monitor: HMONITOR,
    src: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, (u32, u32))>,
    /// Has presented at least once.
    shown: bool,
}

/// One refracting glass pane: a swap chain the caller puts in its visual tree.
pub struct Pane(Rc<RefCell<PaneState>>);

impl Pane {
    /// Excludes `hwnd` from capture and returns the pane with its swap chain.
    pub fn new(hwnd: HWND, tint: Color, style: Style) -> Option<(Self, IDXGISwapChain1)> {
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
        exclude_from_capture(hwnd, true);
        let state = Rc::new(RefCell::new(PaneState {
            hwnd,
            swapchain: swapchain.clone(),
            size: (0, 0),
            radius: 0.0,
            tint,
            style,
            monitor: unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) },
            src: None,
            shown: false,
        }));
        with_engine(|engine| {
            engine.panes.push(Rc::downgrade(&state));
            let monitor = state.borrow().monitor;
            engine.monitor(monitor);
        });
        Some((Self(state), swapchain))
    }

    /// Size and corner radius in physical pixels.
    pub fn set_shape(&self, w: f32, h: f32, radius: f32) {
        let mut s = self.0.borrow_mut();
        let size = (w.round().max(0.0) as u32, h.round().max(0.0) as u32);
        s.radius = radius;
        if size != s.size && size.0 > 0 && size.1 > 0 {
            s.size = size;
            unsafe {
                if let Err(e) = s.swapchain.ResizeBuffers(2, size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0)) {
                    log!("glass resize failed: {e}");
                }
            }
        }
    }

    pub fn set_tint(&self, tint: Color) {
        self.0.borrow_mut().tint = tint;
        self.render();
    }

    /// Re-renders now (after the window moved or reshaped).
    pub fn render(&self) {
        let Ok(mut s) = self.0.try_borrow_mut() else { return };
        with_engine(|engine| {
            if let Err(e) = engine.render(&mut s) {
                log!("glass render failed: {e}");
            }
        });
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // The capture itself stops on the next frame, once `prune` sees no pane on its monitor.
        exclude_from_capture(self.0.borrow().hwnd, false);
    }
}
