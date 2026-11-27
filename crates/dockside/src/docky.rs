//! Docky, DockSide's little friend, drawn in code to the concept art: a white,
//! soft-domed mochi body with a cobalt-blue outline, cool inner rim shading,
//! expressive navy eyes, and two plump mitten paws sitting at his bottom corners.
//!
//! * [`Pose`]: everything that changes as he moves: his eyes, lift, squash,
//!   upper-body lean and tilt, whether he's curled up asleep, his paws, and the
//!   extras around him (sparkle dashes, jump arcs, rising Zs, a heart).
//! * [`Pet`]: what he's doing. He acts in short bursts (a blink, a look
//!   around, a jump), each a smooth physical curve sampled by time, blending
//!   cleanly whenever one interrupts another; between them he's a still picture.
//! * [`draw`] / [`draw_pixel`]: the smooth Docky and the pixel-art one.
//! * [`draw_tile`] / [`draw_icon`]: DockSide's live animated Dock tile and
//!   static app icon (`assets/logo-256.png`).

use std::f32::consts::{PI, TAU};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

use crate::geom::{Color, Rect};
use crate::gfx::{pt, Painter};

// ---------------------------------------------------------------------------
// Poses & curves
// ---------------------------------------------------------------------------

/// Where Docky is in a motion. Lengths are fractions of his width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    /// Eyes: 1 open ovals, 0 shut to a line (> 1 wide/surprised).
    pub open: f32,
    /// 0..1 towards happy curved `∩ ∩` arc eyes (Happy, Jump, Love, Dance).
    pub happy: f32,
    /// 0..1 towards squinted chevron `^ ^` eyes (Blink).
    pub squint: f32,
    /// 0..1 towards sleeping `‿ ‿` eyes.
    pub sleepy: f32,
    /// 0..1 towards curled up on his side, as in the concept's Sleep Widget.
    pub lying: f32,
    /// Where he's looking, -1..1 each way (y down).
    pub look: (f32, f32),
    /// Horizontal upper-body lean (-1..1), while planted paws stay grounded.
    pub lean: f32,
    /// How far he's lifted off the ground (negative is up).
    pub lift: f32,
    /// Above 1 wider and shorter, below 1 taller and thinner.
    pub squash: f32,
    /// Leaning, in degrees clockwise.
    pub tilt: f32,
    /// Each paw's offset from where it rests.
    pub paw_l: (f32, f32),
    pub paw_r: (f32, f32),
    /// 0..1: the two expressive dashes.
    pub sparkle: f32,
    /// 0..1: blends the dashes towards pure white (for Happy's bright pop).
    pub sparkle_bright: f32,
    /// 0..1: anchors the dashes beside his waving right paw instead of his head.
    pub sparkle_paw: f32,
    /// 0..1: tiny curved motion arcs by his shadow as he jumps.
    pub boing: f32,
    /// Which side of his head the dashes are on: 1 right, -1 left.
    pub side: f32,
    /// 0..1 through a pair of Zs rising and fading (0: none).
    pub zz: f32,
    /// 0..1 through a heart floating up (0: none).
    pub heart: f32,
    /// His shadow's size: 1 on the ground, smaller as he jumps.
    pub shadow: f32,
    /// Where he's walked to across his card: -1 the left end, 1 the right.
    pub walk: f32,
    /// What he's holding or wearing.
    pub prop: Prop,
    /// The prop's motion, 0..1: a sip of coffee, a page turning, a note rising.
    pub prop_t: f32,
    /// Reading glasses on.
    pub glasses: bool,
    /// 0..1: a drop of sweat on his brow.
    pub sweat: f32,
}

/// Something Docky holds or wears for an activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prop {
    None,
    Mug,
    Book,
    Headphones,
    Laptop,
}

/// Something Docky spends a while doing (an Auto Docky picks them himself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Waddles to another spot on his card.
    Walk,
    /// Holds a mug, sipping now and then.
    Coffee,
    /// Glasses on, a book open, turning a page now and then.
    Read,
    /// Curls up for a while.
    Nap,
    /// Headphones on, grooving (while music plays).
    Music,
    /// Tapping away at a laptop (while you type).
    Laptop,
}

impl Pose {
    pub const IDLE: Pose = Pose {
        open: 1.0,
        happy: 0.0,
        squint: 0.0,
        sleepy: 0.0,
        lying: 0.0,
        look: (0.0, 0.0),
        lean: 0.0,
        lift: 0.0,
        squash: 1.0,
        tilt: 0.0,
        paw_l: (0.0, 0.0),
        paw_r: (0.0, 0.0),
        sparkle: 0.0,
        sparkle_bright: 0.0,
        sparkle_paw: 0.0,
        boing: 0.0,
        side: 1.0,
        zz: 0.0,
        heart: 0.0,
        shadow: 1.0,
        walk: 0.0,
        prop: Prop::None,
        prop_t: 0.0,
        glasses: false,
        sweat: 0.0,
    };
    /// Curled up on his side, eyes closed, Zs drifting up.
    pub const ASLEEP: Pose = Pose { open: 0.0, sleepy: 1.0, lying: 1.0, zz: 0.52, ..Pose::IDLE };
    /// Nodding off upright, paws at his corners (the concept's Animations Sleep tile).
    pub const DOZING: Pose = Pose { open: 0.0, sleepy: 1.0, squash: 1.03, zz: 0.52, ..Pose::IDLE };
    /// Happy to see you: bright dashes by his head.
    pub const HAPPY: Pose = Pose { sparkle: 1.0, sparkle_bright: 1.0, ..Pose::IDLE };

