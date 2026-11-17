//! A camera remux that fails for the machine (a killed worker, the write limit, the deadline) on the real
//! server: a camera the result published on disk plays is never dropped for it. The attempt stops, the
//! previous result stays byte for byte and plays, the record is `ready` again with the failure among its
//! warnings, and the staging goes; a later conversion completes. Without a published video of that
//! camera (a reconversion clears `prepared/` first) it ends as Python's `prepare_video` failure does,
//! ready with a warning naming the resource class, and the original stays whatever
//! `keep_original_video` says. No failure, repeated or deterministic, leaves the record in `error`.
//! Real executable, synthetic media and log.
#![cfg(unix)]
mod common;
#[path = "common/recording.rs"]
mod recording;
use common::{csrf, http, parse_response, read_all, request_bytes, web};
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
    /// The server parked at this crash point.
    Crash(&'static str),
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
            Fault::Crash(point) => {
                command.env("HARS_CRASH_POINT", point);
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
    /// The product's reconversion: remove the converted data (a ready record is not converted
    /// again otherwise), then convert.
    async fn reconvert(&self, id: &str) {
        let (status, _) = self.delete(&format!("/api/logs/{id}/prepared")).await;
        assert_eq!(status, 200);
        let (status, body) = self.post(&format!("/api/logs/{id}/convert")).await;
        assert_eq!((status, body), (200, json!({"status":"queued"})));
    }
    /// The first bytes of a camera's video, as a player asks for them.
    async fn range(&self, id: &str, source: &str) -> (u16, Vec<u8>) {
        use tokio::io::AsyncWriteExt;
        let mut stream = tokio::net::TcpStream::connect(self.ui).await.unwrap();
        let path = format!("/api/logs/{id}/video?source={source}");
        stream
            .write_all(&request_bytes(
                "GET",
                &path,
                &[("Range", "bytes=0-99".into())],
                b"",
            ))
            .await
            .unwrap();
        let raw = read_all(&mut stream).await;
        let body = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| raw[i + 4..].to_vec())
            .unwrap_or_default();
        (parse_response(&raw).0, body)
    }
    /// The server's own view of the record reaches `ready`. `wait_status` reads `meta.json`, which
    /// the server renames into place (and syncs) before it updates the record it answers from, so
    /// the file can say `ready` a moment before the API does.
    async fn ready(&self, id: &str) {
        let start = Instant::now();
        loop {
            let (status, body) = http(self.ui, "GET", "/api/logs", &[], b"").await;
            let row = body["logs"]
                .as_array()
                .and_then(|logs| logs.iter().find(|r| r["id"] == id).cloned());
            if status == 200 && row.as_ref().is_some_and(|r| r["status"] == "ready") {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the server did not report ready: {status} {row:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    async fn plays(&self, id: &str, source: &str, file: &Path) {
        self.ready(id).await;
        let (status, body) = self.range(id, source).await;
        assert_eq!(status, 206, "{source}");
        assert_eq!(body, std::fs::read(file).unwrap()[..100], "{source}");
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
        server.reconvert(&self.id).await;
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
        let m = wait_status(&r.folder, "ready");
        assert_eq!(files(&r.folder.join("prepared")), prepared, "{fault:?}");
        for key in ["videos", "video", "duration", "video_downloads"] {
            assert_eq!(m[key], ready[key], "{fault:?} {key}");
        }
        assert_eq!(m["error"], Value::Null, "{fault:?}");
        kept_warning(&ready, &m);
        for (source, mp4) in [("front", "fcamera.mp4"), ("qcamera", "camera.mp4")] {
            server
                .plays(&r.id, source, &r.folder.join("prepared").join(mp4))
                .await;
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

/// The previous warnings and one more: which camera failed, for which resource, and that the
/// previous result stays.
fn kept_warning(ready: &Value, m: &Value) {
    let before = ready["warnings"].as_array().unwrap();
    let after = m["warnings"].as_array().unwrap();
    assert_eq!(after.len(), before.len() + 1, "{m}");
    assert_eq!(&after[..before.len()], &before[..], "{m}");
    let added = after.last().unwrap().as_str().unwrap();
    assert!(
        added.contains("영상 준비 실패: resource (") && added.ends_with("(이전 변환 결과 유지)"),
        "{added}"
    );
}

/// Retention off: the stored MP4s are the only video copies and the conversion inputs. With the
/// converted data removed there is no published result to keep: the record is ready without the
/// cameras, as in Python, and the stored MP4s stay untouched and playable.
#[tokio::test(flavor = "multi_thread")]
async fn a_resource_failure_without_a_published_result_keeps_the_stored_videos_playable() {
    let r = recording(false);
    r.converted().await;
    let stored = ["camera.mp4", "fcamera.mp4"].map(|k| std::fs::read(r.folder.join(k)).unwrap());
    assert!(!r.folder.join("fcamera.hevc").exists() && !r.folder.join("qcamera.ts").exists());
    let server = faulted(&r, Fault::InspectDeadline, 0).await;
    let (status, _) = server.delete(&format!("/api/logs/{}/prepared", r.id)).await;
    assert_eq!(status, 200);
    let (status, body) = server.post(&format!("/api/logs/{}/convert", r.id)).await;
    assert_eq!((status, body), (200, json!({"status":"queued"})));
    let m = wait_status(&r.folder, "ready");
    assert!(m["videos"].as_object().unwrap().is_empty(), "{m}");
    resource_warnings(&m, &["qcamera", "fcamera.hevc"]);
    assert_eq!(
        ["camera.mp4", "fcamera.mp4"].map(|k| std::fs::read(r.folder.join(k)).unwrap()),
        stored
    );
    for (source, mp4) in [("front", "fcamera.mp4"), ("qcamera", "camera.mp4")] {
        server.plays(&r.id, source, &r.folder.join(mp4)).await;
    }
    no_attempts(&r.folder);
    assert!(!server.log().contains("conversion_kept_previous"));
    drop(server);
    let server = start(&r.root, Fault::None, 0).await;
    server.reconvert(&r.id).await;
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

/// Each original has a verified Drive copy, and each MP4 was removed by an earlier archive cleanup: retention off
/// makes the copies the stored originals and the new local MP4s visible again.
fn with_remote_copies(r: &Recording) {
    use sha2::Digest;
    let mut m = meta(&r.folder);
    let copy = |kind: &str| {
        let bytes = &r.originals[kind];
        json!({"location":"local","remote":{"backend":"gdrive","file_id":format!("drive-{kind}"),
            "bytes":bytes.len(),"sha256":hex::encode(sha2::Sha256::digest(bytes)),"uploaded_at":1.0}})
    };
    let removed = json!({"location":"removed","bytes":1,"removed_at":1.0,"reason":"nas_full"});
    m["storage"] = json!({"qcamera.ts":copy("qcamera.ts"),"fcamera.hevc":copy("fcamera.hevc"),
        "camera.mp4":removed,"fcamera.mp4":removed});
    std::fs::write(r.folder.join("meta.json"), m.to_string()).unwrap();
}

/// What a finished retention-off publication leaves: the MP4s next to the log, no original, the index naming
/// the Drive copies as the stored originals and hiding no MP4.
fn published_without_originals(r: &Recording) -> Value {
    let m = wait_status(&r.folder, "ready");
    assert_eq!(m["videos"]["front"]["frames"], FRAMES, "{m}");
    assert_eq!(m["videos"]["qcamera"]["frames"], FRAMES, "{m}");
    for (raw, mp4) in [
        ("qcamera.ts", "camera.mp4"),
        ("fcamera.hevc", "fcamera.mp4"),
    ] {
        assert!(!r.folder.join(raw).exists(), "{raw}");
        assert!(r.folder.join(mp4).is_file(), "{mp4}");
        assert_eq!(m["storage"][raw]["location"], "gdrive", "{raw}: {m}");
        assert_eq!(
            m["storage"][raw]["file_id"],
            format!("drive-{raw}"),
            "{raw}: {m}"
        );
        assert!(m["storage"].get(mp4).is_none(), "{mp4}: {m}");
    }
    no_attempts(&r.folder);
    m["storage"].clone()
}

/// Retention off with archived copies: a server killed at any point of the publication (an MP4 moved, the
/// metadata saved, an original deleted) restarts into the same end state as an uninterrupted one, with the index
/// and the disk agreeing. An original deleted before its index change was durable once left its Drive copy
/// unpromoted and its MP4 hidden behind `removed`.
#[tokio::test(flavor = "multi_thread")]
async fn retention_off_publication_converges_from_every_crash_point() {
    let r = recording(false);
    with_remote_copies(&r);
    let server = start(&r.root, Fault::None, 0).await;
    let expected = published_without_originals(&r);
    drop(server);
    for point in [
        "publish_mp4_moved",
        "publish_original_deleted",
        "publish_meta_saved",
    ] {
        let r = recording(false);
        with_remote_copies(&r);
        let mut server = start(&r.root, Fault::Crash(point), 0).await;
        wait_for(point, Duration::from_secs(60), || server.marker.exists());
        server.child.kill().unwrap();
        server.child.wait().unwrap();
        drop(server);
        let server = start(&r.root, Fault::None, 0).await;
        assert_eq!(published_without_originals(&r), expected, "{point}");
        assert_eq!(
            std::fs::read(r.folder.join("rlog.zst")).unwrap(),
            r.originals["rlog.zst"]
        );
        drop(server);
    }
}

/// A conversion that read the stored MP4s (retention off, originals gone) leaves no MP4 of its own in
/// `prepared/`. An original restored next to a stored MP4 before a restart resumes that publication is not
/// this conversion's source: it stays, as does the MP4.
#[tokio::test(flavor = "multi_thread")]
async fn a_restored_original_next_to_a_stored_mp4_survives_a_resumed_publication() {
    let r = recording(false);
    r.converted().await;
    let stored = ["camera.mp4", "fcamera.mp4"].map(|k| std::fs::read(r.folder.join(k)).unwrap());
    r.requeue();
    let mut server = start(&r.root, Fault::Crash("publish_installed"), 0).await;
    wait_for("publish_installed", Duration::from_secs(60), || {
        server.marker.exists()
    });
    for raw in ["qcamera.ts", "fcamera.hevc"] {
        std::fs::write(r.folder.join(raw), &r.originals[raw]).unwrap();
    }
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    drop(server);
    let server = start(&r.root, Fault::None, 0).await;
    wait_status(&r.folder, "ready");
    r.originals_kept();
    assert_eq!(
        ["camera.mp4", "fcamera.mp4"].map(|k| std::fs::read(r.folder.join(k)).unwrap()),
        stored
    );
    no_attempts(&r.folder);
    drop(server);
}

/// A published camera whose original the engine refuses on every attempt (here B-frames in the TS,
/// standing in for an input an earlier version published) does not hold the record back: a camera
/// added later is converted and published, the refused camera stays as it was published, MP4 and
/// summary entry, with a warning, and its original stays. With retention off at that point the
/// new camera's MP4 goes next to its original as usual, and the carried one is neither moved
/// nor its original deleted. Converting again ends in the same state.
/// The state is built by hand (files moved, record requeued): the upload path that adds a camera
/// clears `prepared/` first and so never reaches the takeover.
#[tokio::test(flavor = "multi_thread")]
async fn a_camera_the_engine_refuses_keeps_its_published_video_and_a_new_camera_publishes() {
    for keep in [true, false] {
        let r = recording(true);
        let front = r.folder.with_extension("fcamera.hevc");
        std::fs::rename(r.folder.join("fcamera.hevc"), &front).unwrap();
        let mut m = meta(&r.folder);
        m["files"].as_object_mut().unwrap().remove("fcamera.hevc");
        std::fs::write(r.folder.join("meta.json"), m.to_string()).unwrap();
        let server = start(&r.root, Fault::None, 0).await;
        let published = wait_status(&r.folder, "ready");
        assert_eq!(
            published["videos"]["qcamera"]["frames"], FRAMES,
            "{published}"
        );
        assert!(published["videos"].get("front").is_none(), "{published}");
        drop(server);
        let camera = std::fs::read(r.folder.join("prepared/camera.mp4")).unwrap();
        // Where the video starts on the log's clock, which a carried entry must keep.
        let offset = |m: &Value| {
            let data =
                roadviewer_hars::logs::read_gzip(&r.folder.join("prepared/data.json.gz")).unwrap();
            m["videos"]["qcamera"]["start"].as_f64().unwrap() - data["logStart"].as_f64().unwrap()
        };
        let published_offset = offset(&published);

        encode(
            &[
                "-c:v", "libx264", "-bf", "2", "-g", "20", "-qp", "12", "-pix_fmt", "yuv420p",
                "-f", "mpegts",
            ],
            &r.folder.join("qcamera.ts"),
        );
        let refused = std::fs::read(r.folder.join("qcamera.ts")).unwrap();
        std::fs::rename(&front, r.folder.join("fcamera.hevc")).unwrap();
        let mut m = meta(&r.folder);
        m["files"]["fcamera.hevc"] = json!("fcamera.hevc");
        std::fs::write(r.folder.join("meta.json"), m.to_string()).unwrap();
        std::fs::write(
            r.root.join(".processing-settings.json"),
            json!({"auto_convert":false,"keep_original_video":keep}).to_string(),
        )
        .unwrap();

        let mut previous = None;
        for attempt in 0..2 {
            r.requeue();
            let server = start(&r.root, Fault::None, 0).await;
            let m = wait_status(&r.folder, "ready");
            assert_eq!(m["error"], Value::Null, "{keep} {attempt}");
            assert_eq!(
                m["videos"]["front"]["frames"], FRAMES,
                "{keep} {attempt} {m}"
            );
            assert!(
                (offset(&m) - published_offset).abs() < 2e-6,
                "{keep} {attempt}: {} {published_offset}",
                offset(&m)
            );
            for key in [
                "frames",
                "duration",
                "width",
                "height",
                "codec",
                "sourcePts",
                "timestampSpreadMs",
            ] {
                assert_eq!(
                    m["videos"]["qcamera"][key], published["videos"]["qcamera"][key],
                    "{keep} {attempt} {key}"
                );
            }
            let warnings = m["warnings"].as_array().unwrap();
            assert_eq!(warnings.len(), 1, "{keep} {attempt} {m}");
            let warning = warnings[0].as_str().unwrap();
            assert!(
                warning.starts_with("qcamera 영상 준비 실패: ")
                    && !warning.contains("resource (")
                    && warning.ends_with("(이전 변환 결과 유지)"),
                "{warning}"
            );
            assert_eq!(
                std::fs::read(r.folder.join("prepared/camera.mp4")).unwrap(),
                camera,
                "{keep} {attempt}"
            );
            assert_eq!(
                std::fs::read(r.folder.join("qcamera.ts")).unwrap(),
                refused,
                "{keep} {attempt}"
            );
            assert!(!r.folder.join("camera.mp4").exists(), "{keep} {attempt}");
            let front_mp4 = if keep {
                assert_eq!(
                    std::fs::read(r.folder.join("fcamera.hevc")).unwrap(),
                    r.originals["fcamera.hevc"]
                );
                r.folder.join("prepared/fcamera.mp4")
            } else {
                assert!(!r.folder.join("fcamera.hevc").exists(), "{attempt}");
                r.folder.join("fcamera.mp4")
            };
            assert!(
                !r.folder
                    .join("prepared")
                    .join(roadviewer_hars::convert::CARRIED_PREVIOUS)
                    .exists()
            );
            server
                .plays(&r.id, "qcamera", &r.folder.join("prepared/camera.mp4"))
                .await;
            server.plays(&r.id, "front", &front_mp4).await;
            assert!(server.log().contains("conversion_carried_previous"));
            assert!(!server.log().contains("conversion_kept_previous"));
            no_attempts(&r.folder);
            let state = ["videos", "video", "duration", "warnings", "video_downloads"]
                .map(|k| m[k].clone());
            if let Some(previous) = &previous {
                assert_eq!(&state, previous, "{keep}: a second conversion converges");
            }
            previous = Some(state);
            drop(server);
        }
    }
}

/// A warning naming the resource class for each camera label.
fn resource_warnings(m: &Value, cameras: &[&str]) {
    let warnings = m["warnings"].as_array().unwrap();
    for camera in cameras {
        assert!(
            warnings.iter().any(|w| w
                .as_str()
                .unwrap()
                .starts_with(&format!("{camera} 영상 준비 실패: resource ("))),
            "{camera}: {warnings:?}"
        );
    }
}

/// A server whose fault applies to the conversion requested after it starts.
async fn reconvert_faulted(r: &Recording, fault: Fault) -> Server {
    let limit = if matches!(fault, Fault::WriteLimit) {
        write_limit(r)
    } else {
        0
    };
    let server = start(&r.root, fault, limit).await;
    server.reconvert(&r.id).await;
    if matches!(fault, Fault::Kill) {
        server.kill_worker();
        server.kill_worker();
    }
    server
}

/// A requested reconversion clears `prepared/` first and leaves `meta.videos` of the old result:
/// that stale entry once made a resource failure end the conversion in `error` (with nothing kept to
/// play), and every retry the same. It ends as a first conversion does.
#[tokio::test(flavor = "multi_thread")]
async fn a_reconversion_failing_for_resources_is_ready_with_a_warning_not_an_error() {
    for fault in [Fault::Kill, Fault::WriteLimit, Fault::Deadline] {
        let r = recording(true);
        r.converted().await;
        let server = reconvert_faulted(&r, fault).await;
        let m = wait_status(&r.folder, "ready");
        assert!(m["videos"].as_object().unwrap().is_empty(), "{fault:?} {m}");
        assert_eq!(m["error"], Value::Null, "{fault:?}");
        resource_warnings(&m, &["qcamera", "fcamera.hevc"]);
        for mp4 in ["camera.mp4", "fcamera.mp4"] {
            assert!(!r.folder.join("prepared").join(mp4).exists(), "{fault:?}");
        }
        r.originals_kept();
        no_attempts(&r.folder);
        if !matches!(fault, Fault::WriteLimit) {
            assert!(
                !server.log().contains("conversion_kept_previous"),
                "{fault:?}"
            );
        }
        drop(server);
        r.convert_again().await;
        r.originals_kept();
    }
}

/// A camera original the server cannot read (permission denied, as for a bad sector: every attempt
/// fails the same way) is the input's failure, not the machine's: the camera is left out with a
/// warning, on the first conversion and on each reconversion, and nothing is kept failing.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_camera_original_ends_ready_without_that_camera() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid(2) has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root reads a file of mode 000");
        return;
    }
    let r = recording(true);
    let unreadable = |mode| {
        std::fs::set_permissions(
            r.folder.join("fcamera.hevc"),
            std::fs::Permissions::from_mode(mode),
        )
        .unwrap()
    };
    let check = |m: &Value| {
        assert_eq!(m["error"], Value::Null, "{m}");
        assert_eq!(m["videos"]["qcamera"]["frames"], FRAMES, "{m}");
        assert!(m["videos"].get("front").is_none(), "{m}");
        let warnings = m["warnings"].as_array().unwrap();
        assert!(
            warnings.iter().any(|w| {
                let w = w.as_str().unwrap();
                w.starts_with("fcamera.hevc 영상 준비 실패: ") && !w.contains("resource (")
            }),
            "{warnings:?}"
        );
    };
    unreadable(0o000);
    let server = start(&r.root, Fault::None, 0).await;
    check(&wait_status(&r.folder, "ready"));
    for _ in 0..2 {
        server.reconvert(&r.id).await;
        check(&wait_status(&r.folder, "ready"));
        no_attempts(&r.folder);
    }
    drop(server);
    unreadable(0o644);
    r.originals_kept();
    r.convert_again().await;
}

/// The same record failing for resources again and again, with and without a published result in
/// place, is ready after every attempt, and a conversion on a sound machine completes it.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_resource_failures_on_one_record_stay_ready() {
    let r = recording(true);
    let ready = r.converted().await;
    r.requeue();
    let server = faulted(&r, Fault::WriteLimit, 0).await;
    let m = wait_status(&r.folder, "ready");
    kept_warning(&ready, &m);
    drop(server);
    for _ in 0..2 {
        let server = reconvert_faulted(&r, Fault::WriteLimit).await;
        let m = wait_status(&r.folder, "ready");
        assert!(m["videos"].as_object().unwrap().is_empty(), "{m}");
        resource_warnings(&m, &["qcamera", "fcamera.hevc"]);
        no_attempts(&r.folder);
        r.originals_kept();
        drop(server);
    }
    r.requeue();
    let server = faulted(&r, Fault::WriteLimit, 0).await;
    let m = wait_status(&r.folder, "ready");
    resource_warnings(&m, &["qcamera", "fcamera.hevc"]);
    drop(server);
    r.convert_again().await;
    r.originals_kept();
}
