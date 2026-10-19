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

/// A damped spring, parameterised like SwiftUI's `.spring(response:dampingFraction:)`:
/// `response` is roughly the period in seconds, `damping` below 1.0 overshoots.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    response: f32,
    damping: f32,
}

impl Spring {
    pub const fn new(value: f32, response: f32, damping: f32) -> Self {
        Self { value, velocity: 0.0, target: value, response, damping }
    }

    pub fn set(&mut self, target: f32) {
        self.target = target;
    }

    /// Jumps to `value` with no motion.
    pub fn snap(&mut self, value: f32) {
        self.value = value;
        self.target = value;
        self.velocity = 0.0;
    }

    pub fn with(mut self, response: f32, damping: f32) -> Self {
        self.response = response;
        self.damping = damping;
        self
    }

    pub fn settled(&self) -> bool {
        self.value == self.target && self.velocity == 0.0
    }

    /// Advances by `dt` seconds; returns true while still moving.
    pub fn step(&mut self, dt: f32) -> bool {
        if self.settled() {
            return false;
        }
        let omega = std::f32::consts::TAU / self.response.max(0.01);
        let (k, c) = (omega * omega, 2.0 * self.damping * omega);
        // Fixed small substeps keep stiff springs stable at any frame rate.
        let steps = (dt / (1.0 / 240.0)).ceil().max(1.0) as u32;
        let h = dt / steps as f32;
        for _ in 0..steps {
            let accel = -k * (self.value - self.target) - c * self.velocity;
            self.velocity += accel * h;
            self.value += self.velocity * h;
        }
        let scale = self.target.abs().max(1.0);
        if (self.target - self.value).abs() < 0.002 * scale && self.velocity.abs() < 0.02 * scale {
            self.snap(self.target);
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::Spring;

    #[test]
    fn spring_settles_on_target() {
        let mut s = Spring::new(0.0, 0.4, 0.7);
        s.set(100.0);
        let mut peak = 0.0_f32;
        for _ in 0..240 {
            s.step(1.0 / 60.0);
            peak = peak.max(s.value);
        }
        assert!(s.settled());
        assert_eq!(s.value, 100.0);
        // Under-damped: overshoots a little, like the island's bounce.
        assert!(peak > 100.5 && peak < 115.0, "peak {peak}");
    }

    #[test]
    fn critically_damped_never_overshoots() {
        let mut s = Spring::new(0.0, 0.3, 1.0);
        s.set(1.0);
        for _ in 0..120 {
            s.step(1.0 / 60.0);
            assert!(s.value <= 1.0 + 1e-4);
        }
    }
}
