//! Docky's chat window: a macOS Messages-style app, opened from Docky's tile
//! in the Dock. A sidebar with his conversation, your messages in blue on the
//! right and his in grey on the left (his little face beside them), a typing
//! indicator while he thinks, and a message field. It shares one conversation
//! with the speech box on his desktop widget (see `docky_ai`).

use std::time::Instant;

use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_METRICS;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{Motion, Scroll};
use crate::chrome::{self, Light, WindowAnim, SHADOW};
use crate::docky::{self, Palette as DockyPalette, Pose};
use crate::docky_ai::{self, Message};
use crate::finder::text::TextField;
use crate::geom::{Color, Rect};
use crate::gfx::{Align, Canvas, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::theme::Theme;
use crate::win::{self, Monitor};

/// Dock path for the window: Docky's own tile, so it shows as running there.
pub const CHAT_PATH: &str = crate::dock::DOCKY_PATH;
pub const TITLE: &str = "Docky";

const W: f32 = 820.0;
const H: f32 = 580.0;
const SIDEBAR: f32 = 260.0;
const HEADER: f32 = 78.0;
const INPUT_H: f32 = 56.0;
/// Bubbles: the widest a message gets (a share of the conversation's width), padding, gaps.
const BUBBLE_MAX: f32 = 0.68;
const PAD_X: f32 = 12.0;
const PAD_Y: f32 = 7.0;
const GAP: f32 = 4.0;
const GROUP_GAP: f32 = 12.0;
const AVATAR: f32 = 28.0;
const FONT: f32 = 13.5;
/// A time is shown between messages this far apart (seconds).
const TIME_GAP: i64 = 15 * 60;

/// A new message rises into place and fades in over this long (seconds),
/// each of several arriving together a little after the last.
const ARRIVE: f32 = 0.34;
const STAGGER: f32 = 0.07;
/// The typing dots fade in over this long.
const DOTS_IN: f32 = 0.22;

const BLUE: Color = Color::rgba(10, 132, 255, 1.0);
const DOCKY_TOP: Color = Color::rgba(133, 164, 249, 1.0);
const DOCKY_BOTTOM: Color = Color::rgba(102, 134, 228, 1.0);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Target {
    Light(Light),
    Compose,
    Send,
    Field,
    Setup,
}

pub enum ChatEvent {
    None,
    Close,
    Action(Action),
}

pub struct DockyChat {
    pub hwnd: HWND,
    canvas: Canvas,
    scale: f32,
    msg_hwnd: isize,
    field: TextField,
    /// The conversation's version at the last layout, and each message's bubble size.
    laid_for: (u64, f32),
    sizes: Vec<(f32, f32)>,
    scroll: Scroll,
    max_scroll: f32,
    hits: Vec<(Rect, Target)>,
    hover: Option<Target>,
    pressed: Option<Target>,
    lights_hover: bool,
    window: WindowAnim,
    motion: Motion<Target>,
    /// For the typing dots.
    born: Instant,
    /// Follow the newest message (unless you've scrolled up to read).
    stick_to_bottom: bool,
    /// How many messages were shown, and since which (and when) new ones arrived.
    seen: usize,
    arrived: (usize, Instant),
    /// When he started typing (for his dots fading in).
    typing_since: Option<Instant>,
    /// Shown (and focused) after its first frame is drawn, so it never appears blank.
    shown: bool,
    pub tracking: bool,
    pub dirty: bool,
}

impl DockyChat {
    pub fn new(gfx: &Gfx, monitor: &Monitor, msg_hwnd: isize) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + (mon.bottom - mon.top - h) * 2 / 5;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        // The minimize box lets the genie tell an app window from a popup.
        let hwnd = win::create_window(WS_EX_LAYERED | WS_EX_APPWINDOW, WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX, rect, HWND::default())?;
        unsafe {
            let _ = SetWindowTextW(hwnd, w!("Docky"));
        }
        let mut chat = Self {
            hwnd,
            canvas: Canvas::new(gfx, w, h, s)?,
            scale: s,
            msg_hwnd,
            field: TextField::default(),
            laid_for: (u64::MAX, 0.0),
            sizes: Vec::new(),
            scroll: Scroll::default(),
            max_scroll: 0.0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            lights_hover: false,
            window: WindowAnim::opening(),
            motion: Motion::default(),
            born: Instant::now(),
            stick_to_bottom: true,
            // What's already there when it opens doesn't animate in.
            seen: docky_ai::chat().0.len(),
            arrived: (usize::MAX, Instant::now()),
            typing_since: None,
            shown: false,
            tracking: false,
            dirty: true,
        };
        chat.dirty = true;
        Ok(chat)
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn show(&mut self) {
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.dirty = true;
    }

    pub fn on_size(&mut self) {
        if self.window.restored(self.hwnd) {
            self.dirty = true;
        }
    }

    /// The conversation changed (a reply came in): show it.
    pub fn update(&mut self) {
        self.dirty = true;
    }

    pub fn tick(&mut self, dt: f32) -> bool {
        let mut moving = self.window.tick(dt, self.hwnd) | self.motion.tick(dt) | self.scroll.tick(dt);
        // The typing dots bounce while he thinks; new messages rise in (briefly).
        if (docky_ai::chat_pending() || self.arriving()) && !unsafe { IsIconic(self.hwnd).as_bool() } {
            moving = true;
        }
        self.dirty |= moving;
        moving
    }

    pub fn animating(&self) -> bool {
        self.window.moving() || self.motion.moving() || self.scroll.moving() || docky_ai::chat_pending() || self.arriving()
    }

    /// Whether new messages are still rising in.
    fn arriving(&self) -> bool {
        let (from, at) = self.arrived;
        let n = self.seen.saturating_sub(from.min(self.seen)) as f32;
        from != usize::MAX && at.elapsed().as_secs_f32() < ARRIVE + STAGGER * n + 0.03
    }

    fn body(&self) -> Rect {
        Rect::new(SHADOW, SHADOW, W, H)
    }

    fn content(&self) -> Rect {
        let b = self.body();
        Rect::new(b.x + SIDEBAR, b.y, b.w - SIDEBAR, b.h)
    }

    /// Where the messages scroll: between the header and the message field.
    fn viewport(&self) -> Rect {
        let c = self.content();
        Rect::new(c.x, c.y + HEADER, c.w, c.h - HEADER - INPUT_H)
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Target> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, t)| *t)
    }

    /// The window drags by its header and the sidebar's top, apart from controls.
    pub fn is_caption(&self, x: f32, y: f32) -> bool {
        let b = self.body();
        let top = Rect::new(b.x, b.y, b.w, HEADER);
        top.contains(x, y) && self.hit_at(x, y).is_none()
    }

    fn send(&mut self) {
        let text = self.field.string();
        if text.trim().is_empty() || docky_ai::chat_pending() {
            return;
        }
        if docky_ai::send(&text, self.msg_hwnd) {
            self.field.clear();
            self.stick_to_bottom = true;
        }
        self.dirty = true;
    }

    pub fn on_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.field.insert(&c.to_string());
        self.dirty = true;
    }

    pub fn on_key(&mut self, vk: u16, ctrl: bool, shift: bool) -> ChatEvent {
        const ENTER: u16 = 0x0D;
        self.dirty = true;
        match vk {
            ENTER => self.send(),
            0x57 if ctrl => return ChatEvent::Close,
            0x56 if ctrl => {
                if let Some(s) = crate::finder::fs::paste_text() {
                    // One line: paragraphs become spaces.
                    self.field.insert(&s.replace(['\r', '\n'], " "));
                }
            }
            _ => {
                self.field.on_key(vk, ctrl, shift);
            }
        }
        ChatEvent::None
    }

    pub fn on_wheel(&mut self, delta: i32) {
        self.scroll.wheel_smooth(delta, 40.0, self.max_scroll);
        self.stick_to_bottom = self.scroll.target() >= self.max_scroll - 2.0;
        self.dirty = true;
    }

    pub fn on_move(&mut self, x: f32, y: f32) {
        let h = self.hit_at(x, y);
        let lights = Rect::new(SHADOW, SHADOW, 76.0, 40.0).contains(x, y);
        if h != self.hover || lights != self.lights_hover {
            self.hover = h;
            self.lights_hover = lights;
            self.dirty = true;
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        self.lights_hover = false;
        if self.hover.take().is_some() {
            self.dirty = true;
        }
        self.dirty = true;
    }

    pub fn on_down(&mut self, x: f32, y: f32) {
        self.pressed = self.hit_at(x, y);
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32) -> ChatEvent {
        let pressed = self.pressed.take();
        self.dirty = true;
        let Some(t) = self.hit_at(x, y).filter(|t| Some(*t) == pressed) else { return ChatEvent::None };
        match t {
            Target::Light(Light::Close) => ChatEvent::Close,
            Target::Light(Light::Minimize) => {
                self.window.minimize();
                ChatEvent::None
            }
            Target::Light(Light::Zoom) => ChatEvent::None,
            Target::Send => {
                self.send();
                ChatEvent::None
            }
            Target::Field => {
                let end = self.field.text.len();
                self.field.set_caret(end, false);
                ChatEvent::None
            }
            Target::Compose => {
                if !docky_ai::chat().0.is_empty()
                    && win::system::confirm("New Conversation", "Start over with Docky? Your conversation with him will be cleared.")
                {
                    docky_ai::clear_chat();
                    self.laid_for = (u64::MAX, 0.0);
                }
                ChatEvent::None
            }
            Target::Setup => ChatEvent::Action(Action::OpenSettings(Some(crate::settings::Page::Docky))),
        }
    }

    /// Each message's bubble size (text width and height, before padding), laid
    /// out again only when the conversation changes.
    fn layout(&mut self, gfx: &Gfx, theme: &Theme, messages: &[Message], version: u64) {
        let max_w = self.content().w * BUBBLE_MAX - 2.0 * PAD_X;
        if self.laid_for == (version, max_w) {
            return;
        }
        self.laid_for = (version, max_w);
        let f = gfx.format_block(&theme.font, FONT, Weight::Regular, Align::Leading);
        self.sizes = messages
            .iter()
            .map(|m| {
                let mut met = DWRITE_TEXT_METRICS::default();
                match gfx.layout(&m.text, &f, max_w, 10_000.0) {
                    Some(l) if unsafe { l.GetMetrics(&mut met) }.is_ok() => (met.widthIncludingTrailingWhitespace.ceil().min(max_w), met.height.ceil()),
                    _ => (max_w, FONT * 1.4),
                }
            })
            .collect();
    }

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme) {
        self.dirty = false;
        if !self.window.begin_render(self.hwnd) {
            return;
        }
        let (messages, pending, version) = docky_ai::chat();
        let now = Instant::now();
        if messages.len() < self.seen {
            self.seen = messages.len();
        } else if messages.len() > self.seen {
            self.arrived = (self.seen, now);
            self.seen = messages.len();
        }
        self.typing_since = match (pending, self.typing_since) {
            (true, None) => Some(now),
            (true, since) => since,
            (false, _) => None,
        };
        let arrived = self.arrived;
        let dots_in = self.typing_since.map_or(1.0, |t| ease_out(now.duration_since(t).as_secs_f32() / DOTS_IN));
        self.layout(gfx, theme, &messages, version);
        self.motion.sweep();
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        let body = self.body();
        let content = self.content();
        let view = self.viewport();

        // How tall the conversation is, for scrolling (bubbles, times, the dots).
        let mut total = 16.0;
        for (i, m) in messages.iter().enumerate() {
            if shows_time(&messages, i) {
                total += 28.0;
            } else if i > 0 && messages[i - 1].me != m.me {
                total += GROUP_GAP - GAP;
            }
            total += self.sizes.get(i).map_or(20.0, |s| s.1) + 2.0 * PAD_Y + GAP;
        }
        if pending {
            total += GROUP_GAP + 34.0;
        }
        total += 12.0;
        self.max_scroll = (total - view.h).max(0.0);
        if self.stick_to_bottom {
            self.scroll.to(self.max_scroll);
        }
        self.scroll.clamp(self.max_scroll);
        let scroll = self.scroll.offset();

        let (zx, zy, zoom, alpha) = self.window.transform(body);
        let ready = docky_ai::ready();
        let field_text = self.field.string();
        let (caret, (sa, sb)) = (self.field.caret, self.field.range());
        let hover = self.hover;
        let lights_hover = self.lights_hover;
        let motion = &self.motion;
        let sizes = &self.sizes;
        let t = self.born.elapsed().as_secs_f32();
        let mut hits = Vec::new();

        let r = self.canvas.draw(gfx, |p| {
            p.scaled_about(zx, zy, zoom, || {
                let pal = chrome::Palette::new(theme);
                chrome::window(p, body, SIDEBAR, theme, &pal, chrome::RADIUS);
                let lights = chrome::traffic_lights(p, body, if lights_hover { 1.0 } else { 0.0 }, false);
                hits.extend(lights.into_iter().map(|(r, l)| (r, Target::Light(l))));

                // --- Sidebar: compose, Docky pinned, his conversation ---
                let icons = p.gfx.format("Segoe Fluent Icons", 15.0, Weight::Regular, Align::Center);
                let compose = Rect::new(body.x + SIDEBAR - 42.0, body.y + 8.0, 30.0, 26.0);
                let hot = motion.fade(Target::Compose, hover == Some(Target::Compose));
                if hot > 0.0 {
                    p.fill_round(compose, 7.0, theme.text.alpha(0.08 * hot));
                }
                p.text("\u{E70F}", &icons, compose, theme.text_secondary);
                hits.push((compose, Target::Compose));

                let pin = body.y + 64.0;
                avatar(p, body.x + SIDEBAR / 2.0, pin + 30.0, 30.0, &Pose::HAPPY);
                let name = p.gfx.format(&theme.font, 12.0, Weight::Regular, Align::Center);
                p.text("Docky", &name, Rect::new(body.x, pin + 64.0, SIDEBAR, 18.0), theme.text_secondary);

                let row = Rect::new(body.x + 10.0, pin + 98.0, SIDEBAR - 20.0, 60.0);
                p.fill_round(row, 10.0, BLUE);
                avatar(p, row.x + 28.0, row.cy(), 20.0, &Pose::IDLE);
                let bold = p.gfx.format(&theme.font, 13.0, Weight::SemiBold, Align::Leading);
                let small = p.gfx.format(&theme.font, 12.0, Weight::Regular, Align::Leading);
                let when_f = p.gfx.format(&theme.font, 11.5, Weight::Regular, Align::Trailing);
                p.text("Docky", &bold, Rect::new(row.x + 56.0, row.y + 10.0, row.w - 120.0, 18.0), Color::WHITE);
                let last = messages.last();
                if let Some(m) = last {
                    p.text(&clock(m.at), &when_f, Rect::new(row.x, row.y + 10.0, row.w - 10.0, 18.0), Color::gray(255, 0.8));
                }
                let preview = match (pending, last) {
                    (true, _) => "is typing\u{2026}".to_string(),
                    (false, Some(m)) => one_line(&m.text),
                    (false, None) => "say hi!".to_string(),
                };
                p.text(&preview, &small, Rect::new(row.x + 56.0, row.y + 30.0, row.w - 66.0, 18.0), Color::gray(255, 0.85));

                // --- Header: his face and name, centred ---
                let hx = content.cx();
                avatar(p, hx, body.y + 28.0, 19.0, if pending { &THINKING } else { &Pose::IDLE });
                let tag_f = p.gfx.format(&theme.font, 11.5, Weight::SemiBold, Align::Center);
                let tag = Rect::new(hx - 34.0, body.y + 50.0, 68.0, 20.0);
                p.fill_round(tag, 10.0, theme.text.alpha(0.07));
                p.text("Docky", &tag_f, tag, theme.text);
                p.line(content.x, content.y + HEADER, content.right(), content.y + HEADER, pal.divider, 1.0);

                // --- The conversation ---
                p.clipped(view, || {
                    if messages.is_empty() && !pending {
                        avatar(p, view.cx(), view.cy() - 30.0, 46.0, &Pose::HAPPY);
                        let f = p.gfx.format(&theme.font, 14.0, Weight::SemiBold, Align::Center);
                        p.text("say hi to docky!", &f, Rect::new(view.x, view.cy() + 26.0, view.w, 22.0), theme.text_secondary);
                        return;
                    }
                    let text_f = p.gfx.format_block(&theme.font, FONT, Weight::Regular, Align::Leading);
                    let time_f = p.gfx.format(&theme.font, 11.0, Weight::Regular, Align::Center);
                    let (left_x, right_x) = (view.x + 16.0 + AVATAR + 8.0, view.right() - 16.0);
                    let mut y = view.y + 16.0 - scroll;
                    let dark = theme.dark;
                    let theirs = if dark { Color::rgba(58, 58, 60, 1.0) } else { Color::rgba(233, 233, 235, 1.0) };
                    let their_ink = if dark { Color::WHITE } else { Color::rgba(20, 20, 22, 1.0) };
                    for (i, m) in messages.iter().enumerate() {
                        if shows_time(&messages, i) {
                            p.text(&clock(m.at), &time_f, Rect::new(view.x, y + 2.0, view.w, 20.0), theme.text_secondary);
                            y += 28.0;
                        } else if i > 0 && messages[i - 1].me != m.me {
                            y += GROUP_GAP - GAP;
                        }
                        let (tw, th) = sizes.get(i).copied().unwrap_or((60.0, 18.0));
                        let (bw, bh) = (tw + 2.0 * PAD_X, th + 2.0 * PAD_Y);
                        let b = if m.me { Rect::new(right_x - bw, y, bw, bh) } else { Rect::new(left_x, y, bw, bh) };
                        // Arriving: rises a little and fades in, growing from its own side.
                        let k = if i >= arrived.0 {
                            let late = STAGGER * (i - arrived.0) as f32;
                            ease_out((now.duration_since(arrived.1).as_secs_f32() - late) / ARRIVE)
                        } else {
                            1.0
                        };
                        let b = b.offset(0.0, 14.0 * (1.0 - k));
                        if b.bottom() > view.y - 40.0 && b.y < view.bottom() + 40.0 && k > 0.0 {
                            let corner = if m.me { b.right() } else { b.x };
                            p.faded_in(b.inset(-4.0, -4.0), k, || {
                                p.scaled_about(corner, b.bottom(), 0.92 + 0.08 * k, || {
                                    p.fill_round(b, 17.0f32.min(bh / 2.0), if m.me { BLUE } else { theirs });
                                    if let Some(l) = p.gfx.layout(&m.text, &text_f, tw + 1.0, th + 4.0) {
                                        p.text_layout(&l, b.x + PAD_X, b.y + PAD_Y, if m.me { Color::WHITE } else { their_ink });
                                    }
                                });
                            });
                            // His face beside the last of his messages in a row.
                            let last_of_his = !m.me && messages.get(i + 1).is_none_or(|n| n.me);
                            if last_of_his && !(pending && i + 1 == messages.len()) {
                                avatar(p, view.x + 16.0 + AVATAR / 2.0, b.bottom() - AVATAR / 2.0, AVATAR / 2.0, &Pose::IDLE);
                            }
                        }
                        y += bh + GAP;
                    }
                    if pending {
                        // He's typing: three dots, bouncing in turn.
                        y += GROUP_GAP - GAP;
                        let b = Rect::new(left_x, y + 10.0 * (1.0 - dots_in), 58.0, 34.0);
                        p.faded_in(b.inset(-4.0, -4.0), dots_in, || {
                            p.scaled_about(b.x, b.bottom(), 0.85 + 0.15 * dots_in, || {
                                p.fill_round(b, 17.0, theirs);
                                for k in 0..3 {
                                    let phase = (t * 6.0 - k as f32 * 0.9).sin().max(0.0);
                                    let dot = their_ink.alpha(0.35 + 0.45 * phase);
                                    p.fill_circle(b.x + 17.0 + k as f32 * 12.0, b.cy() - 2.5 * phase, 3.6, dot);
                                }
                            });
                        });
                        avatar(p, view.x + 16.0 + AVATAR / 2.0, b.bottom() - AVATAR / 2.0, AVATAR / 2.0, &THINKING);
                    }
                });

                // --- The message field ---
                let bar = Rect::new(content.x, content.bottom() - INPUT_H, content.w, INPUT_H);
                let field = Rect::new(bar.x + 16.0, bar.y + 12.0, bar.w - 32.0, 32.0);
                let field_bg = if theme.dark { Color::gray(255, 0.06) } else { Color::WHITE };
                p.fill_round(field, 16.0, field_bg);
                p.stroke_round(field.inset(0.5, 0.5), 16.0, theme.text.alpha(0.14), 1.0);
                let inner = Rect::new(field.x + 14.0, field.y, field.w - 52.0, field.h);
                let input_f = p.gfx.format(&theme.font, 13.5, Weight::Regular, Align::Leading);
                if let Err(why) = &ready {
                    // Not set up yet: say so, and where.
                    p.text(&format!("{why}"), &input_f, inner, theme.text_secondary);
                    let setup = Rect::new(field.right() - 84.0, field.y + 4.0, 78.0, 24.0);
                    p.fill_round(setup, 12.0, BLUE);
                    let bf = p.gfx.format(&theme.font, 12.0, Weight::SemiBold, Align::Center);
                    p.text("Set Up", &bf, setup, Color::WHITE);
                    hits.push((setup, Target::Setup));
                } else {
                    hits.push((field, Target::Field));
                    let prefix = |n: usize| p.gfx.measure(&field_text.chars().take(n).collect::<String>(), &input_f);
                    let caret_x = prefix(caret);
                    // Long messages slide left to keep the caret in view.
                    let shift = (caret_x - (inner.w - 6.0)).max(0.0);
                    p.clipped(inner, || {
                        if field_text.is_empty() {
                            p.text("Message Docky", &input_f, inner, theme.text_disabled);
                        } else {
                            if sa != sb {
                                let (x0, x1) = (prefix(sa), prefix(sb));
                                p.fill_rect(Rect::new(inner.x + x0 - shift, inner.cy() - 9.0, x1 - x0, 18.0), theme.accent.alpha(0.3));
                            }
                            p.text(&field_text, &input_f, Rect::new(inner.x - shift, inner.y, inner.w + shift + 400.0, inner.h), theme.text);
                        }
                        let cx = inner.x + caret_x - shift + 0.5;
                        p.line(cx, inner.cy() - 8.0, cx, inner.cy() + 8.0, theme.accent, 1.5);
                    });
                    // Send: an arrow in a blue circle once there's something to send.
                    let can_send = !field_text.trim().is_empty() && !pending;
                    let (sx, sy) = (field.right() - 18.0, field.cy());
                    p.fill_circle(sx, sy, 12.0, if can_send { BLUE } else { theme.text.alpha(0.12) });
                    let ink = Color::WHITE;
                    p.line(sx, sy + 5.0, sx, sy - 5.0, ink, 1.8);
                    p.line(sx - 4.5, sy - 0.5, sx, sy - 5.0, ink, 1.8);
                    p.line(sx + 4.5, sy - 0.5, sx, sy - 5.0, ink, 1.8);
                    if can_send {
                        hits.push((Rect::new(sx - 14.0, sy - 14.0, 28.0, 28.0), Target::Send));
                    }
                }
            });
        });
        self.hits = hits;
        let presented = r.and_then(|_| self.canvas.present_alpha(self.hwnd, rc.left, rc.top, alpha));
        if let Err(e) = presented {
            log!("docky chat render failed: {e}");
        }
        if !self.shown {
            self.shown = true;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOW);
                let _ = SetForegroundWindow(self.hwnd);
            }
        }
    }
}

