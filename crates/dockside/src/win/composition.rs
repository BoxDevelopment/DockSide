//! True rounded acrylic via Windows.UI.Composition.
//!
//! A `DesktopWindowTarget` on the backdrop window hosts a small visual tree:
//! a host-backdrop brush (DWM's blurred view of what's behind the window,
//! the same source WinUI's Acrylic uses) under a tint layer, both clipped to
//! an antialiased rounded rectangle of any radius.

use std::ffi::c_void;

use windows::core::{Interface, Result};
use windows::Foundation::Numerics::{Vector2, Vector3};
use windows::System::DispatcherQueueController;
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_HOSTBACKDROPBRUSH};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
};
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionColorBrush, CompositionRoundedRectangleGeometry, Compositor, ContainerVisual, SpriteVisual,
};

use crate::geom::Color;

struct Shared {
    compositor: Compositor,
    _queue: DispatcherQueueController,
}

thread_local! {
    static SHARED: Option<Shared> = init().map_err(|e| log!("composition unavailable: {e}")).ok();
}

fn init() -> Result<Shared> {
    let options = DispatcherQueueOptions {
        dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
        threadType: DQTYPE_THREAD_CURRENT,
        apartmentType: DQTAT_COM_NONE,
    };
    let queue = unsafe { CreateDispatcherQueueController(options)? };
    Ok(Shared { compositor: Compositor::new()?, _queue: queue })
}

fn ui_color(c: Color) -> windows::UI::Color {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    windows::UI::Color { A: b(c.a), R: b(c.r), G: b(c.g), B: b(c.b) }
}

pub struct CompositionBackdrop {
    _target: DesktopWindowTarget,
    shape: ContainerVisual,
    geometry: CompositionRoundedRectangleGeometry,
    tint: CompositionColorBrush,
    _layers: (SpriteVisual, SpriteVisual),
}

impl CompositionBackdrop {
    /// `hwnd` should be created with `WS_EX_NOREDIRECTIONBITMAP`.
    pub fn new(hwnd: HWND, tint: Color) -> Result<Self> {
        let compositor = SHARED.with(|s| s.as_ref().map(|s| s.compositor.clone()));
        let Some(compositor) = compositor else {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL));
        };
        unsafe {
            let on = BOOL(1);
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_HOSTBACKDROPBRUSH,
                &on as *const _ as *const c_void,
                std::mem::size_of::<BOOL>() as u32,
            )?;
        }
        let interop: ICompositorDesktopInterop = compositor.cast()?;
        let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, false)? };

        let root = compositor.CreateContainerVisual()?;
        root.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        target.SetRoot(&root)?;

        let geometry = compositor.CreateRoundedRectangleGeometry()?;
        let shape = compositor.CreateContainerVisual()?;
        shape.SetClip(&compositor.CreateGeometricClipWithGeometry(&geometry)?)?;

        let blur = compositor.CreateSpriteVisual()?;
        blur.SetBrush(&compositor.CreateHostBackdropBrush()?)?;
        blur.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;

        let tint_brush = compositor.CreateColorBrushWithColor(ui_color(tint))?;
        let tint_layer = compositor.CreateSpriteVisual()?;
        tint_layer.SetBrush(&tint_brush)?;
        tint_layer.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;

        let children = shape.Children()?;
        children.InsertAtTop(&blur)?;
        children.InsertAtTop(&tint_layer)?;
        root.Children()?.InsertAtTop(&shape)?;

        Ok(Self { _target: target, shape, geometry, tint: tint_brush, _layers: (blur, tint_layer) })
    }

    /// Position, size and corner radius in window-local physical pixels.
    pub fn set_shape(&self, x: f32, y: f32, w: f32, h: f32, radius: f32) {
        let size = Vector2 { X: w.max(0.0), Y: h.max(0.0) };
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        let _ = self.shape.SetOffset(Vector3 { X: x, Y: y, Z: 0.0 });
        let _ = self.shape.SetSize(size);
        let _ = self.geometry.SetSize(size);
        let _ = self.geometry.SetCornerRadius(Vector2 { X: r, Y: r });
    }

    pub fn set_tint(&self, tint: Color) {
        let _ = self.tint.SetColor(ui_color(tint));
    }

}
