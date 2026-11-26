use std::io::Write;

/// Write a diagnostic line without allowing a missing or broken stderr handle
/// to panic a GUI-subsystem backend.
pub fn line(message: impl AsRef<str>) {
    let message = message.as_ref();
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = writeln!(lock, "{message}");
}
