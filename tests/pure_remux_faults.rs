//! The real executable's pure remux worker under faults (synthetic media),
//! one test per failure class: the log event it records (no other engine
//! retries any class), and the disk afterwards (original intact, no partial
//! output, a previous result kept, a retry completes normally).
#![cfg(unix)]
use serde_json::Value;
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_roadviewer-hars");

fn hevc(path: &Path) {
    assert!(
        Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=320x240:rate=20")
            .args(["-frames:v", "40", "-an", "-c:v", "libx265", "-x265-params"])
            .arg("pools=1:frame-threads=1:bframes=0:keyint=20:log-level=error")
            .args(["-pix_fmt", "yuv420p", "-f", "hevc"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}

struct Case {
    _dir: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    clock: PathBuf,
    original: Vec<u8>,
}
fn case() -> Case {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let source = root.join("fcamera.hevc");
    hevc(&source);
    let clock = root.join("clock.json");
    let times = (0..40).map(|i| i as f64 * 0.05).collect::<Vec<_>>();
    std::fs::write(&clock, serde_json::to_vec(&times).unwrap()).unwrap();
    let original = std::fs::read(&source).unwrap();
    Case {
        _dir: dir,
        root,
        source,
        clock,
        original,
    }
}
impl Case {
    fn command(&self, output: &Path) -> Command {
        let mut c = Command::new(BIN);
        c.arg("remux")
            .arg(&self.source)
            .arg("fcamera.hevc")
            .arg(&self.clock)
            .arg(output)
            .env_remove("HARS_REMUX_ENGINE")
            .env_remove("HARS_CRASH_POINT")
            .env_remove("HARS_FAULT")
            .env_remove("HARS_MEDIA_DEADLINE_SECONDS")
            .env("RUST_LOG", "roadviewer_hars=info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }
    /// Only the original, the clock and finished outputs may remain.
    fn clean(&self, keep: &[&str]) {
        assert_eq!(std::fs::read(&self.source).unwrap(), self.original);
        for entry in std::fs::read_dir(&self.root).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            assert!(
                ["fcamera.hevc", "clock.json", "marker"].contains(&name.as_str())
                    || keep.contains(&name.as_str()),
                "left behind {name}"
            );
        }
    }
    fn retry(&self, name: &str) -> String {
        let output = self.root.join(name);
        let log = result(&self.command(&output).output().unwrap());
        assert_eq!(engines(&log), ["pure"], "{log}");
        assert!(event(&log, "remux_fallback").is_none(), "{log}");
        assert!(event(&log, "remux_failure").is_none(), "{log}");
        log
    }
}

/// The `remux` command's stdout is exactly its one JSON result line; its log,
/// returned here, goes to stderr.
fn result(out: &Output) -> String {
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{log}");
    let stdout = std::str::from_utf8(&out.stdout).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "{stdout}");
    let value: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["validated"], true, "{stdout}");
    log
}

/// A failed remux prints no result and says why on stderr.
fn failed(out: &Output) -> String {
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "{log}");
    assert!(out.stdout.is_empty(), "a failed remux prints no result");
    log
}

/// (level, fields) of the first `name` event of a JSON log.
fn event(log: &str, name: &str) -> Option<(String, Value)> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| e["fields"]["event"] == name)
        .map(|e| (e["level"].as_str().unwrap().to_owned(), e["fields"].clone()))
}

fn engines(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["fields"]["event"] == "remux_engine")
        .map(|e| e["fields"]["engine"].as_str().unwrap().to_owned())
        .collect()
}

/// The pure failure was recorded with this class and code at `level`, and no
/// other engine retried the input.
fn failed_with(log: &str, class: &str, code: &str, level: &str) {
    let (got, fields) = event(log, "remux_failure").unwrap_or_else(|| panic!("{log}"));
    assert_eq!(
        (
            got.as_str(),
            fields["failure_class"].as_str(),
            fields["reason_code"].as_str(),
            fields["fallback"].as_bool(),
        ),
        (level, Some(class), Some(code), Some(false)),
        "{log}"
    );
    assert!(event(log, "remux_fallback").is_none(), "{log}");
    assert!(engines(log).is_empty(), "{log}");
}

fn no_fallback(log: &str, code: &str) {
    let (_, fields) = event(log, "remux_failure").unwrap_or_else(|| panic!("{log}"));
    assert_eq!(
        (
            fields["failure_class"].as_str(),
            fields["reason_code"].as_str(),
            fields["fallback"].as_bool()
        ),
        (Some("resource"), Some(code), Some(false)),
        "{log}"
    );
    assert!(event(log, "remux_fallback").is_none(), "{log}");
    assert!(engines(log).is_empty(), "{log}");
}

