//! True rounded acrylic via Windows.UI.Composition.
//!
//! A `DesktopWindowTarget` on the backdrop window hosts a small visual tree:
//! a host-backdrop brush (DWM's blurred view of what's behind the window,
//! the same source WinUI's Acrylic uses) under a tint layer, both clipped to
//! an antialiased rounded rectangle of any radius.
//!
//! Liquid Glass adds two layers on top: a soft vertical sheen and a specular
//! rim whose brightness sweeps around the edge, as if lit from the top-left.
//! When refraction is available (see `glass`), a swap chain rendered by its
//! shader covers all of that; the layers below stay as the fallback until the
//! first frame arrives.

use std::cell::Cell;
use std::ffi::c_void;

use windows::core::{Interface, Result};
use windows::Foundation::Numerics::{Vector2, Vector3};
use windows::System::DispatcherQueueController;
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_HOSTBACKDROPBRUSH};
use windows::Win32::System::WinRT::Composition::{ICompositorDesktopInterop, ICompositorInterop};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
};
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionColorBrush, CompositionLinearGradientBrush, CompositionRoundedRectangleGeometry, Compositor,
    ContainerVisual, ShapeVisual, SpriteVisual,
};

use crate::geom::Color;
use crate::win::glass::{Pane, Source, Style};

struct Shared {
    compositor: Compositor,
    _queue: DispatcherQueueController,
}

thread_local! {
    static SHARED: Option<Shared> = init().map_err(|e| log!("composition unavailable: {e}")).ok();
    /// Whether `SHARED` was ever touched, so `shutdown` doesn't start it.
    static STARTED: Cell<bool> = const { Cell::new(false) };
}