    /// Blends every continuous channel from `a` to `b` by `t` (0..1).
    pub fn lerp(a: Pose, b: Pose, t: f32) -> Pose {
        let t = t.clamp(0.0, 1.0);
        Pose {
            open: lerp(a.open, b.open, t),
            happy: lerp(a.happy, b.happy, t),
            squint: lerp(a.squint, b.squint, t),
            sleepy: lerp(a.sleepy, b.sleepy, t),
            lying: lerp(a.lying, b.lying, t),
            look: (lerp(a.look.0, b.look.0, t), lerp(a.look.1, b.look.1, t)),
            lean: lerp(a.lean, b.lean, t),
            lift: lerp(a.lift, b.lift, t),
            squash: lerp(a.squash, b.squash, t),
            tilt: lerp(a.tilt, b.tilt, t),
            paw_l: (lerp(a.paw_l.0, b.paw_l.0, t), lerp(a.paw_l.1, b.paw_l.1, t)),
            paw_r: (lerp(a.paw_r.0, b.paw_r.0, t), lerp(a.paw_r.1, b.paw_r.1, t)),
            sparkle: lerp(a.sparkle, b.sparkle, t),
            sparkle_bright: lerp(a.sparkle_bright, b.sparkle_bright, t),
            sparkle_paw: lerp(a.sparkle_paw, b.sparkle_paw, t),
            boing: lerp(a.boing, b.boing, t),
            side: if t < 0.5 { a.side } else { b.side },
            zz: lerp(a.zz, b.zz, t),
            heart: lerp(a.heart, b.heart, t),
            shadow: lerp(a.shadow, b.shadow, t),
            walk: lerp(a.walk, b.walk, t),
            prop: if t < 0.5 { a.prop } else { b.prop },
            prop_t: if a.prop == b.prop {
                lerp(a.prop_t, b.prop_t, t)
            } else if t < 0.5 {
                a.prop_t * (1.0 - t * 2.0)
            } else {
                b.prop_t * (t * 2.0 - 1.0)
            },
            glasses: if t < 0.5 { a.glasses } else { b.glasses },
            sweat: lerp(a.sweat, b.sweat, t),
        }
    }
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Quintic C² smoothstep: zero velocity and acceleration at both ends.
fn smoother(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Snappy ease-out with a gentle springy overshoot `s`, settling cleanly to 1.
fn ease_out_back(t: f32, s: f32) -> f32 {
    let t = t.clamp(0.0, 1.0) - 1.0;
    1.0 + (s + 1.0) * t * t * t + s * t * t
}

/// 0 before `a`, easing smoothly to 1 at `b`.
fn ramp(t: f32, a: f32, b: f32) -> f32 {
    smoother((t - a) / (b - a))
}

/// Up over `a`..`a + attack`, held, then down over `b - release`..`b`.
fn envelope(t: f32, a: f32, b: f32, attack: f32, release: f32) -> f32 {
    ramp(t, a, a + attack) * (1.0 - ramp(t, b - release, b))
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Something Docky does, from start to finish.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    /// A quick squint: `^ ^`.
    Blink,
    /// Leans and looks one way, then the other.
    Look,
    /// `∩ ∩`, bright dashes and two joyful little hops.
    Happy,
    /// Crouches, springs up with his paws spread wide, and cushions his landing.
    Jump,
    /// Settles down compactly for a moment, paws tucked in, breathing gently.
    Sit,
    /// Leans warmly and waves a paw at mid-cheek height with motion ticks.
    Wave,
    /// Happy, swaying gently as a heart floats up.
    Love,
    /// Music's playing: bobs to the beat, swaying side to side, paws tapping in turn.
    Dance,
    /// The pointer was shaken at him: he wobbles, eyes wide and spinning, then blinks it off.
    Dizzy,
    /// The computer's working hard: he pants, a drop of sweat on his brow.
    Hot,
    /// Falling asleep: a stretch-yawn, eyes drooping shut, curling onto his side.
    Doze,
    /// Waking up: uncurls, opens his eyes, and does a morning hop.
    Wake,
    /// Asleep: breathes slowly in and out as a pair of Zs drifts up.
    Snore,
    /// Asleep: lifts his sleepy head for a moment, peeks about, and curls back up.
    Stir,
}

impl Act {
    pub fn secs(self) -> f32 {
        match self {
            Act::Blink => 0.24,
            Act::Look => 2.3,
            Act::Happy => 1.25,
            Act::Jump => 1.08,
            Act::Sit => 2.5,
            Act::Wave => 1.75,
            Act::Love => 1.65,
            Act::Dance => 2.2,
            Act::Dizzy => 1.75,
            Act::Hot => 2.4,
            Act::Doze => 2.1,
            Act::Wake => 1.2,
            Act::Snore => 3.6,
            Act::Stir => 2.4,
        }
    }

    /// Where he is `t` seconds in, starting from (and ending at) `base`.
    pub fn pose(self, t: f32, base: Pose) -> Pose {
        let mut p = base;
        match self {
            Act::Blink => {
                let k = envelope(t, 0.0, 0.24, 0.06, 0.11);
                p.squint = p.squint.max(k);
                p.squash *= 1.0 + 0.022 * k;
                p.lift += 0.006 * k;
            }
            Act::Look => {
                // Eyes lead the head slightly for natural overlapping action.
                let eye_l = envelope(t, 0.06, 1.02, 0.18, 0.24);
                let eye_r = envelope(t, 1.00, 2.18, 0.20, 0.28);
                let body_l = envelope(t, 0.12, 1.06, 0.24, 0.26);
                let body_r = envelope(t, 1.06, 2.22, 0.26, 0.30);
                p.look = (0.9 * (eye_r - eye_l), -0.15 * (eye_l + eye_r));
                p.lean = 0.55 * (body_r - body_l);
                p.tilt = 3.2 * (body_r - body_l);
                p.squash *= 1.0 - 0.02 * (body_l + body_r);
                // Slight curious lift on the leading paw as he perks up.
                p.paw_r.1 -= 0.02 * body_r;
                p.paw_l.1 -= 0.02 * body_l;
                if eye_r >= eye_l {
                    p.side = 1.0;
                    p.sparkle = p.sparkle.max(envelope(t, 1.12, 2.05, 0.14, 0.24));
                } else if eye_l > 0.25 {
                    p.side = -1.0;
                    p.sparkle = p.sparkle.max(0.65 * envelope(t, 0.2, 0.88, 0.14, 0.20));
                }
            }
            Act::Happy => {
                let k = envelope(t, 0.0, 1.25, 0.10, 0.22);
                p.happy = p.happy.max(k);
                p.sparkle = p.sparkle.max(k);
                p.sparkle_bright = p.sparkle_bright.max(k);
                p.side = 1.0;
                // Two cheerful little hops with squash on cushion and stretch at apex.
                let hop = |t0: f32, dur: f32, amp: f32| -> (f32, f32) {
                    if t >= t0 && t < t0 + dur {
                        let u = (t - t0) / dur;
                        let air = (u * PI).sin().powf(0.82);
                        (-amp * air, -0.045 * (u * TAU).cos() * amp / 0.065)
                    } else {
                        (0.0, 0.0)
                    }
                };
                let (l1, s1) = hop(0.08, 0.46, 0.068);
                let (l2, s2) = hop(0.56, 0.46, 0.052);
                p.lift += (l1 + l2) * k;
                p.squash *= 1.0 + (s1 + s2) * k;
                p.shadow = 1.0 + 1.8 * (l1 + l2) * k;
                p.paw_l.1 += 0.45 * (l1 + l2) * k;
                p.paw_r.1 += 0.45 * (l1 + l2) * k;
            }
            Act::Jump => {
                // 1) Anticipation crouch, 2) Ballistic hang-time flight, 3) Cushioned landing.
                let crouch = envelope(t, 0.0, 0.22, 0.09, 0.08);
                let (air, vel) = if (0.16..0.72).contains(&t) {
                    let u = (t - 0.16) / 0.56;
                    ((u * PI).sin().powf(0.76), (u * PI).cos())
                } else {
                    (0.0, 0.0)
                };
                let land = if (0.68..1.08).contains(&t) {
                    let u = (t - 0.68) / 0.40;
                    (u * PI * 1.4).sin() * (1.0 - smooth(u))
                } else {
                    0.0
                };
                p.squash = 1.0 + 0.13 * crouch - 0.06 * air - 0.05 * vel.abs() * air + 0.11 * land;
                p.lift = 0.016 * crouch - 0.37 * air + 0.012 * land.max(0.0);
                // Gentle playful tilt in mid-air (pivots around body center, staying over shadow).
                p.tilt = -2.2 * air;
                p.shadow = (1.0 + 0.08 * crouch - 0.44 * air + 0.06 * land.max(0.0)).clamp(0.4, 1.15);
                p.happy = envelope(t, 0.10, 0.96, 0.10, 0.18);
                p.boing = envelope(t, 0.18, 0.64, 0.08, 0.18);
                // Paws spread wide on his sides at mid-body, with subtle secondary velocity lag.
                let lag = 0.035 * vel * air;
                p.paw_l = (-0.105 * air - 0.02 * crouch, -0.22 * air + lag);
                p.paw_r = (0.105 * air + 0.02 * crouch, -0.22 * air + lag);
            }
            Act::Sit => {
                let k = envelope(t, 0.0, 2.5, 0.34, 0.38);
                let breath = 0.5 - 0.5 * ((t - 0.3).max(0.0) * TAU / 1.8).cos();
                // Compact, polite tuck: narrower/rounder base, paws tucked warmly in.
                p.squash = 1.0 - 0.035 * k + 0.018 * breath * k;
                p.lift = 0.028 * k;
                p.paw_l = (0.072 * k, 0.008 * k);
                p.paw_r = (-0.072 * k, 0.008 * k);
                p.look.1 += 0.12 * k;
                // A gentle calm blink mid-sit so he never freezes like a statue.
                p.squint = p.squint.max(envelope(t, 1.12, 1.34, 0.06, 0.10));
            }
            Act::Wave => {
                let k = envelope(t, 0.0, 1.75, 0.22, 0.28);
                let lift_k = ease_out_back((t / 0.26).clamp(0.0, 1.0), 0.9) * (1.0 - ramp(t, 1.44, 1.75));
                let w = ((t - 0.14).max(0.0) * TAU * 2.5).sin();
                // Left paw stays planted; body leans warmly left while right paw waves at cheek height.
                p.lean = -0.28 * k;
                p.tilt = -5.2 * k + 1.4 * w * k;
                p.paw_r = ((0.085 + 0.048 * w) * lift_k, (-0.255 - 0.028 * w.abs()) * lift_k);
                p.sparkle = p.sparkle.max(k * (0.78 + 0.22 * w.abs()));
                p.sparkle_paw = k;
                p.side = 1.0;
                p.look = (0.18 * k, -0.06 * k);
            }
            Act::Love => {
                let k = envelope(t, 0.0, 1.65, 0.12, 0.22);
                let sway = (t * TAU * 1.2).sin();
                let bounce = (t * PI / 0.41).sin().abs();
                p.happy = k;
                p.heart = (t / 1.65).clamp(0.0, 1.0);
                p.lift = -0.042 * bounce * k;
                p.squash = 1.0 + 0.03 * (1.0 - bounce) * k;
                p.lean = 0.2 * sway * k;
                p.tilt = 2.6 * sway * k;
                p.paw_l.0 += 0.045 * k;
                p.paw_r.0 -= 0.045 * k;
            }
            Act::Dance => {
                let k = envelope(t, 0.0, 2.2, 0.18, 0.22);
                let beat = (t * TAU * 2.0).sin();
                let sway = (t * TAU).sin();
                p.happy = k;
                p.lift = -0.058 * beat.abs() * k;
                p.squash = 1.0 + 0.055 * (1.0 - beat.abs()) * k;
                p.lean = 0.42 * sway * k;
                p.tilt = 4.5 * sway * k;
                p.paw_l.1 = -0.11 * beat.max(0.0) * k;
                p.paw_r.1 = -0.11 * (-beat).max(0.0) * k;
            }
            Act::Dizzy => {
                let k = envelope(t, 0.0, 1.55, 0.16, 0.28);
                let decay = (-1.1 * (t / 1.55)).exp();
                let a = t * TAU * 2.4;
                p.look = (0.95 * a.cos() * k, 0.72 * a.sin() * k);
                p.lean = 0.55 * (t * TAU * 2.1).sin() * k * decay;
                p.tilt = 6.5 * (t * TAU * 2.1).sin() * k * decay;
                p.open = lerp(1.0, 1.14, k);
                p.paw_l.0 -= 0.035 * k;
                p.paw_r.0 += 0.035 * k;
                // Shakes it off with a quick blink at the end.
                p.squint = p.squint.max(envelope(t, 1.48, 1.72, 0.06, 0.10));
            }
            Act::Hot => {
                let k = envelope(t, 0.0, 2.4, 0.3, 0.4);
                // Quick little pants.
                let pant = (t * TAU * 3.0).sin();
                p.squash *= 1.0 + 0.04 * pant * k;
                p.open = lerp(p.open, 0.6, k);
                p.sweat = k;
                p.tilt += -3.0 * k;
            }
            Act::Doze => {
                // Stretch-yawn upright, droop lids into `‿ ‿`, then curl onto his side.
                let yawn = envelope(t, 0.05, 0.82, 0.24, 0.28);
                let lids = ramp(t, 0.45, 1.15);
                let curl = ramp(t, 0.92, 1.88);
                p.open = (1.0 - lids) * (1.0 + 0.08 * yawn);
                p.sleepy = base.sleepy * smooth((lids - 0.25) / 0.75);
                p.lying = base.lying * curl;
                p.squash = lerp(1.0, base.squash, curl) - 0.075 * yawn;
                p.lift = -0.028 * yawn;
                p.zz = base.zz * ramp(t, 1.65, 2.1);
                p.look = (0.0, 0.15 * lids);
            }
            Act::Wake => {
                // Uncurls from his side, opens his eyes, and does a cheerful morning hop.
                let uncurl = 1.0 - ramp(t, 0.0, 0.52);
                p.lying = uncurl;
                p.sleepy = smooth((uncurl - 0.2) / 0.8);
                p.open = 1.0 - uncurl;
                let hop = if (0.48..0.98).contains(&t) {
                    let u = (t - 0.48) / 0.50;
                    (u * PI).sin().powf(0.8)
                } else {
                    0.0
                };
                let land = envelope(t, 0.92, 1.16, 0.07, 0.14);
                p.lift = -0.16 * hop + 0.01 * land;
                p.squash = 1.0 - 0.05 * hop + 0.06 * land;
                p.shadow = 1.0 - 0.28 * hop;
                p.happy = envelope(t, 0.50, 1.05, 0.10, 0.18);
                p.sparkle = envelope(t, 0.50, 1.15, 0.12, 0.20);
                p.sparkle_bright = p.sparkle;
                p.paw_l.1 = -0.09 * hop;
                p.paw_r.1 = -0.09 * hop;
            }
            Act::Snore => {
                let breath = 0.5 - 0.5 * (t / 3.6 * TAU).cos();
                p.squash = base.squash + 0.036 * breath;
                p.lift = base.lift - 0.008 * breath;
                p.zz = (t / 3.6).clamp(0.0, 1.0);
            }
            Act::Stir => {
                let k = envelope(t, 0.0, 2.4, 0.42, 0.48);
                p.lying = base.lying * (1.0 - 0.75 * k);
                p.sleepy = base.sleepy * (1.0 - k);
                p.open = lerp(base.open, 0.78, k);
                p.zz = base.zz * (1.0 - k);
                p.sparkle = 0.75 * envelope(t, 0.38, 1.35, 0.16, 0.24);
                let look_l = envelope(t, 0.52, 1.18, 0.18, 0.20);
                let look_r = envelope(t, 1.18, 1.85, 0.18, 0.22);
                p.look = (0.65 * (look_r - look_l), 0.08 * k);
                p.lean = 0.25 * (look_r - look_l);
            }
        }
        p
    }
}

// ---------------------------------------------------------------------------
// What he's doing
// ---------------------------------------------------------------------------

const BLEND_SECS: f32 = 0.18;

pub struct Pet {
    asleep: bool,
    /// How often he does things on his own: 1 normal, less is calmer.
    energy: f32,
    act: Option<(Act, Instant)>,
    /// Smooth cross-fade when a new act or state interrupts whatever he was doing.
    blend: Option<(Pose, Instant)>,
    /// When he next does something on his own.
    next: Instant,
    /// Where his eyes are looking (-1..1 each way), springing towards `target`.
    gaze: (f32, f32),
    gaze_vel: (f32, f32),
    /// Where the pointer is from his eyes, while he can see it.
    aim: Option<(f32, f32)>,
    /// A quick look somewhere else (a window you just switched to), until then.
    glance: Option<((f32, f32), Instant)>,
    /// A little nod (each key you type), dying away.
    bob: f32,
    /// You've been away a while: his eyes are heavy.
    drowsy: bool,
    /// Music's playing: he dances more than anything.
    music: bool,
    /// What he's spending a while doing: since when, and until when (none: until stopped).
    doing: Option<(Activity, Instant, Option<Instant>)>,
    /// Where he stands across his card (-1..1), and a walk under way: from, to, start, seconds.
    pos: f32,
    walking: Option<(f32, f32, Instant, f32)>,
    /// At the laptop: which paw taps next.
    tap: bool,
    /// Whether he was moving at the last update (so the frame he stops on is drawn).
    was_moving: bool,
    /// He has a mind of his own: now and then he stops watching the pointer
    /// and looks off somewhere else for a while.
    bored: Option<((f32, f32), Instant)>,
    next_bored: Instant,
    /// Clicks before this are ignored (no frantic switching).
    poke_after: Instant,
    rng: u64,
    /// Nobody's watching: no new acts (see `set_still`).
    still: bool,
}

impl Default for Pet {
    fn default() -> Self {
        Self::new()
    }
}

impl Pet {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        Pet {
            asleep: false,
            energy: 1.0,
            act: None,
            blend: None,
            next: Instant::now() + Duration::from_millis(1200),
            gaze: (0.0, 0.0),
            gaze_vel: (0.0, 0.0),
            aim: None,
            glance: None,
            bob: 0.0,
            drowsy: false,
            music: false,
            doing: None,
            pos: 0.0,
            walking: None,
            tap: false,
            was_moving: false,
            bored: None,
            next_bored: Instant::now() + Duration::from_secs(8),
            poke_after: Instant::now(),
            still: false,
            rng: seed | 1,
        }
    }

    fn snapshot_blend(&mut self, now: Instant) {
        if crate::anim::motion_enabled() {
            let current = self.pose(now);
            self.blend = Some((current, now));
        }
    }

    fn blending(&self, now: Instant) -> bool {
        self.blend.is_some_and(|(_, t0)| now.saturating_duration_since(t0).as_secs_f32() < BLEND_SECS)
    }

    /// Puts him to sleep or wakes him; true when that changed anything.
    pub fn set_asleep(&mut self, on: bool) -> bool {
        if on == self.asleep {
            return false;
        }
        let now = Instant::now();
        self.snapshot_blend(now);
        self.asleep = on;
        // He dozes off and wakes up gently, never all at once.
        self.act = crate::anim::motion_enabled().then(|| (if on { Act::Doze } else { Act::Wake }, now));
        self.next = now + Duration::from_millis(600);
        true
    }

    /// "calm", "normal" or "playful".
    pub fn set_energy(&mut self, energy: &str) {
        self.energy = match energy {
            "calm" => 0.55,
            "playful" => 1.8,
            _ => 1.0,
        };
    }

    pub fn asleep(&self) -> bool {
        self.asleep
    }

    /// Where the pointer is from his eyes (-1..1 each way), or None when he
    /// can't see it. True when that's far enough from before to need frames.
    pub fn aim(&mut self, aim: Option<(f32, f32)>) -> bool {
        let moved = match (aim, self.aim) {
            // Under ~0.2 pt of eye travel isn't worth a frame.
            (Some(a), Some(b)) => (a.0 - b.0).abs() + (a.1 - b.1).abs() > 0.03,
            (a, b) => a.is_some() != b.is_some(),
        };
        if moved {
            self.aim = aim;
        }
        moved
    }

    /// Looks towards `at` (like `aim`) for a moment.
    pub fn glance(&mut self, at: (f32, f32)) {
        if !self.asleep && !self.busy() {
            self.glance = Some((at, Instant::now() + Duration::from_millis(1100)));
        }
    }

    /// A key was typed: at his laptop, he taps along (otherwise he lets you get on with it).
    pub fn nod(&mut self) {
        if !self.asleep && crate::anim::motion_enabled() && self.doing() == Some(Activity::Laptop) {
            self.bob = 1.0;
            self.tap = !self.tap;
        }
    }

    pub fn doing(&self) -> Option<Activity> {
        self.doing.map(|d| d.0)
    }

    /// Starts `a`, for `secs` (none: until stopped). Returns how long it'll take.
    pub fn start(&mut self, a: Activity, secs: Option<f32>) -> f32 {
        let now = Instant::now();
        self.snapshot_blend(now);
        self.stop_inner(now, false);
        if a == Activity::Nap {
            self.act = crate::anim::motion_enabled().then_some((Act::Doze, now));
        }
        if a == Activity::Walk {
            let mut to = (self.roll() * 2.0 - 1.0) * 0.95;
            if (to - self.pos).abs() < 0.45 {
                to = if self.pos > 0.0 { -0.6 - 0.35 * self.roll() } else { 0.6 + 0.35 * self.roll() };
            }
            let secs = ((to - self.pos).abs() / 0.28).max(1.3);
            self.walking = Some((self.pos, to, now, secs));
            self.doing = Some((a, now, Some(now + Duration::from_secs_f32(secs))));
            return secs;
        }
        self.doing = Some((a, now, secs.map(Duration::from_secs_f32).map(|d| now + d)));
        secs.unwrap_or(0.0)
    }

    /// Stops what he's doing (a walk stops where he's got to; a nap ends with him waking).
    pub fn stop(&mut self) {
        let now = Instant::now();
        self.snapshot_blend(now);
        self.stop_inner(now, true);
    }

    fn stop_inner(&mut self, now: Instant, wake_nap: bool) {
        if wake_nap && self.doing() == Some(Activity::Nap) && crate::anim::motion_enabled() {
            self.act = Some((Act::Wake, now));
        }
        self.pos = self.position(now);
        self.walking = None;
        self.doing = None;
    }

    /// Where he is across his card, -1..1.
    pub fn position(&self, now: Instant) -> f32 {
        match self.walking {
            Some((from, to, t0, secs)) => lerp(from, to, smoother(now.saturating_duration_since(t0).as_secs_f32() / secs)),
            None => self.pos,
        }
    }

    /// Whether what he's doing is moving right now (and so needs frames).
    fn active(&self, now: Instant) -> bool {
        let Some((a, t0, _)) = self.doing else { return false };
        let t = now.saturating_duration_since(t0).as_secs_f32();
        match a {
            Activity::Walk | Activity::Music => true,
            Activity::Coffee => (1.8..3.8).contains(&(t % 5.5)),
            Activity::Read => (5.8..7.0).contains(&(t % 7.0)),
            Activity::Nap | Activity::Laptop => false,
        }
    }

    /// Whether an act, transition blend, or activity is moving him now.
    pub fn moving(&self, now: Instant) -> bool {
        self.act.is_some() || self.blending(now) || self.active(now)
    }

    /// True when that changed how he looks.
    pub fn set_drowsy(&mut self, on: bool) -> bool {
        let changed = on != self.drowsy;
        self.drowsy = on;
        changed
    }

    pub fn set_music(&mut self, on: bool) {
        self.music = on;
    }

    /// Where his eyes are heading: a glance, his own thoughts, or the pointer.
    fn target(&self) -> (f32, f32) {
        if self.asleep {
            return (0.0, 0.0);
        }
        self.glance.map(|g| g.0).or(self.bored.map(|b| b.0)).or(self.aim).unwrap_or((0.0, 0.0))
    }

    /// Springs his eyes smoothly towards what he's looking at and lets a nod die away.
    /// True while either is still moving.
    pub fn ease(&mut self, dt: f32) -> bool {
        let now = Instant::now();
        if self.glance.is_some_and(|(_, until)| now >= until) {
            self.glance = None;
        }
        if self.bored.is_some_and(|(_, until)| now >= until) {
            self.bored = None;
            self.next_bored = now + Duration::from_secs_f32(6.0 + 12.0 * self.roll());
        }
        if self.bored.is_none() && self.aim.is_some() && now >= self.next_bored {
            let at = ((self.roll() * 2.0 - 1.0) * 0.9, (self.roll() * 2.0 - 1.0) * 0.5);
            self.bored = Some((at, now + Duration::from_secs_f32(2.5 + 3.5 * self.roll())));
        }
        let to = self.target();
        let from = self.gaze;
        if !crate::anim::motion_enabled() {
            self.gaze = to;
            self.gaze_vel = (0.0, 0.0);
            self.bob = 0.0;
            return self.gaze != from;
        }
        // Critically-damped 2D eye spring: silky velocity ramp with zero snap-jerk.
        let dt_clamped = dt.clamp(0.0, 0.05);
        let steps = (dt_clamped / (1.0 / 240.0)).ceil().max(1.0) as u32;
        let h = dt_clamped / steps as f32;
        let omega = TAU / 0.145;
        let (k, c) = (omega * omega, 1.96 * omega);
        for _ in 0..steps {
            let ax = -k * (self.gaze.0 - to.0) - c * self.gaze_vel.0;
            let ay = -k * (self.gaze.1 - to.1) - c * self.gaze_vel.1;
            self.gaze_vel.0 += ax * h;
            self.gaze_vel.1 += ay * h;
            self.gaze.0 += self.gaze_vel.0 * h;
            self.gaze.1 += self.gaze_vel.1 * h;
        }
        if (self.gaze.0 - to.0).hypot(self.gaze.1 - to.1) < 0.008 && self.gaze_vel.0.hypot(self.gaze_vel.1) < 0.05 {
            self.gaze = to;
            self.gaze_vel = (0.0, 0.0);
        }
        let bobbing = self.bob > 0.0;
        self.bob = crate::anim::approach(self.bob, 0.0, dt, 9.0);
        if self.bob < 0.01 {
            self.bob = 0.0;
        }
        self.gaze != from || bobbing
    }

    /// Whether his eyes or a nod are still on their way somewhere.
    pub fn settling(&self) -> bool {
        let to = self.target();
        self.bob > 0.0 || self.gaze != to || self.gaze_vel != (0.0, 0.0)
    }

    pub fn play(&mut self, act: Act) {
        if crate::anim::motion_enabled() {
            let now = Instant::now();
            self.snapshot_blend(now);
            self.act = Some((act, now));
        }
    }

    /// Clicked: something cheerful, or (asleep) a moment awake. Clicks come
    /// faster than he answers: false when this one was ignored.
    pub fn poke(&mut self) -> bool {
        let now = Instant::now();
        if now < self.poke_after {
            return false;
        }
        self.poke_after = now + Duration::from_millis(1350);
        if self.asleep {
            self.play(Act::Stir);
            return true;
        }
        let acts = [Act::Happy, Act::Jump, Act::Love, Act::Wave];
        let i = (self.roll() * acts.len() as f32) as usize;
        self.play(acts[i.min(acts.len() - 1)]);
        true
    }

    /// Ends a finished act and starts the next one when it's time. True while
    /// he's moving, and once more as he comes to rest, so that frame is drawn.
    pub fn update(&mut self, now: Instant) -> bool {
        let was = self.act.is_some() || self.was_moving || self.blending(now);
        if self.blend.is_some_and(|(_, t0)| now.saturating_duration_since(t0).as_secs_f32() >= BLEND_SECS) {
            self.blend = None;
        }
        if let Some((_, _, Some(until))) = self.doing {
            if now >= until {
                self.stop();
            }
        }
        if !crate::anim::motion_enabled() {
            self.act = None;
            self.blend = None;
            self.was_moving = false;
            return was;
        }
        if let Some((a, t0)) = self.act {
            if now.saturating_duration_since(t0).as_secs_f32() >= a.secs() {
                self.act = None;
                self.next = now + self.gap();
            }
        }
        if self.act.is_none() && now >= self.next && !self.still {
            let a = self.pick();
            self.act = Some((a, now));
        }
        let moving = self.moving(now);
        self.was_moving = moving;
        was || moving
    }

    /// Nobody's watching (he's asleep and you're away): he stops starting
    /// new things, so he's a still picture and costs nothing.
    pub fn set_still(&mut self, on: bool) {
        self.still = on;
    }

    /// How many frames a second his motion right now needs to look smooth:
    /// quick moves get ~60, slow ones (breathing, drifting Zs, sipping) ~30.
    pub fn fps(&self, now: Instant) -> f32 {
        let quick = match self.act {
            Some((a, _)) => !matches!(a, Act::Snore | Act::Stir | Act::Doze | Act::Sit | Act::Blink),
            None => false,
        };
        let walking = self.doing() == Some(Activity::Walk);
        let eyes = self.gaze_vel.0.hypot(self.gaze_vel.1) > 0.05 || self.bob > 0.0;
        if quick || walking || eyes || self.blending(now) {
            60.0
        } else {
            30.0
        }
    }

    /// Whether an act or transition is playing.
    pub fn busy(&self) -> bool {
        self.act.is_some() || self.blending(Instant::now())
    }

    pub fn pose(&self, now: Instant) -> Pose {
        let napping = self.doing() == Some(Activity::Nap);
        let mut base = if self.asleep || napping { Pose::ASLEEP } else { Pose::IDLE };
        if !self.asleep && !napping {
            base.look = self.gaze;
            // Subtle head parallax towards whatever he's watching while paws stay planted.
            base.lean = 0.18 * self.gaze.0;
            base.tilt = 1.1 * self.gaze.0;
            if self.drowsy {
                base.open = 0.52;
                base.sleepy = 0.2;
            }
            base.lift -= 0.035 * self.bob;
            base.squash += 0.03 * self.bob;
        }
        base.walk = self.position(now);
        if let (Some((a, t0, _)), false) = (self.doing, self.asleep) {
            base = self.activity_pose(a, now.saturating_duration_since(t0).as_secs_f32(), base);
        }
        let mut out = match self.act {
            Some((a, t0)) => a.pose(now.saturating_duration_since(t0).as_secs_f32().min(a.secs()), base),
            None => base,
        };
        if let Some((from, t0)) = self.blend {
            let u = now.saturating_duration_since(t0).as_secs_f32() / BLEND_SECS;
            if u < 1.0 {
                out = Pose::lerp(from, out, smoother(u));
            }
        }
        out
    }

    /// `base` as he goes about activity `a`, `t` seconds in.
    fn activity_pose(&self, a: Activity, t: f32, mut p: Pose) -> Pose {
        match a {
            Activity::Walk => {
                if let Some((from, to, _, secs)) = self.walking {
                    if t < secs {
                        let env = envelope(t, 0.0, secs, 0.22, 0.28);
                        let step = (t * TAU * 2.3).sin();
                        let dir = (to - from).signum();
                        p.lift -= 0.034 * step.abs() * env;
                        p.squash += 0.025 * (1.0 - step.abs()) * env;
                        p.paw_l.1 -= 0.065 * step.max(0.0) * env;
                        p.paw_r.1 -= 0.065 * (-step).max(0.0) * env;
                        p.paw_l.0 += 0.025 * step * dir * env;
                        p.paw_r.0 -= 0.025 * step * dir * env;
                        p.lean = (0.3 * dir + 0.16 * step) * env;
                        p.tilt = (2.4 * step + 1.8 * dir) * env;
                        p.look = (0.78 * dir * env, 0.04 * env);
                    }
                }
            }
            Activity::Coffee => {
                p.prop = Prop::Mug;
                let sip = envelope(t % 5.5, 1.9, 3.65, 0.42, 0.45);
                p.prop_t = sip;
                p.happy = p.happy.max(smooth((sip - 0.35) / 0.5));
                p.lean = -0.1 * sip;
            }
            Activity::Read => {
                p.prop = Prop::Book;
                p.glasses = true;
                p.look = (0.25 * self.gaze.0 + 0.18 * (t * TAU / 3.5).sin(), 0.55);
                let f = t % 7.0;
                p.prop_t = if f >= 5.9 { smoother(((f - 5.9) / 0.85).min(1.0)) } else { 0.0 };
                p.paw_l = (0.125, -0.045);
                p.paw_r = (-0.125, -0.045);
            }
            Activity::Music => {
                p.prop = Prop::Headphones;
                let beat = (t * TAU * 2.0).sin();
                let sway = (t * TAU).sin();
                p.happy = 1.0;
                p.lift -= 0.042 * beat.abs();
                p.squash += 0.032 * (1.0 - beat.abs());
                p.lean = 0.28 * sway;
                p.tilt = 3.8 * sway;
                p.prop_t = (t % 1.5) / 1.5;
            }
            Activity::Laptop => {
                p.prop = Prop::Laptop;
                p.look = (0.2 * self.gaze.0, 0.32);
                let tap = -0.075 * self.bob;
                if self.tap {
                    p.paw_l.1 += tap;
                    p.paw_l.0 += 0.03;
                    p.paw_r.0 -= 0.03;
                } else {
                    p.paw_r.1 += tap;
                    p.paw_l.0 += 0.03;
                    p.paw_r.0 -= 0.03;
                }
            }
            Activity::Nap => {}
        }
        p
    }

    /// 0..1, uniformly.
    fn roll(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 40) as f32 / (1u64 << 24) as f32
    }

