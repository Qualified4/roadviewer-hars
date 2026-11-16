//! A camera remux that fails for the machine (a killed worker, the write limit, the deadline) on the real
//! server: a camera the published result plays is never dropped for it. The conversion fails, the
//! previous result stays byte for byte and the staging goes; a later conversion completes. Without a
//! previous video of that camera it ends as Python's `prepare_video` failure does, ready with a warning
//! naming the resource class, and the original stays whatever `keep_original_video` says.
//! Real executable, synthetic media and log.
#![cfg(unix)]
mod common;
#[path = "common/recording.rs"]
mod recording;
use common::{csrf, http, web};
use recording::{FRAMES, rlog};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const ORIGINALS: [&str; 3] = ["rlog.zst", "qcamera.ts", "fcamera.hevc"];

#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    /// The pure worker parked at its sample write, then SIGKILLed (the OOM killer, an operator).
    Kill,
    /// Writes beyond the limit fail with EFBIG, as on a full disk.
    WriteLimit,
    /// Too little free space for the pure engine, so FFmpeg writes the single copy, and that
    /// write fails at the limit: the FFmpeg worker's exit status says resource.
    SingleCopyWriteLimit,
    /// The pure worker parked past the media deadline.
    Deadline,
    /// Media inspection parked past the media deadline (MP4 inputs have no remux worker).
    InspectDeadline,
    /// The server parked in publication after an MP4 moved next to its original, before the
    /// directory sync that precedes deleting the original.
    Publish,
}

