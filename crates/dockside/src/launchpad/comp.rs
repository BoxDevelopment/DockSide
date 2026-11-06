//! GPU layers for Launchpad: Windows.UI.Composition sprites whose content is
//! drawn with Direct2D on the graphics card. Each is drawn once and then just
//! moved, scaled and faded by the compositor, so full-screen animation costs
//! a few property changes a frame instead of repainting the screen.

use std::cell::RefCell;
use std::rc::Rc;

use windows::core::{Interface, Result};
use windows::Foundation::Numerics::{Vector2, Vector3};
use windows::Foundation::Size;
use windows::Graphics::DirectX::{DirectXAlphaMode, DirectXPixelFormat};
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, POINT};
use windows::Win32::Graphics::Direct2D::{ID2D1Device, ID2D1DeviceContext, ID2D1Factory1};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Composition::{ICompositionDrawingSurfaceInterop, ICompositorInterop};
use windows::UI::Composition::{
    CompositionBitmapInterpolationMode, CompositionDrawingSurface, CompositionGraphicsDevice, Compositor, SpriteVisual,
};

use crate::gfx::{Gfx, Painter};

pub struct Gpu {
    pub compositor: Compositor,
    graphics: CompositionGraphicsDevice,
    _d2d: ID2D1Device,
}

thread_local! {
    static GPU: RefCell<Option<Rc<Gpu>>> = const { RefCell::new(None) };
}

/// The shared GPU device for Launchpad's layers (made on first use).
pub fn gpu(gfx: &Gfx) -> Result<Rc<Gpu>> {
    if let Some(g) = GPU.with(|g| g.borrow().clone()) {
        return Ok(g);
    }
    let compositor = crate::win::composition::compositor().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL))?;
    let mut device: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut device), None, None)?;
    }
    let device = device.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;
    let dxgi: IDXGIDevice = device.cast()?;
    let factory: ID2D1Factory1 = gfx.d2d.cast()?;
    let d2d = unsafe { factory.CreateDevice(&dxgi)? };
    let interop: ICompositorInterop = compositor.cast()?;
    let graphics = unsafe { interop.CreateGraphicsDevice(&d2d)? };
    let g = Rc::new(Gpu { compositor, graphics, _d2d: d2d });
    GPU.with(|s| *s.borrow_mut() = Some(g.clone()));
    Ok(g)
}

/// Lets go of the GPU device once nothing uses it (Launchpad closed): it
/// holds tens of megabytes of driver memory. The next Launchpad makes a new one.
pub fn release() {
    GPU.with(|g| *g.borrow_mut() = None);
}

/// A sprite showing a drawing surface. Positions and sizes are in points;
/// the visual tree itself works in physical pixels.
pub struct Layer {
    pub visual: SpriteVisual,
    surface: CompositionDrawingSurface,
    scale: f32,
    pub w: f32,
    pub h: f32,
}

impl Layer {
    pub fn new(gpu: &Gpu, w: f32, h: f32, scale: f32) -> Result<Layer> {
        let px = Size { Width: (w * scale).ceil().max(1.0), Height: (h * scale).ceil().max(1.0) };
        let surface = gpu.graphics.CreateDrawingSurface(px, DirectXPixelFormat::B8G8R8A8UIntNormalized, DirectXAlphaMode::Premultiplied)?;
        let brush = gpu.compositor.CreateSurfaceBrushWithSurface(&surface)?;
        brush.SetBitmapInterpolationMode(CompositionBitmapInterpolationMode::Linear)?;
        let visual = gpu.compositor.CreateSpriteVisual()?;
        visual.SetBrush(&brush)?;
        visual.SetSize(Vector2 { X: px.Width, Y: px.Height })?;
        Ok(Layer { visual, surface, scale, w, h })
    }

    /// Stretches the layer over `w` × `h` points (its pixels stay as drawn).
    pub fn set_size(&self, w: f32, h: f32) {
        let _ = self.visual.SetSize(Vector2 { X: w * self.scale, Y: h * self.scale });
    }

    /// Resizes the surface itself (its content is lost: draw again).
    pub fn resize(&mut self, w: f32, h: f32) {
        if (w - self.w).abs() < 0.5 && (h - self.h).abs() < 0.5 {
            return;
        }
        self.w = w;
        self.h = h;
        let px = SizeInt32 { Width: (w * self.scale).ceil().max(1.0) as i32, Height: (h * self.scale).ceil().max(1.0) as i32 };
        let _ = self.surface.Resize(px);
        let _ = self.visual.SetSize(Vector2 { X: px.Width as f32, Y: px.Height as f32 });
    }

    /// Redraws the whole surface (cleared first), in points.
    pub fn draw(&self, gfx: &Gfx, f: impl FnOnce(&Painter)) {
        let r = (|| -> Result<()> {
            let interop: ICompositionDrawingSurfaceInterop = self.surface.cast()?;
            let mut offset = POINT::default();
            let ctx: ID2D1DeviceContext = unsafe { interop.BeginDraw(None, &mut offset)? };
            let s = self.scale;
            unsafe {
                ctx.SetDpi(96.0 * s, 96.0 * s);
                ctx.SetTransform(&windows::Foundation::Numerics::Matrix3x2::translation(offset.x as f32 / s, offset.y as f32 / s));
                ctx.Clear(Some(&crate::geom::Color::TRANSPARENT.d2d()));
            }
            let painted = crate::gfx::paint_context(gfx, &ctx, f);
            unsafe { interop.EndDraw()? };
            painted
        })();
        if let Err(e) = r {
            log!("launchpad layer draw failed: {e}");
        }
    }

    pub fn set_offset(&self, x: f32, y: f32) {
        let _ = self.visual.SetOffset(Vector3 { X: (x * self.scale).round(), Y: (y * self.scale).round(), Z: 0.0 });
    }

    pub fn set_opacity(&self, a: f32) {
        let _ = self.visual.SetOpacity(a.clamp(0.0, 1.0));
    }

    /// Scales about a point given in the layer's own points.
    pub fn set_scale(&self, k: f32, cx: f32, cy: f32) {
        let _ = self.visual.SetCenterPoint(Vector3 { X: cx * self.scale, Y: cy * self.scale, Z: 0.0 });
        let _ = self.visual.SetScale(Vector3 { X: k, Y: k, Z: 1.0 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GPU device, a drawing surface and a draw through Painter all work
    /// (no window: nothing appears on screen).
    #[test]
    fn draws_a_layer() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let gfx = Gfx::new().unwrap();
        let gpu = gpu(&gfx).expect("gpu");
        let layer = Layer::new(&gpu, 200.0, 100.0, 1.5).expect("layer");
        let mut drew = false;
        layer.draw(&gfx, |p| {
            p.fill_round(crate::geom::Rect::new(10.0, 10.0, 80.0, 40.0), 8.0, crate::geom::Color::WHITE);
            drew = true;
        });
        assert!(drew);
    }
}
