//! True rounded acrylic via Windows.UI.Composition.
//!
//! A `DesktopWindowTarget` on the backdrop window hosts a small visual tree:
//! a host-backdrop brush (DWM's blurred view of what's behind the window,
//! the same source WinUI's Acrylic uses) under a tint layer, both clipped to
//! an antialiased rounded rectangle of any radius.
//!
//! DWM applies Composition property changes 50-260 ms late at times, while a
//! window's own moves and resizes land on time. So the shape takes its size
//! from the window and needs no Composition change when the window resizes
//! (the Dock magnifying, Spotlight growing into a panel): nine parts, fixed
//! rounded corners pinned to the window's corners, and edges and a middle that
//! stretch with it. Only a new corner radius changes Composition.
//!
//! Liquid Glass adds two layers on top: a soft vertical sheen and a specular
//! rim whose brightness sweeps around the edge, as if lit from the top-left.
//! When refraction is available (see `glass`), a swap chain rendered by its
//! shader covers all of that; the layers below stay as the fallback until its
//! first frame arrives, then they're hidden: DWM would otherwise go on
//! blurring what's behind the pane for layers no one can see.

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
    CompositionBrush, CompositionColorBrush, CompositionLinearGradientBrush, CompositionNineGridBrush,
    CompositionRoundedRectangleGeometry, Compositor, ContainerVisual, ShapeVisual, SpriteVisual,
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

