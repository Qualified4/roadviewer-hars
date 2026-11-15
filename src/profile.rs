//! Opt-in diagnostic counters, never payloads. Inclusive scopes must not be summed.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{LazyLock, Mutex},
    time::Instant,
};
static ENABLED: LazyLock<bool> = LazyLock::new(|| std::env::var_os("HARS_PROFILE_FILE").is_some());
static STAGES: Mutex<BTreeMap<&'static str, Entry>> = Mutex::new(BTreeMap::new());
#[derive(Default, Serialize)]
struct Entry {
    calls: u64,
    wall_seconds: f64,
    cpu_seconds: f64,
    child_cpu_seconds: f64,
    cumulative_peak_rss_kib: i64,
}
fn usage() -> (f64, f64, i64) {
    #[cfg(unix)]
    {
        // SAFETY: getrusage writes the two initialized, correctly sized rusage structs.
        unsafe {
            let mut own: libc::rusage = std::mem::zeroed();
            let mut child: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut own);
            libc::getrusage(libc::RUSAGE_CHILDREN, &mut child);
            let cpu = |r: libc::rusage| {
                r.ru_utime.tv_sec as f64
                    + r.ru_stime.tv_sec as f64
                    + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e6
            };
            (cpu(own), cpu(child), own.ru_maxrss)
        }
    }
    #[cfg(not(unix))]
    {
        (0., 0., 0)
    }
}
pub struct Scope {
    name: &'static str,
    start: Option<(Instant, (f64, f64, i64))>,
}
impl Scope {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: if *ENABLED {
                Some((Instant::now(), usage()))
            } else {
                None
            },
        }
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((start, before)) = self.start {
            let after = usage();
            let mut stages = STAGES.lock().unwrap();
            let entry = stages.entry(self.name).or_default();
            entry.calls += 1;
            entry.wall_seconds += start.elapsed().as_secs_f64();
            entry.cpu_seconds += after.0 - before.0;
            entry.child_cpu_seconds += after.1 - before.1;
            entry.cumulative_peak_rss_kib = entry.cumulative_peak_rss_kib.max(after.2);
        }
    }
}
pub fn finish() {
    use std::io::Write;
    let Some(path) = std::env::var_os("HARS_PROFILE_FILE") else {
        return;
    };
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        let stages = STAGES.lock().unwrap();
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({"pid":std::process::id(),"inclusive":true,"stages":*stages}),
        )?;
        file.write_all(b"\n")
    })();
    if result.is_err() {
        eprintln!("profiling output failed");
    }
}
