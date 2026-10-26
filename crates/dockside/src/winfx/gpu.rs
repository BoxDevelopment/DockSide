//! The GPU side of window animations: one D3D11 device, a click-through
//! overlay that draws warped window images into a composition swap chain, and
//! the two ways of getting a window's pixels.
//!
//! - [`Grab`]: a DWM thumbnail of the window, hosted 1:1 in an off-screen window
//!   of ours, captured once with Windows.Graphics.Capture. DWM keeps a minimized
//!   window's last frame for its thumbnails, so this works after the window has
//!   already gone (WGC on the window itself delivers nothing once it's minimized).
//! - [`Capturer::follow`]: a throttled live capture of the focused window,
//!   keeping only its newest frame, for closes (once a window is destroyed
//!   there's nothing left to capture) and for minimizes to start instantly.
//!
//! Capture sessions start, and their frames arrive, on a capture thread: the
//! UI thread only copies a frame when an animation needs it. Nothing here runs
//! while no window is animating, apart from following the focused window.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use windows::core::{s, Interface, Result};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Foundation::Numerics::Vector2;
use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::SpriteVisual;
use windows::Win32::Foundation::{BOOL, HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Composition::{ICompositorDesktopInterop, ICompositorInterop};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::create_window;

/// Mesh rows: enough that the Genie's curved sides look smooth.
pub const ROWS: usize = 96;

const SHADER: &str = r#"
Texture2D src : register(t0);
SamplerState smp : register(s0);

cbuffer C : register(b0) {
    float2 target;   // overlay, px
    float2 texSize;  // window image, px
    float radius;    // window corner radius, image px
    float shadow;    // shadow opacity (0: none)
    float blur;      // shadow softness, image px
    float drop;      // shadow's downward offset, image px
    float hole;      // 1: no image yet (a thumbnail shows through), just the shadow
    float3 pad;
};

struct VI { float2 pos : POSITION; float2 uv : TEXCOORD0; float a : TEXCOORD1; };
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; float a : TEXCOORD1; };

V vs(VI i) {
    V o;
    o.pos = float4(i.pos / target * float2(2, -2) + float2(-1, 1), 0, 1);
    o.uv = i.uv;
    o.a = i.a;
    return o;
}

float sd(float2 p, float2 b, float r) {
    float2 q = abs(p) - b + r;
    return length(max(q, 0)) + min(max(q.x, q.y), 0) - r;
}

float4 ps(V i) : SV_Target {
    // The window is opaque inside its rounded corners (captured corner alpha
    // varies by app); the mesh extends past it for the shadow.
    float2 h = texSize * 0.5;
    float2 p = i.uv * texSize - h;
    float d = sd(p, h, radius);
    float m = saturate(0.5 - d / max(fwidth(d), 1e-3));
    // A soft drop shadow: strongest under the window, fading out over `blur`.
    float ds = sd(p - float2(0, drop), h, radius);
    float s = shadow * pow(saturate(1 - ds / blur), 2.2);
    float img = m * (1 - hole);
    float a = (img + s * (1 - m)) * i.a;
    return float4(src.Sample(smp, saturate(i.uv)).rgb * img * i.a, a);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub u: f32,
    pub v: f32,
    pub a: f32,
}

#[repr(C)]
struct Constants {
    target: [f32; 2],
    tex: [f32; 2],
    radius: f32,
    shadow: f32,
    blur: f32,
    drop: f32,
    hole: f32,
    pad: [f32; 3],
}

/// A window's drop shadow, in image pixels.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Shadow {
    pub strength: f32,
    pub blur: f32,
    pub offset: f32,
}

impl Shadow {
    pub const NONE: Shadow = Shadow { strength: 0.0, blur: 1.0, offset: 0.0 };

    /// How far it reaches past the window.
    pub fn margin(&self) -> f32 {
        if self.strength > 0.0 { self.blur + self.offset } else { 0.0 }
    }
}

pub struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    factory: IDXGIFactory2,
    winrt: IDirect3DDevice,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    sampler: ID3D11SamplerState,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    constants: ID3D11Buffer,
    vertices: ID3D11Buffer,
    indices: ID3D11Buffer,
    /// Bound when there's no image (the shader ignores it then).
    blank: ID3D11ShaderResourceView,
}

