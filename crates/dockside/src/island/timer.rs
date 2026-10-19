//! Timer and stopwatch for the Dynamic Island.

use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Counts down from `total` seconds.
    Timer { total: f32 },
    Stopwatch,
}

#[derive(Clone, Copy, Debug)]
pub struct Clock {
    pub mode: Mode,
    pub running: bool,
    /// Remaining (timer) or elapsed (stopwatch) seconds at `at`.
    value: f32,
    at: Instant,
}

impl Clock {
    pub fn timer(secs: u32) -> Self {
        let total = secs.max(1) as f32;
        Self { mode: Mode::Timer { total }, running: true, value: total, at: Instant::now() }
    }

    pub fn stopwatch() -> Self {
        Self { mode: Mode::Stopwatch, running: true, value: 0.0, at: Instant::now() }
    }

    /// Remaining seconds for a timer, elapsed seconds for a stopwatch.
    pub fn now(&self) -> f32 {
        let dt = if self.running { self.at.elapsed().as_secs_f32() } else { 0.0 };
        match self.mode {
            Mode::Timer { .. } => (self.value - dt).max(0.0),
            Mode::Stopwatch => self.value + dt,
        }
    }

    /// Fraction of a timer still left (1 → 0); a stopwatch sweeps once a minute.
    pub fn fraction(&self) -> f32 {
        match self.mode {
            Mode::Timer { total } => (self.now() / total).clamp(0.0, 1.0),
            Mode::Stopwatch => (self.now() % 60.0) / 60.0,
        }
    }

    pub fn is_timer(&self) -> bool {
        matches!(self.mode, Mode::Timer { .. })
    }

    pub fn finished(&self) -> bool {
        self.is_timer() && self.now() <= 0.0
    }

    pub fn toggle(&mut self) {
        self.value = self.now();
        self.at = Instant::now();
        self.running = !self.running;
    }

    /// Display text: a timer rounds up ("0:01" until it hits zero), a stopwatch shows tenths under an hour.
    pub fn label(&self) -> String {
        match self.mode {
            Mode::Timer { .. } => hms(self.now().ceil() as u32),
            Mode::Stopwatch => {
                let t = self.now();
                if t < 3600.0 {
                    format!("{}:{:02}.{}", t as u32 / 60, t as u32 % 60, (t.fract() * 10.0) as u32)
                } else {
                    hms(t as u32)
                }
            }
        }
    }
}

pub fn hms(s: u32) -> String {
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Parses "10", "10m", "10 min", "90s", "1h30m", "1:30" (m:ss) into seconds.
/// A bare number means minutes.
pub fn parse_duration(s: &str) -> Option<u32> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Some((m, sec)) = s.split_once(':') {
        let (m, sec): (u32, u32) = (m.trim().parse().ok()?, sec.trim().parse().ok()?);
        return (sec < 60).then_some(m * 60 + sec).filter(|&t| t > 0);
    }
    let mut total = 0f32;
    let mut num = String::new();
    let mut unit = String::new();
    let mut any = false;
    let flush = |num: &mut String, unit: &mut String, total: &mut f32| -> Option<()> {
        if num.is_empty() {
            return if unit.is_empty() { Some(()) } else { None };
        }
        let n: f32 = num.parse().ok()?;
        let k = match unit.as_str() {
            "" | "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
            _ => return None,
        };
        *total += n * k;
        num.clear();
        unit.clear();
        Some(())
    };
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            if !unit.is_empty() {
                flush(&mut num, &mut unit, &mut total)?;
            }
            num.push(c);
            any = true;
        } else if c.is_alphabetic() {
            unit.push(c);
        } else if c.is_whitespace() {
            continue;
        } else {
            return None;
        }
    }
    flush(&mut num, &mut unit, &mut total)?;
    let secs = total.round() as u32;
    (any && secs > 0 && secs <= 24 * 3600).then_some(secs)
}

/// Spotlight commands: "timer 10m", "10 min timer", "stopwatch".
pub enum Command {
    Timer(u32),
    Stopwatch,
}

pub fn parse_command(q: &str) -> Option<Command> {
    let q = q.trim().to_lowercase();
    if q == "stopwatch" || q == "start stopwatch" {
        return Some(Command::Stopwatch);
    }
    let rest = q
        .strip_prefix("timer")
        .or_else(|| q.strip_prefix("set timer"))
        .or_else(|| q.strip_suffix("timer"))?;
    let rest = rest.trim().trim_start_matches("for ").trim();
    parse_duration(rest).map(Command::Timer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("10"), Some(600));
        assert_eq!(parse_duration("10m"), Some(600));
        assert_eq!(parse_duration("10 min"), Some(600));
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("1h 30"), Some(5400));
        assert_eq!(parse_duration("1:30"), Some(90));
        assert_eq!(parse_duration("2.5m"), Some(150));
        assert_eq!(parse_duration("banana"), None);
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn commands() {
        assert!(matches!(parse_command("timer 10m"), Some(Command::Timer(600))));
        assert!(matches!(parse_command("Timer for 5 minutes"), Some(Command::Timer(300))));
        assert!(matches!(parse_command("25 min timer"), Some(Command::Timer(1500))));
        assert!(matches!(parse_command("stopwatch"), Some(Command::Stopwatch)));
        assert!(parse_command("timer").is_none());
        assert!(parse_command("timetable").is_none());
    }

    #[test]
    fn labels() {
        let mut c = Clock::timer(65);
        c.toggle();
        assert_eq!(c.label(), "1:05");
        assert!(!c.finished());
        assert_eq!(hms(3725), "1:02:05");
    }
}
