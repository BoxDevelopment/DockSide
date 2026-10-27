//! The genie renderer: one thread with its own Direct3D 11 device that draws every
//! running genie into a click-through ghost window (DirectComposition), paced on
//! the compositor clock. The IPC handler (on DockSide's UI thread) hands it jobs;
//! the DLL that asked is told through events when the first frame is on screen and
//! when the last one is done.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use windows::core::{s, w, Interface, PCSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HMODULE, HWND, LPARAM, LRESULT, RECT, S_OK, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::{DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual};
use windows::Win32::Graphics::Dwm::{DwmFlush, DwmSetWindowAttribute, DWMWA_EXCLUDED_FROM_PEEK, DWMWA_TRANSITIONS_FORCEDISABLED};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentThread, SetEvent, SetThreadPriority, WaitForSingleObject, INFINITE, THREAD_PRIORITY_HIGHEST};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::effect::{self, GenieParams};
use super::shared::{wide, GHOST_WNDCLASS};

const SHADER: &str = include_str!("genie.hlsl");

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct GenieConstants {
    win: [f32; 4],
    icon: [f32; 4],
    canvas: [f32; 4],
    params: [f32; 4],
    tex: [f32; 4],
    shadow: [f32; 4],
    shadow2: [f32; 4],
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct PrepConstants {
    prep: [i32; 4],
}

pub fn qpc() -> i64 {
    let mut v = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

fn qpc_freq() -> i64 {
    let mut f = 0;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f.max(1)
}

fn signal_and_close(h: &mut HANDLE) {
    if !h.is_invalid() {
        unsafe {
            let _ = SetEvent(*h);
            let _ = CloseHandle(*h);
        }
        *h = HANDLE::default();
    }
}

fn close(h: &mut HANDLE) {
    if !h.is_invalid() {
        unsafe {
            let _ = CloseHandle(*h);
        }
        *h = HANDLE::default();
    }
}

fn set(h: HANDLE) {
    if !h.is_invalid() {
        unsafe {
            let _ = SetEvent(h);
        }
    }
}

// --- jobs ----------------------------------------------------------------------

/// One accepted animation request, filled in by the IPC handler and handed to the
/// render thread.
pub struct Job {
    pub hwnd: HWND,
    pub rising: bool,

    /// Snapshot: a fresh capture (raw texture + crop rect in it) or the cached one.
    pub raw: Option<ID3D11Texture2D>,
    pub crop_x: i32,
    pub crop_y: i32,
    pub crop_w: i32,
    pub crop_h: i32,
    pub force_opaque: bool,

    pub win_rect: RECT,   // DWM extended frame, physical px
    pub mon: HMONITOR,    // monitor whose dock the genie targets
    pub corner_radius_px: f32, // physical px, 0 for maximized windows
    pub corner_smoothing: f32, // 0..1, Figma-style (see corner_shape)
    pub style: i32,
    pub params: GenieParams,
    pub duration_ms: i32,
    pub tiles: i32,

    /// Duplicated from the requesting DLL; owned by the job.
    pub h_first: HANDLE,
    pub h_done: HANDLE,
    pub h_revealed: HANDLE,
    pub requested_at: i64,
}

// Handles and a free-threaded device's texture: fine to move to the render thread.
unsafe impl Send for Job {}

impl Job {
    pub fn new(hwnd: HWND) -> Box<Job> {
        Box::new(Job {
            hwnd,
            rising: false,
            raw: None,
            crop_x: 0,
            crop_y: 0,
            crop_w: 0,
            crop_h: 0,
            force_opaque: false,
            win_rect: RECT::default(),
            mon: HMONITOR::default(),
            corner_radius_px: 0.0,
            corner_smoothing: 0.0,
            style: effect::STYLE_MODERN,
            params: GenieParams::default(),
            duration_ms: 450,
            tiles: 48,
            h_first: HANDLE::default(),
            h_done: HANDLE::default(),
            h_revealed: HANDLE::default(),
            requested_at: 0,
        })
    }

    pub fn close_handles(&mut self) {
        close(&mut self.h_first);
        close(&mut self.h_done);
        close(&mut self.h_revealed);
    }
}

// --- snapshot cache (snapshot_cache.cpp) ----------------------------------------

/// A prepared snapshot on the GPU (cropped, alpha fixed, mipmapped); the view keeps
/// its texture alive.
#[derive(Clone, Default)]
pub struct Snapshot {
    srv: Option<ID3D11ShaderResourceView>,
    w: i32,
    h: i32,
    bytes: usize,
    stamp: u64,
}

unsafe impl Send for Snapshot {}

/// Minimize snapshots kept on the GPU for the restore animation, keyed by window.
/// Bounded by a byte budget (least recently stored is evicted first) and emptied as
/// windows are destroyed. The IPC handler asks `has`, the render thread `put`s and
/// `take`s and erases destroyed windows.
#[derive(Default)]
struct SnapshotCache {
    map: HashMap<isize, Snapshot>,
    total: usize,
    budget: usize,
    clock: u64,
}

impl SnapshotCache {
    fn put(&mut self, hwnd: HWND, mut e: Snapshot) {
        if let Some(old) = self.map.remove(&(hwnd.0 as isize)) {
            self.total -= old.bytes;
        }
        self.clock += 1;
        e.stamp = self.clock;
        self.total += e.bytes;
        self.map.insert(hwnd.0 as isize, e);
        while self.total > self.budget && self.map.len() > 1 {
            let oldest = *self.map.iter().min_by_key(|(_, v)| v.stamp).unwrap().0;
            let gone = self.map.remove(&oldest).unwrap();
            self.total -= gone.bytes;
        }
    }

    /// Removes the entry: a size mismatch makes it useless anyway.
    fn take(&mut self, hwnd: HWND, w: i32, h: i32) -> Option<Snapshot> {
        let e = self.map.remove(&(hwnd.0 as isize))?;
        self.total -= e.bytes;
        (e.w == w && e.h == h).then_some(e)
    }

    fn has(&self, hwnd: HWND, w: i32, h: i32) -> bool {
        self.map.get(&(hwnd.0 as isize)).is_some_and(|e| e.w == w && e.h == h)
    }

    fn erase(&mut self, hwnd: HWND) {
        if let Some(e) = self.map.remove(&(hwnd.0 as isize)) {
            self.total -= e.bytes;
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.total = 0;
    }
}

// --- shared between the IPC handler and the render thread -----------------------

struct Device(Option<ID3D11Device>);
unsafe impl Send for Device {}

struct Shared {
    wake: HANDLE,
    stop: AtomicBool,
    queue: Mutex<Vec<Box<Job>>>,
    /// Accepted jobs per window (queued or animating). A request for an active window
    /// never needs a new snapshot or a fresh start: it joins or turns around the genie
    /// that is already running (see `retarget`).
    active: Mutex<HashMap<isize, i32>>,
    device: Mutex<Device>,
    cache: Mutex<SnapshotCache>,
}

unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    /// Tells the DLL its animation is over (it then reveals / tears down) and drops the job.
    fn finish(&self, mut job: Box<Job>) {
        signal_and_close(&mut job.h_first);
        signal_and_close(&mut job.h_done);
        self.release(job);
    }

    fn release(&self, mut job: Box<Job>) {
        job.close_handles();
        let mut active = lock(&self.active);
        let key = job.hwnd.0 as isize;
        if let Some(n) = active.get_mut(&key) {
            *n -= 1;
            if *n <= 0 {
                active.remove(&key);
            }
        }
    }
}

// --- public side (any thread) ----------------------------------------------------

pub struct Renderer {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Renderer {
    pub fn start() -> Option<Renderer> {
        let wake = unsafe { CreateEventW(None, false, false, None) }.ok()?;
        let shared = Arc::new(Shared {
            wake,
            stop: AtomicBool::new(false),
            queue: Mutex::new(Vec::new()),
            active: Mutex::new(HashMap::new()),
            device: Mutex::new(Device(None)),
            cache: Mutex::new(SnapshotCache::default()),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let for_thread = shared.clone();
        let thread = std::thread::Builder::new()
            .name("genie render".into())
            .spawn(move || {
                let Some(mut t) = RenderThread::new(for_thread) else {
                    let _ = tx.send(false);
                    return;
                };
                let _ = tx.send(true);
                t.run();
            })
            .ok()?;
        if !rx.recv().unwrap_or(false) {
            let _ = thread.join();
            unsafe {
                let _ = CloseHandle(wake);
            }
            return None;
        }
        Some(Renderer { shared, thread: Some(thread) })
    }

    /// Free-threaded device for snapshot uploads from the IPC handler.
    pub fn device(&self) -> Option<ID3D11Device> {
        lock(&self.shared.device).0.clone()
    }

    pub fn retain(&self, hwnd: HWND) {
        *lock(&self.shared.active).entry(hwnd.0 as isize).or_insert(0) += 1;
    }

    pub fn is_active(&self, hwnd: HWND) -> bool {
        lock(&self.shared.active).contains_key(&(hwnd.0 as isize))
    }

    pub fn has_snapshot(&self, hwnd: HWND, w: i32, h: i32) -> bool {
        lock(&self.shared.cache).has(hwnd, w, h)
    }

    /// Takes a retained job.
    pub fn submit(&self, job: Box<Job>) {
        lock(&self.shared.queue).push(job);
        unsafe {
            let _ = SetEvent(self.shared.wake);
        }
    }

    /// Finishes a retained job that won't be animated: the DLL is told to finish now.
    pub fn fail(&self, job: Box<Job>) {
        self.shared.finish(job);
    }
}

impl Drop for Renderer {
    /// Every running genie is finished (its DLL reveals / tears down) before this returns.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        unsafe {
            let _ = SetEvent(self.shared.wake);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            let _ = CloseHandle(self.shared.wake);
        }
    }
}

// --- render thread ---------------------------------------------------------------

unsafe extern "system" fn ghost_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// The lowest always-on-top window (other than the ghost itself): a ghost placed right
/// after it is under the Dock, the menu bar and every other always-on-top window, and
/// above all programs. Not right after the taskbar: while the desktop is shown (Win+D),
/// Windows draws the desktop's own windows (the widgets) over a window placed there.
fn lowest_topmost_window(ghost: HWND) -> Option<HWND> {
    let mut lowest = None;
    unsafe {
        let mut h = GetTopWindow(None).unwrap_or_default();
        while !h.is_invalid() && GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0 {
            if h != ghost {
                lowest = Some(h);
            }
            h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
        }
    }
    lowest
}

fn round_up(v: i32, step: i32) -> i32 {
    (v + step - 1) / step * step
}

type WaitClockFn = unsafe extern "system" fn(u32, *const HANDLE, u32) -> u32;

thread_local! {
    /// For the destroy WinEvent, which arrives on this thread with no context.
    static EVENT_SHARED: std::cell::RefCell<Option<Arc<Shared>>> = const { std::cell::RefCell::new(None) };
}

/// A destroyed window drops its cached snapshot (win_events.cpp). Out of context, on
/// the render thread: it fires for every window in the system, and this thread is idle
/// nearly always, where the UI thread has frames to draw.
unsafe extern "system" fn on_destroy(_: HWINEVENTHOOK, _event: u32, hwnd: HWND, id_object: i32, id_child: i32, _: u32, _: u32) {
    if hwnd.is_invalid() || id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    EVENT_SHARED.with(|s| {
        if let Some(s) = &*s.borrow() {
            lock(&s.cache).erase(hwnd);
        }
    });
}

#[derive(Default)]
struct Slot {
    hwnd: HWND,
    swap: Option<IDXGISwapChain1>,
    _target: Option<IDCompositionTarget>,
    visual: Option<IDCompositionVisual>,
    msaa: Option<ID3D11Texture2D>,
    rtv: Option<ID3D11RenderTargetView>,
    alloc_w: i32,
    alloc_h: i32,
    busy: bool,
    broken: bool, // a frame failed in it: replaced, never used again
}

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Running,
    Linger,
    Retire,
}

/// One genie per window. Its direction can flip any number of times; the jobs of the
/// current direction are its waiters and are finished when it ends.
struct Anim {
    hwnd: HWND,
    rising: bool,
    duration_ms: i32,
    tiles: i32,
    corner_radius_px: f32,
    corner_smoothing: f32,
    params: GenieParams,
    snap: Snapshot,
    slot: Option<usize>, // index into `slots`
    /// Z-order: at the bottom of the always-on-top windows, so the Dock stays on top (the
    /// window seems to slip into its icon) and every program stays below.
    insert_after: HWND,
    waiters: Vec<Box<Job>>,
    start: i64,
    done_at: i64,
    requested_at: i64,
    phase: Phase,
    shown: bool,
    first_pending: bool,
    frames: i32,
    reversals: i32,
    cpu_ms: f64,
    first_frame_ms: f64,
}

/// A job whose direction was overridden. A superseded restore is released only once
/// the window is minimized again, so its DLL never reveals the window too early.
struct Superseded {
    job: Box<Job>,
    was_rising: bool,
    since: i64,
}

#[derive(Default)]
struct Pipeline {
    ctx: Option<ID3D11DeviceContext>,
    factory: Option<IDXGIFactory2>,
    dcomp: Option<IDCompositionDevice>,
    genie_vs: Option<ID3D11VertexShader>,
    classic_vs: Option<ID3D11VertexShader>,
    scale_vs: Option<ID3D11VertexShader>,
    shatter_vs: Option<ID3D11VertexShader>,
    fullscreen_vs: Option<ID3D11VertexShader>,
    genie_ps: Option<ID3D11PixelShader>,
    shatter_ps: Option<ID3D11PixelShader>,
    shadow_ps: Option<ID3D11PixelShader>,
    prep_ps: Option<ID3D11PixelShader>,
    genie_cb: Option<ID3D11Buffer>,
    prep_cb: Option<ID3D11Buffer>,
    sampler: Option<ID3D11SamplerState>,
    blend: Option<ID3D11BlendState>,
    raster: Option<ID3D11RasterizerState>,
    grids: HashMap<i32, ID3D11Buffer>,
    msaa_count: u32,
}

struct RenderThread {
    shared: Arc<Shared>,
    device: Option<ID3D11Device>,
    p: Pipeline,
    warp: bool,
    broken: bool, // no device could be made again: every job fails over to Windows
    wait_clock: Option<WaitClockFn>,
    slots: Vec<Slot>,
    anims: Vec<Anim>,
    superseded: Vec<Superseded>,
    freq: i64,
    destroy_hook: HWINEVENTHOOK,
}

/// Error for a step that returned no object.
fn missing() -> windows::core::Error {
    windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)
}

fn compile(entry: PCSTR, target: PCSTR) -> windows::core::Result<Vec<u8>> {
    let (mut code, mut errors) = (None, None);
    let r = unsafe { D3DCompile(SHADER.as_ptr() as _, SHADER.len(), s!("genie.hlsl"), None, None, entry, target, 1 << 15, 0, &mut code, Some(&mut errors)) };
    if let Err(e) = r {
        if let Some(b) = errors {
            let msg = unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) };
            log!("genie: shader {}: {}", unsafe { entry.display() }, String::from_utf8_lossy(msg));
        }
        return Err(e);
    }
    let b = code.ok_or_else(missing)?;
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec())
}

