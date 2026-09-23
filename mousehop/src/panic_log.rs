//! Best-effort panic logging to a durable file.
//!
//! The daemon runs as a LaunchServices-spawned child whose stderr is
//! routed to `/dev/null`, so a Rust panic — and, with `panic =
//! "abort"`, the process death that immediately follows — leaves no
//! trace the user can find. (That is precisely why the
//! screensaver-lockup panic went undiagnosed for so long.) [`install`]
//! adds a panic hook that appends the panic's location, message,
//! thread, and a backtrace to a logfile under the platform log
//! directory, then chains to the previous hook so the usual stderr
//! output and the `abort` still happen.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Preserve connection warnings even when LaunchServices discards stderr.
/// Separate roles avoid GUI/daemon rotation races; each retains two 4MiB files.
pub fn record_warning(role: &str, message: &str) {
    let Some(mut path) = log_file_path() else {
        return;
    };
    path.set_file_name(format!("connection-{role}.log"));
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Serialise warning writes and rotation within this process.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let Ok(_guard) = LOCK.lock() else {
        return;
    };
    if fs::metadata(&path).is_ok_and(|m| m.len() >= 4 * 1024 * 1024) {
        let backup = path.with_extension("previous.log");
        let _ = fs::remove_file(&backup);
        if fs::rename(&path, backup).is_err() {
            return;
        }
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

/// Install the panic-logging hook for this process. `role` is a short
/// tag recorded in each entry (e.g. `"daemon"` or `"gui"`) so a shared
/// logfile stays attributable. Best-effort: if the log directory can't
/// be determined or created the call is a no-op and the process
/// behaves exactly as before.
pub fn install(role: &'static str) {
    let Some(path) = log_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Everything here is best-effort; on any failure we still fall
        // through to `previous` so behaviour is never worse than the
        // default hook.
        let when = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let pid = std::process::id();
        let record = format!(
            "\n==== mousehop {role} panic — pid {pid}, unix {when}s ====\n\
             thread '{thread_name}' panicked at {location}:\n{message}\n\
             stack backtrace:\n{backtrace}\n"
        );
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(record.as_bytes());
        }
        previous(info);
    }));
}

#[cfg(target_os = "macos")]
fn log_file_path() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push("Library/Logs/Mousehop/panic.log");
    Some(p)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn log_file_path() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        let mut p = PathBuf::from(state);
        p.push("mousehop/panic.log");
        return Some(p);
    }
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push(".local/state/mousehop/panic.log");
    Some(p)
}

#[cfg(windows)]
fn log_file_path() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    p.push("Mousehop");
    p.push("logs");
    p.push("panic.log");
    Some(p)
}