/// Closes the compositor and its dispatcher queue on quit, pumping messages
/// until the queue has drained. Left to process exit, they're torn down after
/// the UI thread is gone, which can hang the process.
pub fn shutdown() {
    use windows::Foundation::AsyncStatus;
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};
    if !STARTED.with(|s| s.get()) {
        return;
    }
    SHARED.with(|shared| {
        let Some(shared) = shared else { return };
        let _ = shared.compositor.Close();
        let Ok(op) = shared._queue.ShutdownQueueAsync() else { return };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while op.Status().is_ok_and(|s| s == AsyncStatus::Started) && std::time::Instant::now() < deadline {
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
}

fn init() -> Result<Shared> {
    STARTED.with(|s| s.set(true));
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

fn gradient(compositor: &Compositor, from: Vector2, to: Vector2, stops: &[(f32, Color)]) -> Result<CompositionLinearGradientBrush> {
    let brush = compositor.CreateLinearGradientBrush()?;
    brush.SetStartPoint(from)?;
    brush.SetEndPoint(to)?;
    let collection = brush.ColorStops()?;
    for &(offset, color) in stops {
        collection.Append(&compositor.CreateColorGradientStopWithOffsetAndColor(offset, ui_color(color))?)?;
    }
    Ok(brush)
}

/// Rim width in physical pixels.
const RIM: f32 = 1.25;

/// The Liquid Glass sheen and rim.
struct Glass {
    rim: CompositionRoundedRectangleGeometry,
    _layers: (SpriteVisual, ShapeVisual),
}

impl Glass {
    fn new(compositor: &Compositor, dark: bool, style: Style) -> Result<Self> {
        let white = |a: f32| Color::rgba(255, 255, 255, a);
        // Dark glass catches less light; subtle glass less still.
        let k = if dark { 0.6 } else { 1.0 } * if style == Style::Subtle { 0.45 } else { 1.0 };

        let sheen = compositor.CreateSpriteVisual()?;
        sheen.SetBrush(&gradient(
            compositor,
            Vector2 { X: 0.5, Y: 0.0 },
            Vector2 { X: 0.5, Y: 1.0 },
            &[(0.0, white(0.20 * k)), (0.45, white(0.03 * k)), (0.8, white(0.0)), (1.0, white(0.07 * k))],
        )?)?;
        sheen.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;

        let rim = compositor.CreateRoundedRectangleGeometry()?;
        let stroke = compositor.CreateSpriteShapeWithGeometry(&rim)?;
        stroke.SetStrokeThickness(RIM)?;
        stroke.SetStrokeBrush(&gradient(
            compositor,
            Vector2 { X: 0.0, Y: 0.0 },
            Vector2 { X: 1.0, Y: 1.0 },
            &[(0.0, white(0.75 * k)), (0.3, white(0.22 * k)), (0.6, white(0.10 * k)), (1.0, white(0.45 * k))],
        )?)?;
        let rim_layer = compositor.CreateShapeVisual()?;
        rim_layer.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        rim_layer.Shapes()?.Append(&stroke)?;

        Ok(Self { rim, _layers: (sheen, rim_layer) })
    }
}

pub struct CompositionBackdrop {
    _target: DesktopWindowTarget,
    shape: ContainerVisual,
    geometry: CompositionRoundedRectangleGeometry,
    tint: CompositionColorBrush,
    glass: Option<Glass>,
    refraction: Option<(Pane, SpriteVisual)>,
    _layers: (SpriteVisual, SpriteVisual),
}

impl CompositionBackdrop {
    /// `hwnd` should be created with `WS_EX_NOREDIRECTIONBITMAP`. `glass` adds
    /// Liquid Glass over the tint: refraction (of `source`) when available, else sheen and rim.
    pub fn new(hwnd: HWND, tint: Color, glass: Option<Style>, source: Source) -> Result<Self> {
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
        let style = glass;
        let glass = if let Some(style) = style {
            let g = Glass::new(&compositor, tint.r + tint.g + tint.b < 1.5, style)?;
            children.InsertAtTop(&g._layers.0)?;
            children.InsertAtTop(&g._layers.1)?;
            Some(g)
        } else {
            None
        };
        let refraction = if let Some(style) = style.filter(|_| crate::win::glass::enabled()) {
            Pane::new(hwnd, tint, style, source).and_then(|(pane, swapchain)| {
                let layer = (|| -> Result<SpriteVisual> {
                    let interop: ICompositorInterop = compositor.cast()?;
                    let surface = unsafe { interop.CreateCompositionSurfaceForSwapChain(&swapchain)? };
                    let layer = compositor.CreateSpriteVisual()?;
                    layer.SetBrush(&compositor.CreateSurfaceBrushWithSurface(&surface)?)?;
                    layer.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
                    children.InsertAtTop(&layer)?;
                    Ok(layer)
                })();
                layer.map_err(|e| log!("glass layer failed: {e}")).ok().map(|l| (pane, l))
            })
        } else {
            None
        };
        root.Children()?.InsertAtTop(&shape)?;

        Ok(Self { _target: target, shape, geometry, tint: tint_brush, glass, refraction, _layers: (blur, tint_layer) })
    }

    /// Position, size and corner radius in window-local physical pixels.
    pub fn set_shape(&self, x: f32, y: f32, w: f32, h: f32, radius: f32) {
        let size = Vector2 { X: w.max(0.0), Y: h.max(0.0) };
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        let _ = self.shape.SetOffset(Vector3 { X: x, Y: y, Z: 0.0 });
        let _ = self.shape.SetSize(size);
        let _ = self.geometry.SetSize(size);
        let _ = self.geometry.SetCornerRadius(Vector2 { X: r, Y: r });
        if let Some(g) = &self.glass {
            // Inset by half the stroke so the whole rim sits inside the clip.
            // Square shapes (the full-width menu bar) get no rim, like macOS.
            let h = RIM / 2.0;
            let rim = if r > 0.0 { Vector2 { X: (size.X - RIM).max(0.0), Y: (size.Y - RIM).max(0.0) } } else { Vector2 { X: 0.0, Y: 0.0 } };
            let _ = g.rim.SetOffset(Vector2 { X: h, Y: h });
            let _ = g.rim.SetSize(rim);
            let _ = g.rim.SetCornerRadius(Vector2 { X: (r - h).max(0.0), Y: (r - h).max(0.0) });
        }
        if let Some((pane, _)) = &self.refraction {
            pane.set_shape(size.X, size.Y, r);
        }
    }

    pub fn set_tint(&self, tint: Color) {
        let _ = self.tint.SetColor(ui_color(tint));
        if let Some((pane, _)) = &self.refraction {
            pane.set_tint(tint);
        }
    }

    /// True when a pane refracting the live screen covers this backdrop, so its
    /// window (and anything drawn over it) must be hidden from capture.
    pub fn hidden_from_capture(&self) -> bool {
        self.refraction.as_ref().is_some_and(|(pane, _)| pane.hidden_from_capture())
    }

    /// Re-renders the refraction after the window moved or reshaped.
    pub fn refresh(&self) {
        if let Some((pane, _)) = &self.refraction {
            pane.render();
        }
    }

}