impl RenderThread {
    fn new(shared: Arc<Shared>) -> Option<Self> {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let class = wide(GHOST_WNDCLASS);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(ghost_wndproc),
                hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
                lpszClassName: windows::core::PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            RegisterClassExW(&wc);
        }
        let dcomp_dll = unsafe { LoadLibraryExW(w!("dcomp.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.unwrap_or_default();
        // Windows 11 only; DwmFlush paces frames without it.
        let wait_clock = (!dcomp_dll.is_invalid())
            .then(|| unsafe { GetProcAddress(dcomp_dll, s!("DCompositionWaitForCompositorClock")) })
            .flatten()
            .map(|f| unsafe { std::mem::transmute::<_, WaitClockFn>(f) });
        let mut t = RenderThread {
            shared: shared.clone(),
            device: None,
            p: Pipeline::default(),
            warp: false,
            broken: false,
            wait_clock,
            slots: Vec::new(),
            anims: Vec::new(),
            superseded: Vec::new(),
            freq: qpc_freq(),
            destroy_hook: HWINEVENTHOOK::default(),
        };
        if !t.create_device() {
            t.destroy_device();
            return None;
        }
        EVENT_SHARED.with(|s| *s.borrow_mut() = Some(shared));
        t.destroy_hook = unsafe {
            SetWinEventHook(EVENT_OBJECT_DESTROY, EVENT_OBJECT_DESTROY, None, Some(on_destroy), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)
        };
        Some(t)
    }

    fn ms(&self, ticks: i64) -> f64 {
        ticks as f64 * 1000.0 / self.freq as f64
    }

    fn ctx(&self) -> &ID3D11DeviceContext {
        self.p.ctx.as_ref().unwrap()
    }

    fn run(&mut self) {
        let wake = self.shared.wake;
        while !self.shared.stop.load(Ordering::Acquire) {
            let jobs = std::mem::take(&mut *lock(&self.shared.queue));
            if !jobs.is_empty() && self.device_lost() {
                self.recover_device();
            }
            let now = qpc();
            for j in jobs {
                if self.broken {
                    self.shared.finish(j);
                } else {
                    self.start_job(j, now);
                }
            }

            pump_messages();
            if self.anims.is_empty() && self.superseded.is_empty() {
                // Idle: sleep until a job arrives (no polling, no timer).
                unsafe { MsgWaitForMultipleObjectsEx(Some(&[wake]), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
                continue;
            }

            let now = qpc();
            for i in 0..self.anims.len() {
                self.step_anim(i, now);
            }
            if self.device_lost() {
                self.recover_device(); // finishes every animation
                continue;
            }
            self.process_superseded(now, false);
            if self.anims.is_empty() {
                // Only superseded jobs left, waiting for their window to settle.
                unsafe { MsgWaitForMultipleObjectsEx(Some(&[wake]), 15, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
                continue;
            }
            self.commit();

            if !self.wait_for_tick() {
                continue; // woken by a new job, not by the compositor
            }

            // Everything presented before this tick is on screen now.
            let mut i = 0;
            while i < self.anims.len() {
                let a = &mut self.anims[i];
                if a.first_pending {
                    a.first_pending = false;
                    for w in &a.waiters {
                        set(w.h_first);
                    }
                }
                if a.phase == Phase::Retire {
                    let a = self.anims.remove(i);
                    self.retire_anim(a);
                } else {
                    i += 1;
                }
            }
            if self.anims.is_empty() {
                self.commit();
            }
        }

        self.abort_all();
        for j in std::mem::take(&mut *lock(&self.shared.queue)) {
            self.shared.finish(j);
        }
        self.drop_slots();
        lock(&self.shared.cache).clear();
        self.destroy_device();
        unsafe {
            if !self.destroy_hook.is_invalid() {
                let _ = UnhookWinEvent(self.destroy_hook);
            }
        }
        EVENT_SHARED.with(|s| *s.borrow_mut() = None);
    }

    fn commit(&self) {
        if let Some(d) = &self.p.dcomp {
            unsafe {
                let _ = d.Commit();
            }
        }
    }

    /// Any GPU or none: a driver that fails anywhere on the way gets WARP, Direct3D on
    /// the CPU, which is plenty for one mesh.
    fn create_device(&mut self) -> bool {
        match self.create_device_on(D3D_DRIVER_TYPE_HARDWARE) {
            Ok(()) => return true,
            Err(e) => log!("genie: hardware device unusable ({e}), falling back to WARP"),
        }
        self.destroy_device();
        match self.create_device_on(D3D_DRIVER_TYPE_WARP) {
            Ok(()) => true,
            Err(e) => {
                log!("genie: WARP device failed: {e}");
                false
            }
        }
    }

    fn create_device_on(&mut self, kind: D3D_DRIVER_TYPE) -> windows::core::Result<()> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
        let (mut dev, mut ctx, mut fl) = (None, None, D3D_FEATURE_LEVEL::default());
        self.warp = kind == D3D_DRIVER_TYPE_WARP;
        unsafe {
            D3D11CreateDevice(None, kind, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, Some(&levels), D3D11_SDK_VERSION, Some(&mut dev), Some(&mut fl), Some(&mut ctx))?;
        }
        let (dev, ctx) = (dev.ok_or_else(missing)?, ctx.ok_or_else(missing)?);
        let dxgi: IDXGIDevice = dev.cast()?;
        let adapter = unsafe { dxgi.GetAdapter()? };
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };
        let ad = unsafe { adapter.GetDesc()? };
        // Minimize snapshots wait on the GPU for the restore. Integrated graphics and WARP
        // take that memory from the system RAM, so they get a smaller budget.
        const MB: usize = 1 << 20;
        lock(&self.shared.cache).budget = if self.warp {
            96 * MB
        } else if ad.DedicatedVideoMemory >= 2 << 30 {
            384 * MB
        } else {
            160 * MB
        };
        let dcomp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi)? };

        lock(&self.shared.device).0 = Some(dev.clone());
        self.device = Some(dev.clone());
        self.p.ctx = Some(ctx);
        self.p.factory = Some(factory);
        self.p.dcomp = Some(dcomp);

        self.p.msaa_count = 1;
        if unsafe { dev.CheckMultisampleQualityLevels(DXGI_FORMAT_B8G8R8A8_UNORM, 4) }.is_ok_and(|q| q > 0) {
            self.p.msaa_count = 4;
        }

        self.compile_shaders(&dev)?;
        unsafe {
            let mut desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<GenieConstants>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            dev.CreateBuffer(&desc, None, Some(&mut self.p.genie_cb))?;
            desc.ByteWidth = std::mem::size_of::<PrepConstants>() as u32;
            dev.CreateBuffer(&desc, None, Some(&mut self.p.prep_cb))?;

            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_ANISOTROPIC,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxAnisotropy: 8,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            dev.CreateSamplerState(&desc, Some(&mut self.p.sampler))?;

            // Premultiplied "over" (the warped mesh can fold onto itself near the neck).
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
            dev.CreateBlendState(&desc, Some(&mut self.p.blend))?;

            let desc = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_NONE,
                DepthClipEnable: BOOL(1),
                MultisampleEnable: BOOL((self.p.msaa_count > 1) as i32),
                ..Default::default()
            };
            dev.CreateRasterizerState(&desc, Some(&mut self.p.raster))?;
        }
        log!(
            "genie: {}{}, feature level {:x}, MSAA {}x, compositor clock {}",
            String::from_utf16_lossy(&ad.Description[..ad.Description.iter().position(|&c| c == 0).unwrap_or(ad.Description.len())]),
            if self.warp { " (WARP)" } else { "" },
            fl.0,
            self.p.msaa_count,
            if self.wait_clock.is_some() { "yes" } else { "no (DwmFlush)" }
        );
        Ok(())
    }

    fn compile_shaders(&mut self, dev: &ID3D11Device) -> windows::core::Result<()> {
        let vs = |entry: PCSTR, out: &mut Option<ID3D11VertexShader>| -> windows::core::Result<()> {
            let code = compile(entry, s!("vs_4_0"))?;
            unsafe { dev.CreateVertexShader(&code, None, Some(out)) }
        };
        vs(s!("GenieVS"), &mut self.p.genie_vs)?;
        vs(s!("ClassicVS"), &mut self.p.classic_vs)?;
        vs(s!("ScaleVS"), &mut self.p.scale_vs)?;
        vs(s!("ShatterVS"), &mut self.p.shatter_vs)?;
        vs(s!("FullscreenVS"), &mut self.p.fullscreen_vs)?;
        let ps = |entry: PCSTR, out: &mut Option<ID3D11PixelShader>| -> windows::core::Result<()> {
            let code = compile(entry, s!("ps_4_0"))?;
            unsafe { dev.CreatePixelShader(&code, None, Some(out)) }
        };
        ps(s!("GeniePS"), &mut self.p.genie_ps)?;
        ps(s!("ShatterPS"), &mut self.p.shatter_ps)?;
        ps(s!("ShadowPS"), &mut self.p.shadow_ps)?;
        ps(s!("PrepPS"), &mut self.p.prep_ps)?;
        Ok(())
    }

    fn device_lost(&self) -> bool {
        self.device.as_ref().is_some_and(|d| unsafe { d.GetDeviceRemovedReason() }.is_err())
    }

    /// Device removed (driver update, TDR, GPU unplugged): every animation is finished
    /// (the DLLs reveal their windows) and the device is made again.
    fn recover_device(&mut self) {
        let reason = self.device.as_ref().map_or(S_OK, |d| match unsafe { d.GetDeviceRemovedReason() } {
            Ok(()) => S_OK,
            Err(e) => e.code(),
        });
        log!("genie: graphics device lost (0x{:08X}), making it again", reason.0 as u32);
        self.abort_all();
        self.drop_slots();
        lock(&self.shared.cache).clear();
        self.destroy_device();
        // A driver that is being updated or reset can take a few seconds to come back.
        for attempt in 0..8u32 {
            if self.shared.stop.load(Ordering::Acquire) {
                break;
            }
            if self.create_device() {
                self.broken = false;
                log!("genie: device ready again");
                return;
            }
            self.destroy_device();
            unsafe { WaitForSingleObject(self.shared.wake, 250 << attempt.min(4)) };
        }
        self.broken = true;
        log!("genie: no graphics device; windows keep Windows' animations until DockSide restarts");
    }

    fn destroy_device(&mut self) {
        if let Some(ctx) = &self.p.ctx {
            unsafe { ctx.ClearState() };
        }
        self.p = Pipeline::default();
        lock(&self.shared.device).0 = None;
        self.device = None;
    }

    /// Triangle-list indices of a tiles x tiles grid, cached per tile count.
    fn grid_indices(&mut self, tiles: i32) -> Option<(ID3D11Buffer, u32)> {
        let count = (tiles * tiles * 6) as u32;
        if let Some(b) = self.p.grids.get(&tiles) {
            return Some((b.clone(), count));
        }
        let stride = (tiles + 1) as u16;
        let mut idx: Vec<u16> = Vec::with_capacity(count as usize);
        for y in 0..tiles as u16 {
            for x in 0..tiles as u16 {
                let a = y * stride + x;
                let b = a + 1;
                let c = a + stride;
                let d = c + 1;
                idx.extend_from_slice(&[a, b, c, b, d, c]);
            }
        }
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: (idx.len() * 2) as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_INDEX_BUFFER.0 as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA { pSysMem: idx.as_ptr() as _, ..Default::default() };
        let mut buf = None;
        unsafe { self.device.as_ref()?.CreateBuffer(&desc, Some(&data), Some(&mut buf)) }.ok()?;
        let buf = buf?;
        self.p.grids.insert(tiles, buf.clone());
        Some((buf, count))
    }

    /// Crops the raw capture to the extended frame, fixes alpha and builds the mip
    /// chain: one tiny GPU pass per snapshot; the result is also what gets cached for
    /// the restore.
    fn prepare_snapshot(&self, job: &mut Job) -> Option<Snapshot> {
        let raw = job.raw.take()?; // the raw capture is no longer needed after this
        let dev = self.device.as_ref()?;
        // Uploaded before a device reset: from another device.
        if unsafe { raw.GetDevice() }.ok()?.as_raw() != dev.as_raw() {
            return None;
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: job.crop_w as u32,
            Height: job.crop_h as u32,
            MipLevels: 0,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
            ..Default::default()
        };
        let ctx = self.ctx();
        unsafe {
            let mut tex = None;
            dev.CreateTexture2D(&desc, None, Some(&mut tex)).ok()?;
            let tex = tex?;
            let rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: desc.Format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let (mut rtv, mut srv, mut raw_srv) = (None, None, None);
            dev.CreateRenderTargetView(&tex, Some(&rtv_desc), Some(&mut rtv)).ok()?;
            dev.CreateShaderResourceView(&tex, None, Some(&mut srv)).ok()?;
            dev.CreateShaderResourceView(&raw, None, Some(&mut raw_srv)).ok()?;
            let srv = srv?;

            let prep_cb = self.p.prep_cb.as_ref()?;
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(prep_cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m)).ok()?;
            *(m.pData as *mut PrepConstants) = PrepConstants { prep: [job.crop_x, job.crop_y, job.force_opaque as i32, 0] };
            ctx.Unmap(prep_cb, 0);

            ctx.OMSetRenderTargets(Some(&[rtv]), None);
            ctx.OMSetBlendState(None, None, 0xffff_ffff);
            ctx.RSSetState(self.p.raster.as_ref());
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: job.crop_w as f32, Height: job.crop_h as f32, MaxDepth: 1.0, ..Default::default() }]));
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(self.p.fullscreen_vs.as_ref(), None);
            ctx.PSSetShader(self.p.prep_ps.as_ref(), None);
            ctx.PSSetConstantBuffers(1, Some(&[Some(prep_cb.clone())]));
            ctx.PSSetShaderResources(0, Some(&[raw_srv]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(None, None);
            ctx.GenerateMips(&srv);

            let bytes = job.crop_w as usize * job.crop_h as usize * 4;
            Some(Snapshot { srv: Some(srv), w: job.crop_w, h: job.crop_h, bytes: bytes + bytes / 3, stamp: 0 })
        }
    }

    fn setup_slot(&self, s: &mut Slot) -> windows::core::Result<()> {
        let class = wide(GHOST_WNDCLASS);
        unsafe {
            s.hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                windows::core::PCWSTR(class.as_ptr()),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                GetModuleHandleW(None).unwrap_or_default(),
                None,
            )?;
            // Layered + transparent = click-through. Content comes from DirectComposition.
            SetLayeredWindowAttributes(s.hwnd, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA)?;
            let on = BOOL(1);
            let _ = DwmSetWindowAttribute(s.hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, &on as *const _ as _, 4);
            let _ = DwmSetWindowAttribute(s.hwnd, DWMWA_EXCLUDED_FROM_PEEK, &on as *const _ as _, 4);

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: 256,
                Height: 256,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let dev = self.device.as_ref().ok_or_else(missing)?;
            let swap = self.p.factory.as_ref().ok_or_else(missing)?.CreateSwapChainForComposition(dev, &desc, None)?;
            let dcomp = self.p.dcomp.as_ref().ok_or_else(missing)?;
            let target = dcomp.CreateTargetForHwnd(s.hwnd, true)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            target.SetRoot(&visual)?;
            s.swap = Some(swap);
            s._target = Some(target);
            s.visual = Some(visual);
            s.alloc_w = desc.Width as i32;
            s.alloc_h = desc.Height as i32;
        }
        self.create_slot_targets(s)
    }

    fn create_slot_targets(&self, s: &mut Slot) -> windows::core::Result<()> {
        s.rtv = None;
        s.msaa = None;
        let dev = self.device.as_ref().ok_or_else(missing)?;
        unsafe {
            if self.p.msaa_count > 1 {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: s.alloc_w as u32,
                    Height: s.alloc_h as u32,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: self.p.msaa_count, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    ..Default::default()
                };
                dev.CreateTexture2D(&desc, None, Some(&mut s.msaa))?;
                dev.CreateRenderTargetView(s.msaa.as_ref().ok_or_else(missing)?, None, Some(&mut s.rtv))?;
                return Ok(());
            }
            let back: ID3D11Texture2D = s.swap.as_ref().ok_or_else(missing)?.GetBuffer(0)?;
            dev.CreateRenderTargetView(&back, None, Some(&mut s.rtv))
        }
    }

    /// Ghost windows are reused; buffers only ever grow (in 128 px steps), so a typical
    /// animation reuses everything and starts without any allocation.
    fn acquire_slot(&mut self, w: i32, h: i32) -> Option<usize> {
        // Beyond what a texture can be: a bogus geometry, never worth a resize attempt.
        let max = D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION as i32;
        if w <= 0 || h <= 0 || w > max || h > max {
            log!("genie: canvas {w} x {h} refused");
            return None;
        }
        let i = match self.slots.iter().position(|s| !s.busy) {
            Some(i) => i,
            None => {
                let mut fresh = Slot::default();
                if let Err(e) = self.setup_slot(&mut fresh) {
                    log!("genie: ghost setup failed: {e}");
                    destroy_slot(&fresh);
                    return None;
                }
                self.slots.push(fresh);
                self.slots.len() - 1
            }
        };
        let (aw, ah) = (self.slots[i].alloc_w, self.slots[i].alloc_h);
        if w > aw || h > ah {
            let nw = aw.max(round_up(w, 128));
            let nh = ah.max(round_up(h, 128));
            let mut s = std::mem::take(&mut self.slots[i]);
            s.rtv = None;
            s.msaa = None;
            unsafe {
                self.ctx().OMSetRenderTargets(None, None);
                self.ctx().Flush();
            }
            let resized = unsafe { s.swap.as_ref().unwrap().ResizeBuffers(2, nw as u32, nh as u32, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0)) };
            if let Err(e) = resized {
                log!("genie: ResizeBuffers({nw} x {nh}) failed: {e}");
                // The buffers keep their old size: give the slot its targets back, or
                // every later animation drawn in it would fail too.
                if self.create_slot_targets(&mut s).is_ok() {
                    self.slots[i] = s;
                } else {
                    destroy_slot(&s);
                    self.slots.remove(i);
                }
                return None;
            }
            s.alloc_w = nw;
            s.alloc_h = nh;
            if self.create_slot_targets(&mut s).is_err() {
                destroy_slot(&s);
                self.slots.remove(i);
                return None;
            }
            self.slots[i] = s;
        }
        self.slots[i].busy = true;
        Some(i)
    }

    fn drop_slots(&mut self) {
        for a in &mut self.anims {
            a.slot = None;
        }
        for s in self.slots.drain(..) {
            destroy_slot(&s);
        }
    }

    fn find_anim(&self, hwnd: HWND) -> Option<usize> {
        self.anims.iter().position(|a| a.hwnd == hwnd)
    }

    fn start_job(&mut self, mut job: Box<Job>, now: i64) {
        if let Some(i) = self.find_anim(job.hwnd) {
            let running = &self.anims[i];
            // A closing window's genie is never turned around (the window is going away),
            // and a close meeting a running genie is left to Windows. Nor are flying
            // pieces: a window minimized while its pieces fly together just appears.
            if running.params.closing || job.params.closing || effect::is_shatter_style(running.params.style) {
                self.shared.finish(job);
                return;
            }
            self.retarget(i, job, now);
            return;
        }

        // A fresh capture if the DLL sent one, else the minimize snapshot from the cache
        // (a request for a window whose genie just ended can arrive without one).
        let snap = match self.prepare_snapshot(&mut job) {
            Some(s) => Some(s),
            None => lock(&self.shared.cache).take(job.hwnd, job.crop_w, job.crop_h),
        };
        let Some(snap) = snap else {
            log!("genie: no snapshot for {:?}, skipping", job.hwnd.0);
            self.shared.finish(job);
            return;
        };

        let g = job.params;
        let Some(slot) = self.acquire_slot(g.canvas_width, g.canvas_height) else {
            self.shared.finish(job);
            return;
        };
        let s = &self.slots[slot];
        let ghost = s.hwnd;
        unsafe {
            if let Some(v) = &s.visual {
                let _ = v.SetClip2(&D2D_RECT_F { left: 0.0, top: 0.0, right: g.canvas_width as f32, bottom: g.canvas_height as f32 });
            }
        }
        let insert_after = lowest_topmost_window(ghost).unwrap_or(HWND_TOPMOST);
        unsafe {
            let _ = SetWindowPos(ghost, insert_after, g.canvas_left, g.canvas_top, g.canvas_width, g.canvas_height, SWP_NOACTIVATE | SWP_NOOWNERZORDER);
        }
        self.anims.push(Anim {
            hwnd: job.hwnd,
            rising: job.rising,
            duration_ms: job.duration_ms,
            tiles: job.tiles,
            corner_radius_px: job.corner_radius_px,
            corner_smoothing: job.corner_smoothing,
            params: g,
            snap,
            slot: Some(slot),
            insert_after,
            requested_at: job.requested_at,
            waiters: vec![job],
            start: now,
            done_at: 0,
            phase: Phase::Running,
            shown: false,
            first_pending: false,
            frames: 0,
            reversals: 0,
            cpu_ms: 0.0,
            first_frame_ms: -1.0,
        });
    }

    /// A request for a window whose genie is already in flight (fast clicking: restore
    /// while it is still minimizing, minimize again while it is still coming back, ...).
    ///  - same direction: the job just waits for the running genie,
    ///  - opposite direction: the genie turns around from exactly where it is. Both time
    ///    curves are symmetric (f(1 - x) = 1 - f(x)), so continuing at raw progress
    ///    1 - r lands on the very same frame: no jump, no restart, any number of times.
    fn retarget(&mut self, i: usize, mut job: Box<Job>, now: i64) {
        job.raw = None; // the running genie keeps its snapshot
        let a = &self.anims[i];
        let r = if a.phase == Phase::Running { (self.ms(now - a.start) / a.duration_ms as f64).clamp(0.0, 1.0) } else { 1.0 };

        if job.rising == a.rising {
            let a = &mut self.anims[i];
            if a.phase != Phase::Running {
                set(job.h_done); // already there
            }
            a.waiters.push(job);
            if a.shown {
                a.first_pending = true;
            }
            return;
        }

        // The jobs of the old direction are released once their window has settled.
        let a = &mut self.anims[i];
        let was_rising = a.rising;
        for w in a.waiters.drain(..) {
            self.superseded.push(Superseded { job: w, was_rising, since: now });
        }
        // Turned back to a direction it had before: those jobs simply wait again.
        let mut k = 0;
        while k < self.superseded.len() {
            let s = &self.superseded[k];
            if s.job.hwnd == a.hwnd && s.was_rising == job.rising {
                a.waiters.push(self.superseded.remove(k).job);
            } else {
                k += 1;
            }
        }
        a.rising = job.rising;
        a.duration_ms = job.duration_ms;
        a.waiters.push(job);
        let back = (1.0 - r) * a.duration_ms as f64;
        let a = &mut self.anims[i];
        a.start = now - (back * self.freq as f64 / 1000.0) as i64;
        a.phase = Phase::Running;
        a.reversals += 1;
        if a.shown {
            a.first_pending = true;
        }
    }

    /// Superseded minimize jobs are released right away (their DLL only re-enables DWM
    /// transitions; the restore already happened). A superseded restore is released only
    /// once the window is minimized again - its DLL then uncloaks a window nobody can see,
    /// instead of revealing it in the middle of the reversed genie.
    fn process_superseded(&mut self, now: i64, force: bool) {
        let mut i = 0;
        while i < self.superseded.len() {
            let s = &self.superseded[i];
            let h = s.job.hwnd;
            let release = force || !s.was_rising || unsafe { !IsWindow(h).as_bool() || IsIconic(h).as_bool() } || self.ms(now - s.since) > 600.0;
            if release {
                let s = self.superseded.remove(i);
                self.shared.finish(s.job);
            } else {
                i += 1;
            }
        }
    }

    fn step_anim(&mut self, i: usize, now: i64) {
        if self.anims[i].phase == Phase::Running {
            let a = &self.anims[i];
            let elapsed = self.ms(now - a.start);
            let last = elapsed >= a.duration_ms as f64;
            let raw = (elapsed / a.duration_ms as f64).min(1.0) as f32;
            let eased = effect::genie_ease(&a.params, raw);
            // Minimize runs 0 -> 1, restore 1 -> 0: the same warp played in reverse.
            let t = if a.rising { 1.0 - eased } else { eased };

            // A collapsed mesh is never presented: the ghost stays hidden until the first
            // real frame (restore) or holds its last good frame (minimize).
            if !effect::genie_frame_degenerate(&a.params, t) {
                if !self.draw(i, t) {
                    self.anims[i].phase = Phase::Retire;
                    return;
                }
                let first_ms = self.ms(now - self.anims[i].requested_at);
                let a = &mut self.anims[i];
                if !a.shown {
                    if let Some(slot) = a.slot {
                        unsafe {
                            let _ = SetWindowPos(self.slots[slot].hwnd, a.insert_after, 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOOWNERZORDER);
                        }
                    }
                    a.shown = true;
                    a.first_pending = true;
                    a.first_frame_ms = first_ms;
                }
            }
            if last {
                let a = &mut self.anims[i];
                for w in &a.waiters {
                    set(w.h_done);
                }
                a.phase = Phase::Linger;
                a.done_at = now;
            }
        }
        if self.anims[i].phase == Phase::Linger {
            // Restore: keep the final frame up until the DLL has revealed the real window,
            // so there is never a gap between the two.
            let a = &self.anims[i];
            let revealed = !a.rising || a.waiters.iter().any(|w| !w.h_revealed.is_invalid() && unsafe { WaitForSingleObject(w.h_revealed, 0) } == WAIT_OBJECT_0);
            if revealed || self.ms(now - a.done_at) > 150.0 {
                self.anims[i].phase = Phase::Retire;
            }
        }
    }

    fn draw(&mut self, i: usize, t: f32) -> bool {
        let t0 = qpc();
        let tiles = self.anims[i].tiles;
        let Some((ib, index_count)) = self.grid_indices(tiles) else {
            return false;
        };
        let a = &self.anims[i];
        let Some(slot) = a.slot else {
            return false;
        };
        let g = &a.params;
        let ok = self.draw_frame(slot, a, g, t, &ib, index_count);
        if let Err(e) = ok {
            log!("genie: frame failed: {e}");
            self.slots[slot].broken = true;
            return false;
        }
        let ms = self.ms(qpc() - t0);
        let a = &mut self.anims[i];
        a.frames += 1;
        a.cpu_ms += ms;
        true
    }

    fn draw_frame(&self, slot: usize, a: &Anim, g: &GenieParams, t: f32, ib: &ID3D11Buffer, index_count: u32) -> windows::core::Result<()> {
        let s = &self.slots[slot];
        let ctx = self.ctx();
        let cb = self.p.genie_cb.as_ref().ok_or_else(missing)?;
        let rtv = s.rtv.as_ref().ok_or_else(missing)?;
        let (snap_w, snap_h) = (a.snap.w as f32, a.snap.h as f32);

        let mut c = GenieConstants {
            win: [g.win.x, g.win.y, g.win.width, g.win.height],
            icon: [g.icon.x, g.icon.y, g.icon.width, g.icon.height],
            canvas: [g.canvas_left as f32, g.canvas_top as f32, g.canvas_width as f32, g.canvas_height as f32],
            // Radius in snapshot pixels (the snapshot may be a DPI-virtualized size).
            params: [t, if g.win.width > 0.0 { a.corner_radius_px * snap_w / g.win.width } else { 0.0 }, a.tiles as f32, effect::genie_fade(g, t)],
            tex: [snap_w, snap_h, 0.0, 0.0],
            ..Default::default()
        };
        let (extent, exponent) = effect::corner_shape(c.params[1], a.corner_smoothing, snap_w, snap_h);
        c.tex[2] = extent;
        c.tex[3] = exponent;
        let upload = |c: &GenieConstants| -> windows::core::Result<()> {
            unsafe {
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
                *(m.pData as *mut GenieConstants) = *c;
                ctx.Unmap(cb, 0);
            }
            Ok(())
        };

        let shatter = effect::is_shatter_style(g.style);
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.ClearRenderTargetView(rtv, &[0.0; 4]);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: g.canvas_width as f32, Height: g.canvas_height as f32, MaxDepth: 1.0, ..Default::default() }]));
            ctx.RSSetState(self.p.raster.as_ref());
            ctx.OMSetBlendState(self.p.blend.as_ref(), None, 0xffff_ffff);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetIndexBuffer(ib, DXGI_FORMAT_R16_UINT, 0);
            let vs = if g.style == effect::STYLE_CLASSIC {
                &self.p.classic_vs
            } else if g.style == effect::STYLE_SCALE || shatter {
                &self.p.scale_vs
            } else {
                &self.p.genie_vs
            };
            if shatter {
                // The shadow stays where the window was (the scale warp with no end of
                // its own) and fades out while the shards come loose.
                c.icon = c.win;
                c.params[3] = 1.0 - (t / 0.3).min(1.0);
            }
            ctx.VSSetShader(vs.as_ref(), None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
            ctx.PSSetShaderResources(0, Some(&[a.snap.srv.clone()]));
            ctx.PSSetSamplers(0, Some(&[self.p.sampler.clone()]));

            // The shadow first, under the window: the same look the real window has, so
            // neither end of the animation shows it appear or vanish. In snapshot px, like
            // the corners.
            if g.shadow_scale > 0.0 {
                let l = effect::window_shadow(g.shadow_scale);
                let px = if g.win.width > 0.0 { snap_w / g.win.width } else { 1.0 };
                c.shadow = [l.margin as f32 * px, l.sigma1 * px, l.dy1 * px, 0.0];
                c.shadow2 = [l.sigma2 * px, l.dy2 * px, l.op1, l.op2];
                upload(&c)?;
                ctx.PSSetShader(self.p.shadow_ps.as_ref(), None);
                ctx.DrawIndexed(index_count, 0, 0);
                c.shadow[0] = 0.0; // the window itself: the grid ends at its edges
            }
            if shatter {
                c.icon = [g.icon.x, g.icon.y, g.icon.width, g.icon.height];
                c.params[3] = effect::genie_fade(g, t);
                upload(&c)?;
                // Two triangles per crack-grid cell, three vertices each (ShatterVS).
                ctx.VSSetShader(self.p.shatter_vs.as_ref(), None);
                ctx.PSSetShader(self.p.shatter_ps.as_ref(), None);
                ctx.Draw((g.icon.x * g.icon.y) as u32 * 6, 0);
            } else {
                upload(&c)?;
                ctx.PSSetShader(self.p.genie_ps.as_ref(), None);
                ctx.DrawIndexed(index_count, 0, 0);
            }

            let swap = s.swap.as_ref().ok_or_else(missing)?;
            if let Some(msaa) = &s.msaa {
                let back: ID3D11Texture2D = swap.GetBuffer(0)?;
                ctx.ResolveSubresource(&back, 0, msaa, 0, DXGI_FORMAT_B8G8R8A8_UNORM);
            }
            swap.Present(0, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }

    fn retire_anim(&mut self, mut a: Anim) {
        if let Some(slot) = a.slot.take() {
            if self.slots[slot].broken {
                let s = self.slots.remove(slot);
                destroy_slot(&s);
                // Later slots moved down by one.
                for other in &mut self.anims {
                    if let Some(o) = &mut other.slot {
                        if *o > slot {
                            *o -= 1;
                        }
                    }
                }
            } else {
                unsafe {
                    let _ = ShowWindow(self.slots[slot].hwnd, SW_HIDE);
                }
                self.slots[slot].busy = false;
            }
        }
        let style = match a.params.style {
            effect::STYLE_CLASSIC => "classic",
            effect::STYLE_SCALE => "scale",
            effect::STYLE_SHATTER => "shatter",
            effect::STYLE_FRAGMENTS => "fragments",
            _ => "modern",
        };
        log!(
            "genie: {:?} {} ({style}): {} frames, {} reversal(s), {:.3} ms CPU/frame, first frame {:.1} ms after request",
            a.hwnd.0,
            if a.params.closing { "close" } else if a.rising { "restore/open" } else { "minimize" },
            a.frames,
            a.reversals,
            if a.frames > 0 { a.cpu_ms / a.frames as f64 } else { 0.0 },
            a.first_frame_ms
        );
        // Ended minimized: keep the snapshot for the matching restore.
        if !a.rising && !a.params.closing && a.snap.srv.is_some() && unsafe { IsWindow(a.hwnd).as_bool() } {
            lock(&self.shared.cache).put(a.hwnd, std::mem::take(&mut a.snap));
        }
        for w in a.waiters.drain(..) {
            self.shared.finish(w);
        }
    }

    fn abort_all(&mut self) {
        for a in std::mem::take(&mut self.anims) {
            self.retire_anim(a);
        }
        self.process_superseded(qpc(), true);
    }

    /// Paces frames on the compositor clock (Windows 11): exactly one frame per composed
    /// frame, no busy loop and no 2x-refresh overdraw. Returns false when woken by a new
    /// job instead of a tick.
    fn wait_for_tick(&self) -> bool {
        if let Some(wait) = self.wait_clock {
            // The loop drains the job queue at the top of every iteration, so the
            // (auto-reset) wake signal needs no re-arming.
            return unsafe { wait(1, &self.shared.wake, 100) } != WAIT_OBJECT_0.0;
        }
        unsafe {
            let _ = DwmFlush();
        }
        true
    }
}

fn destroy_slot(s: &Slot) {
    if !s.hwnd.is_invalid() {
        unsafe {
            let _ = DestroyWindow(s.hwnd);
        }
    }
}

fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
