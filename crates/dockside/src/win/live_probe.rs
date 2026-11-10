//! Tries every way of getting a live wallpaper's pixels and saves what each
//! one sees. With Wallpaper Engine (or Lively) running:
//! `cargo test live_probe -- --ignored --nocapture --test-threads 1`

use std::time::{Duration, Instant};

use windows::core::{w, Interface, Result, HSTRING};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{BOOL, GENERIC_WRITE, HMODULE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::*;

fn out_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join("dockside-live-probe");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn describe(hwnd: HWND) -> String {
    let mut pid = 0;
    let mut r = RECT::default();
    let ex;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let _ = GetWindowRect(hwnd, &mut r);
        ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    }
    let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
    let parent = unsafe { GetParent(hwnd).unwrap_or_default() };
    format!(
        "{:#x} class={:?} pid={pid} visible={visible} rect=({},{})-({},{}) ex={ex:#x} parent={:#x}",
        hwnd.0 as isize,
        crate::win::class_name(hwnd),
        r.left,
        r.top,
        r.right,
        r.bottom,
        parent.0 as isize
    )
}

/// Saves BGRA as PNG and reports how much is in it.
fn save(name: &str, w: u32, h: u32, stride: u32, px: &[u8]) {
    let (mut sum, mut n, mut colors, mut alpha) = (0f64, 0u64, std::collections::HashSet::new(), [0u32; 3]);
    for y in (0..h).step_by((h / 40).max(1) as usize) {
        for x in (0..w).step_by((w / 40).max(1) as usize) {
            let i = (y * stride + x * 4) as usize;
            sum += (px[i] as f64 + px[i + 1] as f64 + px[i + 2] as f64) / 3.0;
            n += 1;
            colors.insert((px[i] >> 3, px[i + 1] >> 3, px[i + 2] >> 3));
            alpha[match px[i + 3] { 0 => 0, 255 => 2, _ => 1 }] += 1;
        }
    }
    let path = out_dir().join(format!("{name}.png"));
    let r = (|| -> Result<()> {
        unsafe {
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let stream = wic.CreateStream()?;
            stream.InitializeFromFilename(&HSTRING::from(path.to_string_lossy().as_ref()), GENERIC_WRITE.0)?;
            let enc = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame.unwrap();
            frame.Initialize(None)?;
            frame.SetSize(w, h)?;
            let mut fmt = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut fmt)?;
            let mut rgb = Vec::with_capacity((w * h * 3) as usize);
            for y in 0..h {
                for x in 0..w {
                    let i = (y * stride + x * 4) as usize;
                    rgb.extend_from_slice(&px[i..i + 3]);
                }
            }
            frame.WritePixels(h, w * 3, &rgb)?;
            frame.Commit()?;
            enc.Commit()?;
        }
        Ok(())
    })();
    println!("  {name}: {w}x{h} mean={:.0} distinct={} alpha 0/partial/255={alpha:?} -> {} {:?}", sum / n.max(1) as f64, colors.len(), path.display(), r.err());
}

fn print_window(name: &str, hwnd: HWND, flags: u32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    let (w, h) = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(screen);
        let mut bits = std::ptr::null_mut();
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: 40, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() },
            ..Default::default()
        };
        let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
        let old = SelectObject(dc, bmp);
        let start = Instant::now();
        let ok = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(flags)).as_bool();
        let took = start.elapsed();
        let px = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        println!("  PrintWindow({name}, {flags}) ok={ok} in {took:?}");
        save(&format!("printwindow-{name}-{flags}"), w as u32, h as u32, w as u32 * 4, &px);
    }
}

fn wgc(name: &str, device: &ID3D11Device, item: Result<GraphicsCaptureItem>) {
    let r = (|| -> Result<()> {
        let item = item?;
        let dxgi: IDXGIDevice = device.cast()?;
        let winrt: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;
        let start = Instant::now();
        let frame = loop {
            if let Ok(f) = pool.TryGetNextFrame() {
                break f;
            }
            if start.elapsed() > Duration::from_secs(3) {
                println!("  WGC {name}: no frame in 3 s");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(30));
        };
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging))? };
        let staging = staging.unwrap();
        let ctx = unsafe { device.GetImmediateContext()? };
        unsafe {
            ctx.CopyResource(&staging, &tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let px = std::slice::from_raw_parts(m.pData as *const u8, (m.RowPitch * desc.Height) as usize).to_vec();
            ctx.Unmap(&staging, 0);
            save(&format!("wgc-{name}"), desc.Width, desc.Height, m.RowPitch, &px);
        }
        session.Close()?;
        pool.Close()?;
        Ok(())
    })();
    if let Err(e) = r {
        println!("  WGC {name}: {e}");
    }
}