    /// The pause before the next act.
    fn gap(&mut self) -> Duration {
        let secs = if self.asleep { 0.35 + 0.85 * self.roll() } else { (2.2 + 4.0 * self.roll()) / self.energy };
        Duration::from_secs_f32(secs)
    }

    fn pick(&mut self) -> Act {
        if self.asleep || self.doing() == Some(Activity::Nap) {
            return Act::Snore;
        }
        if self.doing.is_some() {
            return Act::Blink;
        }
        let r = self.roll();
        if self.music && r < 0.25 {
            return Act::Dance;
        }
        let fun = (0.44 * self.energy).min(0.75);
        if r > fun {
            return Act::Blink;
        }
        match (r / fun * 100.0) as u32 {
            0..=34 => Act::Look,
            35..=52 => Act::Happy,
            53..=68 => Act::Sit,
            69..=84 => Act::Jump,
            _ => Act::Wave,
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub body: Color,
    /// Soft cool periwinkle inner rim/belly shade.
    pub belly: Color,
    pub line: Color,
    pub eye: Color,
    /// Dashes, jump arcs and Zs.
    pub extra: Color,
    pub heart: Color,
    /// Transparent for none.
    pub shadow: Color,
}

const BLUE_HEART: Color = Color::rgba(66, 108, 238, 1.0);

impl Palette {
    /// On a pale background (the concept sheet's tiles).
    #[cfg_attr(not(test), allow(dead_code))]
    pub const LIGHT: Palette = Palette {
        body: Color::rgba(255, 255, 255, 1.0),
        belly: Color::rgba(222, 230, 252, 1.0),
        line: Color::rgba(72, 106, 214, 1.0),
        eye: Color::rgba(24, 35, 84, 1.0),
        extra: Color::rgba(82, 116, 224, 1.0),
        heart: BLUE_HEART,
        shadow: Color::rgba(104, 130, 218, 0.34),
    };
    /// On a blue widget card: a rich cobalt outline, crisp white dashes and Zs.
    pub const CARD: Palette = Palette {
        body: Color::rgba(255, 255, 255, 1.0),
        belly: Color::rgba(223, 231, 252, 1.0),
        line: Color::rgba(42, 76, 184, 1.0),
        eye: Color::rgba(22, 33, 82, 1.0),
        extra: Color::WHITE,
        heart: BLUE_HEART,
        shadow: Color::rgba(28, 52, 156, 0.30),
    };
    pub const fn shadowless(self) -> Palette {
        Palette { shadow: Color::rgba(0, 0, 0, 0.0), ..self }
    }
}

/// A rounded stroke: `Painter::line` already draws with `Gfx::round_stroke`
/// (round caps and round joins), avoiding double-blended endpoint dots.
fn dash(p: &Painter, (x0, y0): (f32, f32), (x1, y1): (f32, f32), c: Color, w: f32) {
    if c.a > 0.005 {
        p.line(x0, y0, x1, y1, c, w);
    }
}

/// Multi-stop vertical gradient fill on an arbitrary path geometry.
fn fill_geom_gradient(p: &Painter, g: &ID2D1PathGeometry, bounds: Rect, top: Color, mid: f32, bottom: Color) {
    let stops = [
        D2D1_GRADIENT_STOP { position: 0.0, color: top.d2d() },
        D2D1_GRADIENT_STOP { position: mid, color: top.mix(bottom, 0.16).d2d() },
        D2D1_GRADIENT_STOP { position: 1.0, color: bottom.d2d() },
    ];
    unsafe {
        let Ok(coll) = p.rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else {
            return p.fill_geometry(g, top);
        };
        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: pt(bounds.cx(), bounds.y), endPoint: pt(bounds.cx(), bounds.bottom()) };
        match p.rt.CreateLinearGradientBrush(&props, None, &coll) {
            Ok(brush) => p.rt.FillGeometry(g, &brush, None),
            Err(_) => p.fill_geometry(g, top),
        }
    }
}

/// Builds Docky's soft mochi-domed body path: generous continuous-curvature top
/// shoulders, subtle upward crown taper, optional horizontal upper lean, and a
/// flatter grounded base.
fn domed_body_path(p: &Painter, b: Rect, k_loaf: f32, lean_px: f32) -> Option<ID2D1PathGeometry> {
    let (l, r, top, bot, w, h) = (b.x, b.right(), b.y, b.bottom(), b.w, b.h);
    let rb = (h * lerp(0.21, 0.18, k_loaf)).min(w * 0.26);
    let rt = (h * lerp(0.36, 0.43, k_loaf)).min(w * 0.44);
    let taper = w * lerp(0.016, 0.032, k_loaf);
    let c_sq = 0.42;
    p.gfx.path(|sink| unsafe {
        sink.BeginFigure(pt(b.cx(), bot), D2D1_FIGURE_BEGIN_FILLED);
        // Bottom-right grounded squircle corner.
        sink.AddLine(pt(r - rb, bot));
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(r - rb * c_sq, bot),
            point2: pt(r, bot - rb * c_sq),
            point3: pt(r, bot - rb),
        });
        // Gently tapered right cheek into the plush top-right squircle shoulder.
        sink.AddLine(pt(r - taper + lean_px * 0.65, top + rt));
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(r - taper + lean_px * 0.85, top + rt * c_sq),
            point2: pt(r - taper - rt * c_sq + lean_px, top),
            point3: pt(r - taper - rt + lean_px, top),
        });
        // Softly arched crown.
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(b.cx() + rt * 0.25 + lean_px, top - h * 0.010),
            point2: pt(b.cx() - rt * 0.25 + lean_px, top - h * 0.010),
            point3: pt(l + taper + rt + lean_px, top),
        });
        // Top-left squircle shoulder down the left cheek.
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(l + taper + rt * c_sq + lean_px, top),
            point2: pt(l + taper + lean_px * 0.85, top + rt * c_sq),
            point3: pt(l + taper + lean_px * 0.65, top + rt),
        });
        sink.AddLine(pt(l, bot - rb));
        // Bottom-left grounded squircle corner.
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(l, bot - rb * c_sq),
            point2: pt(l + rb * c_sq, bot),
            point3: pt(l + rb, bot),
        });
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
    })
}