/// The UI thread's compositor (for surfaces that build their own visual trees).
pub fn compositor() -> Option<Compositor> {
    SHARED.with(|s| s.as_ref().map(|s| s.compositor.clone()))
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

/// The Liquid Glass sheen (a brush, painted over each part of the shape) and rim.
struct Glass {
    sheen: CompositionLinearGradientBrush,
    rim: CompositionRoundedRectangleGeometry,
    rim_layer: ShapeVisual,
}

impl Glass {
    fn new(compositor: &Compositor, dark: bool, style: Style) -> Result<Self> {
        let white = |a: f32| Color::rgba(255, 255, 255, a);
        // Dark glass catches less light; subtle glass less still.
        let k = if dark { 0.6 } else { 1.0 } * if style == Style::Subtle { 0.45 } else { 1.0 };

        // Vertical, so it's seamless across the parts of the shape.
        let sheen = gradient(
            compositor,
            Vector2 { X: 0.5, Y: 0.0 },
            Vector2 { X: 0.5, Y: 1.0 },
            &[(0.0, white(0.20 * k)), (0.45, white(0.03 * k)), (0.8, white(0.0)), (1.0, white(0.07 * k))],
        )?;

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

        Ok(Self { sheen, rim, rim_layer })
    }
}

pub struct CompositionBackdrop {
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    shape: ContainerVisual,
    /// Nine parts, row by row: rounded corners of a fixed size pinned to the
    /// window's corners, edges and a middle that stretch (see the module docs).
    parts: Vec<ContainerVisual>,
    /// The corners' clips: top-left, top-right, bottom-left, bottom-right.
    clips: [CompositionRoundedRectangleGeometry; 4],
    /// The radius the parts are laid out for, and how far the shape runs past
    /// the window's bottom (the Dock sliding off a screen with one below).
    radius: Cell<Option<f32>>,
    overflow: Cell<f32>,
    /// The layers under the refraction are hidden (see `hide_fallback`).
    bare: Cell<bool>,
    tint: CompositionColorBrush,
    glass: Option<Glass>,
    /// The pane's swap chain, through a nine-grid so a frame rendered at an
    /// older width stretches only its straight middle, not its rounded ends.
    refraction: Option<(Pane, CompositionNineGridBrush)>,
    _layers: Vec<SpriteVisual>,
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

        let style = glass;
        let glass = style.map(|style| Glass::new(&compositor, tint.r + tint.g + tint.b < 1.5, style)).transpose()?;
        let tint_brush = compositor.CreateColorBrushWithColor(ui_color(tint))?;
        let mut brushes: Vec<CompositionBrush> = vec![compositor.CreateHostBackdropBrush()?.cast()?, tint_brush.cast()?];
        if let Some(g) = &glass {
            brushes.push(g.sheen.cast()?);
        }

        // Its size from the window (see the module docs).
        let shape = compositor.CreateContainerVisual()?;
        shape.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        let children = shape.Children()?;
        let mut parts = Vec::with_capacity(9);
        for row in 0..3 {
            for col in 0..3 {
                let part = compositor.CreateContainerVisual()?;
                let rel = |i: usize, mid: f32, end: f32| if i == 1 { mid } else if i == 2 { end } else { 0.0 };
                part.SetRelativeSizeAdjustment(Vector2 { X: rel(col, 1.0, 0.0), Y: rel(row, 1.0, 0.0) })?;
                part.SetRelativeOffsetAdjustment(Vector3 { X: rel(col, 0.0, 1.0), Y: rel(row, 0.0, 1.0), Z: 0.0 })?;
                parts.push(part);
            }
        }
        let rounded = |part: &ContainerVisual| -> Result<CompositionRoundedRectangleGeometry> {
            let geometry = compositor.CreateRoundedRectangleGeometry()?;
            part.SetClip(&compositor.CreateGeometricClipWithGeometry(&geometry)?)?;
            Ok(geometry)
        };
        let clips = [rounded(&parts[0])?, rounded(&parts[2])?, rounded(&parts[6])?, rounded(&parts[8])?];
        let mut layers = Vec::new();
        for part in &parts {
            for brush in &brushes {
                let layer = compositor.CreateSpriteVisual()?;
                layer.SetBrush(brush)?;
                layer.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
                part.Children()?.InsertAtTop(&layer)?;
                layers.push(layer);
            }
            children.InsertAtTop(part)?;
        }

        let refraction = if let Some(style) = style.filter(|_| crate::win::glass::enabled()) {
            Pane::new(hwnd, tint, style, source).and_then(|(pane, swapchain)| {
                let brush = (|| -> Result<CompositionNineGridBrush> {
                    let interop: ICompositorInterop = compositor.cast()?;
                    let surface = unsafe { interop.CreateCompositionSurfaceForSwapChain(&swapchain)? };
                    let brush = compositor.CreateNineGridBrush()?;
                    brush.SetSource(&compositor.CreateSurfaceBrushWithSurface(&surface)?)?;
                    // The shader rounds the pane itself, so it needs no clip.
                    let layer = compositor.CreateSpriteVisual()?;
                    layer.SetBrush(&brush)?;
                    layer.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
                    children.InsertAtTop(&layer)?;
                    layers.push(layer);
                    Ok(brush)
                })();
                brush.map_err(|e| log!("glass layer failed: {e}")).ok().map(|b| (pane, b))
            })
        } else {
            None
        };
        // Refraction draws its own rim. This one isn't split like the rest, so
        // it can lag a resize.
        if let Some(g) = glass.as_ref().filter(|_| refraction.is_none()) {
            children.InsertAtTop(&g.rim_layer)?;
        }
        root.Children()?.InsertAtTop(&shape)?;

        Ok(Self {
            _target: target,
            root,
            shape,
            parts,
            clips,
            radius: Cell::new(None),
            overflow: Cell::new(0.0),
            bare: Cell::new(false),
            tint: tint_brush,
            glass,
            refraction,
            _layers: layers,
        })
    }

    /// Size and corner radius in physical pixels. The shape fills the window
    /// (its size comes from the window: see the module docs), running
    /// `overflow` pixels past its bottom when the window is cut short.
    pub fn set_shape(&self, w: f32, h: f32, radius: f32, overflow: f32) {
        let size = Vector2 { X: w.max(0.0), Y: h.max(0.0) };
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0).round();
        if self.overflow.get() != overflow {
            self.overflow.set(overflow);
            let _ = self.shape.SetSize(Vector2 { X: 0.0, Y: overflow });
        }
        if self.radius.get() != Some(r) {
            self.radius.set(Some(r));
            // Offsets and sizes on top of each part's window-relative ones.
            let at = |i: usize| [0.0, r, -r][i];
            let len = |i: usize| [r, -2.0 * r, r][i];
            for (k, part) in self.parts.iter().enumerate() {
                let (row, col) = (k / 3, k % 3);
                let _ = part.SetOffset(Vector3 { X: at(col), Y: at(row), Z: 0.0 });
                let _ = part.SetSize(Vector2 { X: len(col), Y: len(row) });
            }
            // Each corner's clip is a whole rounded rect 2r across, its other three quarters outside the part.
            for (clip, (dx, dy)) in self.clips.iter().zip([(0.0, 0.0), (-r, 0.0), (0.0, -r), (-r, -r)]) {
                let _ = clip.SetOffset(Vector2 { X: dx, Y: dy });
                let _ = clip.SetSize(Vector2 { X: 2.0 * r, Y: 2.0 * r });
                let _ = clip.SetCornerRadius(Vector2 { X: r, Y: r });
            }
            if let Some((_, nine)) = &self.refraction {
                let _ = nine.SetLeftInset(r);
                let _ = nine.SetRightInset(r);
                let _ = nine.SetTopInset(r);
                let _ = nine.SetBottomInset(r);
            }
        }
        if let Some(g) = &self.glass {
            // Inset by half the stroke so the whole rim sits inside the clip.
            // Square shapes (the full-width menu bar) get no rim, like macOS.
            // (A geometry: it can't follow the window, so it may lag a resize.)
            let h = RIM / 2.0;
            let rim = if r > 0.0 { Vector2 { X: (size.X - RIM).max(0.0), Y: (size.Y + overflow - RIM).max(0.0) } } else { Vector2 { X: 0.0, Y: 0.0 } };
            let _ = g.rim.SetOffset(Vector2 { X: h, Y: h });
            let _ = g.rim.SetSize(rim);
            let _ = g.rim.SetCornerRadius(Vector2 { X: (r - h).max(0.0), Y: (r - h).max(0.0) });
        }
        if let Some((pane, _)) = &self.refraction {
            pane.set_shape(size.X, size.Y, r);
        }
    }

    /// Whether the glass refracts (see `glass`): then it can hold several shapes.
    pub fn refracts(&self) -> bool {
        self.refraction.is_some()
    }

    /// Several shapes in the glass, flowing together where they near each
    /// other (see `Pane::set_shapes`); true when they changed. The blurred
    /// layers under the refraction can only make the one rounded rect, so
    /// they go at once.
    pub fn set_shapes(&self, shapes: &[([f32; 4], f32)]) -> bool {
        let Some((pane, _)) = &self.refraction else { return false };
        self.hide_fallback();
        pane.set_shapes(shapes)
    }

    /// Hides the blur, tint and sheen under the refraction, for good.
    fn hide_fallback(&self) {
        if !self.bare.replace(true) {
            for part in &self.parts {
                let _ = part.SetIsVisible(false);
            }
        }
    }

    /// See `Pane::set_coat`; false when the glass doesn't refract (the surface paints them itself).
    pub fn set_coat(&self, coat: Color, border: Color) -> bool {
        let Some((pane, _)) = &self.refraction else { return false };
        pane.set_coat(coat, border);
        true
    }

    /// Fades the whole backdrop (blur, tint and glass) to `opacity`.
    pub fn set_opacity(&self, opacity: f32) {
        let _ = self.root.SetOpacity(opacity.clamp(0.0, 1.0));
    }

    pub fn set_tint(&self, tint: Color) {
        let _ = self.tint.SetColor(ui_color(tint));
        if let Some((pane, _)) = &self.refraction {
            pane.set_tint(tint);
        }
    }

    /// Re-renders the refraction after the window moved or reshaped.
    pub fn refresh(&self) {
        if let Some((pane, _)) = &self.refraction {
            pane.render();
            // Its frame covers the layers below from here on. (Hiding them is
            // a Composition change, which lands no sooner than the frame.)
            if pane.shown() {
                self.hide_fallback();
            }
        }
    }

}