fn compile(entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Result<Vec<u8>> {
    let (mut code, mut errors) = (None, None);
    let r = unsafe {
        D3DCompile(SHADER.as_ptr() as _, SHADER.len(), s!("winfx.hlsl"), None, None, entry, target, 1 << 15, 0, &mut code, Some(&mut errors))
    };
    if let Err(e) = r {
        if let Some(b) = errors {
            let msg = unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) };
            log!("window animation shader: {}", String::from_utf8_lossy(msg));
        }
        return Err(e);
    }
    let b = code.unwrap();
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec())
}

impl Gpu {
    pub fn new() -> Result<Self> {
        let (mut device, mut context) = (None, None);
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
        // Capture frame pools use this device from the capture thread.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        let dxgi: IDXGIDevice = device.cast()?;
        let factory: IDXGIFactory2 = unsafe { dxgi.GetAdapter()?.GetParent()? };
        let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;

        let vs_code = compile(s!("vs"), s!("vs_4_0"))?;
        let ps_code = compile(s!("ps"), s!("ps_4_0"))?;
        let (mut vs, mut ps, mut layout, mut sampler, mut blend, mut raster) = (None, None, None, None, None, None);
        let (mut constants, mut vertices, mut indices) = (None, None, None);
        unsafe {
            device.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps))?;
            let element = |name, index, format, offset| D3D11_INPUT_ELEMENT_DESC {
                SemanticName: name,
                SemanticIndex: index,
                Format: format,
                AlignedByteOffset: offset,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                ..Default::default()
            };
            let elements = [
                element(s!("POSITION"), 0, DXGI_FORMAT_R32G32_FLOAT, 0),
                element(s!("TEXCOORD"), 0, DXGI_FORMAT_R32G32_FLOAT, 8),
                element(s!("TEXCOORD"), 1, DXGI_FORMAT_R32_FLOAT, 16),
            ];
            device.CreateInputLayout(&elements, &vs_code, Some(&mut layout))?;
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            device.CreateSamplerState(&desc, Some(&mut sampler))?;
            let mut desc = D3D11_BLEND_DESC::default();
            desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: BOOL(1),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            device.CreateBlendState(&desc, Some(&mut blend))?;
            let desc = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, DepthClipEnable: BOOL(1), ..Default::default() };
            device.CreateRasterizerState(&desc, Some(&mut raster))?;
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Constants>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            device.CreateBuffer(&desc, None, Some(&mut constants))?;
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: (ROWS * 2 * std::mem::size_of::<Vertex>()) as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            device.CreateBuffer(&desc, None, Some(&mut vertices))?;
            // Each pair of rows (left and right edge vertices) is a quad.
            let idx: Vec<u16> = (0..ROWS as u16 - 1).flat_map(|r| [2 * r, 2 * r + 1, 2 * r + 2, 2 * r + 1, 2 * r + 3, 2 * r + 2]).collect();
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: (idx.len() * 2) as u32,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_INDEX_BUFFER.0 as u32,
                ..Default::default()
            };
            let data = D3D11_SUBRESOURCE_DATA { pSysMem: idx.as_ptr() as _, ..Default::default() };
            device.CreateBuffer(&desc, Some(&data), Some(&mut indices))?;
        }
        let blank = unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: 1,
                Height: 1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let (mut tex, mut srv) = (None, None);
            device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            device.CreateShaderResourceView(&tex.unwrap(), None, Some(&mut srv))?;
            srv.unwrap()
        };
        Ok(Self {
            device,
            context,
            factory,
            winrt,
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            layout: layout.unwrap(),
            sampler: sampler.unwrap(),
            blend: blend.unwrap(),
            raster: raster.unwrap(),
            constants: constants.unwrap(),
            vertices: vertices.unwrap(),
            indices: indices.unwrap(),
            blank,
        })
    }

    /// Copies the top-left `w`Ã—`h` of `src` into a mipmapped texture, so the
    /// image stays smooth as it shrinks into the Dock.
    fn snapshot(&self, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<Snapshot> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 0,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
            ..Default::default()
        };
        let (mut tex, mut srv) = (None, None);
        unsafe {
            self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.unwrap();
            let area = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
            self.context.CopySubresourceRegion(&tex, 0, 0, 0, 0, src, 0, Some(&area));
            self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
            let srv = srv.unwrap();
            self.context.GenerateMips(&srv);
            Ok(Snapshot { srv, size: (w as f32, h as f32) })
        }
    }

    fn frame_snapshot(&self, frame: &Direct3D11CaptureFrame) -> Result<Snapshot> {
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let size = frame.ContentSize()?;
        let w = (size.Width.max(1) as u32).min(desc.Width);
        let h = (size.Height.max(1) as u32).min(desc.Height);
        self.snapshot(&tex, w, h)
    }
}