/// Fills and strokes a domed body with soft 3D inner belly rim shading.
fn draw_domed(p: &Painter, r: Rect, k_loaf: f32, lean_px: f32, lw: f32, pal: &Palette) {
    let Some(outer) = domed_body_path(p, r, k_loaf, lean_px) else { return };
    // Soft cool base fill, then a brighter inner upper-face highlight shifted
    // slightly up so a subtle periwinkle crescent wraps the lower rim and cheeks.
    fill_geom_gradient(p, &outer, r, pal.body.mix(pal.belly, 0.28), 0.45, pal.belly);
    let hi_rect = Rect::new(r.x + lw * 0.65, r.y + lw * 0.35, (r.w - lw * 1.3).max(1.0), (r.h - lw * 1.35).max(1.0));
    if let Some(hi) = domed_body_path(p, hi_rect, k_loaf, lean_px * 0.92) {
        fill_geom_gradient(p, &hi, hi_rect, pal.body, 0.68, pal.body.mix(pal.belly, 0.35));
    }
    p.stroke_geometry(&outer, pal.line, lw);
}

/// His head/body rect: `s` wide at rest, standing on `ground` under `cx`.
/// Curled up (`lying` -> 1), his head settles left onto the ground as his
/// tucked body loaf appears behind his right side.
pub fn body_rect(cx: f32, ground: f32, s: f32, pose: &Pose) -> Rect {
    let q = pose.squash.max(0.5);
    let k = pose.lying.clamp(0.0, 1.0);
    let bw = s * q * lerp(1.0, 1.04, k);
    let bh = 0.80 * s / q * lerp(1.0, 0.90, k);
    let bottom = ground - lerp(0.026, 0.012, k) * s + pose.lift * s;
    let head_cx = cx - 0.11 * s * k;
    Rect::new(head_cx - bw / 2.0, bottom - bh, bw, bh)
}

/// Where his eyes are looking from, for aiming his gaze at the pointer.
pub fn eyes_at(cx: f32, ground: f32, s: f32) -> (f32, f32) {
    let b = body_rect(cx, ground, s, &Pose::IDLE);
    (b.cx(), b.y + 0.55 * b.h)
}

fn rotate_pt((px, py): (f32, f32), (ox, oy): (f32, f32), deg: f32) -> (f32, f32) {
    if deg.abs() < 0.01 {
        return (px, py);
    }
    let (sn, cs) = deg.to_radians().sin_cos();
    let (dx, dy) = (px - ox, py - oy);
    (ox + dx * cs - dy * sn, oy + dx * sn + dy * cs)
}

/// Draws a single eye with continuous morphing between open vertical ovals,
/// curved `∩` happy arcs, pointed `^` blink chevrons, and `‿` sleeping curves.
fn draw_eye(p: &Painter, ex: f32, ey: f32, s: f32, pose: &Pose, c: Color) {
    let arc = pose.happy.clamp(0.0, 1.0);
    let chev = pose.squint.clamp(0.0, 1.0);
    let slp = pose.sleepy.clamp(0.0, 1.0);
    let expr = arc.max(chev).max(slp);
    let stroke = 0.050 * s;

    if expr < 0.34 {
        // Smoothly squash the vertical capsule down to a closed lid right at expr == 0.34.
        let squash_t = expr / 0.34;
        let open_k = pose.open.clamp(0.0, 1.2) * (1.0 - squash_t);
        let pupil_scale = if pose.glasses { 0.80 } else { 1.0 };
        let ew = lerp(0.126 * s * pupil_scale, 0.148 * s, squash_t);
        let eh = (0.232 * s * pupil_scale * open_k).max(stroke);
        p.fill_round(Rect::new(ex - ew / 2.0, ey - eh / 2.0, ew, eh), ew.min(eh) / 2.0, c);
        return;
    }

    let u = smoother((expr - 0.34) / 0.66);
    if arc >= chev && arc >= slp {
        // Smooth joyful upward crescent `∩` (the concept's Happy and Jump).
        let hw = 0.072 * s;
        let y0 = ey + 0.026 * s * u;
        let rise = 0.096 * s * u;
        let g = p.gfx.path(|sink| unsafe {
            sink.BeginFigure(pt(ex - hw, y0), D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                point1: pt(ex - hw * 0.55, y0 - rise),
                point2: pt(ex + hw * 0.55, y0 - rise),
                point3: pt(ex + hw, y0),
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        });
        if let Some(g) = g {
            p.stroke_geometry(&g, c, stroke);
        }
    } else if chev >= slp {
        // Crisp chevron `^` (the concept's Blink).
        let hw = 0.072 * s;
        let y0 = ey + 0.028 * s * u;
        let apex = ey - 0.042 * s * u;
        let g = p.gfx.path(|sink| unsafe {
            sink.BeginFigure(pt(ex - hw, y0), D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddLine(pt(ex, apex));
            sink.AddLine(pt(ex + hw, y0));
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        });
        if let Some(g) = g {
            p.stroke_geometry(&g, c, stroke);
        }
    } else {
        // Cozy downward sleeping curve `‿` (the concept's Sleep and Sleep Widget).
        let hw = 0.076 * s;
        let y0 = ey - 0.014 * s * u;
        let dip = 0.072 * s * u;
        let g = p.gfx.path(|sink| unsafe {
            sink.BeginFigure(pt(ex - hw, y0), D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                point1: pt(ex - hw * 0.48, y0 + dip),
                point2: pt(ex + hw * 0.48, y0 + dip),
                point3: pt(ex + hw, y0),
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        });
        if let Some(g) = g {
            p.stroke_geometry(&g, c, stroke);
        }
    }
}

/// Draws Docky `s` points wide, standing on `ground` (where his paws rest), centred on `cx`.
pub fn draw(p: &Painter, cx: f32, ground: f32, s: f32, pose: &Pose, pal: &Palette) {
    let lw = 0.056 * s;
    let k = pose.lying.clamp(0.0, 1.0);
    let shadow_cx = cx + 0.03 * s * k + pose.lean * 0.025 * s * (1.0 - k);

    if pal.shadow.a > 0.0 {
        // A soft ground contact shadow: wider under his curled loaf, smaller as he jumps.
        let g = pose.shadow.clamp(0.3, 1.2) * lerp(1.0, 1.28, k);
        for i in 0..5 {
            let grow = i as f32 * 0.032 * s;
            let (sw, sh) = (0.94 * s * g + grow * 2.0, 0.105 * s + grow * 0.85);
            let a = pal.shadow.a * (0.45 + 0.55 * pose.shadow.min(1.0)) * (5 - i) as f32 / 14.0;
            p.fill_round(Rect::new(shadow_cx - sw / 2.0, ground - 0.022 * s - grow * 0.42, sw, sh), sh / 2.0, pal.shadow.alpha(a));
        }
    }

    // Little curved jump-boing arcs beside his shadow (the concept's Jump tile).
    if pose.boing > 0.01 {
        let b_a = pose.boing.clamp(0.0, 1.0);
        let c = pal.extra.alpha(pal.extra.a * 0.55 * b_a);
        for (i, r_arc) in [(0usize, 0.08 * s), (1, 0.125 * s)] {
            let ox = cx + 0.34 * s + i as f32 * 0.055 * s;
            let oy = ground - 0.03 * s;
            let arc = p.gfx.path(|sink| unsafe {
                sink.BeginFigure(pt(ox, oy - r_arc), D2D1_FIGURE_BEGIN_HOLLOW);
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(ox + r_arc * 0.55, oy - r_arc * 0.5),
                    point2: pt(ox + r_arc * 0.55, oy + r_arc * 0.2),
                    point3: pt(ox + r_arc * 0.15, oy + r_arc * 0.55),
                });
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
            });
            if let Some(g) = arc {
                p.stroke_geometry(&g, c, 0.032 * s);
            }
        }
    }

    let body = body_rect(cx, ground, s, pose);
    let lean_px = pose.lean.clamp(-1.0, 1.0) * 0.056 * s * (1.0 - k);

    // Pivot around his base while grounded, smoothly shifting to his center of mass in mid-air
    // so jumps go straight up instead of swinging sideways on a long pendulum.
    let air_pivot = ((-pose.lift) / 0.12).clamp(0.0, 1.0);
    let piv = (body.cx(), lerp(body.bottom(), body.cy(), air_pivot));

    // Paws: plump shaded squircle-mittens. Planted paws stay anchored to `ground`
    // while lifted paws move and tilt with his body; curling up tucks them away.
    let paw_vis = 1.0 - smooth((k - 0.22) / 0.56);
    let pscale = 1.0 - 0.32 * smooth(k / 0.78);
    let (pw, ph, pr) = (0.288 * s * pscale, 0.268 * s * pscale, 0.102 * s * pscale);
    let plw = lw * 0.94 * pscale;
    let draw_paw = |center: (f32, f32), deg: f32| {
        if paw_vis <= 0.01 {
            return;
        }
        p.rotated(center.0, center.1, deg, || {
            let r = Rect::new(center.0 - pw / 2.0, center.1 - ph / 2.0, pw, ph);
            p.fill_round_gradient(r, pr, pal.body, pal.belly);
            p.stroke_round(r.inset(plw * 0.38, plw * 0.38), (pr - plw * 0.38).max(1.0), pal.line, plw);
        });
    };

    let ground_paw_y = ground - 0.098 * s;
    let body_paw_y = body.bottom() - 0.072 * s;
    let tuck_x = 0.09 * s * k;
    let l_air = ((-pose.lift * 4.5).max(-pose.paw_l.1 * 4.0)).clamp(0.0, 1.0);
    let r_air = if pose.prop == Prop::Mug {
        1.0
    } else {
        ((-pose.lift * 4.5).max(-pose.paw_r.1 * 4.0)).clamp(0.0, 1.0)
    };

    let l_grounded = (body.x + 0.048 * s + tuck_x + pose.paw_l.0 * s, ground_paw_y + pose.paw_l.1 * s);
    let l_body = rotate_pt((body.x + 0.048 * s + tuck_x + pose.paw_l.0 * s, body_paw_y + pose.paw_l.1 * s), piv, pose.tilt);
    let left_pt = (lerp(l_grounded.0, l_body.0, l_air), lerp(l_grounded.1, l_body.1, l_air));

    let r_grounded = (body.right() - 0.048 * s - tuck_x + pose.paw_r.0 * s, ground_paw_y + pose.paw_r.1 * s);
    let r_body_local = (body.right() - 0.048 * s - tuck_x + pose.paw_r.0 * s, body_paw_y + pose.paw_r.1 * s);
    let r_body = rotate_pt(r_body_local, piv, pose.tilt);
    let right_pt = (lerp(r_grounded.0, r_body.0, r_air), lerp(r_grounded.1, r_body.1, r_air));

    p.rotated(piv.0, piv.1, pose.tilt, || {
        // Curled side-loaf: his tucked rear body/paw rests behind the right side of his head.
        if k > 0.14 {
            let rk = smoother((k - 0.14) / 0.86);
            let rw = (0.20 + 0.27 * rk) * s;
            let rh = 0.44 * s * (0.58 + 0.42 * rk);
            let rear = Rect::new(body.right() - 0.21 * s, body.bottom() - rh, rw, rh);
            draw_domed(p, rear, 0.85, 0.0, lw * 0.95, pal);
        }

        draw_domed(p, body, k, lean_px, lw, pal);

        // Eyes: shift slightly left and lower as he curls onto his side, matching Sleep Widget.
        let (dx, dy) = (
            pose.look.0.clamp(-1.0, 1.0) * 0.068 * s + lean_px * 0.45 - 0.035 * s * k,
            pose.look.1.clamp(-1.0, 1.0) * 0.044 * s,
        );
        let ey = body.y + lerp(0.55, 0.67, k) * body.h + dy;
        let spread = 0.170 * s * lerp(1.0, 1.04, k);
        for side in [-1.0, 1.0] {
            draw_eye(p, body.cx() + side * spread + dx, ey, s, pose, pal.eye);
        }

        // Square reading glasses: crisp soft-cornered rectangular rims, tinted lenses and glint.
        if pose.glasses && k < 0.5 {
            let gy = body.y + 0.55 * body.h + 0.006 * s;
            let gx_c = body.cx() + lean_px * 0.45;
            let (gw, gh, gr, w) = (0.248 * s, 0.214 * s, 0.044 * s, 0.028 * s);
            let frame = Color::rgba(42, 68, 166, 1.0);
            let xs = [gx_c - 0.172 * s, gx_c + 0.172 * s];
            for x in xs {
                let lens = Rect::new(x - gw / 2.0, gy - gh / 2.0, gw, gh);
                p.fill_round(lens.inset(w * 0.35, w * 0.35), (gr - w * 0.35).max(1.0), Color::rgba(232, 240, 255, 0.44));
                dash(
                    p,
                    (lens.x + 0.048 * s, lens.y + 0.068 * s),
                    (lens.x + 0.088 * s, lens.y + 0.032 * s),
                    Color::WHITE.alpha(0.88),
                    0.018 * s,
                );
                p.stroke_round(lens, gr, frame, w);
            }
            let bridge_y = gy - 0.032 * s;
            dash(p, (xs[0] + gw / 2.0, bridge_y), (xs[1] - gw / 2.0, bridge_y), frame, w);
            let arm_y = gy - 0.052 * s;
            dash(p, (xs[0] - gw / 2.0, arm_y), (body.x + 0.022 * s, arm_y - 0.022 * s), frame, w * 0.92);
            dash(p, (xs[1] + gw / 2.0, arm_y), (body.right() - 0.022 * s, arm_y - 0.022 * s), frame, w * 0.92);
        }

        match pose.prop {
            Prop::Book if k < 0.5 => book(p, body.cx() + lean_px * 0.3, body.bottom() - 0.125 * s, s, pose.prop_t, pal),
            Prop::Laptop if k < 0.5 => laptop(p, body.cx(), body.bottom() + 0.02 * s, s, pal),
            _ => {}
        }

        if pose.prop == Prop::Headphones && k < 0.5 {
            headphones(p, body, lean_px, s, pal);
        }
    });

    // Mitten paws (drawn in world coordinates so grounded paws stay flat on the floor).
    draw_paw(left_pt, pose.tilt * l_air);
    if pose.prop == Prop::Mug && k < 0.5 {
        let rest_mug = (right_pt.0 + 0.065 * s, right_pt.1 - 0.085 * s);
        let sip_mug = rotate_pt((body.cx() + 0.135 * s, body.y + 0.74 * body.h), piv, pose.tilt);
        let at = (lerp(rest_mug.0, sip_mug.0, pose.prop_t), lerp(rest_mug.1, sip_mug.1, pose.prop_t));
        // Paw cradles the lower side of the mug so both happy eyes and the warm mug stay visible.
        let hold_pt = (
            lerp(right_pt.0, at.0 + 0.105 * s, pose.prop_t),
            lerp(right_pt.1, at.1 + 0.075 * s, pose.prop_t),
        );
        p.rotated(at.0, at.1, -14.0 * pose.prop_t, || mug(p, at, s, pose.prop_t, pal));
        draw_paw(hold_pt, pose.tilt * 0.5 - 8.0 * pose.prop_t);
    } else {
        draw_paw(right_pt, pose.tilt * r_air);
    }

    // Expressive dashes: either beside his waving paw or radiating off a top dome corner.
    if pose.sparkle > 0.01 {
        let a = pose.sparkle.clamp(0.0, 1.0);
        let base_col = pal.extra.mix(Color::WHITE, pose.sparkle_bright.clamp(0.0, 1.0));
        let col = base_col.alpha(base_col.a * a);
        if pose.sparkle_paw > 0.5 {
            // Small motion ticks above his waving right paw (the concept's Wave tile).
            let (ax, ay) = (right_pt.0 + 0.03 * s, right_pt.1 - 0.10 * s);
            for deg in [-68.0f32, -32.0] {
                let (sn, cs) = deg.to_radians().sin_cos();
                let (r0, len) = (0.115 * s, 0.095 * s * (0.45 + 0.55 * a));
                dash(p, (ax + r0 * cs, ay + r0 * sn), (ax + (r0 + len) * cs, ay + (r0 + len) * sn), col, 0.048 * s);
            }
        } else {
            let side = if pose.side < 0.0 { -1.0 } else { 1.0 };
            let corner = rotate_pt((body.cx() + lean_px + side * (body.w * 0.44), body.y + 0.05 * s), piv, pose.tilt);
            for deg in [-64.0f32, -24.0] {
                let (sn, cs) = deg.to_radians().sin_cos();
                let cs = cs * side;
                let (r0, len) = (0.135 * s, 0.145 * s * (0.42 + 0.58 * a));
                dash(p, (corner.0 + r0 * cs, corner.1 + r0 * sn), (corner.0 + (r0 + len) * cs, corner.1 + (r0 + len) * sn), col, 0.066 * s);
            }
        }
    }

    // Zs drifting up and gently swaying (single continuous stroke path: no corner blobs).
    if pose.zz > 0.0 {
        let t = pose.zz.clamp(0.0, 1.0);
        let rise = 0.15 * s * t;
        let sway = 0.025 * s * (t * TAU).sin();
        let (ax, ay) = (body.right() - lerp(0.18, 0.02, k) * s + sway, body.y - 0.02 * s - rise);
        let draw_z = |x: f32, y: f32, w: f32, h: f32, a: f32, thick: f32| {
            if a > 0.01 {
                let c = pal.extra.alpha(pal.extra.a * a);
                let g = p.gfx.path(|sink| unsafe {
                    sink.BeginFigure(pt(x, y), D2D1_FIGURE_BEGIN_HOLLOW);
                    sink.AddLine(pt(x + w, y));
                    sink.AddLine(pt(x, y + h));
                    sink.AddLine(pt(x + w, y + h));
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                });
                if let Some(g) = g {
                    p.stroke_geometry(&g, c, thick);
                }
            }
        };
        let fade = 1.0 - ramp(t, 0.72, 1.0);
        draw_z(ax, ay - 0.11 * s, 0.125 * s, 0.13 * s, ramp(t, 0.0, 0.18) * fade, 0.048 * s);
        draw_z(ax + 0.19 * s, ay - 0.34 * s, 0.165 * s, 0.17 * s, ramp(t, 0.14, 0.34) * fade, 0.054 * s);
    }

    // A drop of sweat on his brow.
    if pose.sweat > 0.01 {
        let (x, y, r) = (body.x + 0.16 * s, body.y + 0.18 * body.h, 0.055 * s);
        let c = Color::rgba(120, 200, 255, pose.sweat);
        p.fill_circle(x, y + r * 0.6, r, c);
        let tip = p.gfx.path(|sink| unsafe {
            sink.BeginFigure(pt(x - r * 0.95, y + r * 0.4), D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLine(pt(x, y - r * 1.6));
            sink.AddLine(pt(x + r * 0.95, y + r * 0.4));
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        });
        if let Some(g) = tip {
            p.fill_geometry(&g, c);
        }
    }

    // Music notes floating up from his headphones.
    if pose.prop == Prop::Headphones && k < 0.5 {
        let t = pose.prop_t;
        let a = ramp(t, 0.0, 0.15) * (1.0 - ramp(t, 0.7, 1.0));
        let sway = 0.03 * s * (t * TAU).sin();
        note(p, body.right() + 0.03 * s + sway, body.y - 0.02 * s - 0.26 * s * t, 0.16 * s, pal.extra.alpha(pal.extra.a * a));
    }

    // A heart floating up with a gentle sway.
    if pose.heart > 0.0 {
        let t = pose.heart.clamp(0.0, 1.0);
        let a = ramp(t, 0.0, 0.12) * (1.0 - ramp(t, 0.72, 1.0));
        let size = 0.24 * s * (0.58 + 0.42 * ease_out_back((t / 0.28).clamp(0.0, 1.0), 1.1));
        let sway = 0.035 * s * (t * TAU * 0.85).sin();
        let (hx, hy) = (body.right() + 0.02 * s + sway, body.y - 0.08 * s - 0.34 * s * t);
        heart(p, hx - size / 2.0, hy - size / 2.0, size, pal.heart.alpha(a));
    }
}

/// An open hardcover book held in front of him (seen from the outside cover and
/// spine, with white page-tops peeking above and `flip` arching a page across).
fn book(p: &Painter, cx: f32, cy: f32, s: f32, flip: f32, pal: &Palette) {
    let (hw, hh) = (0.33 * s, 0.145 * s);
    let lw = 0.036 * s;
    let dip = 0.022 * s;
    let top_outer = cy - hh - 0.010 * s;
    let top_spine = cy - hh + dip;
    let bot_outer = cy + hh - 0.010 * s;
    let bot_spine = cy + hh + dip;

    // 1. White open page stacks peeking above the top rim of the covers, dipping into the spine.
    if let Some(pages) = p.gfx.path(|sink| unsafe {
        sink.BeginFigure(pt(cx - hw + 0.028 * s, top_outer + 0.015 * s), D2D1_FIGURE_BEGIN_FILLED);
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(cx - hw * 0.72, top_outer - 0.048 * s),
            point2: pt(cx - hw * 0.25, top_outer - 0.038 * s),
            point3: pt(cx, top_spine),
        });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(cx + hw * 0.25, top_outer - 0.038 * s),
            point2: pt(cx + hw * 0.72, top_outer - 0.048 * s),
            point3: pt(cx + hw - 0.028 * s, top_outer + 0.015 * s),
        });
        sink.AddLine(pt(cx + hw - 0.028 * s, cy));
        sink.AddLine(pt(cx - hw + 0.028 * s, cy));
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
    }) {
        fill_geom_gradient(p, &pages, Rect::new(cx - hw, top_outer - 0.05 * s, hw * 2.0, hh), Color::WHITE, 0.6, pal.belly);
        p.stroke_geometry(&pages, pal.line, lw * 0.82);
    }

    // 2. Turning a page: a single white sheet lifts from his right (our left), arches over the spine, and settles on our right.
    if flip > 0.0 && flip < 1.0 {
        let k = -(flip * PI).cos();
        let arch = (flip * PI).sin();
        let tip_x = cx + k * (hw * 0.82);
        let tip_y = top_outer - 0.022 * s - 0.068 * s * arch;
        if let Some(sheet) = p.gfx.path(|sink| unsafe {
            sink.BeginFigure(pt(cx, top_spine + 0.01 * s), D2D1_FIGURE_BEGIN_FILLED);
            sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                point1: pt(cx + k * hw * 0.28, tip_y - 0.028 * s * arch),
                point2: pt(tip_x - k * hw * 0.18, tip_y - 0.012 * s),
                point3: pt(tip_x, tip_y + 0.018 * s),
            });
            sink.AddLine(pt(tip_x * 0.85 + cx * 0.15, cy));
            sink.AddLine(pt(cx, cy));
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        }) {
            p.fill_geometry(&sheet, Color::WHITE);
            p.stroke_geometry(&sheet, pal.line.alpha(0.75), lw * 0.62);
        }
    }

    // 3. Outside hardcover boards angled gently into a shallow V at the center spine.
    let r = 0.036 * s;
    if let Some(cover) = p.gfx.path(|sink| unsafe {
        sink.BeginFigure(pt(cx, top_spine), D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(pt(cx + hw - r, top_outer));
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(cx + hw, top_outer),
            point2: pt(cx + hw, top_outer + r * 0.4),
            point3: pt(cx + hw, top_outer + r),
        });
        sink.AddLine(pt(cx + hw, bot_outer - r));
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(cx + hw, bot_outer),
            point2: pt(cx + hw - r * 0.4, bot_outer),
            point3: pt(cx + hw - r, bot_outer),
        });
        sink.AddLine(pt(cx, bot_spine));
        sink.AddLine(pt(cx - hw + r, bot_outer));
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(cx - hw + r * 0.4, bot_outer),
            point2: pt(cx - hw, bot_outer),
            point3: pt(cx - hw, bot_outer - r),
        });
        sink.AddLine(pt(cx - hw, top_outer + r));
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(cx - hw, top_outer + r * 0.4),
            point2: pt(cx - hw, top_outer),
            point3: pt(cx - hw + r, top_outer),
        });
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
    }) {
        let bounds = Rect::new(cx - hw, top_outer, hw * 2.0, bot_spine - top_outer);
        fill_geom_gradient(p, &cover, bounds, Color::rgba(86, 126, 246, 1.0), 0.5, Color::rgba(54, 90, 212, 1.0));

        // Center spine band with classic raised ribs, and a front-cover bookplate on his left (our right).
        let sw = 0.082 * s;
        let spine = Rect::new(cx - sw / 2.0, top_spine, sw, bot_spine - top_spine);
        p.fill_round(spine, 0.018 * s, Color::rgba(38, 66, 174, 1.0));
        p.line(spine.x, top_spine + 0.01 * s, spine.x, bot_spine - 0.01 * s, pal.line.alpha(0.7), lw * 0.55);
        p.line(spine.right(), top_spine + 0.01 * s, spine.right(), bot_spine - 0.01 * s, pal.line.alpha(0.7), lw * 0.55);
        for fy in [0.24f32, 0.76] {
            let ry = lerp(top_spine, bot_spine, fy);
            p.line(spine.x + 0.012 * s, ry, spine.right() - 0.012 * s, ry, Color::WHITE.alpha(0.38), lw * 0.48);
        }

        // Front cover title bookplate (our right panel).
        let plate = Rect::new(cx + sw * 0.85, cy - 0.048 * s, hw - sw * 1.35, 0.088 * s);
        if plate.w > 0.04 * s {
            p.fill_round(plate, 0.02 * s, Color::rgba(240, 245, 255, 0.86));
            p.line(plate.x + 0.025 * s, plate.cy(), plate.right() - 0.025 * s, plate.cy(), Color::rgba(54, 90, 212, 0.55), lw * 0.45);
        }

        p.stroke_geometry(&cover, pal.line, lw);
    }
}