/// His face while he thinks: looking up and away.
const THINKING: Pose = Pose { look: (0.6, -0.6), ..Pose::IDLE };

/// Docky's round avatar: his blue, and him peeking up from the bottom.
fn avatar(p: &Painter, cx: f32, cy: f32, r: f32, pose: &Pose) {
    p.fill_round_gradient(Rect::new(cx - r, cy - r, 2.0 * r, 2.0 * r), r, DOCKY_TOP, DOCKY_BOTTOM);
    let pal = DockyPalette::CARD.shadowless();
    let clip = Rect::new(cx - r, cy - r, 2.0 * r, 2.0 * r);
    p.clipped(clip, || docky::draw(p, cx, cy + r * 0.62, r * 1.15, pose, &pal));
}

/// 0..1 easing out (fast, then settling), clamped.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Whether a time goes above message `i` (the first, or after a long pause).
fn shows_time(messages: &[Message], i: usize) -> bool {
    i == 0 || messages[i].at - messages[i - 1].at >= TIME_GAP
}

/// "9:41 AM" today, else the day too.
fn clock(at: i64) -> String {
    use chrono::TimeZone;
    let Some(t) = chrono::Local.timestamp_opt(at, 0).single() else { return String::new() };
    if t.date_naive() == chrono::Local::now().date_naive() {
        t.format("%-I:%M %p").to_string()
    } else {
        t.format("%a %-d %b, %-I:%M %p").to_string()
    }
}

