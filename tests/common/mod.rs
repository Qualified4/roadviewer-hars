//! Shared fault-test harness: raw loopback HTTP, RV1 signing and data-root invariants.
#![allow(dead_code)]
use hmac::{Hmac, Mac};
use roadviewer_hars::{
    auth::random_token,
    http::{App, device, ingress},
    storage,
    store::{directory_size, now, sha, valid_id},
};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub const DEADLINE: Duration = Duration::from_secs(30);

pub fn web() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("reference/python/roadviewer/app/web")
}

pub struct Server {
    pub app: App,
    pub ui: SocketAddr,
    pub device: SocketAddr,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
pub async fn open(root: &Path) -> App {
    let start = tokio::time::Instant::now();
    loop {
        // A previous in-process instance releases the data lock once its last connection task ends.
        match App::open(root.to_path_buf(), web(), false, 512).await {
            Ok(app) => return app,
            Err(e) if start.elapsed() > DEADLINE => panic!("reopen failed: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}
pub async fn serve(app: App) -> Server {
    let ui = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dev = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (ui_addr, dev_addr) = (ui.local_addr().unwrap(), dev.local_addr().unwrap());
    let a = app.clone();
    let b = app.clone();
    let tasks = vec![
        tokio::spawn(async move {
            let _ = axum::serve(
                ui,
                ingress(a).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        }),
        tokio::spawn(async move {
            let _ = axum::serve(
                dev,
                device(b).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        }),
    ];
    Server {
        app,
        ui: ui_addr,
        device: dev_addr,
        tasks,
    }
}

pub fn request_bytes(method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}
pub fn parse_response(raw: &[u8]) -> (u16, Value) {
    let text = String::from_utf8_lossy(raw);
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let body = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| &raw[i + 4..])
        .unwrap_or(b"");
    (status, serde_json::from_slice(body).unwrap_or(Value::Null))
}
pub async fn read_all(stream: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    tokio::time::timeout(DEADLINE, stream.read_to_end(&mut raw))
        .await
        .expect("response deadline")
        .ok();
    raw
}
pub async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&request_bytes(method, path, headers, body))
        .await
        .unwrap();
    parse_response(&read_all(&mut stream).await)
}
pub fn csrf() -> Vec<(&'static str, String)> {
    vec![("X-RoadViewer-Request", "1".into())]
}
pub async fn pair(ui: SocketAddr, dev: SocketAddr) -> Value {
    let (status, pairing) = http(ui, "POST", "/api/settings/devices/pairing", &csrf(), b"").await;
    assert_eq!(status, 200);
    let body =
        serde_json::to_vec(&json!({"code":pairing["code"],"metadata":{"name":"fault test"}}))
            .unwrap();
    let (status, creds) = http(dev, "POST", "/api/device/pair", &[], &body).await;
    assert_eq!(status, 201, "{creds}");
    creds
}
pub fn signed(creds: &Value, body: &[u8], nonce: &str, stamp: u64) -> Vec<(&'static str, String)> {
    let id = creds["device_id"].as_str().unwrap();
    let canonical = format!(
        "RV1\n{id}\n{stamp}\n{nonce}\nPOST\n/api/device/uploads\n{}",
        sha(body)
    );
    let mut mac =
        Hmac::<Sha256>::new_from_slice(creds["device_secret"].as_str().unwrap().as_bytes())
            .unwrap();
    mac.update(canonical.as_bytes());
    vec![
        ("Content-Type", "application/json".into()),
        ("X-RV-Device", id.into()),
        ("X-RV-Timestamp", stamp.to_string()),
        ("X-RV-Nonce", nonce.into()),
        ("X-RV-Signature", hex::encode(mac.finalize().into_bytes())),
        ("X-RV-Max-Chunk", "8388608".into()),
        ("X-RV-Checksum", "sha256-stream".into()),
    ]
}
pub async fn begin_raw(dev: SocketAddr, creds: &Value, batch: &Value) -> (u16, Value) {
    let body = serde_json::to_vec(batch).unwrap();
    let headers = signed(creds, &body, &random_token(), now() as u64);
    http(dev, "POST", "/api/device/uploads", &headers, &body).await
}
pub async fn begin(dev: SocketAddr, creds: &Value, batch: &Value) -> Value {
    let (status, session) = begin_raw(dev, creds, batch).await;
    assert!(status == 201 || status == 200, "{status} {session}");
    session
}
pub fn bearer(session: &Value) -> Vec<(&'static str, String)> {
    vec![(
        "Authorization",
        format!("Bearer {}", session["token"].as_str().unwrap()),
    )]
}
/// Synthetic segment payloads. `qcamera.ts` is not media-validated, so arbitrary bytes are accepted.
pub fn segment(seed: u8, len: usize) -> (Vec<u8>, Vec<u8>) {
    let log = (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect();
    let video = (0..len / 2 + 7)
        .map(|i| (i as u8).wrapping_add(seed))
        .collect();
    (log, video)
}
pub fn batch(batch_id: &str, number: u32, files: &[(&str, &[u8])]) -> Value {
    let files = files
        .iter()
        .map(|(kind, data)| json!({"kind":kind,"size":data.len(),"sha256":null}))
        .collect::<Vec<_>>();
    json!({"batch_id":batch_id,"segments":[{"route":"2026-11-15--12-00-00","segment":number,"files":files}]})
}
pub fn finish_body(files: &[&[u8]]) -> Vec<u8> {
    serde_json::to_vec(&json!({"sha256":files.iter().map(sha).collect::<Vec<_>>()})).unwrap()
}
pub async fn put(
    dev: SocketAddr,
    session: &Value,
    index: usize,
    offset: usize,
    data: &[u8],
) -> (u16, Value) {
    http(
        dev,
        "PUT",
        &format!(
            "/api/device/uploads/{}/files/{index}?offset={offset}",
            session["id"].as_str().unwrap()
        ),
        &bearer(session),
        data,
    )
    .await
}
pub async fn status(dev: SocketAddr, session: &Value) -> (u16, Value) {
    http(
        dev,
        "GET",
        &format!("/api/device/uploads/{}", session["id"].as_str().unwrap()),
        &bearer(session),
        b"",
    )
    .await
}
pub async fn finish(dev: SocketAddr, session: &Value, files: &[&[u8]]) -> (u16, Value) {
    let mut headers = bearer(session);
    headers.push(("Content-Type", "application/json".into()));
    http(
        dev,
        "POST",
        &format!(
            "/api/device/uploads/{}/finish",
            session["id"].as_str().unwrap()
        ),
        &headers,
        &finish_body(files),
    )
    .await
}
pub async fn upload_all(dev: SocketAddr, session: &Value, files: &[&[u8]], chunk: usize) {
    for (index, data) in files.iter().enumerate() {
        let mut offset = session["files"][index]["received"].as_u64().unwrap() as usize;
        while offset < data.len() {
            let end = (offset + chunk).min(data.len());
            let (code, body) = put(dev, session, index, offset, &data[offset..end]).await;
            assert_eq!(code, 200, "{body}");
            offset = end;
        }
    }
}
pub async fn poll<F, Fut>(what: &str, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = tokio::time::Instant::now();
    while !probe().await {
        assert!(start.elapsed() < DEADLINE, "deadline waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// What a data root must look like after a fault has been handled.
pub struct Expect<'a> {
    /// Log payloads that must be registered exactly once, with their companion files.
    pub registered: &'a [&'a [&'a [u8]]],
    /// Resumable sessions that legitimately remain.
    pub sessions: usize,
}
/// Checks disk state against the HTTP view. Returns registered record ids keyed by log SHA-256.
pub async fn invariants(
    root: &Path,
    ui: SocketAddr,
    expect: Expect<'_>,
) -> BTreeMap<String, String> {
    let mut temps = Vec::new();
    walk(root, &mut temps);
    assert!(temps.is_empty(), "temporary files left behind: {temps:?}");
    assert!(
        !root.join(storage::JOURNAL).exists(),
        "a deletion journal was left behind"
    );
    let hidden = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(storage::HIDDEN_PREFIX))
        .collect::<Vec<_>>();
    assert!(
        hidden.is_empty(),
        "hidden deletions left behind: {hidden:?}"
    );

    let uploads = root.join(".uploads");
    let mut sessions = 0;
    let mut reserved = 0u64;
    let mut journaled = BTreeSet::new();
    for entry in std::fs::read_dir(&uploads).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            valid_id(&name) && entry.file_type().unwrap().is_dir(),
            "unexpected upload entry {name}"
        );
        let path = entry.path();
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(path.join("manifest.json")).unwrap()).unwrap();
        assert!(
            path.join("reservation.json").is_file(),
            "session {name} lost its reservation"
        );
        for child in std::fs::read_dir(&path).unwrap() {
            let child = child.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                matches!(
                    child.as_str(),
                    "manifest.json" | "reservation.json" | "device.json" | "commit.json"
                ) || child
                    .parse::<usize>()
                    .is_ok_and(|i| i < manifest.as_array().unwrap().len()),
                "unexpected session file {name}/{child}"
            );
        }
        let mut total = 0;
        for (i, file) in manifest.as_array().unwrap().iter().enumerate() {
            let size = file["size"].as_u64().unwrap();
            let received = std::fs::metadata(path.join(i.to_string()))
                .map(|m| m.len())
                .unwrap_or(0);
            assert!(
                received <= size,
                "session {name} file {i} exceeds its manifest"
            );
            total += size;
        }
        let reservation: Value =
            serde_json::from_slice(&std::fs::read(path.join("reservation.json")).unwrap()).unwrap();
        let bytes = reservation["bytes"].as_u64().unwrap();
        assert_eq!(
            bytes,
            total + 65536,
            "session {name} reserves its files plus metadata"
        );
        // Python `reserved`: whatever the session folder already holds (chunks and its own JSON)
        // is counted as used, not reserved.
        reserved += bytes.saturating_sub(directory_size(&path).unwrap());
        if path.join("commit.json").is_file() {
            let plan: Value =
                serde_json::from_slice(&std::fs::read(path.join("commit.json")).unwrap()).unwrap();
            for m in plan["moves"].as_array().unwrap() {
                journaled.insert(m["id"].as_str().unwrap().to_owned());
            }
        }
        sessions += 1;
    }
    assert_eq!(sessions, expect.sessions, "resumable session count");
    let (code, storage) = http(ui, "GET", "/api/settings/storage", &[], b"").await;
    assert_eq!(code, 200);
    assert_eq!(
        storage["reserved_bytes"].as_u64().unwrap(),
        reserved,
        "reservation must equal unreceived bytes plus per-session overhead"
    );

    let mut by_log = BTreeMap::new();
    let mut disk = BTreeSet::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !valid_id(&name) || !entry.file_type().unwrap().is_dir() {
            continue;
        }
        let path = entry.path();
        if !path.join("meta.json").is_file() {
            assert!(journaled.contains(&name), "orphan recording folder {name}");
            continue;
        }
        let meta: Value =
            serde_json::from_slice(&std::fs::read(path.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["id"], name);
        for (kind, info) in meta["content_hashes"].as_object().unwrap() {
            let bytes = std::fs::read(path.join(kind)).expect("registered file missing");
            assert_eq!(
                info["sha256"].as_str().unwrap(),
                sha(&bytes),
                "{name}/{kind} hash"
            );
            assert_eq!(info["size"].as_u64().unwrap(), bytes.len() as u64);
        }
        for kind in meta["files"].as_object().unwrap().keys() {
            assert!(path.join(kind).is_file(), "{name}/{kind} listed but absent");
        }
        let log = meta["content_hashes"]["rlog.zst"]["sha256"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            by_log.insert(log, name.clone()).is_none(),
            "the same log was registered twice"
        );
        disk.insert(name);
    }
    for files in expect.registered {
        let record = by_log
            .get(&sha(files[0]))
            .expect("expected recording is not registered");
        let meta: Value =
            serde_json::from_slice(&std::fs::read(root.join(record).join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(
            meta["content_hashes"].as_object().unwrap().len(),
            files.len()
        );
        for data in files.iter() {
            let digest = sha(data);
            assert!(
                meta["content_hashes"]
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|v| v["sha256"] == digest),
                "registered recording is missing an uploaded file"
            );
        }
    }
    assert_eq!(
        by_log.len(),
        expect.registered.len(),
        "unexpected registrations"
    );

    let (code, list) = http(ui, "GET", "/api/logs", &[], b"").await;
    assert_eq!(code, 200);
    let ids = list["logs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let unique = ids.iter().cloned().collect::<BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        ids.len(),
        "duplicate rows in the library list"
    );
    assert_eq!(unique, disk, "library list disagrees with disk");
    by_log
}
fn walk(path: &Path, temps: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".tmp") || name.ends_with(".creating") || name.ends_with(".removing") {
            temps.push(entry.path());
        }
        if entry.file_type().unwrap().is_dir() {
            walk(&entry.path(), temps);
        }
    }
}
