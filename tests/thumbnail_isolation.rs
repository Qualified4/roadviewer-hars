//! A camera the thumbnail decoder cannot read (HEVC Main 10: the remux engine carries it, the pure
//! decoder takes 8-bit 4:2:0 only) costs the record its thumbnail and nothing else: the record is
//! `ready` without an error or warning, the video plays with Range, the log, telemetry and track
//! APIs answer, the list shows the record without a thumbnail, and the original stays.
//! Real executable, synthetic media and log.
#![cfg(unix)]
mod common;
#[path = "common/recording.rs"]
mod recording;
use common::{http, parse_response, read_all, request_bytes, web};
use recording::{FRAMES, encode, rlog};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn start(root: &Path, fault: &str) -> (Server, SocketAddr) {
    for _ in 0..5 {
        let ui: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let device: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let mut server = Server(
            Command::new(env!("CARGO_BIN_EXE_roadviewer-hars"))
                .env("HARS_DATA", root)
                .env("HARS_WEB", web())
                .env("HARS_INGRESS_ONLY", "0")
                .env("HARS_BIND", ui.to_string())
                .env("HARS_DEVICE_BIND", device.to_string())
                .env_remove("HARS_REMUX_ENGINE")
                .env_remove("HARS_CRASH_POINT")
                .env("HARS_FAULT", fault)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let begun = Instant::now();
        while begun.elapsed() < Duration::from_secs(30) {
            if server.0.try_wait().unwrap().is_some() {
                break;
            }
            if tokio::net::TcpStream::connect(ui).await.is_ok()
                && http(ui, "GET", "/health", &[], b"").await.0 == 200
            {
                return (server, ui);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    panic!("server did not start");
}

async fn get(ui: SocketAddr, path: &str, headers: &[(&str, String)]) -> (u16, Vec<u8>) {
    use tokio::io::AsyncWriteExt;
    let mut stream = tokio::net::TcpStream::connect(ui).await.unwrap();
    stream
        .write_all(&request_bytes("GET", path, headers, b""))
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

async fn row(ui: SocketAddr, id: &str) -> Option<Value> {
    let (status, body) = http(ui, "GET", "/api/logs", &[], b"").await;
    assert_eq!(status, 200);
    body["logs"]
        .as_array()
        .and_then(|logs| logs.iter().find(|r| r["id"] == id).cloned())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_camera_without_a_thumbnail_leaves_the_record_ready_and_playable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("data");
    let id = roadviewer_hars::store::id();
    let folder = root.join(&id);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        root.join(".processing-settings.json"),
        json!({"auto_convert":false,"keep_original_video":true}).to_string(),
    )
    .unwrap();
    let original = encode(
        &[
            "-c:v",
            "libx265",
            "-profile:v",
            "main10",
            "-x265-params",
            "pools=1:frame-threads=1:bframes=0:keyint=20:log-level=error",
            "-pix_fmt",
            "yuv420p10le",
            "-f",
            "hevc",
        ],
        &folder.join("fcamera.hevc"),
    );
    std::fs::write(folder.join("rlog.zst"), rlog()).unwrap();
    std::fs::write(
        folder.join("meta.json"),
        json!({"id":id,"name":"main10","status":"queued","manual_conversion":true,"uploaded":1.0,
            "files":{"rlog.zst":"rlog.zst","fcamera.hevc":"fcamera.hevc"}})
        .to_string(),
    )
    .unwrap();

    let (server, ui) = start(&root, "").await;
    let begun = Instant::now();
    let listed = loop {
        if let Some(r) = row(ui, &id).await
            && matches!(r["status"].as_str(), Some("ready" | "error"))
        {
            break r;
        }
        assert!(begun.elapsed() < Duration::from_secs(120), "not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(listed["status"], "ready", "{listed}");
    assert_ne!(listed["thumbnail"], json!(true), "{listed}");

    let meta: Value =
        serde_json::from_slice(&std::fs::read(folder.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["error"], Value::Null, "{meta}");
    assert_eq!(meta["videos"]["front"]["frames"], FRAMES, "{meta}");
    assert_eq!(meta["warnings"], json!([]), "{meta}");
    assert!(folder.join("prepared/thumbnail.failed").is_file());
    assert!(!folder.join("prepared/thumbnail.jpg").exists());

    let (status, body) = get(ui, &format!("/api/logs/{id}/thumbnail"), &[]).await;
    assert_eq!(status, 404, "{}", String::from_utf8_lossy(&body));

    let mp4 = std::fs::read(folder.join("prepared/fcamera.mp4")).unwrap();
    let (status, body) = get(
        ui,
        &format!("/api/logs/{id}/video?source=front"),
        &[("Range", "bytes=0-99".into())],
    )
    .await;
    assert_eq!((status, body.as_slice()), (206, &mp4[..100]));

    for api in ["data", "telemetry", "track"] {
        let (status, body) = get(ui, &format!("/api/logs/{id}/{api}"), &[]).await;
        assert_eq!(status, 200, "{api}: {}", String::from_utf8_lossy(&body));
    }
    let (status, body) = get(ui, &format!("/api/logs/{id}/original/fcamera.hevc"), &[]).await;
    assert_eq!((status, body == original), (200, true));
    drop(server);
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(debug_assertions)]
async fn thumbnail_memory_pressure_does_not_block_publication() {
    for (fault, has_thumbnail) in [("", true), ("thumbnail_memory_747_mib", false)] {
        let root = tempfile::tempdir().unwrap();
        let id = roadviewer_hars::store::id();
        let folder = root.path().join(&id);
        std::fs::create_dir_all(&folder).unwrap();
        encode(
            &[
                "-c:v",
                "libx265",
                "-x265-params",
                "pools=1:frame-threads=1:bframes=0:keyint=20:log-level=error",
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
            json!({"id":id,"name":"thumbnail memory","status":"queued","manual_conversion":true,"uploaded":1.0,
                "files":{"rlog.zst":"rlog.zst","fcamera.hevc":"fcamera.hevc"}})
            .to_string(),
        )
        .unwrap();

        let (_server, ui) = start(root.path(), fault).await;
        let listed = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(r) = row(ui, &id).await
                    && matches!(r["status"].as_str(), Some("ready" | "error"))
                {
                    break r;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("conversion waited for optional thumbnail memory");
        assert_eq!(listed["status"], "ready", "{listed}");
        let prepared = folder.join("prepared");
        let summary: Value =
            serde_json::from_slice(&std::fs::read(prepared.join("summary.json")).unwrap()).unwrap();
        assert_eq!(summary["videos"]["front"]["frames"], FRAMES);
        assert_eq!(summary["warnings"], json!([]));
        assert!(prepared.join("data.json.gz").is_file());
        assert!(prepared.join("telemetry.json.gz").is_file());
        assert!(prepared.join("fcamera.mp4").is_file());
        assert_eq!(prepared.join("thumbnail.failed").is_file(), !has_thumbnail);
        let (status, jpeg) = get(ui, &format!("/api/logs/{id}/thumbnail"), &[]).await;
        assert_eq!(status, if has_thumbnail { 200 } else { 404 });
        if has_thumbnail {
            assert!(jpeg.starts_with(&[0xff, 0xd8]));
        }
    }
}