unsafe extern "system" fn add(h: HWND, lp: LPARAM) -> BOOL {
    (*(lp.0 as *mut Vec<HWND>)).push(h);
    BOOL(1)
}

#[test]
#[ignore]
fn live_probe() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    println!("live wallpaper detected: {}", super::wallpaper::is_live());

    // The window tree behind the desktop.
    let progman = unsafe { FindWindowW(w!("Progman"), None).unwrap_or_default() };
    let mut desktop = vec![progman];
    let mut worker = HWND::default();
    while let Ok(wk) = unsafe { FindWindowExW(None, worker, w!("WorkerW"), None) } {
        desktop.push(wk);
        worker = wk;
    }
    let mut children: Vec<HWND> = Vec::new();
    println!("desktop windows:");
    for &d in &desktop {
        println!("  top {}", describe(d));
        let mut c: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumChildWindows(d, Some(add), LPARAM(&mut c as *mut _ as isize));
        }
        for h in c {
            println!("    child {}", describe(h));
            children.push(h);
        }
    }
    let mut explorer = 0;
    unsafe { GetWindowThreadProcessId(progman, Some(&mut explorer)) };
    let foreign: Vec<HWND> = children
        .iter()
        .copied()
        .filter(|&h| {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
            pid != explorer && pid != std::process::id()
        })
        .collect();

    // GDI: PrintWindow, plain and with PW_RENDERFULLCONTENT.
    println!("PrintWindow:");
    for (i, &h) in desktop.iter().enumerate() {
        print_window(&format!("desktop{i}"), h, 2);
    }
    for (i, &h) in foreign.iter().enumerate() {
        print_window(&format!("live{i}"), h, 0);
        print_window(&format!("live{i}"), h, 2);
    }

    // Windows.Graphics.Capture.
    let mut device = None;
    unsafe {
        D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut device), None, None).unwrap();
    }
    let device = device.unwrap();
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().unwrap();
    println!("Windows.Graphics.Capture:");
    for (i, &h) in desktop.iter().enumerate() {
        wgc(&format!("desktop{i}"), &device, unsafe { interop.CreateForWindow(h) });
    }
    for (i, &h) in foreign.iter().enumerate() {
        wgc(&format!("live{i}"), &device, unsafe { interop.CreateForWindow(h) });
    }
    dirty_rate(&device, unsafe { interop.CreateForWindow(progman) });
    let monitor = unsafe { MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY) };
    wgc("monitor", &device, unsafe { interop.CreateForMonitor(monitor) });
}

/// Counts frames and their dirty regions over two seconds of a capture.
fn dirty_rate(device: &ID3D11Device, item: Result<GraphicsCaptureItem>) {
    use windows::Graphics::Capture::GraphicsCaptureDirtyRegionMode;
    let r = (|| -> Result<()> {
        let item = item?;
        let dxgi: IDXGIDevice = device.cast()?;
        let winrt: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, item.Size()?)?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsBorderRequired(false);
        let dirty = session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportOnly).is_ok();
        // Time to show the desktop (Win+D) so the wallpaper animates.
        std::thread::sleep(Duration::from_secs(10));
        session.StartCapture()?;
        let start = Instant::now();
        let (mut frames, mut empty, mut area) = (0, 0, 0i64);
        while start.elapsed() < Duration::from_secs(4) {
            if let Ok(f) = pool.TryGetNextFrame() {
                frames += 1;
                if let Ok(rects) = f.DirtyRegions() {
                    let v: Vec<_> = rects.into_iter().collect();
                    if v.is_empty() {
                        empty += 1;
                    }
                    area += v.iter().map(|r| r.Width as i64 * r.Height as i64).sum::<i64>();
                }
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        println!("  progman over 4 s: {frames} frames, dirty mode {dirty}, {empty} with no dirty rects, mean dirty area {} px", area / frames.max(1) as i64);
        session.Close()?;
        pool.Close()?;
        Ok(())
    })();
    if let Err(e) = r {
        println!("  dirty rate: {e}");
    }
}
