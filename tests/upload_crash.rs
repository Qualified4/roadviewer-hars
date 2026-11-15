//! SIGKILL of the real server executable at upload crash points, then restart and recovery.
#![cfg(unix)]
mod common;
use common::*;
use serde_json::json;
use std::{
    net::SocketAddr,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

const KIB: usize = 1024;

struct Proc {
    child: Child,
    ui: SocketAddr,
    device: SocketAddr,
}
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Proc {
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
async fn start(root: &Path, crash: Option<(&str, &Path)>) -> Proc {
    start_limited(root, crash, None).await
}
async fn start_limited(root: &Path, crash: Option<(&str, &Path)>, fsize: Option<u64>) -> Proc {
    for _ in 0..5 {
        let ui: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let device: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_roadviewer-hars"));
        command
            .env("HARS_DATA", root)
            .env("HARS_WEB", web())
            .env("HARS_INGRESS_ONLY", "0")
            .env("HARS_BIND", ui.to_string())
            .env("HARS_DEVICE_BIND", device.to_string())
            .env("RUST_LOG", "off")
            .env_remove("HARS_CRASH_POINT")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some((point, marker)) = crash {
            command
                .env("HARS_CRASH_POINT", point)
                .env("HARS_CRASH_MARKER", marker);
        }
        if let Some(limit) = fsize {
            use std::os::unix::process::CommandExt;
            // SAFETY: only async-signal-safe calls run between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    // Ignored SIGXFSZ survives exec, so oversized writes fail with EFBIG instead of killing.
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
        let mut proc = Proc {
            child: command.spawn().unwrap(),
            ui,
            device,
        };
        let begun = tokio::time::Instant::now();
        while begun.elapsed() < DEADLINE {
            if proc.child.try_wait().unwrap().is_some() {
                break; // Lost the port race; pick new ports.
            }
            if tokio::net::TcpStream::connect(device).await.is_ok()
                && tokio::net::TcpStream::connect(ui).await.is_ok()
                && http(ui, "GET", "/health", &[], b"").await.0 == 200
            {
                return proc;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = proc.child.kill();
    }
    panic!("server did not start");
}
async fn crashed(marker: &Path, point: &str) {
    poll(point, || async {
        std::fs::read_to_string(marker).is_ok_and(|v| v == point)
    })
    .await;
}
fn files<'a>(log: &'a [u8], video: &'a [u8]) -> [&'a [u8]; 2] {
    [log, video]
}
/// A data root without automatic conversion, so no background writer races the disk checks.
fn quiet(temp: &tempfile::TempDir) -> std::path::PathBuf {
    let root = temp.path().join("data");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join(".processing-settings.json"),
        br#"{"auto_convert":false}"#,
    )
    .unwrap();
    root
}

#[tokio::test(flavor = "multi_thread")]
async fn sigkill_around_chunk_fsync_resumes_from_disk_length() {
    for point in ["chunk_before_sync", "chunk_after_sync"] {
        let temp = tempfile::tempdir().unwrap();
        let root = quiet(&temp);
        let marker = temp.path().join("marker");
        let server = start(&root, Some((point, &marker))).await;
        let creds = pair(server.ui, server.device).await;
        let (log, video) = segment(21, 192 * KIB);
        let all = files(&log, &video);
        let manifest = batch(
            &format!("kill_chunk_{point}"),
            21,
            &[("rlog.zst", &log), ("qcamera.ts", &video)],
        );
        let session = begin(server.device, &creds, &manifest).await;
        let dev = server.device;
        let first = log[..64 * KIB].to_vec();
        let pending = {
            let session = session.clone();
            tokio::spawn(async move { put(dev, &session, 0, 0, &first).await })
        };
        crashed(&marker, point).await;
        server.kill();
        assert_eq!(
            pending.await.unwrap().0,
            0,
            "no response may be sent from a crash point"
        );

        let server = start(&root, None).await;
        let (code, state) = status(server.device, &session).await;
        assert_eq!(code, 200, "{state}");
        // The write reached the page cache before the process died, so its bytes survive a process kill.
        assert_eq!(state["files"][0]["received"], 64 * KIB);
        invariants(
            &root,
            server.ui,
            Expect {
                registered: &[],
                sessions: 1,
            },
        )
        .await;
        let resumed = begin(server.device, &creds, &manifest).await;
        upload_all(server.device, &resumed, &all, 64 * KIB).await;
        let (code, result) = finish(server.device, &resumed, &all).await;
        assert_eq!(code, 201, "{result}");
        invariants(
            &root,
            server.ui,
            Expect {
                registered: &[&all],
                sessions: 0,
            },
        )
        .await;
        server.kill();
        let server = start(&root, None).await;
        invariants(
            &root,
            server.ui,
            Expect {
                registered: &[&all],
                sessions: 0,
            },
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sigkill_during_finish_recovers_the_journal_and_receipt() {
    for point in [
        "finish_after_journal",
        "finish_after_move",
        "finish_before_receipt",
        "finish_after_receipt",
        "session_removing",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = quiet(&temp);
        let marker = temp.path().join("marker");
        let server = start(&root, Some((point, &marker))).await;
        let creds = pair(server.ui, server.device).await;
        let (log, video) = segment(22, 160 * KIB);
        let all = files(&log, &video);
        let manifest = batch(
            &format!("kill_finish_{point}"),
            22,
            &[("rlog.zst", &log), ("qcamera.ts", &video)],
        );
        let session = begin(server.device, &creds, &manifest).await;
        upload_all(server.device, &session, &all, 64 * KIB).await;
        let dev = server.device;
        let pending = {
            let session = session.clone();
            let (log, video) = (log.clone(), video.clone());
            tokio::spawn(async move { finish(dev, &session, &[&log, &video]).await })
        };
        crashed(&marker, point).await;
        server.kill();
        assert_eq!(
            pending.await.unwrap().0,
            0,
            "{point}: finish response must be lost"
        );

        let server = start(&root, None).await;
        // The uploader retries the lost finish with the same token.
        let (code, result) = finish(server.device, &session, &all).await;
        assert_eq!(code, 200, "{point}: {result}");
        assert_eq!(result["logs"].as_array().unwrap().len(), 1, "{point}");
        assert_eq!(result["duplicates"], json!([]));
        let records = invariants(
            &root,
            server.ui,
            Expect {
                registered: &[&all],
                sessions: 0,
            },
        )
        .await;
        assert_eq!(
            records.values().next().unwrap(),
            result["logs"][0]["id"].as_str().unwrap()
        );
        let completed = begin(server.device, &creds, &manifest).await;
        assert_eq!(
            completed,
            json!({"state":"completed","result":result}),
            "{point}"
        );
        server.kill();
        let server = start(&root, None).await;
        invariants(
            &root,
            server.ui,
            Expect {
                registered: &[&all],
                sessions: 0,
            },
        )
        .await;
        assert_eq!(finish(server.device, &session, &all).await, (200, result));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_writes_are_never_acknowledged_or_committed_over_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = quiet(&temp);
    // Unprivileged stand-in for a full disk: the kernel refuses writes beyond this file offset.
    let server = start_limited(&root, None, Some(12 * KIB as u64)).await;
    let creds = pair(server.ui, server.device).await;
    let (log, video) = segment(25, 32 * KIB);
    let all = files(&log, &video);
    let manifest = batch(
        "fsize_batch_0000001",
        25,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let session = begin(server.device, &creds, &manifest).await;
    assert_eq!(
        put(server.device, &session, 0, 0, &log[..8 * KIB]).await.0,
        200
    );
    let (code, body) = put(server.device, &session, 0, 8 * KIB, &log[8 * KIB..16 * KIB]).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (500, "storage_error")
    );
    // Python releases a session whose chunk write failed; the uploader begins the batch again.
    assert_eq!(status(server.device, &session).await.0, 404);
    invariants(
        &root,
        server.ui,
        Expect {
            registered: &[],
            sessions: 0,
        },
    )
    .await;

    // Every signed request persists a nonce; the growing credential file eventually hits the limit.
    let devices = root.join(".devices.json");
    let mut refused = false;
    for _ in 0..400 {
        let before = std::fs::read(&devices).unwrap();
        let (code, body) = begin_raw(server.device, &creds, &manifest).await;
        if code == 500 {
            assert_eq!(body["error"], "storage_error");
            assert_eq!(
                std::fs::read(&devices).unwrap(),
                before,
                "credential state was replaced"
            );
            refused = true;
            break;
        }
        assert!(code == 200 || code == 201, "{code} {body}");
    }
    assert!(refused, "the file size limit was never reached");
    server.kill();
    let server = start(&root, None).await;
    let session = begin(server.device, &creds, &manifest).await;
    upload_all(server.device, &session, &all, 16 * KIB).await;
    assert_eq!(finish(server.device, &session, &all).await.0, 201);
    invariants(
        &root,
        server.ui,
        Expect {
            registered: &[&all],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_conversion_waits_while_an_upload_is_receiving() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("data");
    let server = start(&root, None).await;
    let (ui, dev) = (server.ui, server.device);
    let settings = |auto: bool| async move {
        let mut headers = csrf();
        headers.push(("Content-Type", "application/json".into()));
        let body = serde_json::to_vec(&json!({"auto_convert":auto})).unwrap();
        assert_eq!(
            http(ui, "POST", "/api/settings/processing", &headers, &body)
                .await
                .0,
            200
        );
    };
    settings(false).await;
    let creds = pair(ui, dev).await;
    let (log, video) = segment(23, 64 * KIB);
    let first = files(&log, &video);
    let session = begin(
        dev,
        &creds,
        &batch(
            "convert_wait_000001",
            23,
            &[("rlog.zst", &log), ("qcamera.ts", &video)],
        ),
    )
    .await;
    upload_all(dev, &session, &first, 64 * KIB).await;
    let (code, result) = finish(dev, &session, &first).await;
    assert_eq!(code, 201);
    let record = result["logs"][0]["id"].as_str().unwrap().to_owned();

    let (other, _) = segment(24, 256 * KIB);
    let receiving = begin(
        dev,
        &creds,
        &batch("convert_wait_000002", 24, &[("rlog.zst", &other)]),
    )
    .await;
    assert_eq!(put(dev, &receiving, 0, 0, &other[..32 * KIB]).await.0, 200);
    settings(true).await;
    let id = record.clone();
    poll("conversion to wait for the upload", || {
        let id = id.clone();
        async move {
            let (_, progress) = http(ui, "GET", "/api/progress", &[], b"").await;
            progress["progress"][&id]["stage"] == "waiting_upload"
        }
    })
    .await;
    let status_of = |id: String| async move {
        let (_, list) = http(ui, "GET", "/api/logs", &[], b"").await;
        list["logs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id.as_str())
            .unwrap()["status"]
            .clone()
    };
    for offset in (32 * KIB..other.len()).step_by(32 * KIB) {
        assert_eq!(
            put(
                dev,
                &receiving,
                0,
                offset,
                &other[offset..offset + 32 * KIB]
            )
            .await
            .0,
            200
        );
        assert_eq!(status_of(record.clone()).await, "queued");
    }
    let (code, second) = finish(dev, &receiving, &[&other]).await;
    assert_eq!(code, 201);
    let second = second["logs"][0]["id"].as_str().unwrap().to_owned();
    poll("conversion to start once the upload ended", || {
        let id = record.clone();
        async move { status_of(id).await != "queued" }
    })
    .await;
    // Disk checks run once no conversion is writing metadata any more.
    for id in [record, second] {
        poll("conversion to settle", || {
            let id = id.clone();
            async move { matches!(status_of(id).await.as_str(), Some("error" | "ready")) }
        })
        .await;
    }
    // The synthetic log fails conversion; originals must be untouched either way.
    invariants(
        &root,
        ui,
        Expect {
            registered: &[&first, &[&other]],
            sessions: 0,
        },
    )
    .await;
}