/// A still image of a window, ready to draw.
pub struct Snapshot {
    srv: ID3D11ShaderResourceView,
    pub size: (f32, f32),
}

/// One window image to draw this frame: rows of left/right edge vertices.
pub struct Mesh<'a> {
    /// None: just the shadow, around a hole for a stand-in thumbnail.
    pub image: Option<&'a Snapshot>,
    /// The image's size in pixels.
    pub size: (f32, f32),
    pub rows: &'a [Vertex],
    /// Corner radius in image pixels.
    pub radius: f32,
    pub shadow: Shadow,
}

/// Click-through, topmost window covering the animations, drawn by the GPU
/// through a composition swap chain.
pub struct Overlay {
    pub hwnd: HWND,
    pub rect: RECT,
    swapchain: IDXGISwapChain1,
    target_view: Option<ID3D11RenderTargetView>,
    shown: bool,
    _target: DesktopWindowTarget,
    _visual: SpriteVisual,
}

impl Overlay {
    pub fn new(gpu: &Gpu, rect: RECT) -> Result<Self> {
        let Some(compositor) = crate::win::composition::compositor() else {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL));
        };
        let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP | WS_EX_LAYERED | WS_EX_TRANSPARENT;
        let hwnd = create_window(ex, WS_POPUP, rect, HWND::default())?;
        let built = (|| -> Result<Self> {
            unsafe { SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA)? };
            let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w as u32,
                Height: h as u32,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swapchain = unsafe { gpu.factory.CreateSwapChainForComposition(&gpu.device, &desc, None)? };
            let interop: ICompositorDesktopInterop = compositor.cast()?;
            let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, false)? };
            let surface = unsafe { compositor.cast::<ICompositorInterop>()?.CreateCompositionSurfaceForSwapChain(&swapchain)? };
            let visual = compositor.CreateSpriteVisual()?;
            visual.SetBrush(&compositor.CreateSurfaceBrushWithSurface(&surface)?)?;
            visual.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
            target.SetRoot(&visual)?;
            Ok(Self { hwnd, rect, swapchain, target_view: None, shown: false, _target: target, _visual: visual })
        })();
        if built.is_err() {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        built
    }

    /// Moves and resizes the overlay (buffers are recreated).
    pub fn set_rect(&mut self, rect: RECT) -> Result<()> {
        if rect == self.rect {
            return Ok(());
        }
        self.target_view = None;
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        unsafe {
            self.swapchain.ResizeBuffers(0, w as u32, h as u32, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, rect.left, rect.top, w, h, SWP_NOACTIVATE);
        }
        self.rect = rect;
        Ok(())
    }

    /// Hides the overlay until it's needed again. Its size and buffers stay:
    /// resizing them when a window minimizes costs the first frame. It's
    /// cleared first, so showing it again can't flash the last animation.
    pub fn park(&mut self, gpu: &Gpu) {
        if self.shown {
            let _ = self.draw(gpu, &[]);
            self.shown = false;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }

    /// Draws `meshes` (vertices in screen pixels) over a clear background.
    pub fn draw(&mut self, gpu: &Gpu, meshes: &[Mesh]) -> Result<()> {
        let ctx = &gpu.context;
        if self.target_view.is_none() {
            let buffer: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0)? };
            let mut view = None;
            unsafe { gpu.device.CreateRenderTargetView(&buffer, None, Some(&mut view))? };
            self.target_view = view;
        }
        let (w, h) = ((self.rect.right - self.rect.left) as f32, (self.rect.bottom - self.rect.top) as f32);
        let (ox, oy) = (self.rect.left as f32, self.rect.top as f32);
        unsafe {
            ctx.ClearRenderTargetView(self.target_view.as_ref().unwrap(), &[0.0; 4]);
            ctx.OMSetRenderTargets(Some(&[self.target_view.clone()]), None);
            ctx.OMSetBlendState(&gpu.blend, None, 0xffff_ffff);
            ctx.RSSetState(&gpu.raster);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: w, Height: h, MaxDepth: 1.0, ..Default::default() }]));
            ctx.IASetInputLayout(&gpu.layout);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            let stride = std::mem::size_of::<Vertex>() as u32;
            ctx.IASetVertexBuffers(0, 1, Some(&Some(gpu.vertices.clone())), Some(&stride), Some(&0));
            ctx.IASetIndexBuffer(&gpu.indices, DXGI_FORMAT_R16_UINT, 0);
            ctx.VSSetShader(&gpu.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(gpu.constants.clone())]));
            ctx.PSSetShader(&gpu.ps, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(gpu.constants.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(gpu.sampler.clone())]));
            for mesh in meshes {
                // Two vertices (left and right edge) per row.
                let rows = (mesh.rows.len() / 2).min(ROWS);
                if rows < 2 {
                    continue;
                }
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&gpu.vertices, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
                let out = std::slice::from_raw_parts_mut(mapped.pData as *mut Vertex, rows * 2);
                for (o, v) in out.iter_mut().zip(mesh.rows) {
                    *o = Vertex { x: v.x - ox, y: v.y - oy, ..*v };
                }
                ctx.Unmap(&gpu.vertices, 0);
                let sh = mesh.shadow;
                let c = Constants {
                    target: [w, h],
                    tex: [mesh.size.0, mesh.size.1],
                    radius: mesh.radius,
                    shadow: sh.strength,
                    blur: sh.blur,
                    drop: sh.offset,
                    hole: mesh.image.is_none() as u32 as f32,
                    pad: [0.0; 3],
                };
                ctx.UpdateSubresource(&gpu.constants, 0, None, &c as *const _ as _, 0, 0);
                let srv = mesh.image.map_or(&gpu.blank, |i| &i.srv);
                ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
                ctx.DrawIndexed(((rows - 1) * 6) as u32, 0, 0);
            }
            ctx.PSSetShaderResources(0, Some(&[None]));
            self.swapchain.Present(0, DXGI_PRESENT(0)).ok()?;
            // Shown only once there's a frame: never a blank (or black) one.
            if !self.shown {
                self.shown = true;
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
        Ok(())
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// A DWM thumbnail of `source` in `host`, `dest` in host client pixels.
pub fn thumbnail(host: HWND, source: HWND, dest: RECT) -> Option<isize> {
    unsafe {
        let id = DwmRegisterThumbnail(host, source).ok()?;
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY | DWM_TNP_SOURCECLIENTAREAONLY,
            rcDestination: dest,
            opacity: 255,
            fVisible: BOOL(1),
            fSourceClientAreaOnly: BOOL(0),
            ..Default::default()
        };
        if DwmUpdateThumbnailProperties(id, &props).is_err() {
            let _ = DwmUnregisterThumbnail(id);
            return None;
        }
        Some(id)
    }
}

