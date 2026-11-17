//! A HARS instance with its archiver pointed at a temporary NAS folder or the loopback Drive fake.
#![allow(dead_code)]
use super::fake_drive::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use roadviewer_hars::{
    archive::{
        Archive, DriveAuth, DriveBackend, DriveClient, Factory, Host, Http, HttpConfig, NasBackend,
        Poll, Secret, WriteBackend,
        archiver::{CycleResult, drive_settings, nas_settings},
        drive_auth::TOKEN_FILE,
        index,
        settings::{ArchiveSettings, Backend},
    },
    http::{App, ingress},
};
use serde_json::{Map, Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tower::ServiceExt;

pub const SECRET: &str = "client-secret-xyz";
pub const CHUNK: usize = 256 << 10;
pub const OLD: f64 = 1.0;

pub struct Env {
    pub tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub nas: PathBuf,
    pub fake: Shared,
    pub http: Arc<Http>,
    pub app: App,
}

fn web() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web")
}

pub fn http() -> Arc<Http> {
    Arc::new(
        Http::new(HttpConfig {
            sleep: Arc::new(|_| Box::pin(async {})),
            rand: Arc::new(|| 0.5),
            timeout: Duration::from_secs(10),
            probe_timeout: Duration::from_secs(5),
            ..HttpConfig::default()
        })
        .unwrap(),
    )
}

/// Drive through the fake, the NAS as in production (with any absolute path allowed).
pub fn factory(url: &str, http: Arc<Http>) -> Factory {
    let url = url.to_owned();
    Arc::new(move |s: &ArchiveSettings, root: &Path| match s.backend {
        Backend::Gdrive => {
            let auth = Arc::new(DriveAuth::new(
                root.join(TOKEN_FILE),
                &s.gdrive_client_id,
                s.gdrive_client_secret.clone(),
                http.clone(),
                Some(&url),
                roadviewer_hars::archive::wall_clock(),
            ));
            let client = DriveClient::new(
                http.clone(),
                auth.clone(),
                Some(&url),
                Some(&format!("{url}/upload")),
            );
            let mut b = DriveBackend::new(
                drive_settings(s),
                root,
                auth,
                client,
                roadviewer_hars::archive::monotonic_clock(),
            );
            b.set_upload_chunk(CHUNK);
            WriteBackend::Drive(Arc::new(b))
        }
        Backend::Smb => WriteBackend::Nas(Arc::new(NasBackend::new(
            nas_settings(s),
            root,
            roadviewer_hars::archive::monotonic_clock(),
        ))),
    })
}

pub fn options() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("archive_gdrive_client_id".into(), json!("client-id"));
    m.insert("archive_gdrive_client_secret".into(), json!(SECRET));
    m
}

impl Env {
    pub async fn new() -> Self {
        let fake = start_fake().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("hars");
        let nas = tmp.path().join("nas");
        std::fs::create_dir_all(&nas).unwrap();
        let mut app = App::open(root.clone(), web(), false, 512).await.unwrap();
        let http = http();
        app.archive = archive(&app, &fake.url, http.clone());
        Self {
            tmp,
            root,
            nas,
            fake,
            http,
            app,
        }
    }

    /// A process restart as far as the archiver goes: new archiver state and new backend instances, the same
    /// data folder, share and Drive.
    pub fn restart(&mut self) {
        self.app.archive = archive(&self.app, &self.fake.url, self.http.clone());
    }

    pub fn a(&self) -> &Arc<Archive> {
        &self.app.archive
    }

