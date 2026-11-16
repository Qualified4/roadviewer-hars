//! Crash points and injected faults for tests. Debug builds only, and only when an environment variable names one.
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

static INJECTED: LazyLock<Vec<String>> = LazyLock::new(|| {
    if cfg!(debug_assertions) {
        std::env::var("HARS_FAULT")
            .map(|v| v.split(",").map(str::to_owned).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    }
});

/// Whether the comma-separated `HARS_FAULT` (debug builds only) names `name`,
/// for tests of failures that real inputs cannot trigger on demand.
pub fn injected(name: &str) -> bool {
    INJECTED.iter().any(|v| v == name)
}

/// Damages a finished file the way a storage or writer fault would.
pub fn flip_last_byte(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut byte = [0u8];
    file.seek(SeekFrom::End(-1))?;
    file.read_exact(&mut byte)?;
    file.seek(SeekFrom::End(-1))?;
    file.write_all(&[byte[0] ^ 0x5a])
}