/// The size DWM would draw `source` at (its last frame, even when minimized).
pub fn thumbnail_size(host: HWND, source: HWND) -> Option<(i32, i32)> {
    unsafe {
        let id = DwmRegisterThumbnail(host, source).ok()?;
        let size = DwmQueryThumbnailSourceSize(id);
        let _ = DwmUnregisterThumbnail(id);
        size.ok().filter(|s| s.cx > 0 && s.cy > 0).map(|s| (s.cx, s.cy))
    }
}

pub fn drop_thumbnail(id: isize) {
    unsafe {
        let _ = DwmUnregisterThumbnail(id);
    }
}

fn start_session(device: &IDirect3DDevice, item: &GraphicsCaptureItem, size: SizeInt32) -> Result<(Direct3D11CaptureFramePool, GraphicsCaptureSession)> {
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
    let session = pool.CreateCaptureSession(item)?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    // No yellow capture border (Windows 11).
    let _ = session.SetIsBorderRequired(false);
    Ok((pool, session))
}

fn capture_item(hwnd: isize) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    unsafe { interop.CreateForWindow(HWND(hwnd as _)) }
}

/// The device, for the capture thread: it's multithread-protected.
struct FreeThreaded(IDirect3DDevice);
unsafe impl Send for FreeThreaded {}
unsafe impl Sync for FreeThreaded {}
impl FreeThreaded {
    fn get(&self) -> &IDirect3DDevice {
        &self.0
    }
}

