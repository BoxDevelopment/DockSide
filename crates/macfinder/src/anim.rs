//! Vsync-paced frame ticks. A worker thread blocks on `DwmFlush` (which returns
//! once per compositor frame) and posts `WM_APP_FRAME` while animations run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::Thread;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Dwm::DwmFlush;

use crate::win::{post, WM_APP_FRAME};

/// Set while a frame message is in the queue, so a busy UI thread is never flooded.
pub static FRAME_PENDING: AtomicBool = AtomicBool::new(false);

pub struct FrameClock {
    active: Arc<AtomicBool>,
    thread: Thread,
}

impl FrameClock {
    pub fn new(target: isize) -> Self {
        let active = Arc::new(AtomicBool::new(false));
        let flag = active.clone();
        let handle = std::thread::Builder::new()
            .name("frame-clock".into())
            .spawn(move || loop {
                if !flag.load(Ordering::Acquire) {
                    std::thread::park();
                    continue;
                }
                let start = Instant::now();
                unsafe {
                    let _ = DwmFlush();
                }
                // DwmFlush returns immediately when nothing is composing (e.g. display off).
                if start.elapsed() < Duration::from_millis(2) {
                    std::thread::sleep(Duration::from_millis(16));
                }
                if !FRAME_PENDING.swap(true, Ordering::AcqRel) {
                    post(target, WM_APP_FRAME);
                }
            })
            .expect("spawn frame clock");
        Self { active, thread: handle.thread().clone() }
    }

    pub fn start(&self) {
        if !self.active.swap(true, Ordering::AcqRel) {
            self.thread.unpark();
        }
    }

    pub fn stop(&self) {
        self.active.store(false, Ordering::Release);
    }
}

/// Frame-rate independent exponential approach towards `target`.
pub fn approach(current: f32, target: f32, dt: f32, speed: f32) -> f32 {
    let v = current + (target - current) * (1.0 - (-dt * speed).exp());
    if (target - v).abs() < 0.002 {
        target
    } else {
        v
    }
}
