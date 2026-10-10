//! Helpers shared by unit tests.

use std::sync::{Mutex, MutexGuard};

/// Forces `colored` output on or off until dropped. The override is
/// process-global, so concurrent tests take turns through one lock.
pub(crate) struct ColorOverride {
    _lock: MutexGuard<'static, ()>,
}

pub(crate) fn color_override(enabled: bool) -> ColorOverride {
    static LOCK: Mutex<()> = Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    colored::control::set_override(enabled);
    ColorOverride { _lock: lock }
}

impl Drop for ColorOverride {
    fn drop(&mut self) {
        colored::control::unset_override();
    }
}

/// Writes an executable script through a child `sh`, so this process never
/// holds it open for writing. A parallel test that forked during such a write
/// would inherit the handle, and exec would fail with ETXTBSY.
#[cfg(unix)]
pub(crate) fn write_executable(path: &std::path::Path, contents: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sh")
        .args([
            "-c",
            r#"cat > "$1.tmp" && chmod 755 "$1.tmp" && mv "$1.tmp" "$1""#,
            "sh",
        ])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn sh to write an executable");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(contents.as_bytes())
        .expect("write executable contents");
    assert!(child.wait().expect("wait for sh").success());
}
