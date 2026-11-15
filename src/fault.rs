//! Crash points for process-kill tests. Debug builds only, and only when `HARS_CRASH_POINT` names one.
use std::sync::LazyLock;

static ARMED: LazyLock<Option<String>> = LazyLock::new(|| {
    if cfg!(debug_assertions) {
        std::env::var("HARS_CRASH_POINT").ok()
    } else {
        None
    }
});

/// Parks the calling thread forever at the armed point after writing its name to
/// `HARS_CRASH_MARKER`, so the supervising test can SIGKILL the process exactly there.
pub fn point(name: &str) {
    if ARMED.as_deref() != Some(name) {
        return;
    }
    if let Some(marker) = std::env::var_os("HARS_CRASH_MARKER") {
        let _ = std::fs::write(marker, name);
    }
    loop {
        std::thread::park();
    }
}
