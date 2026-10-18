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