fn wait_marker(marker: &Path, child: &mut Child) {
    let start = Instant::now();
    while !marker.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "exited before the crash point"
        );
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "crash point not reached"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Process ids of `pid`'s direct children (Linux /proc).
fn children(pid: u32) -> Vec<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

fn alive(pid: i32) -> bool {
    // A zombie has exited; only its parent's wait is missing.
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        !s.rsplit(')')
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with('Z')
    })
}

/// A killed worker (the OOM killer, a shutdown) is a resource failure, and
/// nothing of the attempt remains.
#[test]
fn killed_pure_worker_fails_without_fallback_and_leaves_no_partial_output() {
    let c = case();
    let marker = c.root.join("marker");
    let output = c.root.join("out.mp4");
    let mut child = c
        .command(&output)
        .env("HARS_CRASH_POINT", "pure_remux_sample")
        .env("HARS_CRASH_MARKER", &marker)
        .spawn()
        .unwrap();
    wait_marker(&marker, &mut child);
    let workers = children(child.id());
    assert_eq!(workers.len(), 1, "one pure worker runs");
    // SAFETY: plain kill(2) on the worker pid read from /proc just now.
    assert_eq!(unsafe { libc::kill(workers[0], libc::SIGKILL) }, 0);
    let log = failed(&child.wait_with_output().unwrap());
    no_fallback(&log, "worker_killed");
    c.clean(&[]);
    c.retry("again.mp4");
    c.clean(&["again.mp4"]);
}

/// Repeated runs write identical files, and the unnamed payload staging file
/// leaves nothing behind.
#[test]
fn repeated_runs_write_the_same_file_and_stage_nothing() {
    let c = case();
    c.retry("once.mp4");
    c.retry("twice.mp4");
    assert_eq!(
        std::fs::read(c.root.join("once.mp4")).unwrap(),
        std::fs::read(c.root.join("twice.mp4")).unwrap()
    );
    c.clean(&["once.mp4", "twice.mp4"]);
}

#[test]
fn killed_conversion_process_takes_its_worker_and_a_retry_completes() {
    let c = case();
    let marker = c.root.join("marker");
    let output = c.root.join("out.mp4");
    let mut child = c
        .command(&output)
        .env("HARS_CRASH_POINT", "pure_remux_sample")
        .env("HARS_CRASH_MARKER", &marker)
        .spawn()
        .unwrap();
    wait_marker(&marker, &mut child);
    let workers = children(child.id());
    assert_eq!(workers.len(), 1, "one pure worker runs");
    // Only the parent is killed: the worker must not outlive it (PDEATHSIG).
    child.kill().unwrap();
    child.wait().unwrap();
    let start = Instant::now();
    while alive(workers[0]) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "orphaned worker still runs"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!output.exists());
    // The temporary next to the output is the only trace; conversion staging
    // (jobs) owns that directory and removes it (tests/recovery.rs).
    let temps = std::fs::read_dir(&c.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains(".tmp.mp4") || n.ends_with(".job.json"))
        .collect::<Vec<_>>();
    assert!(!temps.is_empty() && temps.len() <= 2, "{temps:?}");
    for name in temps {
        std::fs::remove_file(c.root.join(name)).unwrap();
    }
    c.clean(&[]);
    c.retry("out.mp4");
    c.clean(&["out.mp4"]);
}