/// A mug of coffee centred on `at`, steaming softly.
fn mug(p: &Painter, at: (f32, f32), s: f32, sip: f32, pal: &Palette) {
    let (w, h) = (0.26 * s, 0.28 * s);
    let r = Rect::new(at.0 - w / 2.0, at.1 - h / 2.0, w, h);
    let lw = 0.042 * s;
    p.stroke_circle(r.right() - 0.01 * s, r.cy() + 0.01 * s, 0.068 * s, pal.line, lw);
    p.fill_round_gradient(r, 0.055 * s, Color::rgba(255, 194, 118, 1.0), Color::rgba(238, 138, 62, 1.0));
    p.stroke_round(r.inset(lw * 0.4, lw * 0.4), 0.045 * s, pal.line, lw);
    p.fill_round(Rect::new(r.x + lw * 0.9, r.y + lw * 0.85, w - 1.8 * lw, 0.042 * s), 0.02 * s, Color::rgba(104, 58, 34, 1.0));
    let steam = pal.extra.alpha(pal.extra.a * 0.72 * (1.0 - sip));
    if steam.a > 0.02 {
        for (i, dx) in [-0.032f32, 0.038].into_iter().enumerate() {
            let x = at.0 + dx * s;
            let y0 = r.y - 0.045 * s - i as f32 * 0.02 * s;
            let g = p.gfx.path(|sink| unsafe {
                sink.BeginFigure(pt(x, y0), D2D1_FIGURE_BEGIN_HOLLOW);
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(x + 0.03 * s, y0 - 0.04 * s),
                    point2: pt(x - 0.025 * s, y0 - 0.08 * s),
                    point3: pt(x + 0.01 * s, y0 - 0.12 * s),
                });
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
            });
            if let Some(g) = g {
                p.stroke_geometry(&g, steam, 0.026 * s);
            }
        }
    }
}

/// A laptop in front of him, its lid towards us (a little heart on it).
fn laptop(p: &Painter, cx: f32, ground: f32, s: f32, pal: &Palette) {
    let lw = 0.038 * s;
    let lid = Rect::new(cx - 0.26 * s, ground - 0.29 * s, 0.52 * s, 0.24 * s);
    p.fill_round_gradient(lid, 0.042 * s, Color::rgba(236, 240, 252, 1.0), Color::rgba(198, 208, 234, 1.0));
    p.stroke_round(lid.inset(lw * 0.4, lw * 0.4), 0.038 * s, pal.line, lw);
    let base = Rect::new(cx - 0.32 * s, ground - 0.068 * s, 0.64 * s, 0.055 * s);
    p.fill_round(base, 0.028 * s, Color::rgba(182, 192, 222, 1.0));
    p.stroke_round(base.inset(lw * 0.4, lw * 0.4), 0.024 * s, pal.line, lw * 0.82);
    let hs = 0.095 * s;
    heart(p, cx - hs / 2.0, lid.cy() - hs / 2.0, hs, BLUE_HEART.alpha(0.75));
}

/// Headphones hugging the dome of his head: a snug band and two plush cups.
fn headphones(p: &Painter, body: Rect, lean_px: f32, s: f32, pal: &Palette) {
    let band = Color::rgba(38, 48, 96, 1.0);
    let y = body.y + 0.42 * body.h;
    let arc = p.gfx.path(|sink| unsafe {
        sink.BeginFigure(pt(body.x + 0.015 * s, y), D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(body.x - 0.01 * s + lean_px * 0.6, body.y - 0.14 * s),
            point2: pt(body.right() + 0.01 * s + lean_px * 0.6, body.y - 0.14 * s),
            point3: pt(body.right() - 0.015 * s, y),
        });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
    });
    if let Some(g) = arc {
        p.stroke_geometry(&g, band, 0.064 * s);
    }
    for x in [body.x + 0.005 * s, body.right() - 0.005 * s] {
        let cup = Rect::new(x - 0.072 * s, y - 0.125 * s, 0.144 * s, 0.25 * s);
        p.fill_round_gradient(cup, 0.062 * s, Color::rgba(255, 134, 172, 1.0), Color::rgba(238, 96, 142, 1.0));
        p.stroke_round(cup.inset(0.016 * s, 0.016 * s), 0.052 * s, pal.line, 0.038 * s);
    }
}

/// A quaver, `size` tall, its head at (`x`, `y`).
fn note(p: &Painter, x: f32, y: f32, size: f32, c: Color) {
    if c.a <= 0.01 {
        return;
    }
    let r = 0.24 * size;
    p.fill_circle(x, y, r, c);
    let top = y - size;
    let sx = x + r * 0.82;
    let g = p.gfx.path(|sink| unsafe {
        sink.BeginFigure(pt(sx, y), D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddLine(pt(sx, top));
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(sx + 0.22 * size, top + 0.05 * size),
            point2: pt(sx + 0.36 * size, top + 0.20 * size),
            point3: pt(sx + 0.28 * size, top + 0.38 * size),
        });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
    });
    if let Some(g) = g {
        p.stroke_geometry(&g, c, 0.14 * size);
    }
}