/// A followed window's newest frame, where it was on screen, and whether it was maximized.
struct Kept {
    hwnd: isize,
    frame: Direct3D11CaptureFrame,
    rect: RECT,
    zoomed: bool,
    /// When DWM composed it (QPC, 100 ns).
    at: i64,
}

/// The last two followed windows' frames: a window that closes or minimizes
/// hands the focus on, and the next window's first frame mustn't push it out.
type Recent = Arc<Mutex<Vec<Kept>>>;

/// A one-off capture's result, filled in by the capture thread.
#[derive(Default)]
struct GrabSlot {
    frame: Option<Direct3D11CaptureFrame>,
    capture: Option<(Direct3D11CaptureFramePool, GraphicsCaptureSession)>,
    failed: bool,
    /// The `Grab` is gone: close the capture as soon as it exists.
    dropped: bool,
}

enum Cmd {
    /// Follow this window (0: none).
    Follow(isize),
    Grab(isize, SizeInt32, Arc<Mutex<GrabSlot>>),
    Station(isize, SizeInt32, Arc<Mutex<StationSlot>>),
}

/// Window captures, on their own thread: starting a capture session takes
/// ~50 ms, which the UI thread can't spare, and frames arrive there too.
pub struct Capturer {
    tx: Sender<Cmd>,
    recent: Recent,
}

impl Capturer {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        let (tx, rx) = channel::<Cmd>();
        let recent = Recent::default();
        let (device, kept) = (FreeThreaded(gpu.winrt.clone()), recent.clone());
        std::thread::Builder::new()
            .name("window-capture".into())
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                let mut follow: Option<(Direct3D11CaptureFramePool, GraphicsCaptureSession)> = None;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Follow(hwnd) => {
                            if let Some((pool, session)) = follow.take() {
                                let _ = session.Close();
                                let _ = pool.Close();
                            }
                            if hwnd != 0 {
                                follow = start_follow(&device, hwnd, &kept).map_err(|e| log!("window capture failed: {e}")).ok();
                            }
                        }
                        Cmd::Grab(host, size, slot) => {
                            let started = start_grab(device.get(), host, size, &slot);
                            let mut s = slot.lock().unwrap();
                            match started {
                                Ok((pool, session)) if s.dropped => {
                                    let _ = session.Close();
                                    let _ = pool.Close();
                                }
                                Ok(capture) => s.capture = Some(capture),
                                Err(e) => {
                                    log!("window capture failed: {e}");
                                    s.failed = true;
                                }
                            }
                        }
                        Cmd::Station(host, size, slot) => {
                            let started = start_station(device.get(), host, size, &slot);
                            let mut s = slot.lock().unwrap();
                            match started {
                                Ok(capture) => s.capture = Some(capture),
                                Err(e) => {
                                    log!("window capture station failed: {e}");
                                    s.failed = true;
                                }
                            }
                        }
                    }
                }
                if let Some((pool, session)) = follow {
                    let _ = session.Close();
                    let _ = pool.Close();
                }
            })
            .map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;
        Ok(Self { tx, recent })
    }

    /// Keeps the newest frame of `hwnd` (none: stop), at most every `FOLLOW_INTERVAL`.
    pub fn follow(&self, hwnd: Option<HWND>) {
        let _ = self.tx.send(Cmd::Follow(hwnd.map_or(0, |h| h.0 as isize)));
    }

    /// `hwnd`'s last frame as an image, with its screen rect and maximized state.
    pub fn take(&self, gpu: &Gpu, hwnd: HWND) -> Option<(Result<Snapshot>, RECT, bool)> {
        let mut recent = self.recent.lock().unwrap();
        let i = recent.iter().position(|k| k.hwnd == hwnd.0 as isize)?;
        let k = recent.remove(i);
        drop(recent);
        Some((gpu.frame_snapshot(&k.frame), k.rect, k.zoomed))
    }

    /// Where `hwnd` was when its last frame arrived.
    pub fn last_rect(&self, hwnd: HWND) -> Option<RECT> {
        self.recent.lock().unwrap().iter().find(|k| k.hwnd == hwnd.0 as isize).map(|k| k.rect)
    }

    /// Whether `hwnd` has put a frame on screen since `since` (`qpc_now`):
    /// a restored window really is showing again.
    pub fn shown_since(&self, hwnd: HWND, since: i64) -> bool {
        self.recent.lock().unwrap().iter().any(|k| k.hwnd == hwnd.0 as isize && k.at >= since)
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        // The thread closes its capture and ends once the channel does.
        let _ = self.tx.send(Cmd::Follow(0));
    }
}

