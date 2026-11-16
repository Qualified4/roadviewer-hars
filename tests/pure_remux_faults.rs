//! The real executable's pure remux worker under faults (synthetic media):
//! a killed worker, a killed conversion process and a write limit leave the
//! original intact and no partial output, and a retry completes normally.
#![cfg(unix)]
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
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
            .env("RUST_LOG", "roadviewer_hars=info")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
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
        let out = self.command(&output).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{stdout}");
        assert!(
            stdout.contains(r#""event":"remux_engine","engine":"pure""#),
            "{stdout}"
        );
        assert!(!stdout.contains("remux_fallback"), "{stdout}");
        stdout
    }
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

#[test]
fn killed_pure_worker_falls_back_and_leaves_no_partial_output() {
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
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains(r#""event":"remux_fallback""#), "{stdout}");
    assert!(stdout.contains(r#""engine":"ffmpeg""#), "{stdout}");
    c.clean(&["out.mp4"]);
    std::fs::remove_file(&output).unwrap();
    c.retry("again.mp4");
    c.clean(&["again.mp4"]);
}

/// Inputs beyond the in-memory limit are read a second time; that path must
/// write the same bytes as the single-read one.
#[test]
fn second_read_path_writes_the_same_file() {
    let c = case();
    let once = c.root.join("once.mp4");
    c.retry("once.mp4");
    let twice = c.root.join("twice.mp4");
    let out = c
        .command(&twice)
        .env("HARS_PURE_REMUX_BUFFER_LIMIT", "0")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains(r#""engine":"pure""#) && !stdout.contains("remux_fallback"));
    assert_eq!(std::fs::read(once).unwrap(), std::fs::read(twice).unwrap());
    c.clean(&["once.mp4", "twice.mp4"]);
}

#[test]
fn killed_conversion_process_keeps_the_original_and_a_retry_completes() {
    let c = case();
    let marker = c.root.join("marker");
    let output = c.root.join("out.mp4");
    let mut child = c
        .command(&output)
        .env("HARS_CRASH_POINT", "pure_remux_sample")
        .env("HARS_CRASH_MARKER", &marker)
        .process_group(0)
        .spawn()
        .unwrap();
    wait_marker(&marker, &mut child);
    // SIGKILL the whole process group, as a server kill takes its workers down.
    // SAFETY: kill(2) on the group this test just created.
    assert_eq!(
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) },
        0
    );
    child.wait().unwrap();
    assert!(!output.exists());
    // The temporary next to the output is the only trace; conversion staging
    // (jobs) owns that directory and removes it on restart (tests/recovery.rs).
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
fn write_limit_fails_both_engines_cleanly_and_a_retry_completes() {
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
    let out = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains(r#""event":"remux_fallback""#), "{stdout}");
    c.clean(&[]);
    c.retry("out.mp4");
    c.clean(&["out.mp4"]);
}
