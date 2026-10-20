//! A small diagnostic log: %APPDATA%\xiaobaiyun\debug.log, restarted when
//! it passes 512 KB. Records what happened to each selection, never its text.

use std::io::Write;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::settings::data_file;

const MAX_BYTES: u64 = 512 * 1024;

pub fn write(message: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock();
    let Some(path) = data_file("debug.log") else { return };
    let restart = std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES);
    let file = std::fs::OpenOptions::new().create(true).write(true).append(!restart).truncate(restart).open(&path);
    if let Ok(mut file) = file {
        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let _ = writeln!(file, "{secs:.3} {message}");
    }
}

/// `log!("...", args)` writes one line to the debug log.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}