/// How often the focused window's frame is refreshed, in 100 ns units. 250 ms is
/// fresh enough for a close or minimize; at 100 ms DWM's copying of a busy window
/// added about 1% GPU load on an Iris Xe.
const FOLLOW_INTERVAL: i64 = 250 * 10_000;

fn start_follow(device: &FreeThreaded, hwnd: isize, kept: &Recent) -> Result<(Direct3D11CaptureFramePool, GraphicsCaptureSession)> {
    let item = capture_item(hwnd)?;
    // Room for the window to grow to its monitor (maximizing, snapping): a pool that has
    // to be recreated drops that frame, and a window that then sits still sends no more.
    let size = item.Size()?;
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(MonitorFromWindow(HWND(hwnd as _), MONITOR_DEFAULTTONEAREST), &mut info);
    }
    let m = info.rcMonitor;
    let size = SizeInt32 { Width: size.Width.max(m.right - m.left), Height: size.Height.max(m.bottom - m.top) };
    let (pool, session) = start_session(device.get(), &item, size)?;
    // 24H2+: DWM itself skips frames, so a busy window (video) costs little.
    let _ = session.SetMinUpdateInterval(TimeSpan { Duration: FOLLOW_INTERVAL });
    let (kept, device) = (kept.clone(), FreeThreaded(device.get().clone()));
    let pooled = Mutex::new(size);
    pool.FrameArrived(&TypedEventHandler::new(move |pool: &Option<Direct3D11CaptureFramePool>, _| {
        let Some(pool) = pool else { return Ok(()) };
        let Ok(frame) = pool.TryGetNextFrame() else { return Ok(()) };
        let content = frame.ContentSize()?;
        let mut size = pooled.lock().unwrap();
        if content.Width > size.Width || content.Height > size.Height {
            // Outgrew the pool (cropped): later frames come whole.
            *size = SizeInt32 { Width: size.Width.max(content.Width), Height: size.Height.max(content.Height) };
            let _ = pool.Recreate(device.get(), DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, *size);
            return Ok(());
        }
        drop(size);
        let window = HWND(hwnd as _);
        let Some(rect) = window_bounds(window) else { return Ok(()) };
        // A minimized (or minimizing) window's frames aren't the window, and one
        // that just lost the focus (as it minimizes or closes) has redrawn inactive:
        // Mica turns flat gray. The last focused frame is what was on screen.
        let fits = (rect.right - rect.left - content.Width).abs() <= 2 && (rect.bottom - rect.top - content.Height).abs() <= 2;
        if unsafe { IsIconic(window).as_bool() || GetForegroundWindow() != window } || !fits {
            return Ok(());
        }
        let zoomed = unsafe { IsZoomed(window).as_bool() };
        let mut kept = kept.lock().unwrap();
        kept.retain(|k| k.hwnd != hwnd);
        if kept.len() >= 2 {
            kept.remove(0);
        }
        let at = frame.SystemRelativeTime()?.Duration;
        kept.push(Kept { hwnd, frame, rect, zoomed, at });
        Ok(())
    }))?;
    session.StartCapture()?;
    Ok((pool, session))
}

