#![cfg(target_os = "linux")]

mod common;
#[path = "common/recording.rs"]
mod recording;
use common::{csrf, http, web};
use roadviewer_hars::{logs, prepared, store::id};
use serde_json::{Value, json};
use std::{
    fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const SETTINGS: &str = "/api/settings/prepared";
const MOVE: &str = "/api/settings/prepared/move";
const CAMERA: &[u8] = b"a prepared camera fixture";

struct Server {
    child: Child,
    address: SocketAddr,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    async fn start(root: &Path, fake_share: bool, crash: Option<(&str, &Path)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut command = Command::new(env!("CARGO_BIN_EXE_roadviewer-hars"));
        command
            .env("HARS_DATA", root)
            .env("HARS_WEB", web())
            .env("HARS_BIND", address.to_string())
            .env("HARS_DEVICE_BIND", "127.0.0.1:0")
            .env("HARS_INGRESS_ONLY", "0")
            .env(
                "RV_ARCHIVE_ALLOW_ANY_PATH",
                if fake_share { "1" } else { "0" },
            )
            .env_remove("HARS_CRASH_POINT")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some((point, marker)) = crash {
            command
                .env("HARS_CRASH_POINT", point)
                .env("HARS_CRASH_MARKER", marker);
        }
        let mut server = Self {
            child: command.spawn().unwrap(),
            address,
        };
        until(|| async {
            TcpListener::bind(address).is_err()
                && server.request("GET", "/health", Value::Null).await.0 == 200
        })
        .await;
        assert!(server.child.try_wait().unwrap().is_none());
        server
    }

    async fn request(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let mut headers = csrf();
        headers.push(("content-type", "application/json".to_owned()));
        http(
            self.address,
            method,
            path,
            &headers,
            &serde_json::to_vec(&body).unwrap(),
        )
        .await
    }
    async fn configure(&self, share: &Path) -> (u16, Value) {
        self.request("POST", SETTINGS, json!({"location":"nas","path":share}))
            .await
    }
    async fn start_move(&self, direction: &str) {
        assert_eq!(
            self.request("POST", MOVE, json!({"direction":direction}))
                .await
                .0,
            200
        );
    }
    async fn move_and_wait(&self, direction: &str) -> Value {
        self.start_move(direction).await;
        until(|| async { self.request("GET", MOVE, Value::Null).await.1["state"] != "running" })
            .await;
        self.request("GET", MOVE, Value::Null).await.1
    }
}

async fn until(check: impl AsyncFn() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("fixture condition did not settle");
}

fn seed(root: &Path, status: &str, artifacts: bool) -> PathBuf {
    let record = id();
    let folder = root.join(&record);
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("meta.json"),
        serde_json::to_vec(&json!({
            "id": record, "status": status, "name":"prepared test",
            "uploaded":1, "manual_conversion":status == "queued",
        }))
        .unwrap(),
    )
    .unwrap();
    if !artifacts {
        return folder;
    }
    let prepared = folder.join("prepared");
    fs::create_dir(&prepared).unwrap();
    fs::write(prepared.join("camera.mp4"), CAMERA).unwrap();
    fs::write(prepared.join("thumbnail.jpg"), b"thumbnail").unwrap();
    fs::write(
        prepared.join("summary.json"),
        br#"{"has_track":true,"duration":2,"track_distance_m":15}"#,
    )
    .unwrap();
    for (file, value) in [
        ("data.json.gz", json!({"route": "prepared test"})),
        ("telemetry.json.gz", json!({"samples": [1,2]})),
        (
            "track.json.gz",
            json!({"points": [[0, 37.1, 127.1], [1, 37.2, 127.2]]}),
        ),
    ] {
        logs::write_gzip(&prepared.join(file), &value).unwrap();
    }
    folder
}