/// A heart filling a `size` square at (`x`, `y`).
pub fn heart(p: &Painter, x: f32, y: f32, size: f32, c: Color) {
    if c.a <= 0.01 {
        return;
    }
    let q = |fx: f32, fy: f32| pt(x + fx * size, y + fy * size);
    let bez = |sink: &ID2D1GeometrySink, a: (f32, f32), b: (f32, f32), e: (f32, f32)| unsafe {
        sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: q(a.0, a.1), point2: q(b.0, b.1), point3: q(e.0, e.1) });
    };
    let g = p.gfx.path(|sink| unsafe {
        sink.BeginFigure(q(0.5, 0.94), D2D1_FIGURE_BEGIN_FILLED);
        bez(sink, (0.18, 0.72), (0.0, 0.50), (0.0, 0.29));
        bez(sink, (0.0, 0.11), (0.14, 0.02), (0.28, 0.02));
        bez(sink, (0.40, 0.02), (0.47, 0.10), (0.5, 0.19));
        bez(sink, (0.53, 0.10), (0.60, 0.02), (0.72, 0.02));
        bez(sink, (0.86, 0.02), (1.0, 0.11), (1.0, 0.29));
        bez(sink, (1.0, 0.50), (0.82, 0.72), (0.5, 0.94));
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
    });
    if let Some(g) = g {
        p.fill_geometry(&g, c);
    }
}

// ---------------------------------------------------------------------------
// Auto: Docky picks his own look and says things
// ---------------------------------------------------------------------------

/// Something that just happened, for an Auto Docky to answer.
pub enum Cue {
    /// Clicked.
    Click,
    /// You came back after a while away.
    Back,
    /// You switched to this app.
    Switched(String),
    /// The pointer was shaken at him.
    Dizzy,
    /// The charger went in.
    Charger,
    /// The computer's working hard.
    Hot,
    /// A new song started.
    Track,
}

/// What an Auto Docky sees each time he's asked (see [`Auto::tick`]).
pub struct Signals<'a> {
    pub asleep: bool,
    /// Seconds of typing without a pause of more than a few seconds (0 when not typing).
    pub typing_for: f32,
    /// Seconds since the last key.
    pub since_typed: f32,
    /// The pointer is close to him.
    pub near: bool,
    /// Seconds without input.
    pub idle: u32,
    pub music: bool,
    /// Percent and whether it's charging.
    pub battery: Option<(u8, bool)>,
    /// The next calendar event starting soon: its title and seconds until it does.
    pub next_event: Option<(&'a str, i64)>,
}

/// How an Auto Docky's widget looks right now.
pub struct Scene {
    pub look: &'static str,
    /// The look he's switching from, and how far through the switch (0..1).
    pub from: Option<(&'static str, f32)>,
    /// What he's saying, and how far it's faded in (0..1).
    pub say: Option<(String, f32)>,
}

const SWITCH: f32 = 0.42;
const FADE: f32 = 0.22;

/// Chatter, now and then: what he says, and what he does as he says it.
const CHATTER: [(&str, Option<Act>); 12] = [
    ("hi!!", Some(Act::Wave)),
    ("\u{2665}", Some(Act::Love)),
    ("*sniff sniff*", Some(Act::Look)),
    ("mrrp?", Some(Act::Look)),
    ("hehe", Some(Act::Happy)),
    ("*hums* \u{266A}", Some(Act::Dance)),
    ("*wiggles*", Some(Act::Jump)),
    ("boop!", Some(Act::Jump)),
    ("*streeetch*", Some(Act::Sit)),
    ("snack time?", Some(Act::Look)),
    ("*happy noises*", Some(Act::Happy)),
    ("still here!", Some(Act::Wave)),
];
const MORNING: [&str; 3] = ["*yawn* mornin'", "mornin'! \u{2600}", "brekkie?"];
const EVENING: [&str; 3] = ["cozy time\u{2026}", "*sleepy blink*", "snuggle?"];
const CLICKED: [&str; 5] = ["hehe!", "boop!", "\u{2665}", "eep!", "mrrp!"];

pub struct Auto {
    look: &'static str,
    from: Option<(&'static str, Instant)>,
    /// What he's saying: the words, since when, until when.
    say: Option<(String, Instant, Instant)>,
    next_chat: Instant,
    next_peek: Instant,
    peek_until: Instant,
    /// Things said recently (by kind), not to be said again before their time.
    quiet: Vec<(&'static str, Instant)>,
    /// When you started this stretch at the computer.
    active_since: Instant,
    /// When he next starts something to do on his own.
    next_activity: Instant,
    /// Pixel Auto: always in pixels.
    pixel: bool,
    /// Whether music was playing, and he was asleep, at the last tick.
    music_was: bool,
    asleep_was: bool,
    /// When he might next join in with your typing.
    join_after: Instant,
    rng: u64,
}

impl Default for Auto {
    fn default() -> Self {
        Self::new()
    }
}

impl Auto {
    pub fn new() -> Self {
        let now = Instant::now();
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(7, |d| d.as_nanos() as u64);
        Auto {
            look: "classic",
            from: None,
            say: None,
            next_chat: now + Duration::from_secs(8),
            next_peek: now + Duration::from_secs(90),
            peek_until: now,
            quiet: Vec::new(),
            active_since: now,
            next_activity: now + Duration::from_secs(6),
            pixel: false,
            music_was: false,
            asleep_was: false,
            join_after: now,
            rng: seed.rotate_left(17) | 1,
        }
    }

    pub fn look(&self) -> &'static str {
        self.look
    }

    /// Pixel Auto: he stays in pixels whatever he's doing.
    pub fn set_pixel(&mut self, on: bool) {
        if on != self.pixel {
            self.pixel = on;
            self.look = if on { "pixel" } else { "classic" };
            self.from = None;
        }
    }

    fn roll(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 40) as f32 / (1u64 << 24) as f32
    }

    fn secs(&mut self, lo: f32, hi: f32) -> Duration {
        Duration::from_secs_f32(lo + (hi - lo) * self.roll())
    }

    pub fn scene(&self, now: Instant) -> Scene {
        let from = self.from.and_then(|(l, t)| {
            let k = now.saturating_duration_since(t).as_secs_f32() / SWITCH;
            (k < 1.0).then_some((l, smoother(k)))
        });
        let say = self.say.as_ref().and_then(|(text, since, until)| {
            if now >= *until {
                return None;
            }
            let a = (now.saturating_duration_since(*since).as_secs_f32() / FADE).min(until.saturating_duration_since(now).as_secs_f32() / FADE);
            Some((text.clone(), smoother(a)))
        });
        Scene { look: self.look, from, say }
    }

    /// Whether a switch of look or a bubble coming or going needs frames.
    pub fn busy(&self, now: Instant) -> bool {
        let switching = self.from.is_some_and(|(_, t)| now.saturating_duration_since(t).as_secs_f32() < SWITCH);
        let fading = self.say.as_ref().is_some_and(|(_, since, until)| {
            now.saturating_duration_since(*since).as_secs_f32() < FADE || until.saturating_duration_since(now).as_secs_f32() < FADE
        });
        switching || fading
    }

    fn go(&mut self, look: &'static str, now: Instant) -> bool {
        if look == self.look {
            return false;
        }
        self.from = Some((self.look, now));
        self.look = look;
        true
    }

    /// Says `text` for `secs` (replacing whatever he was saying).
    pub fn say(&mut self, text: impl Into<String>, secs: f32) {
        let now = Instant::now();
        let since = match &self.say {
            Some((_, s, until)) if *until > now + Duration::from_secs_f32(FADE) => *s,
            _ => now,
        };
        self.say = Some((text.into(), since, now + Duration::from_secs_f32(secs)));
        self.next_chat = self.next_chat.max(now + Duration::from_secs(20));
    }

    /// True (and then not again for `every`) for this kind of remark.
    fn once(&mut self, key: &'static str, every: Duration) -> bool {
        let now = Instant::now();
        self.quiet.retain(|(_, t)| *t > now);
        if self.quiet.iter().any(|(k, _)| *k == key) {
            return false;
        }
        self.quiet.push((key, now + every));
        true
    }

    fn quiet_for(&self) -> bool {
        self.say.as_ref().is_some_and(|(_, _, until)| *until > Instant::now())
    }

    /// Answers something that just happened.
    pub fn cue(&mut self, cue: Cue, pet: &mut Pet) {
        match cue {
            Cue::Click => {
                if self.once("click", Duration::from_secs(4)) {
                    let i = (self.roll() * CLICKED.len() as f32) as usize;
                    self.say(CLICKED[i.min(CLICKED.len() - 1)], 2.2);
                }
            }
            Cue::Back => self.say("you're back!! \u{2665}", 3.0),
            Cue::Dizzy => self.say("@_@ wobbly\u{2026}", 2.5),
            Cue::Charger => self.say("nom nom \u{26A1}", 2.5),
            Cue::Hot => {
                if self.once("hot", Duration::from_secs(180)) {
                    self.say("*pant pant* hot!!", 2.5);
                }
            }
            Cue::Switched(app) => {
                if !self.quiet_for() && self.roll() < 0.3 && self.once("switch", Duration::from_secs(60)) {
                    self.say(format!("ooh! {}?", app.to_lowercase()), 2.5);
                    pet.glance((0.0, -0.4));
                }
            }
            Cue::Track => {
                if self.roll() < 0.5 && self.once("track", Duration::from_secs(60)) {
                    self.say("\u{266A} bop bop \u{266A}", 2.5);
                }
            }
        }
    }

    /// What he's doing for a while: headphones while music plays, his laptop
    /// while you type, and otherwise, now and then, a walk, a coffee, a book or
    /// a nap. True when that changed.
    fn keep_busy(&mut self, now: Instant, s: &Signals, pet: &mut Pet) -> bool {
        let doing = pet.doing();
        if s.asleep {
            if doing.is_some() {
                pet.stop();
                return true;
            }
            return false;
        }
        if self.look == "peek" {
            return false;
        }
        let music_started = s.music && !self.music_was;
        self.music_was = s.music;
        if !s.music && doing == Some(Activity::Music) {
            pet.stop();
            return true;
        }
        if music_started && doing != Some(Activity::Nap) && self.roll() < 0.75 {
            let secs = 15.0 + 20.0 * self.roll();
            pet.start(Activity::Music, Some(secs));
            self.say("\u{266A}!", 2.0);
            self.next_activity = now + Duration::from_secs_f32(secs) + self.secs(6.0, 15.0);
            return true;
        }
        if s.typing_for >= 6.0 && doing.is_none() && now >= self.join_after {
            self.join_after = now + self.secs(40.0, 100.0);
            if self.roll() < 0.6 {
                let secs = 12.0 + 16.0 * self.roll();
                pet.start(Activity::Laptop, Some(secs));
                if self.once("laptop", Duration::from_secs(600)) {
                    self.say(if self.pixel { "> tap tap" } else { "tap tap!" }, 2.5);
                }
                self.next_activity = now + Duration::from_secs_f32(secs) + self.secs(6.0, 15.0);
                return true;
            }
        }
        if doing == Some(Activity::Laptop) && s.since_typed > 6.0 {
            pet.stop();
            if self.once("done", Duration::from_secs(300)) {
                self.say("yay!!", 2.5);
            }
            self.next_activity = now + self.secs(4.0, 10.0);
            return true;
        }
        if doing == Some(Activity::Nap) && s.near {
            pet.stop();
            self.say("!!", 2.0);
            self.next_activity = now + self.secs(20.0, 40.0);
            return true;
        }
        if doing.is_some() || pet.busy() || now < self.next_activity || s.idle >= 120 {
            return false;
        }
        let r = self.roll();
        let (activity, secs, said): (Activity, Option<f32>, Option<&str>) = if s.music && r < 0.35 {
            (Activity::Music, Some(12.0 + 18.0 * self.roll()), Some("\u{266A} bop bop \u{266A}"))
        } else if s.typing_for > 0.0 && r < 0.2 {
            (Activity::Laptop, Some(10.0 + 15.0 * self.roll()), Some("tap tap!"))
        } else if r < 0.36 {
            (Activity::Walk, None, None)
        } else if r < 0.56 {
            (Activity::Coffee, Some(18.0 + 16.0 * self.roll()), Some("nom\u{2026} \u{2615}"))
        } else if r < 0.78 {
            (Activity::Read, Some(25.0 + 20.0 * self.roll()), Some("*flips pages*"))
        } else if r < 0.88 {
            (Activity::Nap, Some(25.0 + 25.0 * self.roll()), Some("zzz\u{2026}"))
        } else {
            pet.play(Act::Sit);
            self.next_activity = now + self.secs(8.0, 20.0);
            return true;
        };
        let took = pet.start(activity, secs);
        if let Some(text) = said {
            if !self.quiet_for() && self.roll() < 0.6 {
                self.say(text, 2.8);
            }
        }
        self.next_activity = now + Duration::from_secs_f32(took) + self.secs(6.0, 20.0);
        true
    }