fn start_grab(device: &IDirect3DDevice, host: isize, size: SizeInt32, slot: &Arc<Mutex<GrabSlot>>) -> Result<(Direct3D11CaptureFramePool, GraphicsCaptureSession)> {
    let item = capture_item(host)?;
    let (pool, session) = start_session(device, &item, size)?;
    let slot = slot.clone();
    pool.FrameArrived(&TypedEventHandler::new(move |pool: &Option<Direct3D11CaptureFramePool>, _| {
        if let Some(frame) = pool.as_ref().and_then(|p| p.TryGetNextFrame().ok()) {
            let mut s = slot.lock().unwrap();
            if s.frame.is_none() && !s.dropped {
                s.frame = Some(frame);
            }
        }
        Ok(())
    }))?;
    session.StartCapture()?;
    Ok((pool, session))
}

/// What the capture thread hands back from the station's capture.
#[derive(Default)]
struct StationSlot {
    /// The newest frame and when DWM composed it (QPC, 100 ns).
    frame: Option<(Direct3D11CaptureFrame, i64)>,
    capture: Option<(Direct3D11CaptureFramePool, GraphicsCaptureSession)>,
    failed: bool,
}

/// An off-screen window of ours that shows one DWM thumbnail at a time, with
/// a capture of it kept running: Windows' last composed frame of any window,
/// minimized or not and exactly as it was on screen, in a frame or two. With
/// no thumbnail up, nothing changes, so DWM sends nothing.
pub struct Station {
    host: HWND,
    slot: Arc<Mutex<StationSlot>>,
    /// The thumbnail up now, its size, and when it went up (QPC, 100 ns).
    showing: Option<(isize, (i32, i32), i64)>,
}

impl Station {
    pub fn new(capturer: &Capturer) -> Result<Self> {
        // Off every monitor: DWM still composes it, and WGC still captures it.
        let (vx, vy) = unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN)) };
        let (vw, vh) = unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) };
        let rect = RECT { left: vx - 300, top: vy, right: vx - 200, bottom: vy + 100 };
        let host = create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP, WS_POPUP, rect, HWND::default())?;
        unsafe {
            let _ = ShowWindow(host, SW_SHOWNOACTIVATE);
        }
        let slot = Arc::new(Mutex::new(StationSlot::default()));
        // Room for any window up to the whole desktop, so it never needs recreating.
        let _ = capturer.tx.send(Cmd::Station(host.0 as isize, SizeInt32 { Width: vw.max(1), Height: vh.max(1) }, slot.clone()));
        Ok(Self { host, slot, showing: None })
    }

    pub fn busy(&self) -> bool {
        self.showing.is_some()
    }

    /// Puts up `source`'s thumbnail; returns its size (as DWM keeps it).
    pub fn request(&mut self, source: HWND) -> Option<(i32, i32)> {
        self.cancel();
        if self.slot.lock().unwrap().failed {
            return None;
        }
        let size = thumbnail_size(self.host, source)?;
        unsafe {
            let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
            SetWindowPos(self.host, None, vx - size.0 - 200, vy, size.0, size.1, SWP_NOACTIVATE | SWP_NOZORDER).ok()?;
        }
        let thumb = thumbnail(self.host, source, RECT { left: 0, top: 0, right: size.0, bottom: size.1 })?;
        self.showing = Some((thumb, size, qpc_now()));
        Some(size)
    }

    /// The thumbnail's image, once a frame composed after it went up arrives.
    pub fn take(&mut self, gpu: &Gpu) -> Option<Result<Snapshot>> {
        let (_, size, since) = self.showing?;
        let mut s = self.slot.lock().unwrap();
        if s.failed {
            drop(s);
            self.cancel();
            return Some(Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)));
        }
        let fresh = s.frame.as_ref().is_some_and(|(f, t)| *t >= since && f.ContentSize().is_ok_and(|c| c.Width == size.0 && c.Height == size.1));
        if !fresh {
            return None;
        }
        let (frame, _) = s.frame.take().unwrap();
        drop(s);
        let image = gpu.frame_snapshot(&frame);
        self.cancel();
        Some(image)
    }

    /// Takes the thumbnail down.
    pub fn cancel(&mut self) {
        if let Some((thumb, ..)) = self.showing.take() {
            drop_thumbnail(thumb);
        }
    }
}

impl Drop for Station {
    fn drop(&mut self) {
        self.cancel();
        if let Some((pool, session)) = self.slot.lock().unwrap().capture.take() {
            let _ = session.Close();
            let _ = pool.Close();
        }
        unsafe {
            let _ = DestroyWindow(self.host);
        }
    }
}