    pub async fn req(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        };
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("x-roadviewer-request", "1")
            .body(Body::from(bytes))
            .unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            ingress(self.app.clone()).oneshot(request),
        )
        .await
        .expect("HTTP handler deadlock")
        .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    pub async fn configure(&self, body: Value) -> Value {
        let (status, out) = self.req("POST", "/api/settings/archive", body).await;
        assert_eq!(status, StatusCode::OK, "{out}");
        out
    }

    pub async fn use_nas(&self, extra: Value) {
        let mut body = json!({"archive_enabled": true, "archive_backend": "smb",
            "archive_path": self.nas.to_string_lossy(), "archive_batch": 64});
        merge(&mut body, extra);
        self.configure(body).await;
    }

    /// Signs in through the fake's device flow; the token file is what every backend instance reads.
    pub async fn connect_drive(&self) {
        let auth = DriveAuth::new(
            self.root.join(TOKEN_FILE),
            "client-id",
            Secret(SECRET.into()),
            self.http.clone(),
            Some(&self.fake.url),
            roadviewer_hars::archive::wall_clock(),
        );
        auth.start_device_flow(None).await.unwrap();
        assert_eq!(auth.poll_once().await.unwrap(), Poll::Granted);
    }

    pub async fn use_drive(&self, extra: Value) {
        self.connect_drive().await;
        let mut body =
            json!({"archive_enabled": true, "archive_backend": "gdrive", "archive_batch": 64});
        merge(&mut body, extra);
        self.configure(body).await;
        // A rebuilt backend reads the token written above.
        self.a()
            .set_factory(factory(&self.fake.url, self.http.clone()));
    }

    pub async fn cycle(&self) -> CycleResult {
        tokio::time::timeout(Duration::from_secs(60), self.a().run_cycle())
            .await
            .expect("archive cycle deadlock")
    }

    pub async fn recover(&self) {
        tokio::time::timeout(Duration::from_secs(60), self.a().recover_startup())
            .await
            .expect("recovery deadlock");
    }

    /// A ready recording with `files` (kind, bytes), uploaded at `uploaded`, as an upload registers it.
    pub async fn record(&self, c: char, files: &[(&str, Vec<u8>)], uploaded: f64) -> String {
        let id = c.to_string().repeat(32);
        let folder = self.root.join(&id);
        std::fs::create_dir_all(&folder).unwrap();
        let mut names = Map::new();
        let mut hashes = Map::new();
        for (kind, data) in files {
            let path = folder.join(kind);
            std::fs::write(&path, data).unwrap();
            let m = std::fs::metadata(&path).unwrap();
            let mtime = m
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
            names.insert(
                (*kind).into(),
                json!(format!(
                    "2026-01-0{}--10-00-00--0--{kind}",
                    (c as u8 % 9) + 1
                )),
            );
            hashes.insert(
                (*kind).into(),
                json!({"size": data.len(), "mtime_ns": mtime, "sha256": sha_hex(data)}),
            );
        }
        self.app
            .store
            .save_meta(json!({"id": id, "name": format!("rec {c}"), "uploaded": uploaded, "status": "ready",
                "files": names, "content_hashes": hashes, "video": true, "error": null}))
            .await
            .unwrap();
        id
    }

    pub async fn meta(&self, id: &str) -> Value {
        self.app.store.meta(id).await.unwrap()
    }
    pub async fn set(&self, id: &str, f: impl FnOnce(&mut Value)) {
        let mut m = self.meta(id).await;
        f(&mut m);
        self.app.store.save_meta(m).await.unwrap();
    }
    pub async fn loc(&self, id: &str, kind: &str) -> String {
        index::location(&self.meta(id).await, kind)
    }
    pub fn local(&self, id: &str, kind: &str) -> bool {
        self.root.join(id).join(kind).is_file()
    }
    pub fn on_nas(&self, id: &str, kind: &str) -> Option<Vec<u8>> {
        std::fs::read(self.nas.join(id).join(kind)).ok()
    }
    /// Media files on the fake (not folders), by name.
    pub fn drive_files(&self) -> Vec<(String, FFile)> {
        self.fake
            .st()
            .files
            .iter()
            .filter(|(_, f)| f.mime != FOLDER)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    pub fn drive_file(&self, id: &str, kind: &str) -> Option<(String, FFile)> {
        self.drive_files().into_iter().find(|(_, f)| {
            f.props.get("rv_id").map(String::as_str) == Some(id)
                && f.props.get("rv_kind").map(String::as_str) == Some(kind)
        })
    }
}

pub fn archive(app: &App, url: &str, http: Arc<Http>) -> Arc<Archive> {
    Archive::open(
        Host {
            store: app.store.clone(),
            uploads: app.uploads.clone(),
            jobs: app.jobs.clone(),
        },
        options(),
        true,
        Some(factory(url, http)),
    )
}

pub fn merge(a: &mut Value, b: Value) {
    if let (Value::Object(a), Value::Object(b)) = (a, b) {
        a.extend(b);
    }
}

pub fn data(size: usize, seed: u64) -> Vec<u8> {
    bytes_of(size, seed)
}