struct Server {
    child: Child,
    ui: SocketAddr,
    log: PathBuf,
    marker: PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
async fn start(root: &Path, fault: Fault, limit: u64) -> Server {
    let marker = root.with_extension("marker");
    let _ = std::fs::remove_file(&marker);
    for attempt in 0..5 {
        let ui: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let device: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let log = root.with_extension(format!("{attempt}.{}.log", roadviewer_hars::store::id()));
        let mut command = Command::new(env!("CARGO_BIN_EXE_roadviewer-hars"));
        command
            .env("HARS_DATA", root)
            .env("HARS_WEB", web())
            .env("HARS_INGRESS_ONLY", "0")
            .env("HARS_BIND", ui.to_string())
            .env("HARS_DEVICE_BIND", device.to_string())
            .env("RUST_LOG", "roadviewer_hars=info")
            .env("HARS_CRASH_MARKER", &marker)
            .env_remove("HARS_REMUX_ENGINE")
            .env_remove("HARS_CRASH_POINT")
            .env_remove("HARS_FAULT")
            .env_remove("HARS_MEDIA_DEADLINE_SECONDS")
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(Stdio::null());
        match fault {
            Fault::Kill => {
                command.env("HARS_CRASH_POINT", "pure_remux_sample");
            }
            Fault::Deadline => {
                command
                    .env("HARS_CRASH_POINT", "pure_remux_sample")
                    .env("HARS_MEDIA_DEADLINE_SECONDS", "2");
            }
            Fault::InspectDeadline => {
                command
                    .env("HARS_CRASH_POINT", "native_inspect")
                    .env("HARS_MEDIA_DEADLINE_SECONDS", "2");
            }
            Fault::WriteLimit | Fault::SingleCopyWriteLimit => {
                if matches!(fault, Fault::SingleCopyWriteLimit) {
                    command.env("HARS_AVAILABLE_SPACE_BYTES", "0");
                }
                // SAFETY: only async-signal-safe calls run between fork and exec. Ignored SIGXFSZ
                // survives exec, so oversized writes fail with EFBIG instead of killing.
                unsafe {
                    command.pre_exec(move || {
                        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                        let value = libc::rlimit {
                            rlim_cur: limit as libc::rlim_t,
                            rlim_max: limit as libc::rlim_t,
                        };
                        if libc::setrlimit(libc::RLIMIT_FSIZE, &value) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            Fault::Publish => {
                command.env("HARS_CRASH_POINT", "publish_mp4_moved");
            }
            Fault::None => {}
        }
        let mut server = Server {
            child: command.spawn().unwrap(),
            ui,
            log,
            marker: marker.clone(),
        };
        let begun = Instant::now();
        while begun.elapsed() < Duration::from_secs(30) {
            if server.child.try_wait().unwrap().is_some() {
                break;
            }
            if tokio::net::TcpStream::connect(ui).await.is_ok()
                && http(ui, "GET", "/health", &[], b"").await.0 == 200
            {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    panic!("server did not start");
}
impl Server {
    async fn post(&self, path: &str) -> (u16, Value) {
        http(self.ui, "POST", path, &csrf(), b"").await
    }
    async fn delete(&self, path: &str) -> (u16, Value) {
        http(self.ui, "DELETE", path, &csrf(), b"").await
    }
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    /// Kills the pure worker once it is parked at its crash point.
    fn kill_worker(&self) {
        wait_for("crash point", Duration::from_secs(60), || {
            self.marker.exists()
        });
        let workers = descendants(self.child.id())
            .into_iter()
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|c| c.windows(12).any(|w| w == b"__pure-remux"))
            })
            .collect::<Vec<_>>();
        assert_eq!(workers.len(), 1, "one pure worker runs");
        std::fs::remove_file(&self.marker).unwrap();
        // SAFETY: plain kill(2) on the worker pid read from /proc just now.
        assert_eq!(unsafe { libc::kill(workers[0], libc::SIGKILL) }, 0);
        wait_for("killed worker", Duration::from_secs(10), || {
            !descendants(self.child.id()).contains(&workers[0])
        });
    }
}

fn descendants(pid: u32) -> Vec<i32> {
    let mut found = Vec::new();
    let mut pending = vec![pid as i32];
    while let Some(p) = pending.pop() {
        for task in std::fs::read_dir(format!("/proc/{p}/task"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let children =
                std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
            for child in children
                .split_whitespace()
                .map(|c| c.parse::<i32>().unwrap())
            {
                found.push(child);
                pending.push(child);
            }
        }
    }
    found
}

fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "deadline waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn meta(folder: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(folder.join("meta.json")).unwrap()).unwrap()
}
fn wait_status(folder: &Path, status: &str) -> Value {
    let mut last = Value::Null;
    let start = Instant::now();
    loop {
        if let Ok(bytes) = std::fs::read(folder.join("meta.json"))
            && let Ok(m) = serde_json::from_slice::<Value>(&bytes)
        {
            // A publication is complete once its journal is gone; a restart before would publish again.
            let published = !folder.join(".conversion.json").exists()
                && std::fs::read_dir(folder).unwrap().all(|e| {
                    !e.unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".previous-")
                });
            if m["status"] == status && published {
                return m;
            }
            last = m;
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "status {status} not reached: {last}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every file under `dir`, by relative path.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let name = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(name, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// Nothing of an attempt that was not published.
fn no_attempts(folder: &Path) {
    for entry in std::fs::read_dir(folder).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(
            !name.starts_with(".convert-")
                && !name.starts_with(".previous-")
                && !name.contains(".tmp"),
            "left behind {name}"
        );
    }
}

/// `recording::encode` at a size whose MP4s exceed every metadata file, for the write limit.
fn encode(args: &[&str], out: &Path) {
    assert!(
        Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=640x384:rate=20")
            .args(["-frames:v", &FRAMES.to_string(), "-an", "-threads", "1"])
            .args(args)
            .arg(out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}

struct Recording {
    _temp: tempfile::TempDir,
    root: PathBuf,
    id: String,
    folder: PathBuf,
    originals: BTreeMap<&'static str, Vec<u8>>,
}
fn recording(keep_original_video: bool) -> Recording {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("data");
    let id = roadviewer_hars::store::id();
    let folder = root.join(&id);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        root.join(".processing-settings.json"),
        json!({"auto_convert":false,"keep_original_video":keep_original_video}).to_string(),
    )
    .unwrap();
    encode(
        &[
            "-c:v", "libx264", "-bf", "0", "-g", "20", "-qp", "12", "-pix_fmt", "yuv420p", "-f",
            "mpegts",
        ],
        &folder.join("qcamera.ts"),
    );
    encode(
        &[
            "-c:v",
            "libx265",
            "-x265-params",
            "pools=1:frame-threads=1:bframes=0:keyint=20:log-level=error:qp=12",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "hevc",
        ],
        &folder.join("fcamera.hevc"),
    );
    std::fs::write(folder.join("rlog.zst"), rlog()).unwrap();
    std::fs::write(
        folder.join("meta.json"),
        json!({"id":id,"name":"keep","status":"queued","manual_conversion":true,"uploaded":1.0,
            "files":{"rlog.zst":"rlog.zst","qcamera.ts":"qcamera.ts","fcamera.hevc":"fcamera.hevc"}})
        .to_string(),
    )
    .unwrap();
    let originals = ORIGINALS
        .iter()
        .map(|k| (*k, std::fs::read(folder.join(k)).unwrap()))
        .collect();
    Recording {
        _temp: temp,
        root,
        id,
        folder,
        originals,
    }
}
impl Recording {
    fn originals_kept(&self) {
        for (kind, bytes) in &self.originals {
            assert_eq!(
                &std::fs::read(self.folder.join(kind)).unwrap(),
                bytes,
                "{kind}"
            );
        }
    }
    /// A conversion queued with the published result still installed, as a restart's requeue or a
    /// path that does not clear `prepared` first leaves it.
    fn requeue(&self) {
        let mut m = meta(&self.folder);
        m["status"] = json!("queued");
        m["manual_conversion"] = json!(true);
        std::fs::write(self.folder.join("meta.json"), m.to_string()).unwrap();
    }
    async fn converted(&self) -> Value {
        let server = start(&self.root, Fault::None, 0).await;
        let m = wait_status(&self.folder, "ready");
        assert_eq!(m["videos"]["front"]["frames"], FRAMES, "{m}");
        assert_eq!(m["videos"]["qcamera"]["frames"], FRAMES, "{m}");
        drop(server);
        m
    }
    async fn convert_again(&self) {
        let server = start(&self.root, Fault::None, 0).await;
        let (status, body) = server.post(&format!("/api/logs/{}/convert", self.id)).await;
        assert_eq!((status, body), (200, json!({"status":"queued"})));
        let m = wait_status(&self.folder, "ready");
        assert_eq!(m["videos"]["front"]["frames"], FRAMES, "{m}");
        assert_eq!(m["videos"]["qcamera"]["frames"], FRAMES, "{m}");
        assert!(m["warnings"].as_array().unwrap().is_empty(), "{m}");
        no_attempts(&self.folder);
    }
}

/// Smaller than either camera MP4, larger than every metadata file the server writes first.
fn write_limit(r: &Recording) -> u64 {
    let smallest = ["qcamera.ts", "fcamera.hevc"]
        .iter()
        .map(|k| r.originals[k].len() as u64)
        .min()
        .unwrap();
    let limit = smallest / 2;
    assert!(
        limit > 64 * 1024,
        "synthetic media too small for the write limit: {limit}"
    );
    limit
}

/// `workers`: how many pure workers a `Kill` fault kills, one after the other.
async fn faulted(r: &Recording, fault: Fault, workers: usize) -> Server {
    let limit = if matches!(fault, Fault::WriteLimit | Fault::SingleCopyWriteLimit) {
        write_limit(r)
    } else {
        0
    };
    let server = start(&r.root, fault, limit).await;
    if matches!(fault, Fault::Kill) {
        for _ in 0..workers {
            server.kill_worker();
        }
    }
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resource_failure_keeps_the_published_result_and_a_later_conversion_completes() {
    for fault in [
        Fault::Kill,
        Fault::WriteLimit,
        Fault::SingleCopyWriteLimit,
        Fault::Deadline,
    ] {
        let r = recording(true);
        let ready = r.converted().await;
        let prepared = files(&r.folder.join("prepared"));
        assert!(prepared.contains_key("fcamera.mp4") && prepared.contains_key("camera.mp4"));
        r.requeue();
        let server = faulted(&r, fault, 1).await;
        let m = wait_status(&r.folder, "error");
        assert_eq!(files(&r.folder.join("prepared")), prepared, "{fault:?}");
        for key in ["videos", "video", "duration", "warnings", "video_downloads"] {
            assert_eq!(m[key], ready[key], "{fault:?} {key}");
        }
        no_attempts(&r.folder);
        r.originals_kept();
        if !matches!(fault, Fault::WriteLimit | Fault::SingleCopyWriteLimit) {
            let log = server.log();
            assert!(log.contains("conversion_kept_previous"), "{fault:?}: {log}");
            assert!(!log.contains("remux_fallback"), "{fault:?}: {log}");
        }
        drop(server);
        r.convert_again().await;
        r.originals_kept();
    }
}

/// Retention off: the stored MP4s are the only video copies and the conversion inputs.
#[tokio::test(flavor = "multi_thread")]
async fn a_resource_failure_keeps_the_stored_videos_when_originals_are_not_retained() {
    let r = recording(false);
    r.converted().await;
    let stored = ["camera.mp4", "fcamera.mp4"].map(|k| std::fs::read(r.folder.join(k)).unwrap());
    assert!(!r.folder.join("fcamera.hevc").exists() && !r.folder.join("qcamera.ts").exists());
    let server = faulted(&r, Fault::InspectDeadline, 0).await;
    let (status, _) = server.delete(&format!("/api/logs/{}/prepared", r.id)).await;
    assert_eq!(status, 200);
    let (status, body) = server.post(&format!("/api/logs/{}/convert", r.id)).await;
    assert_eq!((status, body), (200, json!({"status":"queued"})));
    wait_status(&r.folder, "error");
    assert_eq!(
        ["camera.mp4", "fcamera.mp4"].map(|k| std::fs::read(r.folder.join(k)).unwrap()),
        stored
    );
    no_attempts(&r.folder);
    assert!(server.log().contains("conversion_kept_previous"));
    drop(server);
    let server = start(&r.root, Fault::None, 0).await;
    let (status, _) = server.post(&format!("/api/logs/{}/convert", r.id)).await;
    assert_eq!(status, 200);
    let m = wait_status(&r.folder, "ready");
    assert_eq!(m["videos"]["front"]["frames"], FRAMES, "{m}");
    assert_eq!(m["videos"]["qcamera"]["frames"], FRAMES, "{m}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_conversion_failing_for_resources_is_ready_with_a_warning_and_keeps_originals() {
    for keep in [true, false] {
        for fault in [
            Fault::Kill,
            Fault::WriteLimit,
            Fault::SingleCopyWriteLimit,
            Fault::Deadline,
        ] {
            let r = recording(keep);
            let server = faulted(&r, fault, 2).await;
            let m = wait_status(&r.folder, "ready");
            assert!(
                m["videos"].as_object().unwrap().is_empty(),
                "{keep} {fault:?} {m}"
            );
            let warnings = m["warnings"].as_array().unwrap();
            for camera in ["qcamera", "fcamera.hevc"] {
                assert!(
                    warnings.iter().any(|w| {
                        let w = w.as_str().unwrap();
                        w.starts_with(&format!("{camera} 영상 준비 실패: resource ("))
                    }),
                    "{keep} {fault:?} {warnings:?}"
                );
            }
            r.originals_kept();
            for mp4 in ["camera.mp4", "fcamera.mp4"] {
                assert!(
                    !r.folder.join(mp4).exists() && !r.folder.join("prepared").join(mp4).exists()
                );
            }
            no_attempts(&r.folder);
            drop(server);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_conversion_keeps_the_published_result() {
    let r = recording(true);
    r.converted().await;
    let prepared = files(&r.folder.join("prepared"));
    r.requeue();
    let server = start(&r.root, Fault::Kill, 0).await;
    wait_for("crash point", Duration::from_secs(60), || {
        server.marker.exists()
    });
    let (status, body) = server.post(&format!("/api/logs/{}/cancel", r.id)).await;
    assert_eq!((status, body), (200, json!({"status":"cancelling"})));
    let m = wait_status(&r.folder, "unconverted");
    assert_eq!(m["error"], Value::Null);
    wait_for("cancelled processes", Duration::from_secs(10), || {
        descendants(server.child.id()).is_empty()
    });
    assert_eq!(files(&r.folder.join("prepared")), prepared);
    no_attempts(&r.folder);
    r.originals_kept();
    drop(server);
    r.convert_again().await;
}

/// Retention off deletes an original only after its MP4 is in place and synced: a server killed
/// between the move and the sync still holds every original, and its restart completes the
/// publication from the journal.
#[tokio::test(flavor = "multi_thread")]
async fn retention_off_deletes_an_original_only_after_its_mp4_is_durable() {
    let r = recording(false);
    let mut server = start(&r.root, Fault::Publish, 0).await;
    wait_for("publication crash point", Duration::from_secs(60), || {
        server.marker.exists()
    });
    let moved = ["camera.mp4", "fcamera.mp4"]
        .iter()
        .filter(|k| r.folder.join(k).is_file())
        .count();
    assert_eq!(moved, 1, "parked after the first MP4 moved");
    r.originals_kept();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    drop(server);
    let server = start(&r.root, Fault::None, 0).await;
    let m = wait_status(&r.folder, "ready");
    assert_eq!(m["videos"]["front"]["frames"], FRAMES, "{m}");
    assert_eq!(m["videos"]["qcamera"]["frames"], FRAMES, "{m}");
    for (raw, mp4) in [
        ("qcamera.ts", "camera.mp4"),
        ("fcamera.hevc", "fcamera.mp4"),
    ] {
        assert!(!r.folder.join(raw).exists(), "{raw}");
        assert!(r.folder.join(mp4).is_file(), "{mp4}");
    }
    assert_eq!(
        std::fs::read(r.folder.join("rlog.zst")).unwrap(),
        r.originals["rlog.zst"]
    );
    no_attempts(&r.folder);
    drop(server);
}