    /// Picks the look for the moment and, now and then, something to say.
    /// True when that changed anything (which then needs drawing).
    pub fn tick(&mut self, now: Instant, s: &Signals, pet: &mut Pet) -> bool {
        let mut changed = false;
        if self.say.as_ref().is_some_and(|(_, _, until)| now >= *until) {
            self.say = None;
            changed = true;
        }
        if s.idle >= 300 {
            self.active_since = now;
        }

        let woke = self.asleep_was && !s.asleep;
        self.asleep_was = s.asleep;
        if woke {
            self.say("*yawn* hi!", 3.0);
            changed = true;
        }
        let want = if self.pixel {
            "pixel"
        } else if s.asleep {
            if self.look == "peek" { "classic" } else { self.look }
        } else if self.look == "peek" {
            if s.near {
                pet.play(Act::Jump);
                self.say("boo!", 2.0);
                "classic"
            } else if now < self.peek_until {
                "peek"
            } else {
                "classic"
            }
        } else if self.look == "classic" && now >= self.next_peek && !s.near && !s.music && s.idle < 60 && !pet.busy() && pet.doing().is_none() && s.typing_for == 0.0 {
            self.peek_until = now + self.secs(20.0, 45.0);
            self.next_peek = now + self.secs(120.0, 300.0);
            "peek"
        } else {
            "classic"
        };
        changed |= self.go(want, now);
        changed |= self.keep_busy(now, s, pet);
        if s.asleep || self.quiet_for() || pet.busy() || pet.doing() == Some(Activity::Nap) {
            return changed;
        }

        if let Some((title, secs)) = s.next_event {
            if (0..=600).contains(&secs) && self.once("event", Duration::from_secs(300)) {
                let text = if secs <= 60 { format!("psst! {title}! now now!") } else { format!("psst! {title} soon!") };
                pet.play(Act::Wave);
                self.say(text, 5.0);
                return true;
            }
        }
        if let Some((p, false)) = s.battery {
            if p <= 15 && self.once("battery", Duration::from_secs(600)) {
                self.say(format!("hungry\u{2026} {p}%"), 3.5);
                return true;
            }
        }
        if s.typing_for >= 60.0 && self.once("roll", Duration::from_secs(300)) {
            self.say("tap tap tap!", 2.5);
            return true;
        }
        let hour = chrono::Timelike::hour(&chrono::Local::now());
        if hour < 5 && self.once("late", Duration::from_secs(2700)) {
            self.say("sleepy\u{2026} bed?", 3.5);
            return true;
        }
        if now.saturating_duration_since(self.active_since) >= Duration::from_secs(50 * 60) && self.once("stretch", Duration::from_secs(50 * 60)) {
            pet.stop();
            pet.play(Act::Sit);
            self.say("*streeetch* you too!", 3.5);
            return true;
        }

        if now >= self.next_chat && self.look != "pixel" {
            self.next_chat = now + self.secs(60.0, 150.0);
            let r = self.roll();
            let (text, act) = match hour {
                5..=10 if r < 0.35 => (MORNING[(self.roll() * 3.0) as usize % 3], Some(Act::Happy)),
                18..=23 if r < 0.35 => (EVENING[(self.roll() * 3.0) as usize % 3], Some(Act::Sit)),
                _ => {
                    let (t, a) = CHATTER[(self.roll() * CHATTER.len() as f32) as usize % CHATTER.len()];
                    (t, a)
                }
            };
            if let (Some(a), None) = (act, pet.doing()) {
                pet.play(a);
            }
            self.say(text, 3.0);
            return true;
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// On the desktop
// ---------------------------------------------------------------------------

/// The Docky widget's looks: (config value, name). Auto switches between the others by
/// itself and keeps busy; Pixel Auto does the same, always in pixels.
pub const STYLES: [(&str, &str); 6] = [("auto", "Auto"), ("pixel-auto", "Pixel Auto"), ("classic", "Classic"), ("sleepy", "Sleepy"), ("peek", "Peeking"), ("pixel", "Pixel")];

/// Where Docky stands in a widget's `body`: (centre x, ground, width).
pub fn place(body: Rect, medium: bool, style: &str) -> (f32, f32, f32) {
    let h = body.h;
    match style {
        // Bold and close, paws gripping right over the card's bottom edge.
        "peek" => {
            let s = if medium { 0.86 * h } else { 0.68 * h };
            (body.cx() + if medium { 0.0 } else { 0.02 * h }, body.bottom() + 0.085 * s, s)
        }
        // Curled loaf resting on the card's cozy floor stage.
        "sleepy" => (body.cx() - 0.02 * h, body.bottom() - 0.16 * h, if medium { 0.62 * h } else { 0.56 * h }),
        // Medium: comfortably to the left so his speech bubble sits cleanly beside him.
        _ => (if medium { body.x + 0.255 * body.w } else { body.cx() }, body.bottom() - 0.14 * h, 0.48 * h),
    }
}

/// Whether a key on the keyboard (not a mouse button) went down since the last call.
pub fn keys_typed() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    let mut any = false;
    for vk in 0x08..=0xFE {
        if unsafe { GetAsyncKeyState(vk) } & 1 != 0 {
            any = true;
        }
    }
    any
}

/// When the last key press or mouse move was (GetTickCount milliseconds).
pub fn last_input() -> u32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    let _ = unsafe { GetLastInputInfo(&mut info) };
    info.dwTime
}

/// How far (points) either way of the middle Docky `s` wide can walk in `body`.
pub fn walk_room(body: Rect, s: f32) -> f32 {
    ((body.w - 1.48 * s) / 2.0 - 8.0).max(0.0)
}

/// How long since the last key press or mouse move, in seconds.
pub fn idle_secs() -> u32 {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return 0;
    }
    unsafe { GetTickCount() }.wrapping_sub(info.dwTime) / 1000
}

// ---------------------------------------------------------------------------
// Pixel art (the concept's ASCII version & ASCII widget)
// ---------------------------------------------------------------------------

/// His grid, in cells: a 17 × 11 head and rounded paws, with room above/right
/// for a floating heart, Zs, or music note: 21 × 15 in all.
const PX_W: f32 = 21.0;
const PX_H: f32 = 15.0;
/// Rows above his head (for floating Zs / heart / headphones).
const PX_TOP: f32 = 3.0;

const HEART: [&str; 4] = ["##.##", "#####", ".###.", "..#.."];
const ZED: [&str; 3] = ["###", ".#.", "###"];
const ZED_LG: [&str; 4] = ["####", "..#.", ".#..", "####"];
const NOTE: [&str; 4] = ["..##", "..#.", "###.", "##.."];

/// The size of a pixel-art cell for Docky to fill `r`: whole points, so his edges stay crisp.
pub fn pixel_cell(r: Rect) -> f32 {
    (r.w / PX_W).min(r.h / PX_H).floor().max(1.0)
}

/// The pixel-art Docky, centred in `r`. `bg` fills inside his rounded paws.
pub fn draw_pixel(p: &Painter, r: Rect, pose: &Pose, ink: Color, accent: Color, bg: Color) {
    let u = pixel_cell(r);
    let room = ((r.w / u - PX_W) / 2.0).floor().max(0.0);
    let ox = (r.cx() - PX_W * u / 2.0).round() + (pose.walk * room).round() * u;
    let lift = (pose.lift * 13.0).round() * u;
    let oy = (r.cy() - PX_H * u / 2.0 + PX_TOP * u).round() + lift;
    let block = |c: f32, row: f32, w: f32, h: f32, color: Color| {
        p.fill_rect(Rect::new(ox + c * u, oy + row * u, w * u, h * u), color)
    };

    // Complete closed head outline (including bottom corners, so lifting a paw
    // to wave or jump never leaves a gap in his cheek).
    block(5.0, 0.0, 7.0, 1.0, ink);
    block(4.0, 1.0, 1.0, 1.0, ink);
    block(12.0, 1.0, 1.0, 1.0, ink);
    block(3.0, 2.0, 1.0, 1.0, ink);
    block(13.0, 2.0, 1.0, 1.0, ink);
    block(2.0, 3.0, 1.0, 6.0, ink);
    block(14.0, 3.0, 1.0, 6.0, ink);
    block(3.0, 9.0, 1.0, 1.0, ink);
    block(13.0, 9.0, 1.0, 1.0, ink);
    block(4.0, 10.0, 9.0, 1.0, ink);

    // Eyes: 2 × 3 blocks, or `∩ ∩` / `^ ^` / `‿ ‿` / a blink line.
    let shift = pose.look.0.round().clamp(-1.0, 1.0);
    let expr_happy = pose.happy > 0.45 || pose.squint > 0.45;
    for left in [5.0, 10.0] {
        let x = left + shift;
        if expr_happy {
            block(x, 5.0, 2.0, 1.0, ink);
            block(x - 1.0, 6.0, 1.0, 1.0, ink);
            block(x + 2.0, 6.0, 1.0, 1.0, ink);
        } else if pose.sleepy > 0.45 {
            block(x - 1.0, 6.0, 1.0, 1.0, ink);
            block(x + 2.0, 6.0, 1.0, 1.0, ink);
            block(x, 7.0, 2.0, 1.0, ink);
        } else if pose.open < 0.45 {
            block(x, 6.0, 2.0, 1.0, ink);
        } else if pose.glasses {
            block(x + 1.0, 6.0, 1.0, 2.0, ink);
        } else {
            block(x, 5.0, 2.0, 3.0, ink);
        }
    }

    // Rounded 4 × 4 pixel mitten paws (matching the concept's `.##.` / `#..#` / `#..#` / `.##.`).
    let paw = |c: f32, row: f32| {
        block(c + 1.0, row + 1.0, 2.0, 2.0, bg);
        block(c + 1.0, row, 2.0, 1.0, ink);
        block(c + 1.0, row + 3.0, 2.0, 1.0, ink);
        block(c, row + 1.0, 1.0, 2.0, ink);
        block(c + 3.0, row + 1.0, 1.0, 2.0, ink);
    };

    // Glasses: crisp compact pixel rims with clear interior around his pupils.
    if pose.glasses {
        let rim = ink.alpha(ink.a * 0.78);
        for left in [4.0, 9.0] {
            block(left, 4.0, 4.0, 1.0, rim);
            block(left, 8.0, 4.0, 1.0, rim);
            block(left, 5.0, 1.0, 3.0, rim);
            block(left + 3.0, 5.0, 1.0, 3.0, rim);
        }
        block(8.0, 5.0, 1.0, 1.0, rim);
    }

    match pose.prop {
        Prop::Book => {
            // Back of an open book: left & right covers with a center spine, below his glasses.
            block(4.0, 9.0, 9.0, 2.0, bg);
            let cover = accent.alpha(accent.a * 0.68);
            block(4.0, 9.0, 4.0, 2.0, cover);
            block(9.0, 9.0, 4.0, 2.0, cover);
            block(10.0, 9.0, 2.0, 1.0, ink);
            block(8.0, 9.0, 1.0, 2.0, ink.alpha(ink.a * 0.4));
            if pose.prop_t > 0.0 && pose.prop_t < 1.0 {
                block(5.0 + (pose.prop_t * 6.0).round(), 8.0, 1.0, 1.0, ink);
            }
        }
        Prop::Laptop => {
            block(5.0, 8.0, 7.0, 3.0, ink);
            block(8.0, 9.0, 1.0, 1.0, bg);
        }
        _ => {}
    }

    let hold_k = if pose.prop == Prop::Book { 0.0 } else { 1.0 };
    let (lx, ly) = ((pose.paw_l.0 * 11.0 * hold_k).round(), 7.0 + (pose.paw_l.1 * 11.0 * hold_k).round());
    let (rx, ry) = (13.0 + (pose.paw_r.0 * 11.0 * hold_k).round(), 7.0 + (pose.paw_r.1 * 11.0 * hold_k).round());
    paw(lx, ly);
    paw(rx, ry);

    // Ground shadow line beneath him when he jumps.
    if pose.lift < -0.08 {
        let ground_y = oy - lift + 11.0 * u;
        p.fill_rect(Rect::new(ox + 5.0 * u, ground_y, 7.0 * u, u), ink.alpha(ink.a * 0.35));
    }

    if pose.prop == Prop::Mug {
        let sip = pose.prop_t > 0.5;
        let (mx, my) = if sip { (14.0, 6.0) } else { (17.0, 6.0) };
        block(mx, my, 3.0, 3.0, accent);
        block(mx + 1.0, my + 1.0, 1.0, 1.0, bg);
        block(mx + 3.0, my + 1.0, 1.0, 1.0, ink);
        if !sip {
            block(mx + 1.0, my - 2.0, 1.0, 1.0, ink.alpha(ink.a * 0.6));
            block(mx + 2.0, my - 3.0, 1.0, 1.0, ink.alpha(ink.a * 0.4));
        }
    }
    if pose.prop == Prop::Headphones {
        block(5.0, -1.0, 7.0, 1.0, ink);
        block(3.0, 0.0, 2.0, 1.0, ink);
        block(12.0, 0.0, 2.0, 1.0, ink);
        block(2.0, 1.0, 1.0, 2.0, ink);
        block(14.0, 1.0, 1.0, 2.0, ink);
        block(0.0, 3.0, 2.0, 3.0, accent);
        block(15.0, 3.0, 2.0, 3.0, accent);
    }

    if pose.sweat > 0.5 {
        // A drop flying off the side of his head.
        block(0.0, 1.0, 1.0, 2.0, accent);
    }

    let glyph = |rows: &[&str], c: f32, row: f32, color: Color| {
        for (y, line) in rows.iter().enumerate() {
            for (x, ch) in line.chars().enumerate() {
                if ch == '#' {
                    block(c + x as f32, row + y as f32, 1.0, 1.0, color);
                }
            }
        }
    };

    // Floating pixel extras: Zs when sleepy, note for headphones, sparkle ticks for wave,
    // or his signature pixel heart when idle/happy (hidden while holding mugs or waving so nothing collides).
    if pose.sleepy > 0.45 {
        let rise = (pose.zz * 2.0).floor();
        glyph(&ZED, 15.0, 0.0 - rise, ink.alpha(ink.a * 0.9));
        if pose.zz > 0.25 {
            glyph(&ZED_LG, 17.0, -4.0 - rise, ink.alpha(ink.a * 0.65));
        }
    } else if pose.prop == Prop::Headphones {
        let rise = (pose.prop_t * 3.0).floor();
        glyph(&NOTE, 17.0, -2.0 - rise, ink);
    } else if pose.sparkle_paw > 0.5 {
        block(rx + 4.0, ry - 1.0, 1.0, 1.0, ink);
        block(rx + 5.0, ry - 2.0, 1.0, 1.0, ink);
    } else if pose.prop == Prop::None {
        let rise = (pose.heart * 3.0).floor();
        let twinkle = pose.heart > 0.0 && ((pose.heart * 10.0) as i32) % 2 == 1;
        if !twinkle {
            glyph(&HEART, 16.0, -2.0 - rise, accent);
        }
    }
}

// ---------------------------------------------------------------------------
// In the Dynamic Island
// ---------------------------------------------------------------------------

static ON_ISLAND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether Docky stands in for the spark in the Dynamic Island (`[docky] island`).
pub fn on_island() -> bool {
    ON_ISLAND.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_on_island(on: bool) {
    ON_ISLAND.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// How Claude Code is getting on, for his face in the island.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mood {
    /// Working: tapping away at his laptop.
    Working,
    /// Needs you: a paw up, waving.
    Waiting,
    /// Finished: happy.
    Done,
}

/// Docky in the island, filling about a `size` square centred on (`cx`, `cy`);
/// `t` is the island's clock (seconds) and `alpha` its fade.
pub fn island(p: &Painter, cx: f32, cy: f32, size: f32, mood: Mood, t: f32, alpha: f32) {
    let mut pose = Pose::IDLE;
    let mut pal = Palette::CARD.shadowless();
    match mood {
        Mood::Working => {
            pose.prop = Prop::Laptop;
            pose.look = (0.0, 0.3);
            let k = (t * 9.0).sin();
            pose.paw_l.1 = -0.09 * k.max(0.0);
            pose.paw_r.1 = -0.09 * (-k).max(0.0);
        }
        Mood::Waiting => {
            pose.paw_r = (0.1 + 0.05 * (t * TAU * 1.5).sin(), -0.5);
            pose.sparkle = 1.0;
            pose.sparkle_paw = 1.0;
            pose.look = (0.2, -0.2);
            pal.extra = Color::rgba(255, 159, 10, 1.0);
        }
        Mood::Done => {
            pose.happy = 1.0;
            pose.sparkle = 1.0;
            pal.extra = Color::rgba(48, 209, 88, 1.0);
        }
    }
    let s = size * 0.8;
    let ground = cy + size * 0.4;
    let paint = || draw(p, cx, ground, s, &pose, &pal);
    if alpha < 0.999 {
        p.faded_in(Rect::new(cx - size, cy - size, 2.0 * size, 2.0 * size), alpha, paint);
    } else {
        paint();
    }
}

// ---------------------------------------------------------------------------
// The computer working hard
// ---------------------------------------------------------------------------

/// The whole machine's CPU use between calls (percent), from GetSystemTimes.
#[derive(Default)]
pub struct Cpu {
    last: Option<(u64, u64)>,
}

impl Cpu {
    /// Percent busy since the last call (None the first time).
    pub fn sample(&mut self) -> Option<f32> {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::GetSystemTimes;
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
        let n = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        // Kernel time includes idle time.
        let (idle, total) = (n(idle), n(kernel) + n(user));
        let prev = self.last.replace((idle, total))?;
        let (di, dt) = (idle.saturating_sub(prev.0), total.saturating_sub(prev.1));
        (dt > 0).then(|| 100.0 * (1.0 - di as f32 / dt as f32))
    }
}

// ---------------------------------------------------------------------------
// The app icon & live Dock tile
// ---------------------------------------------------------------------------

/// Icon-store key for his Dock tile (the app icon).
pub const ICON_KEY: &str = "dockside:docky-icon";

/// What he says when you hover over (and click) his Dock tile.
pub const DOCK_LINES: [&str; 6] = ["hi!! \u{2665}", "hehe", "mrrp?", "boop!", "*wiggles*", "\u{2665}"];

/// Draws DockSide's squircle app tile in vectors (matching `assets/logo-256.png`),
/// with Docky live inside it in `pose` so he blinks, looks around, waves and hops
/// right inside the Settings Dock preview.
pub fn draw_tile(p: &Painter, r: Rect, pose: &Pose) {
    let rad = r.w * 0.225;
    p.soft_shadow(r, rad, 4, r.w * 0.018, 0.18, r.h * 0.03, Color::rgba(14, 28, 96, 1.0));
    p.fill_round_gradient(r, rad, Color::rgba(108, 174, 255, 1.0), Color::rgba(38, 82, 234, 1.0));

    p.clipped(r, || {
        // Subtle glossy top sheen, as in the macOS icon.
        let sheen = Rect::new(r.x, r.y, r.w, r.h * 0.22);
        p.fill_round_gradient(sheen, rad, Color::gray(255, 0.22), Color::gray(255, 0.0));

        // Translucent frosted mini-Dock shelf across the lower third, with two side app cards.
        let shelf = Rect::new(r.x + r.w * 0.10, r.y + r.h * 0.565, r.w * 0.80, r.h * 0.255);
        let srad = shelf.h * 0.36;
        p.fill_round(shelf, srad, Color::rgba(205, 222, 255, 0.36));
        let card_m = shelf.h * 0.16;
        let card_w = shelf.w * 0.22;
        let card_h = shelf.h - 2.0 * card_m;
        for cx_card in [shelf.x + card_m, shelf.right() - card_m - card_w] {
            p.fill_round(Rect::new(cx_card, shelf.y + card_m, card_w, card_h), card_h * 0.32, Color::rgba(232, 240, 255, 0.72));
        }

        // Live Docky sitting in his mini-Dock, with his signature white corner dashes.
        let mut tile_pose = *pose;
        if tile_pose.sleepy < 0.3 && tile_pose.sparkle_paw < 0.3 {
            tile_pose.sparkle = tile_pose.sparkle.max(1.0);
            tile_pose.sparkle_bright = 1.0;
        }
        let tile_pal = Palette {
            line: Color::rgba(45, 86, 214, 1.0),
            shadow: Color::rgba(22, 48, 164, 0.22),
            ..Palette::CARD
        };
        draw(p, r.cx(), shelf.bottom() - shelf.h * 0.10, r.w * 0.42, &tile_pose, &tile_pal);

        // Mini running-indicator dot centered on the mini-Dock's bottom rim.
        p.fill_circle(r.cx(), shelf.bottom() - shelf.h * 0.045, (r.w * 0.015).max(1.0), Color::WHITE);
    });
    p.stroke_round(r.inset(0.35, 0.35), rad, Color::gray(255, 0.28), 0.7);
}

/// DockSide's icon (assets/logo-256.png, from dockside.ico).
#[allow(dead_code)]
static ICON_PNG: &[u8] = include_bytes!("../../../assets/logo-256.png");

/// The icon decoded to `px` square (premultiplied BGRA).
pub fn icon_pixels(gfx: &crate::gfx::Gfx, px: u32) -> Option<crate::gfx::icons::IconPixels> {
    use windows::Win32::Graphics::Imaging::*;
    unsafe {
        let stream = gfx.wic.CreateStream().ok()?;
        stream.InitializeFromMemory(ICON_PNG).ok()?;
        let decoder = gfx.wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?;
        let converter = gfx.wic.CreateFormatConverter().ok()?;
        converter
            .Initialize(&decoder.GetFrame(0).ok()?, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .ok()?;
        let px = px.clamp(1, 256);
        // The logo fills its whole canvas; Dock icons leave a margin round
        // their tile (Finder's and the squircle tiles' 6% a side), so he's
        // shrunk into the same tile and centred, to sit the same size as them.
        let inner = ((px as f32 * 0.88).round() as u32).clamp(1, px);
        let scaler = gfx.wic.CreateBitmapScaler().ok()?;
        scaler.Initialize(&converter, inner, inner, WICBitmapInterpolationModeHighQualityCubic).ok()?;
        let mut tile = vec![0u8; (inner * inner * 4) as usize];
        scaler.CopyPixels(std::ptr::null(), inner * 4, &mut tile).ok()?;
        let at = (px - inner) / 2;
        let mut data = vec![0u8; (px * px * 4) as usize];
        for (y, row) in tile.chunks_exact((inner * 4) as usize).enumerate() {
            let start = (((at + y as u32) * px + at) * 4) as usize;
            data[start..start + row.len()].copy_from_slice(row);
        }
        Some(crate::gfx::icons::IconPixels { width: px, height: px, data })
    }
}

/// Draws DockSide's icon into `r` (decoded once per size, then kept).
#[allow(dead_code)]
pub fn draw_icon(p: &Painter, r: Rect, scale: f32) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ("docky-icon", r.w.to_bits(), r.h.to_bits(), scale.to_bits()).hash(&mut h);
    let px = (r.w.max(r.h) * scale).ceil() as u32;
    p.cached(h.finish(), r, |q| {
        if let Some(b) = icon_pixels(q.gfx, px).and_then(|i| i.to_bitmap(q.rt)) {
            q.bitmap(&b, r, 1.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a sheet of his poses to `DOCKSIDE_DOCKY_OUT` (a folder): `cargo test docky -- --ignored`.
    #[test]
    #[ignore]
    fn writes_pngs() {
        let Ok(out) = std::env::var("DOCKSIDE_DOCKY_OUT") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let gfx = crate::gfx::Gfx::new().unwrap();
        // Row 0: the concept's eight (Animations Examples), then each act across time.
        let concept: [(Act, f32, Pose); 8] = [
            (Act::Blink, 0.0, Pose::IDLE),
            (Act::Blink, 0.11, Pose::IDLE),
            (Act::Look, 1.55, Pose::IDLE),
            (Act::Happy, 0.30, Pose::IDLE),
            (Act::Snore, 1.8, Pose::DOZING),
            (Act::Jump, 0.44, Pose::IDLE),
            (Act::Sit, 0.85, Pose::IDLE),
            (Act::Wave, 0.72, Pose::IDLE),
        ];
        let acts = [Act::Blink, Act::Look, Act::Happy, Act::Jump, Act::Sit, Act::Wave, Act::Love, Act::Dance, Act::Dizzy, Act::Snore, Act::Stir, Act::Doze, Act::Wake];
        let (cw, ch) = (150.0, 150.0);
        let cols = 8;
        let rows = 1 + acts.len();
        let (w, h) = ((cw * cols as f32) as i32, (ch * rows as f32) as i32);
        let mut canvas = crate::gfx::Canvas::new(&gfx, w, h, 1.0).unwrap();
        canvas
            .draw(&gfx, |p| {
                p.fill_rect(Rect::new(0.0, 0.0, w as f32, h as f32), Color::rgba(222, 230, 252, 1.0));
                let tile = |x: f32, y: f32| {
                    let r = Rect::new(x + 8.0, y + 8.0, cw - 16.0, ch - 16.0);
                    p.fill_round(r, 18.0, Color::rgba(203, 218, 252, 1.0));
                };
                for (i, (act, t, base)) in concept.iter().enumerate() {
                    let x = i as f32 * cw;
                    tile(x, 0.0);
                    draw(p, x + cw / 2.0, ch - 34.0, 76.0, &act.pose(*t, *base), &Palette::LIGHT);
                }
                for (row, act) in acts.iter().enumerate() {
                    let base = if matches!(act, Act::Snore | Act::Stir | Act::Doze) { Pose::ASLEEP } else { Pose::IDLE };
                    let y = (row + 1) as f32 * ch;
                    for col in 0..cols {
                        let t = act.secs() * col as f32 / (cols - 1) as f32 * 0.999;
                        let pose = act.pose(t, base);
                        let x = col as f32 * cw;
                        if col == cols - 1 {
                            let r = Rect::new(x + 8.0, y + 8.0, cw - 16.0, ch - 16.0);
                            p.fill_round(r, 16.0, Color::rgba(34, 37, 50, 1.0));
                            draw_pixel(p, r.inset(10.0, 18.0), &act.pose(act.secs() * 0.45, base), Color::rgba(212, 222, 250, 1.0), Color::rgba(212, 222, 250, 1.0), Color::rgba(34, 37, 50, 1.0));
                        } else {
                            tile(x, y);
                            draw(p, x + cw / 2.0, y + ch - 34.0, 76.0, &pose, &Palette::LIGHT);
                        }
                    }
                }
            })
            .unwrap();
        crate::gfx::write_png(&gfx, w as u32, h as u32, &canvas.pixels(), &format!("{out}/poses.png"));

        // His activities: smooth on top, pixels below.
        let things: [(Activity, f32); 8] =
            [(Activity::Walk, 0.6), (Activity::Coffee, 0.5), (Activity::Coffee, 2.75), (Activity::Read, 1.0), (Activity::Read, 6.3), (Activity::Nap, 1.0), (Activity::Music, 0.3), (Activity::Laptop, 1.0)];
        let (w, h) = ((cw * 8.0) as i32, (ch * 2.0) as i32);
        let mut canvas = crate::gfx::Canvas::new(&gfx, w, h, 1.0).unwrap();
        canvas
            .draw(&gfx, |p| {
                p.fill_rect(Rect::new(0.0, 0.0, w as f32, h as f32), Color::rgba(222, 230, 252, 1.0));
                for (i, (a, t)) in things.iter().enumerate() {
                    let mut pet = Pet::new();
                    pet.start(*a, Some(60.0));
                    pet.blend = None;
                    pet.act = None;
                    if let Some(d) = pet.doing.as_mut() {
                        d.1 = Instant::now() - Duration::from_secs_f32(*t);
                    }
                    if let Some(wk) = pet.walking.as_mut() {
                        wk.2 = Instant::now() - Duration::from_secs_f32(*t);
                    }
                    pet.bob = 0.6;
                    let mut pose = pet.pose(Instant::now());
                    pose.walk = 0.0;
                    let x = i as f32 * cw;
                    let r = Rect::new(x + 8.0, 8.0, cw - 16.0, ch - 16.0);
                    p.fill_round_gradient(r, 18.0, Color::rgba(130, 162, 250, 1.0), Color::rgba(98, 130, 228, 1.0));
                    draw(p, x + cw / 2.0, ch - 30.0, 70.0, &pose, &Palette::CARD);
                    let r = Rect::new(x + 8.0, ch + 8.0, cw - 16.0, ch - 16.0);
                    p.fill_round(r, 16.0, Color::rgba(34, 37, 50, 1.0));
                    draw_pixel(p, r.inset(8.0, 20.0), &pose, Color::rgba(212, 222, 250, 1.0), Color::rgba(212, 222, 250, 1.0), Color::rgba(34, 37, 50, 1.0));
                }
            })
            .unwrap();
        crate::gfx::write_png(&gfx, w as u32, h as u32, &canvas.pixels(), &format!("{out}/activities.png"));
    }

    /// Frames of Docky in his widget tile, 60 per second, for videos: one folder
    /// per sequence of acts. `DOCKSIDE_DOCKY_FRAMES=<dir> cargo test docky_video_frames -- --ignored`.
    #[test]
    #[ignore]
    fn docky_video_frames() {
        let Ok(out) = std::env::var("DOCKSIDE_DOCKY_FRAMES") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let gfx = crate::gfx::Gfx::new().unwrap();
        let size = 640;
        let s = size as f32;
        let seqs: [(&str, &[Act]); 3] = [
            ("hello", &[Act::Jump, Act::Happy, Act::Blink, Act::Wave, Act::Look]),
            ("outro", &[Act::Wave, Act::Love, Act::Blink, Act::Happy]),
            ("idle", &[Act::Blink, Act::Look, Act::Blink]),
        ];
        for (name, acts) in seqs {
            let dir = format!("{out}/{name}");
            std::fs::create_dir_all(&dir).unwrap();
            let mut frame = 0;
            for act in acts.iter() {
                let n = (act.secs() * 60.0).round() as i32;
                for k in 0..n {
                    let pose = act.pose(k as f32 / 60.0, Pose::IDLE);
                    let mut canvas = crate::gfx::Canvas::new(&gfx, size, size, 1.0).unwrap();
                    canvas
                        .draw(&gfx, |p| {
                            p.fill_round_gradient(Rect::new(0.0, 0.0, s, s), 0.0, Color::rgba(130, 162, 250, 1.0), Color::rgba(98, 130, 228, 1.0));
                            draw(p, s / 2.0, s * 0.8, s * 0.47, &pose, &Palette::CARD);
                        })
                        .unwrap();
                    crate::gfx::write_png(&gfx, size as u32, size as u32, &canvas.pixels(), &format!("{dir}/{frame:04}.png"));
                    frame += 1;
                }
            }
        }
    }

    /// Docky's island faces (on black, at island and zoomed sizes) and Hot:
    /// `cargo test docky_island -- --ignored`.
    #[test]
    #[ignore]
    fn docky_island_png() {
        let Ok(out) = std::env::var("DOCKSIDE_DOCKY_OUT") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let gfx = crate::gfx::Gfx::new().unwrap();
        let (w, h) = (760, 220);
        let mut canvas = crate::gfx::Canvas::new(&gfx, w, h, 1.0).unwrap();
        canvas
            .draw(&gfx, |p| {
                p.fill_rect(Rect::new(0.0, 0.0, w as f32, h as f32), Color::rgba(222, 230, 252, 1.0));
                for (i, mood) in [Mood::Working, Mood::Waiting, Mood::Done].into_iter().enumerate() {
                    let x = 20.0 + i as f32 * 150.0;
                    p.fill_round(Rect::new(x, 20.0, 130.0, 36.0), 18.0, Color::BLACK);
                    island(p, x + 22.0, 38.0, 26.0, mood, 0.3, 1.0);
                    p.fill_round(Rect::new(x, 70.0, 130.0, 130.0), 24.0, Color::BLACK);
                    island(p, x + 65.0, 135.0, 90.0, mood, 0.3, 1.0);
                }
                p.fill_round(Rect::new(480.0, 70.0, 130.0, 130.0), 24.0, Color::rgba(133, 164, 249, 1.0));
                draw(p, 545.0, 175.0, 80.0, &Act::Hot.pose(1.0, Pose::IDLE), &Palette::CARD);
                p.fill_round(Rect::new(620.0, 70.0, 130.0, 130.0), 16.0, Color::rgba(36, 38, 54, 1.0));
                let ink = Color::rgba(226, 229, 240, 1.0);
                draw_pixel(p, Rect::new(628.0, 90.0, 114.0, 90.0), &Act::Hot.pose(1.0, Pose::IDLE), ink, ink, Color::rgba(36, 38, 54, 1.0));
            })
            .unwrap();
        crate::gfx::write_png(&gfx, w as u32, h as u32, &canvas.pixels(), &format!("{out}/island.png"));
    }

}
