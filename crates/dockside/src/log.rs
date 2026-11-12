use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(&format!($($t)*)) };
}

fn path() -> PathBuf {
    crate::config::dir().join("dockside.log")
}

/// Truncates the log when it grows past 1 MB.
pub fn init() {
    let p = path();
    if std::fs::metadata(&p).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = std::fs::remove_file(&p);
    }
}

pub fn write(msg: &str) {
    let line = format!("[{}] {}\n", chrono::Local::now().format("%H:%M:%S%.3f"), msg);
    #[cfg(debug_assertions)]
    eprint!("{line}");
    let _ = std::fs::create_dir_all(crate::config::dir());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Times the steps of a long job (startup), for one log line saying where the
/// time went: `mark` after each step, `summary` at the end.
pub struct Phases {
    last: std::time::Instant,
    parts: Vec<(&'static str, std::time::Duration)>,
}

impl Phases {
    pub fn new() -> Self {
        Self { last: std::time::Instant::now(), parts: Vec::new() }
    }

    /// Ends the step called `name`, started at the previous mark.
    pub fn mark(&mut self, name: &'static str) {
        let now = std::time::Instant::now();
        self.parts.push((name, now - self.last));
        self.last = now;
    }

    /// Steps of 20 ms or more, in order: "dock 3.2s, island 140ms".
    pub fn summary(&self) -> String {
        let parts: Vec<String> = self
            .parts
            .iter()
            .filter(|(_, d)| d.as_millis() >= 20)
            .map(|(n, d)| if d.as_secs_f64() >= 1.0 { format!("{n} {:.1}s", d.as_secs_f64()) } else { format!("{n} {}ms", d.as_millis()) })
            .collect();
        if parts.is_empty() { "all steps under 20ms".into() } else { parts.join(", ") }
    }
}