fn one_line(s: &str) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 60 {
        format!("{}\u{2026}", line.chars().take(60).collect::<String>())
    } else {
        line
    }
}

impl Drop for DockyChat {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the window (never shown) with a made-up conversation to
    /// `DOCKSIDE_DOCKY_CHAT_OUT`, light then dark: `cargo test renders_docky_chat -- --ignored`.
    #[test]
    #[ignore]
    fn renders_docky_chat() {
        use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        let Ok(out) = std::env::var("DOCKSIDE_DOCKY_CHAT_OUT") else { return };
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let _ = crate::win::register_class(Some(proc_));
        let gfx = Gfx::new().unwrap();
        let now = chrono::Local::now().timestamp();
        let m = |me: bool, text: &str, ago: i64| Message { me, text: text.into(), at: now - ago };
        crate::docky_ai::seed_chat(
            vec![
                m(true, "hey docky!", 600),
                m(false, "hiii! \u{2665} mrrp, what are we up to today?", 590),
                m(true, "can you help me name my new goldfish?", 300),
                m(false, "ooh a fishy friend! how about bubbles, finn, or captain splash? hehe, i like captain splash best.", 290),
                m(true, "captain splash it is", 60),
            ],
            true,
        );
        for dark in [false, true] {
            let mut cfg = crate::config::defaults();
            cfg.general.appearance = if dark { crate::config::Appearance::Dark } else { crate::config::Appearance::Light };
            let theme = Theme::new(&cfg, &gfx);
            let mut c = DockyChat::new(&gfx, &Monitor::primary(), 0).unwrap();
            for _ in 0..60 {
                c.window.tick(1.0 / 60.0, c.hwnd);
            }
            c.field = TextField::new("you're the best");
            c.render(&gfx, &theme);
            let cv = &c.canvas;
            crate::gfx::write_png(&gfx, cv.width as u32, cv.height as u32, &cv.pixels(), &format!("{out}-{}.png", if dark { "dark" } else { "light" }));
        }
    }
}