#[test]
fn write_limit_fails_without_fallback_and_a_retry_completes() {
    let c = case();
    let output = c.root.join("out.mp4");
    let limit = c.original.len() as u64 / 2;
    let mut command = c.command(&output);
    // SAFETY: setrlimit is async-signal-safe; SIGXFSZ is ignored so writes fail
    // with EFBIG instead of killing the process, like a full disk would.
    unsafe {
        command.pre_exec(move || {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let rlimit = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &rlimit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let log = failed(&command.output().unwrap());
    no_fallback(&log, "file_too_large");
    c.clean(&[]);
    c.retry("out.mp4");
    c.clean(&["out.mp4"]);
}

/// The worker deadline is a resource failure: the parked worker is killed and
/// nothing else is started on the same input.
#[test]
fn deadline_fails_without_fallback_and_removes_the_attempt() {
    let c = case();
    let marker = c.root.join("marker");
    let output = c.root.join("out.mp4");
    let log = failed(
        &c.command(&output)
            .env("HARS_CRASH_POINT", "pure_remux_sample")
            .env("HARS_CRASH_MARKER", &marker)
            .env("HARS_MEDIA_DEADLINE_SECONDS", "3")
            .output()
            .unwrap(),
    );
    no_fallback(&log, "deadline");
    c.clean(&[]);
    c.retry("out.mp4");
}

/// Engine bugs are not hidden: the attempt fails, nothing is published, and the
/// event is an error carrying the class and code.
#[test]
fn engine_errors_and_panics_fail_as_error_events_and_publish_nothing() {
    let c = case();
    for (fault, code) in [
        ("pure_remux_error", "engine_error"),
        ("pure_remux_panic", "panic"),
    ] {
        let name = format!("{fault}.mp4");
        let log = failed(
            &c.command(&c.root.join(&name))
                .env("HARS_FAULT", fault)
                .output()
                .unwrap(),
        );
        failed_with(&log, "internal", code, "ERROR");
        c.clean(&[]);
    }
    c.retry("out.mp4");
}

#[test]
fn a_file_that_does_not_read_back_fails_and_the_previous_result_stays() {
    let c = case();
    let log = failed(
        &c.command(&c.root.join("new.mp4"))
            .env("HARS_FAULT", "pure_remux_corrupt")
            .output()
            .unwrap(),
    );
    failed_with(&log, "verification", "readback_mismatch", "ERROR");
    c.clean(&[]);
    // A previous result is never replaced by an attempt that fails its checks.
    c.retry("out.mp4");
    let previous = std::fs::read(c.root.join("out.mp4")).unwrap();
    let log = failed(
        &c.command(&c.root.join("out.mp4"))
            .env("HARS_FAULT", "pure_remux_corrupt")
            .output()
            .unwrap(),
    );
    failed_with(&log, "verification", "readback_mismatch", "ERROR");
    assert_eq!(std::fs::read(c.root.join("out.mp4")).unwrap(), previous);
    c.clean(&["out.mp4"]);
}

/// The worker's memory limit is a resource failure, and the default limit
/// leaves room for the largest valid buffers: a 33 MiB NAL unit is refused
/// by the input bound (invalid), never by running out of memory.
#[test]
fn memory_limit_fails_without_fallback_and_the_default_fits_the_bounds() {
    let c = case();
    let log = failed(
        &c.command(&c.root.join("out.mp4"))
            .env("HARS_PURE_WORKER_MEMORY_MB", "1")
            .output()
            .unwrap(),
    );
    no_fallback(&log, "out_of_memory");
    c.clean(&[]);
    let mut huge = c.original.clone();
    huge.extend([0, 0, 1, 0x02, 0x01]);
    huge.resize(huge.len() + 33 * 1024 * 1024, 0x55);
    std::fs::write(&c.source, &huge).unwrap();
    let log = failed(&c.command(&c.root.join("huge.mp4")).output().unwrap());
    failed_with(&log, "invalid", "invalid_input", "WARN");
    assert!(log.contains("bounded memory limit"), "{log}");
    std::fs::write(&c.source, &c.original).unwrap();
    c.clean(&[]);
    c.retry("out.mp4");
}

/// The pure engine needs about twice the input free (its payload and the MP4 coexist); with less,
/// the remux stops before writing as a resource failure, and with enough it completes.
#[test]
fn low_space_is_a_resource_failure_before_any_write() {
    use roadviewer_hars::media::{SPACE_MARGIN, pure_fits};
    let input = 700 * 1024 * 1024;
    assert!(pure_fits(input, 2 * input + SPACE_MARGIN));
    assert!(!pure_fits(input, 2 * input + SPACE_MARGIN - 1));
    assert!(!pure_fits(input, input + SPACE_MARGIN));
    assert!(!pure_fits(u64::MAX, u64::MAX));
    let c = case();
    let available = (2 * c.original.len() as u64 + SPACE_MARGIN - 1).to_string();
    let log = failed(
        &c.command(&c.root.join("out.mp4"))
            .env("HARS_AVAILABLE_SPACE_BYTES", &available)
            .output()
            .unwrap(),
    );
    failed_with(&log, "resource", "low_space", "WARN");
    no_fallback(&log, "low_space");
    c.clean(&[]);
    let enough = (2 * c.original.len() as u64 + SPACE_MARGIN).to_string();
    let log = result(
        &c.command(&c.root.join("pure.mp4"))
            .env("HARS_AVAILABLE_SPACE_BYTES", &enough)
            .output()
            .unwrap(),
    );
    assert_eq!(engines(&log), ["pure"], "{log}");
    c.clean(&["pure.mp4"]);
}