fn start_station(device: &IDirect3DDevice, host: isize, size: SizeInt32, slot: &Arc<Mutex<StationSlot>>) -> Result<(Direct3D11CaptureFramePool, GraphicsCaptureSession)> {
    let item = capture_item(host)?;
    let (pool, session) = start_session(device, &item, size)?;
    let slot = slot.clone();
    pool.FrameArrived(&TypedEventHandler::new(move |pool: &Option<Direct3D11CaptureFramePool>, _| {
        if let Some(frame) = pool.as_ref().and_then(|p| p.TryGetNextFrame().ok()) {
            let t = frame.SystemRelativeTime()?.Duration;
            slot.lock().unwrap().frame = Some((frame, t));
        }
        Ok(())
    }))?;
    session.StartCapture()?;
    Ok((pool, session))
}

/// Now on the clock capture frames are stamped with (QPC, 100 ns units).
pub fn qpc_now() -> i64 {
    let (mut now, mut freq) = (0i64, 1i64);
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceCounter(&mut now);
        let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut freq);
    }
    (now as i128 * 10_000_000 / freq.max(1) as i128) as i64
}

/// One frame of a window (minimized or not), captured through a DWM thumbnail
/// hosted off screen by us.
pub struct Grab {
    /// The window's size as DWM keeps it (its last frame, when minimized).
    pub size: (i32, i32),
    host: HWND,
    thumb: isize,
    slot: Arc<Mutex<GrabSlot>>,
}

impl Grab {
    pub fn start(capturer: &Capturer, source: HWND) -> Result<Self> {
        // Off every monitor: DWM still composes it, and WGC still captures it.
        let (vx, vy) = unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN)) };
        let rect = RECT { left: vx - 300, top: vy, right: vx - 200, bottom: vy + 100 };
        let host = create_window(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP, WS_POPUP, rect, HWND::default())?;
        let started = (|| -> Result<Self> {
            let fail = || windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL);
            let size = thumbnail_size(host, source).ok_or_else(fail)?;
            unsafe {
                SetWindowPos(host, None, vx - size.0 - 200, vy, size.0, size.1, SWP_NOACTIVATE | SWP_NOZORDER)?;
                let _ = ShowWindow(host, SW_SHOWNOACTIVATE);
            }
            let thumb = thumbnail(host, source, RECT { left: 0, top: 0, right: size.0, bottom: size.1 }).ok_or_else(fail)?;
            let slot = Arc::new(Mutex::new(GrabSlot::default()));
            let _ = capturer.tx.send(Cmd::Grab(host.0 as isize, SizeInt32 { Width: size.0, Height: size.1 }, slot.clone()));
            Ok(Self { size, host, thumb, slot })
        })();
        if started.is_err() {
            unsafe {
                let _ = DestroyWindow(host);
            }
        }
        started
    }

    /// The image, once the first frame has arrived.
    pub fn take(&self, gpu: &Gpu) -> Option<Result<Snapshot>> {
        let mut s = self.slot.lock().unwrap();
        if s.failed {
            return Some(Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)));
        }
        let frame = s.frame.take()?;
        drop(s);
        Some(gpu.frame_snapshot(&frame))
    }
}

impl Drop for Grab {
    fn drop(&mut self) {
        let mut s = self.slot.lock().unwrap();
        s.dropped = true;
        s.frame = None;
        if let Some((pool, session)) = s.capture.take() {
            let _ = session.Close();
            let _ = pool.Close();
        }
        drop(s);
        drop_thumbnail(self.thumb);
        unsafe {
            let _ = DestroyWindow(self.host);
        }
    }
}

/// A window's visible bounds (no invisible resize borders), in screen pixels.
pub fn window_bounds(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as _, std::mem::size_of::<RECT>() as u32).ok()? };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

/// Turns DWM's own open/close/minimize transitions for `hwnd` off or back on.
pub fn set_transitions_disabled(hwnd: HWND, disabled: bool) -> bool {
    let v = BOOL(disabled as i32);
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, &v as *const _ as _, std::mem::size_of::<BOOL>() as u32).is_ok() }
}
