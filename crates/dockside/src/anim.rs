//! Vsync-paced frame ticks. A worker thread blocks on `DwmFlush` (which returns
//! once per compositor frame) and posts `WM_APP_FRAME` while animations run.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::Thread;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Dwm::DwmFlush;

use crate::geom::Rect;
use crate::win::{post, WM_APP_FRAME};

/// Set while a frame message is in the queue, so a busy UI thread is never flooded.
pub static FRAME_PENDING: AtomicBool = AtomicBool::new(false);

/// Windows' "Animation effects" setting: when it's off, everything jumps
/// straight to where it's going (Apple: "Make motion optional").
static MOTION: AtomicBool = AtomicBool::new(true);

pub fn set_motion(on: bool) {
    MOTION.store(on, Ordering::Relaxed);
}

pub fn motion_enabled() -> bool {
    MOTION.load(Ordering::Relaxed)
}

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
    if !motion_enabled() {
        return target;
    }
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
        if !motion_enabled() {
            self.snap(target);
        }
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
        if !motion_enabled() {
            self.snap(self.target);
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

/// Spring presets, as `(response, damping)`. Kept brief: motion should help
/// people follow a change, never make them wait for it.
pub mod feel {
    /// A control's thumb or pill moving to a new choice: quick, no wobble.
    pub const SLIDE: (f32, f32) = (0.2, 0.92);
    /// Things growing or shrinking in place (fields, bars, panels).
    pub const SMOOTH: (f32, f32) = (0.2, 1.0);
    /// A highlight letting go once the pointer leaves. (It appears instantly.)
    pub const LET_GO: (f32, f32) = (0.1, 1.0);
}

/// Springs keyed by UI element, for immediate-mode drawing: a render asks
/// where something is *now* while saying where it should be, and the spring
/// handles the rest. A key starts at its first target (nothing animates in on
/// first draw); keys a render stops asking for are dropped on the next one.
pub struct Motion<K> {
    springs: RefCell<HashMap<K, (Spring, bool)>>,
}

impl<K: Copy + Eq + Hash> Default for Motion<K> {
    fn default() -> Self {
        Self { springs: RefCell::new(HashMap::new()) }
    }
}

impl<K: Copy + Eq + Hash> Motion<K> {
    /// Where `key` is now, heading for `target`.
    pub fn get(&self, key: K, target: f32, feel: (f32, f32)) -> f32 {
        let mut map = self.springs.borrow_mut();
        let (s, seen) = map.entry(key).or_insert_with(|| (Spring::new(target, feel.0, feel.1), true));
        *seen = true;
        if s.target != target {
            s.response = feel.0;
            s.damping = feel.1;
            s.set(target);
        }
        s.value
    }

    /// A 0..1 hover highlight for `key`. It appears the moment the pointer
    /// arrives (direct feedback never lags) and lets go in a blink.
    pub fn fade(&self, key: K, on: bool) -> f32 {
        if on {
            self.get(key, 1.0, feel::LET_GO);
            self.snap(key, 1.0);
            return 1.0;
        }
        self.get(key, 0.0, feel::LET_GO).clamp(0.0, 1.0)
    }

    /// Jumps `key` to `value` (e.g. a list that was replaced, not changed).
    pub fn snap(&self, key: K, value: f32) {
        if let Some((s, _)) = self.springs.borrow_mut().get_mut(&key) {
            s.snap(value);
        }
    }

    /// Call at the start of a render: forgets keys the previous render didn't use.
    pub fn sweep(&self) {
        self.springs.borrow_mut().retain(|_, (_, seen)| std::mem::take(seen));
    }

    /// Advances every spring; returns true while any is still moving.
    pub fn tick(&self, dt: f32) -> bool {
        let mut moving = false;
        for (s, _) in self.springs.borrow_mut().values_mut() {
            moving |= s.step(dt);
        }
        moving
    }

    pub fn moving(&self) -> bool {
        self.springs.borrow().values().any(|(s, _)| !s.settled())
    }
}

/// How fast a glide follows its target: quick enough to keep up with a fast
/// sweep, smooth enough that it never blinks from row to row.
const GLIDE: (f32, f32) = (0.12, 1.0);

/// A highlight that glides from item to item as the pointer (or keyboard)
/// moves it, so a quick sweep across a list reads as one fluid motion instead
/// of a string of blinks over whichever rows Windows happened to report. It
/// appears in place, never sliding in from a stale spot, and lets go quickly.
pub struct Glide {
    x: Spring,
    y: Spring,
    w: Spring,
    h: Spring,
    alpha: Spring,
}

impl Default for Glide {
    fn default() -> Self {
        Self::with_feel(GLIDE)
    }
}

impl Glide {
    fn with_feel(feel: (f32, f32)) -> Self {
        let s = Spring::new(0.0, feel.0, feel.1);
        Self { x: s, y: s, w: s, h: s, alpha: Spring::new(0.0, self::feel::LET_GO.0, self::feel::LET_GO.1) }
    }

    /// For a selection (a sidebar's current page): a touch more deliberate
    /// than a hover, since it can travel further.
    pub fn selection() -> Self {
        Self::with_feel((0.17, 1.0))
    }

    /// Points the highlight at `target` (`None` lets go) and returns where it
    /// is now and how opaque, or `None` once it's gone.
    pub fn update(&mut self, target: Option<Rect>) -> Option<(Rect, f32)> {
        match target {
            Some(r) if self.alpha.value < 0.05 => {
                self.x.snap(r.x);
                self.y.snap(r.y);
                self.w.snap(r.w);
                self.h.snap(r.h);
                self.alpha.snap(1.0);
            }
            Some(r) => {
                self.x.set(r.x);
                self.y.set(r.y);
                self.w.set(r.w);
                self.h.set(r.h);
                self.alpha.snap(1.0);
            }
            None => self.alpha.set(0.0),
        }
        let a = self.alpha.value.clamp(0.0, 1.0);
        (a > 0.004).then(|| (Rect::new(self.x.value, self.y.value, self.w.value, self.h.value), a))
    }

    /// Forgets where it was, so the next target is shown in place (the list
    /// under it was replaced, not moved).
    pub fn reset(&mut self) {
        self.alpha.snap(0.0);
    }

    /// Advances; returns true while it's moving.
    pub fn tick(&mut self, dt: f32) -> bool {
        let mut moving = false;
        for s in [&mut self.x, &mut self.y, &mut self.w, &mut self.h, &mut self.alpha] {
            moving |= s.step(dt);
        }
        moving
    }

    pub fn moving(&self) -> bool {
        [&self.x, &self.y, &self.w, &self.h, &self.alpha].iter().any(|s| !s.settled())
    }
}

/// A wheel notch's glide is a critically damped spring this quick: it picks up
/// speed smoothly (no jump), carries its momentum when notches pile up, and
/// comes to rest about 0.12s after the last one.
const SCROLL_RESPONSE: f32 = 0.15;

/// Rate of the browser-style ease-out: a notch covers most of its distance in
/// the first ~80ms and settles by ~250ms, like Chromium's wheel animation.
const SCROLL_EASE_OUT: f32 = 22.0;

/// Smooth scrolling, the macOS way. A wheel notch glides the content to its new
/// spot along a smooth curve that starts on the very next frame; notches in
/// quick succession keep it moving rather than restarting it. A touchpad's
/// fine-grained deltas follow the fingers directly. Pushing past either end
/// stretches the content a little and springs it back.
#[derive(Clone, Copy)]
pub struct Scroll {
    target: f32,
    shown: f32,
    velocity: f32,
    bounce: Spring,
    /// Glides the browser way ([`Self::wheel_smooth`]) rather than on the spring.
    ease_out: bool,
}

impl Default for Scroll {
    fn default() -> Self {
        Self { target: 0.0, shown: 0.0, velocity: 0.0, bounce: Spring::new(0.0, 0.32, 1.0), ease_out: false }
    }
}

impl Scroll {
    /// Where it's heading (for keeping things in view, saving state).
    pub fn target(&self) -> f32 {
        self.target
    }

    /// The offset to draw content at: the eased position less any stretch.
    pub fn offset(&self) -> f32 {
        self.shown - self.bounce.value
    }

    /// [`Self::offset`] snapped to whole device pixels (at `scale`), so text
    /// stays crisp instead of shimmering as it moves.
    pub fn offset_px(&self, scale: f32) -> f32 {
        (self.offset() * scale).round() / scale
    }

    /// Applies a `WM_MOUSEWHEEL` delta: `per_notch` points per 120, within `0..=max`.
    pub fn wheel(&mut self, delta: i32, per_notch: f32, max: f32) {
        self.ease_out = false;
        let amount = delta as f32 / 120.0 * per_notch;
        let want = self.target - amount;
        self.target = want.clamp(0.0, max.max(0.0));
        let touchpad = delta % 120 != 0;
        if touchpad || !motion_enabled() {
            self.shown = self.target;
            self.velocity = 0.0;
        } else if self.velocity == 0.0 {
            // From rest, draw the glide's first frame now rather than a frame late.
            self.glide(1.0 / 60.0);
        }
        if want != self.target {
            let push = if touchpad { 8.0 } else { 4.0 };
            self.bounce.velocity = (self.bounce.velocity + amount * push).clamp(-900.0, 900.0);
            self.bounce.set(0.0);
        }
    }

    /// Browser-style scrolling: a wheel notch sets off at full speed and eases
    /// out to its new spot, and notches in quick succession just push the
    /// destination further. It stops at the ends rather than stretching. A
    /// touchpad's fine-grained deltas follow the fingers directly.
    pub fn wheel_smooth(&mut self, delta: i32, per_notch: f32, max: f32) {
        self.ease_out = true;
        let amount = delta as f32 / 120.0 * per_notch;
        self.target = (self.target - amount).clamp(0.0, max.max(0.0));
        self.velocity = 0.0;
        self.bounce.snap(0.0);
        if delta % 120 != 0 || !motion_enabled() {
            self.shown = self.target;
        } else {
            // Draw the first step now rather than a frame late.
            self.glide(1.0 / 120.0);
        }
    }

    /// Eases to `pos` (bringing a selected item into view).
    pub fn to(&mut self, pos: f32) {
        self.target = pos.max(0.0);
        if !motion_enabled() {
            self.shown = self.target;
        }
    }

    /// Jumps to `pos` with no motion (somewhere else entirely: a new folder or page).
    pub fn jump(&mut self, pos: f32) {
        self.target = pos.max(0.0);
        self.shown = self.target;
        self.velocity = 0.0;
        self.bounce.snap(0.0);
    }

    /// Keeps it within `0..=max` after the content changed size.
    pub fn clamp(&mut self, max: f32) {
        let max = max.max(0.0);
        self.target = self.target.clamp(0.0, max);
        self.shown = self.shown.clamp(0.0, max);
    }

    /// Advances; returns true while moving.
    pub fn tick(&mut self, dt: f32) -> bool {
        let mut moving = self.bounce.step(dt);
        if self.shown != self.target || self.velocity != 0.0 {
            self.glide(dt);
            moving = true;
        }
        moving
    }

    pub fn moving(&self) -> bool {
        self.shown != self.target || self.velocity != 0.0 || !self.bounce.settled()
    }

    fn glide(&mut self, dt: f32) {
        if !motion_enabled() {
            self.shown = self.target;
            self.velocity = 0.0;
            return;
        }
        if self.ease_out {
            let rest = self.target - self.shown;
            let left = rest * (-dt * SCROLL_EASE_OUT).exp();
            self.shown = if left.abs() < 0.25 { self.target } else { self.target - left };
            return;
        }
        let omega = std::f32::consts::TAU / SCROLL_RESPONSE;
        let steps = (dt / (1.0 / 240.0)).ceil().max(1.0) as u32;
        let h = dt / steps as f32;
        for _ in 0..steps {
            let accel = -omega * omega * (self.shown - self.target) - 2.0 * omega * self.velocity;
            self.velocity += accel * h;
            self.shown += self.velocity * h;
        }
        // Settle in absolute terms (points), so long lists land without a final snap.
        if (self.target - self.shown).abs() < 0.25 && self.velocity.abs() < 6.0 {
            self.shown = self.target;
            self.velocity = 0.0;
        }
    }
}

/// Linear interpolation from `a` to `b`.
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::{Motion, Spring};

    #[test]
    fn motion_starts_at_target_then_animates() {
        let m: Motion<u8> = Motion::default();
        assert_eq!(m.get(1, 5.0, super::feel::SLIDE), 5.0);
        assert!(!m.moving());
        assert_eq!(m.get(1, 10.0, super::feel::SLIDE), 5.0);
        assert!(m.moving());
        for _ in 0..240 {
            m.tick(1.0 / 60.0);
        }
        assert_eq!(m.get(1, 10.0, super::feel::SLIDE), 10.0);
        // Hovers appear at once and let go over a few frames.
        assert_eq!(m.fade(2, true), 1.0);
        assert!(m.fade(2, false) > 0.9);
        // Unused keys are forgotten after a render that skips them.
        m.sweep();
        m.sweep();
        assert_eq!(m.get(1, 3.0, super::feel::SLIDE), 3.0);
    }

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
    fn glide_appears_in_place_then_follows() {
        use crate::geom::Rect;
        let mut g = super::Glide::default();
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(0.0, 40.0, 10.0, 10.0);
        // First appearance: right where it's pointed, fully opaque.
        let (r, alpha) = g.update(Some(a)).unwrap();
        assert_eq!((r.y, alpha), (0.0, 1.0));
        // Then it glides rather than jumps.
        g.update(Some(b));
        g.tick(1.0 / 60.0);
        let y = g.update(Some(b)).unwrap().0.y;
        assert!(y > 0.0 && y < 40.0, "y {y}");
        for _ in 0..60 {
            g.tick(1.0 / 60.0);
        }
        assert_eq!(g.update(Some(b)).unwrap().0.y, 40.0);
        // Letting go fades it out.
        g.update(None);
        for _ in 0..60 {
            g.tick(1.0 / 60.0);
        }
        assert!(g.update(None).is_none());
    }

    #[test]
    fn wheel_eases_and_touchpad_follows() {
        let mut s = super::Scroll::default();
        s.wheel(-120, 60.0, 1000.0);
        assert_eq!(s.target(), 60.0);
        // Moves immediately, but only a little: no jump, a smooth start.
        assert!(s.offset() > 0.0 && s.offset() < 20.0, "{}", s.offset());
        // Picks up speed, never overshooting.
        let mut last = s.offset();
        for _ in 0..30 {
            s.tick(1.0 / 60.0);
            assert!(s.offset() >= last && s.offset() <= 60.0);
            last = s.offset();
        }
        assert_eq!(s.offset(), 60.0);
        // Touchpad deltas land exactly.
        s.wheel(-30, 60.0, 1000.0);
        assert_eq!(s.offset(), 75.0);
        // Past the top: clamped, with a stretch that springs back.
        s.jump(0.0);
        s.wheel(120, 60.0, 1000.0);
        assert_eq!(s.target(), 0.0);
        s.tick(1.0 / 60.0);
        assert!(s.offset() < 0.0);
        for _ in 0..120 {
            s.tick(1.0 / 60.0);
        }
        assert_eq!(s.offset(), 0.0);
    }

    #[test]
    fn smooth_wheel_eases_out_and_stops_at_ends() {
        let mut s = super::Scroll::default();
        s.wheel_smooth(-120, 60.0, 1000.0);
        // Sets off at once, fastest first, never overshooting.
        assert!(s.offset() > 5.0, "{}", s.offset());
        let mut last = s.offset();
        let mut step = f32::MAX;
        for _ in 0..60 {
            s.tick(1.0 / 165.0);
            assert!(s.offset() >= last && s.offset() - last <= step + 0.25 && s.offset() <= 60.0);
            step = s.offset() - last;
            last = s.offset();
        }
        assert_eq!(s.offset(), 60.0);
        // A second notch mid-glide carries on from where it is.
        s.wheel_smooth(-120, 60.0, 1000.0);
        s.wheel_smooth(-120, 60.0, 1000.0);
        assert_eq!(s.target(), 180.0);
        assert!(s.offset() > 60.0 && s.offset() < 180.0);
        // Past the top: no stretch.
        s.jump(0.0);
        s.wheel_smooth(120, 60.0, 1000.0);
        s.tick(1.0 / 165.0);
        assert_eq!(s.offset(), 0.0);
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