fn meta(folder: &Path) -> Value {
    serde_json::from_slice(&fs::read(folder.join("meta.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn missing_mount_defers_conversion_without_writing_to_local_or_share() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = seed(tmp.path(), "unconverted", false);
    fs::write(
        tmp.path().join(".processing-settings.json"),
        br#"{"auto_convert":false}"#,
    )
    .unwrap();
    let record = folder.file_name().unwrap().to_str().unwrap();
    fs::write(folder.join("rlog.zst"), b"converter must not read this").unwrap();
    let server = Server::start(tmp.path(), false, None).await;
    assert_eq!(
        server
            .configure(Path::new("/share/not_a_subfolder"))
            .await
            .0,
        400
    );
    let share = PathBuf::from(format!("/share/rv_test_{}/prepared", id()));
    let (status, configured) = server.configure(&share).await;
    assert_eq!(status, 200);
    assert_eq!(configured["state"], "unavailable");
    assert_eq!(
        server
            .request("POST", &format!("/api/logs/{record}/convert"), Value::Null)
            .await,
        (200, json!({"status":"queued"}))
    );
    until(|| async { meta(&folder)["deferred"] == prepared::WAITING }).await;
    assert_eq!(meta(&folder)["error"], prepared::WAIT_MESSAGE);
    assert!(!folder.join("prepared").exists());
    assert!(!share.exists());
    assert!(
        !fs::read_dir(&folder)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(".convert-"))
    );
}

#[tokio::test]
async fn verified_move_resumes_after_rename_and_restores_local_playback() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("data");
    let share = tmp.path().join("share");
    fs::create_dir(&share).unwrap();
    let folder = seed(&root, "ready", true);
    let record = folder.file_name().unwrap().to_str().unwrap();
    let api = format!("/api/logs/{record}");
    let marker = tmp.path().join("marker");
    let crash = "prepared_generation_installed";
    let server = Server::start(&root, true, Some((crash, &marker))).await;
    assert_eq!(server.configure(&share).await.1["state"], "connected");
    server.start_move("nas").await;
    until(|| async { fs::read_to_string(&marker).is_ok_and(|s| s == crash) }).await;
    assert!(prepared::location(&meta(&folder)).unwrap().is_none());
    assert_eq!(
        fs::read(folder.join("prepared/camera.mp4")).unwrap(),
        CAMERA
    );
    let namespace = share.join(".roadviewer-prepared");
    let target = fs::read_dir(namespace.join(record))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(fs::read(target.join("camera.mp4")).unwrap(), CAMERA);
    drop(server);

    let server = Server::start(&root, true, None).await;
    fs::write(target.join("camera.mp4"), vec![0; CAMERA.len()]).unwrap();
    assert_eq!(
        server.move_and_wait("nas").await["failed"],
        1,
        "equal byte count must not hide a digest mismatch"
    );
    assert!(prepared::location(&meta(&folder)).unwrap().is_none());
    assert_eq!(
        fs::read(folder.join("prepared/camera.mp4")).unwrap(),
        CAMERA
    );
    fs::write(target.join("camera.mp4"), CAMERA).unwrap();

    let moved = server.move_and_wait("nas").await;
    assert_eq!(moved["moved"], 1, "{moved}");
    let location = prepared::location(&meta(&folder)).unwrap().unwrap();
    assert_eq!(location.share, share);
    assert_eq!(
        target.file_name().unwrap().to_str().unwrap(),
        location.generation
    );
    assert!(!folder.join("prepared").exists());
    assert_eq!(
        server.request("GET", SETTINGS, Value::Null).await.1["nas_bytes"],
        prepared::total(&meta(&folder))
    );
    let (_, logs) = server.request("GET", "/api/logs", Value::Null).await;
    assert_eq!(logs["logs"][0]["thumbnail"], true);

    let playback = server
        .request("POST", &format!("{api}/playback"), Value::Null)
        .await;
    assert_eq!(playback.0, 204);
    assert_eq!(server.move_and_wait("local").await["skipped"], 1);
    let (status, data) = server
        .request("GET", &format!("{api}/data"), Value::Null)
        .await;
    assert_eq!(
        (status, data["route"].as_str()),
        (200, Some("prepared test"))
    );
    let (_, telemetry) = server
        .request("GET", &format!("{api}/telemetry"), Value::Null)
        .await;
    assert_eq!(telemetry["samples"], json!([1, 2]));
    let (_, track) = server
        .request("GET", &format!("{api}/track"), Value::Null)
        .await;
    assert_eq!(track["segments"][0]["points"].as_array().unwrap().len(), 2);
    assert_eq!(
        server
            .request("GET", &format!("{api}/thumbnail"), Value::Null)
            .await
            .0,
        200
    );

    let sentinel = namespace.join(roadviewer_hars::archive::SENTINEL);
    let owned = fs::read(&sentinel).unwrap();
    fs::write(
        &sentinel,
        br#"{"owner":"another-installation","storage_id":"foreign"}"#,
    )
    .unwrap();
    let (status, response) = server
        .request("GET", &format!("{api}/data"), Value::Null)
        .await;
    assert_eq!(status, 503);
    assert_eq!(response["error"], "prepared_storage_unavailable");
    assert_eq!(server.request("GET", "/api/logs", Value::Null).await.0, 200);
    fs::write(&sentinel, owned).unwrap();
    drop(server);

    let server = Server::start(&root, true, None).await;
    assert_eq!(server.move_and_wait("local").await["moved"], 1);
    assert!(prepared::location(&meta(&folder)).unwrap().is_none());
    assert!(!target.exists());
    assert_eq!(
        fs::read(folder.join("prepared/camera.mp4")).unwrap(),
        CAMERA
    );
    assert_eq!(
        server
            .request("GET", &format!("{api}/data"), Value::Null)
            .await
            .0,
        200
    );
}

#[tokio::test]
async fn manual_conversion_with_auto_off_resumes_on_ownership_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("data");
    let share = tmp.path().join("share");
    fs::create_dir(&share).unwrap();
    let folder = seed(&root, "unconverted", false);
    fs::write(
        root.join(".processing-settings.json"),
        br#"{"auto_convert":false}"#,
    )
    .unwrap();
    fs::write(folder.join("rlog.zst"), recording::rlog()).unwrap();
    let record = folder.file_name().unwrap().to_str().unwrap();
    let server = Server::start(&root, true, None).await;
    assert_eq!(server.configure(&share).await.1["state"], "connected");
    let marker = share
        .join(".roadviewer-prepared")
        .join(roadviewer_hars::archive::SENTINEL);
    let owned = fs::read(&marker).unwrap();
    fs::remove_file(&marker).unwrap();
    let route = format!("/api/logs/{record}/convert");
    assert_eq!(
        server.request("POST", &route, Value::Null).await,
        (200, json!({"status":"queued"}))
    );
    until(|| async { meta(&folder)["deferred"] == prepared::WAITING }).await;
    assert!(!folder.join("prepared").exists());
    fs::write(&marker, owned).unwrap();
    until(|| async {
        let m = meta(&folder);
        assert_ne!(m["status"], "error", "{m}");
        m["status"] == "ready"
    })
    .await;
    let m = meta(&folder);
    let location = prepared::location(&m).unwrap().unwrap();
    assert_eq!(m["model_frames"], recording::FRAMES);
    assert!(!folder.join("prepared").exists());
    let generation = share
        .join(".roadviewer-prepared")
        .join(record)
        .join(location.generation);
    assert!(generation.join("summary.json").is_file());
    assert_eq!(
        server
            .request("GET", &format!("/api/logs/{record}/data"), Value::Null)
            .await
            .0,
        200
    );
}
