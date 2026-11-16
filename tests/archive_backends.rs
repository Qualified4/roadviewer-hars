//! Archive backends against a loopback fake of Google's OAuth device flow and Drive v3 (a port of the Python
//! `tests/fake_drive.py`) and temporary NAS folders. Nothing here reaches Google or a real share.
use roadviewer_hars::archive::{
    self, ArchiveRead, ArchiveWrite, BackendError, BackendKind, DriveAuth, DriveBackend,
    DriveClient, DriveConnection, DriveLegacy, DriveSettings, ErrorCode, Http, HttpConfig,
    NasBackend, NasLegacy, NasSettings, Poll, Probe, RemoteRef, RemoteStat, Secret, SessionState,
    State as Avail, UploadResume, Usage, Verdict,
    drive_auth::TOKEN_FILE,
    http_client::{self, Request},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const SECRET: &str = "client-secret-xyz";
const C: usize = 256 << 10;
const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KIND: &str = "fcamera.mp4";

mod common;
use common::fake_drive::*;

struct Env {
    fake: Shared,
    _tmp: tempfile::TempDir,
    root: PathBuf,
    sleeps: Arc<Mutex<Vec<f64>>>,
    http: Arc<Http>,
    wall: Arc<Mutex<f64>>,
    mono: Arc<Mutex<f64>>,
}

async fn env() -> Env {
    let fake = start_fake().await;
    let tmp = tempfile::tempdir().unwrap();
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    let recorded = sleeps.clone();
    let http = Arc::new(
        Http::new(HttpConfig {
            sleep: Arc::new(move |d: Duration| {
                recorded.lock().unwrap().push(d.as_secs_f64());
                Box::pin(async {})
            }),
            rand: Arc::new(|| 0.5),
            timeout: Duration::from_secs(10),
            probe_timeout: Duration::from_secs(5),
            ..HttpConfig::default()
        })
        .unwrap(),
    );
    let root = tmp.path().join("hars");
    std::fs::create_dir(&root).unwrap();
    Env {
        fake,
        root,
        _tmp: tmp,
        sleeps,
        http,
        wall: Arc::new(Mutex::new(1000.0)),
        mono: Arc::new(Mutex::new(0.0)),
    }
}

impl Env {
    fn auth_at(&self, path: &Path) -> Arc<DriveAuth> {
        Arc::new(DriveAuth::new(
            path,
            "client-id",
            Secret(SECRET.into()),
            self.http.clone(),
            Some(&self.fake.url),
            clock_of(&self.wall),
        ))
    }
    fn auth(&self) -> Arc<DriveAuth> {
        self.auth_at(&self.root.join(TOKEN_FILE))
    }
    async fn connected(&self) -> Arc<DriveAuth> {
        let auth = self.auth();
        auth.start_device_flow(None).await.unwrap();
        assert_eq!(auth.poll_once().await.unwrap(), Poll::Granted);
        auth
    }
    fn client(&self, auth: &Arc<DriveAuth>) -> DriveClient {
        DriveClient::new(
            self.http.clone(),
            auth.clone(),
            Some(&self.fake.url),
            Some(&format!("{}/upload", self.fake.url)),
        )
    }
    fn backend(&self, auth: &Arc<DriveAuth>) -> DriveBackend {
        let mut b = DriveBackend::new(
            DriveSettings::default(),
            &self.root,
            auth.clone(),
            self.client(auth),
            clock_of(&self.mono),
        );
        b.set_upload_chunk(C);
        b
    }
    fn sleeps(&self) -> Vec<f64> {
        std::mem::take(&mut *self.sleeps.lock().unwrap())
    }
    fn local_id(&self) -> String {
        let v: Value =
            serde_json::from_slice(&std::fs::read(self.root.join(".archive.json")).unwrap())
                .unwrap();
        v["gdrive_storage_id"].as_str().unwrap().to_owned()
    }
    fn media(&self, size: usize, seed: u64) -> (PathBuf, Vec<u8>, String, String) {
        let data = bytes_of(size, seed);
        let path = self.root.join(format!("media-{size}-{seed}"));
        std::fs::write(&path, &data).unwrap();
        let (sha, md5) = (sha_hex(&data), md5_hex(&data));
        (path, data, sha, md5)
    }
}

async fn ready(e: &Env) -> (Arc<DriveAuth>, DriveBackend) {
    let auth = e.connected().await;
    let b = e.backend(&auth);
    b.activate().await;
    assert_eq!(b.probe(false).await, Probe::connected());
    (auth, b)
}

async fn upload(
    e: &Env,
    b: &DriveBackend,
    size: usize,
    kind: &str,
) -> (RemoteRef, Vec<u8>, String, String) {
    let (path, data, sha, md5) = e.media(size, size as u64);
    let r = b
        .upload(
            &path,
            ID,
            kind,
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap();
    (r, data, sha, md5)
}

type Failure = (ErrorCode, Option<String>);
fn err(code: ErrorCode, reason: &str) -> Failure {
    (code, Some(reason.to_owned()))
}
fn code(e: BackendError) -> Failure {
    (e.code, e.reason)
}

// --- OAuth and the token file ---

#[tokio::test]
async fn oauth_device_code_exchange_and_token_file() {
    let e = env().await;
    e.fake
        .st()
        .device_poll
        .extend(["authorization_pending", "slow_down"]);
    let auth = e.auth();
    assert!(!auth.connected());
    let flow = auth.start_device_flow(None).await.unwrap();
    assert_eq!(
        (flow.user_code.as_str(), flow.expires_at, flow.interval),
        ("ABCD-EFGH", 2800.0, 5)
    );
    assert_eq!(auth.poll_once().await.unwrap(), Poll::Pending);
    assert_eq!(auth.poll_once().await.unwrap(), Poll::Pending);
    assert_eq!(auth.interval(), Some(10), "slow_down adds 5 seconds");
    assert_eq!(auth.poll_once().await.unwrap(), Poll::Granted);
    let token = auth.access_token(false).await.unwrap();
    assert!(e.fake.st().access.contains(&token));

    let path = e.root.join(TOKEN_FILE);
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let keys: HashSet<&str> = saved
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, archive::drive_auth::TOKEN_KEYS.into_iter().collect());
    assert_eq!(saved["scope"], archive::drive_auth::DRIVE_SCOPE);
    assert_eq!(
        (saved["folder_id"].clone(), saved["storage_id"].clone()),
        (Value::Null, Value::Null)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let leftovers: Vec<_> = std::fs::read_dir(&e.root)
        .unwrap()
        .map(|d| d.unwrap().file_name())
        .filter(|n| n.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
    let again = e.auth();
    assert!(again.connected());
    assert_eq!(again.access_token(false).await.unwrap(), token);
    assert!(
        e.sleeps().is_empty(),
        "slow_down is the poller's business, not a retry"
    );

    // Refused and expired codes.
    e.fake.st().device_poll.push_back("access_denied");
    let other = e.auth_at(&e.root.join("other.json"));
    other.start_device_flow(None).await.unwrap();
    assert_eq!(other.poll_once().await.unwrap(), Poll::Denied);
    assert_eq!(other.poll_once().await.unwrap(), Poll::Expired);
    other.start_device_flow(None).await.unwrap();
    *e.wall.lock().unwrap() += 1800.0;
    let mark = e.fake.mark();
    assert_eq!(other.poll_once().await.unwrap(), Poll::Expired);
    assert_eq!(
        e.fake.mark(),
        mark,
        "past expires_at the poll answers locally"
    );
    // A client Google does not know.
    let nobody = Arc::new(DriveAuth::new(
        e.root.join("x.json"),
        "",
        Secret(SECRET.into()),
        e.http.clone(),
        Some(&e.fake.url),
        clock_of(&e.wall),
    ));
    assert_eq!(
        code(nobody.start_device_flow(None).await.unwrap_err()),
        err(ErrorCode::AuthExpired, "invalid_client")
    );
}

#[tokio::test]
async fn token_refresh_and_invalid_grant() {
    let e = env().await;
    let auth = e.connected().await;
    let first = auth.access_token(false).await.unwrap();
    *e.wall.lock().unwrap() += 3599.0 - 61.0;
    assert_eq!(auth.access_token(false).await.unwrap(), first);
    *e.wall.lock().unwrap() += 2.0;
    let second = auth.access_token(false).await.unwrap();
    assert_ne!(second, first, "refreshed inside the last minute");
    let saved: Value =
        serde_json::from_slice(&std::fs::read(e.root.join(TOKEN_FILE)).unwrap()).unwrap();
    assert_eq!(saved["access_token"], second.as_str());
    assert_eq!(
        saved["expires_at"].as_f64(),
        Some(*e.wall.lock().unwrap() + 3599.0)
    );

    // A 401 from Drive is answered with one refresh and one resend.
    let client = e.client(&auth);
    e.fake.st().access.clear();
    let mark = e.fake.mark();
    assert!(client.about().await.is_ok());
    assert_eq!(e.fake.count(mark, "POST", "/token"), 1);

    // Revoked at Google: auth_expired, not retryable, and the grant is left for the user to reconnect.
    e.fake.st().refresh.clear();
    *e.wall.lock().unwrap() += 3600.0;
    let failed = auth.access_token(false).await.unwrap_err();
    assert_eq!(
        (failed.code, failed.retryable, failed.reason()),
        (ErrorCode::AuthExpired, false, Some("invalid_grant"))
    );
    e.fake
        .fail_next("/token", 401, Some("invalid_client"), 1, &[]);
    assert_eq!(
        auth.access_token(false).await.unwrap_err().code,
        ErrorCode::AuthExpired
    );
    e.fake.fail_next("/token", 503, None, 5, &[]);
    let outage = auth.access_token(false).await.unwrap_err();
    assert_eq!(
        (outage.code, outage.retryable),
        (ErrorCode::Unavailable, true)
    );
    assert!(auth.connected());
    let never = e.auth_at(&e.root.join("never.json"));
    assert_eq!(
        code(never.access_token(false).await.unwrap_err()),
        err(ErrorCode::AuthExpired, "not_connected")
    );
}

#[tokio::test]
async fn python_token_file_fixture_round_trips() {
    let e = env().await;
    // Exactly what Python's json.dump writes (TOKEN_KEYS order, ", "/": " separators, a float expires_at).
    let fixture = r#"{"refresh_token": "rt-py-secretrefresh", "access_token": "at-py-secretaccess", "expires_at": 4600.25, "scope": "https://www.googleapis.com/auth/drive.file", "folder_id": "1AbCfolder", "storage_id": "0123456789abcdef0123456789abcdef"}"#;
    let path = e.root.join(TOKEN_FILE);
    std::fs::write(&path, fixture).unwrap();
    let auth = e.auth();
    assert!(auth.connected());
    assert_eq!(auth.folder_id().as_deref(), Some("1AbCfolder"));
    assert_eq!(
        auth.storage_id().as_deref(),
        Some("0123456789abcdef0123456789abcdef")
    );
    assert_eq!(
        auth.access_token(false).await.unwrap(),
        "at-py-secretaccess",
        "unexpired token used as is"
    );
    auth.remember(None, Some("0123456789abcdef0123456789abcdef"))
        .unwrap();
    let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        written,
        serde_json::from_str::<Value>(fixture).unwrap(),
        "every key and value survives a rewrite"
    );
    // A damaged file and a non-object read as not connected.
    for bad in ["{not json", "[]", ""] {
        std::fs::write(&path, bad).unwrap();
        assert!(!e.auth().connected());
    }
}

// --- The HTTP client's retry policy ---

#[tokio::test]
async fn retry_backoff_and_limits() {
    let e = env().await;
    let auth = e.connected().await;
    let client = e.client(&auth);
    e.fake.fail_next("/about", 429, None, 2, &[]);
    e.fake
        .fail_next("/about", 403, Some("userRateLimitExceeded"), 1, &[]);
    e.fake.fail_next("/about", 503, None, 1, &[]);
    assert_eq!(
        client.about().await.unwrap(),
        json!({"usage": "0", "usageInDrive": "0"})
    );
    assert_eq!(e.sleeps(), vec![1.5, 2.5, 4.5, 8.5], "min(2**n + rand, 64)");

    e.fake
        .fail_next("/about", 403, Some("rateLimitExceeded"), 5, &[]);
    let mark = e.fake.mark();
    let limited = client.about().await.unwrap_err();
    assert_eq!(
        (limited.code, limited.retryable),
        (ErrorCode::RateLimited, true)
    );
    assert_eq!(
        (
            e.sleeps().len(),
            e.fake.count(mark, "GET", "/drive/v3/about")
        ),
        (4, 5)
    );

    e.fake.fail_next("/about", 502, None, 5, &[]);
    assert_eq!(
        client.about().await.unwrap_err().code,
        ErrorCode::Unavailable
    );
    e.sleeps();
    e.fake
        .fail_next("/about", 403, Some("storageQuotaExceeded"), 1, &[]);
    let full = client.about().await.unwrap_err();
    assert_eq!(
        (full.code, full.retryable),
        (ErrorCode::QuotaExceeded, false)
    );
    assert!(e.sleeps().is_empty(), "out of storage is final");

    // Capped at 64 seconds.
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    let rec = sleeps.clone();
    let long = Http::new(HttpConfig {
        attempts: 9,
        rand: Arc::new(|| 0.25),
        sleep: Arc::new(move |d: Duration| {
            rec.lock().unwrap().push(d.as_secs_f64());
            Box::pin(async {})
        }),
        ..HttpConfig::default()
    })
    .unwrap();
    e.fake.fail_next("/about", 429, None, 9, &[]);
    let token = auth.access_token(false).await.unwrap();
    let req = Request::new(
        reqwest::Method::GET,
        format!("{}/drive/v3/about", e.fake.url),
    )
    .header("authorization", format!("Bearer {token}"));
    assert!(long.request(req).await.is_err());
    assert_eq!(
        *sleeps.lock().unwrap(),
        vec![1.25, 2.25, 4.25, 8.25, 16.25, 32.25, 64.0, 64.0]
    );

    // A redirect is an answer, never followed.
    e.fake.fail_next(
        "/drive/v3/files/f1",
        302,
        None,
        1,
        &[("location", &format!("{}/drive/v3/about", e.fake.url))],
    );
    let mark = e.fake.mark();
    let req = Request::new(
        reqwest::Method::GET,
        format!("{}/drive/v3/files/f1", e.fake.url),
    )
    .header("authorization", format!("Bearer {token}"));
    assert_eq!(e.http.request(req).await.unwrap().status, 302);
    assert_eq!(e.fake.count(mark, "GET", "/drive/v3/about"), 0);

    // files.create is never resent blindly.
    e.fake
        .fail_next("/drive/v3/files?fields=id", 503, None, 1, &[]);
    let mark = e.fake.mark();
    assert!(client.create_folder("x", json!({})).await.is_err());
    assert_eq!(
        (e.fake.count(mark, "POST", "/drive/v3/files"), e.sleeps()),
        (1, vec![])
    );
}

// --- Folder setup and the write scope ---

#[tokio::test]
async fn folder_is_created_once_and_reused() {
    let e = env().await;
    let auth = e.connected().await;
    let b = e.backend(&auth);
    assert_eq!(b.probe(false).await, Probe::unavailable("folder_missing"));
    b.activate().await;
    b.activate().await;
    let folders = e.fake.folders();
    assert_eq!(folders.len(), 1);
    let (folder, f) = folders.into_iter().next().unwrap();
    let sid = e.local_id();
    assert_eq!(f.name, "Road Viewer HARS");
    assert_ne!(f.name, archive::PYTHON_DEFAULT_FOLDER);
    assert!(f.parents.is_empty());
    assert_eq!(
        f.props,
        BTreeMap::from([
            ("hars_owner".into(), "roadviewer-hars".into()),
            ("rv_storage_id".into(), sid.clone())
        ])
    );
    assert_eq!(
        (auth.folder_id(), auth.storage_id()),
        (Some(folder.clone()), Some(sid.clone()))
    );
    assert_eq!(b.probe(false).await, Probe::connected());

    // Restart: reloaded from disk, nothing created.
    e.backend(&e.auth()).activate().await;
    assert_eq!(e.fake.folders().len(), 1);
    // Disconnect forgets the binding; reconnecting finds the folder by storage id and owner mark.
    auth.disconnect().await;
    let again = e.connected().await;
    assert_eq!(again.folder_id(), None);
    let b = e.backend(&again);
    b.activate().await;
    assert_eq!(
        (e.fake.folders().len(), again.folder_id()),
        (1, Some(folder.clone()))
    );
    assert_eq!(b.probe(false).await, Probe::connected());

    // The folder trashed, then answered as an outage: never reported as connected.
    e.fake.st().files.get_mut(&folder).unwrap().trashed = true;
    *e.mono.lock().unwrap() += 10.0;
    assert_eq!(b.probe(false).await, Probe::unavailable("folder_trashed"));
    e.fake.st().files.get_mut(&folder).unwrap().trashed = false;
    b.invalidate();
    e.fake
        .fail_next(&format!("/drive/v3/files/{folder}"), 503, None, 5, &[]);
    assert_eq!(b.probe(false).await, Probe::unavailable("network"));
}

#[tokio::test]
async fn python_folder_is_never_adopted_as_write_root() {
    let e = env().await;
    let auth = e.connected().await;
    // Python's folder carrying the very storage id this token file names (a copied token file).
    e.fake.put(
        "pyfolder",
        FFile::folder(
            "Road Viewer",
            &[("rv_storage_id", "sid1")],
            "2026-01-01T00:00:00.000Z",
        ),
    );
    auth.remember(None, Some("sid1")).unwrap();
    let b = e.backend(&auth);
    b.activate().await;
    let bound = auth.folder_id().unwrap();
    assert_ne!(bound, "pyfolder");
    assert_eq!(
        e.fake
            .file(&bound)
            .unwrap()
            .props
            .get("hars_owner")
            .map(String::as_str),
        Some("roadviewer-hars")
    );
    assert_eq!(
        e.fake.file("pyfolder").unwrap().props,
        BTreeMap::from([("rv_storage_id".into(), "sid1".into())])
    );

    // A binding pointing at Python's folder: probe says not_owned and every write is refused before it starts.
    auth.remember(Some("pyfolder"), None).unwrap();
    b.invalidate();
    assert_eq!(b.probe(false).await, Probe::unavailable("not_owned"));
    let (path, _, sha, md5) = e.media(1000, 1);
    let mark = e.fake.mark();
    let refused = b
        .upload(
            &path,
            ID,
            KIND,
            Some(&sha),
            Some(&md5),
            1000,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap_err();
    assert!(refused.is_foreign(), "{refused}");
    assert_eq!(e.fake.count(mark, "POST", "/upload"), 0);
    assert_eq!(e.fake.count(mark, "PUT", ""), 0);
}

// --- Resumable uploads ---

#[tokio::test]
async fn resumable_upload_completes() {
    let e = env().await;
    let (auth, b) = ready(&e).await;
    let size = 2 * C + C / 2;
    let (path, data, sha, md5) = e.media(size, 7);
    let (mut progress, mut resume) = (Vec::new(), UploadResume::default());
    let mark = e.fake.mark();
    let r = b
        .upload(
            &path,
            ID,
            KIND,
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut resume,
            &mut |n, _: &UploadResume| {
                progress.push(n);
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(progress, vec![0, C as u64, 2 * C as u64, size as u64]);
    assert_eq!(
        e.fake.ranges(mark),
        vec![
            format!("bytes 0-{}/{size}", C - 1),
            format!("bytes {C}-{}/{size}", 2 * C - 1),
            format!("bytes {}-{}/{size}", 2 * C, size - 1)
        ]
    );
    assert_eq!(
        r,
        RemoteRef::new(
            BackendKind::Gdrive,
            r.file_id.clone().unwrap(),
            size as u64,
            Some(sha.clone()),
            Some(md5.clone())
        )
    );
    assert!(
        r.completed && archive::completion_matches(&r, Some(&sha), Some(size as u64), Some(&md5))
    );
    let f = e.fake.file(r.file_id.as_deref().unwrap()).unwrap();
    assert_eq!(f.content, data);
    assert_eq!(
        (f.name.as_str(), f.parents.clone()),
        (
            format!("{ID}--{KIND}").as_str(),
            vec![auth.folder_id().unwrap()]
        )
    );
    assert_eq!(
        f.props,
        BTreeMap::from([
            ("hars_owner".into(), "roadviewer-hars".into()),
            ("rv_id".into(), ID.into()),
            ("rv_kind".into(), KIND.into()),
            ("rv_sha256".into(), sha.clone()),
            ("rv_storage_id".into(), e.local_id()),
        ])
    );
    assert_eq!(resume.bytes_sent, size as u64);
    assert!(
        resume
            .session
            .as_ref()
            .unwrap()
            .expose()
            .contains("upload_id=")
    );
    assert_eq!(
        b.verify(&r, Some(&sha), Some(size as u64), None)
            .await
            .unwrap(),
        Verdict::Ok
    );
    assert_eq!(
        b.verify(&r, Some(&"0".repeat(64)), Some(size as u64), None)
            .await
            .unwrap(),
        Verdict::Mismatch
    );
    assert_eq!(
        b.verify(&r, Some(&sha), Some(size as u64 + 1), None)
            .await
            .unwrap(),
        Verdict::Mismatch
    );
    assert_eq!(
        b.stat(&r).await.unwrap(),
        Some(RemoteStat {
            exists: true,
            bytes: size as u64,
            sha256: Some(sha.clone()),
            md5: Some(md5.clone()),
            trashed: false
        })
    );
    assert_eq!(b.find(ID, KIND).await.unwrap(), vec![r.clone()]);
    assert!(e.sleeps().is_empty());

    // The local file is not what the caller hashed: the last chunk never goes out, nothing is created.
    let mark = e.fake.mark();
    let changed = b
        .upload(
            &path,
            ID,
            "ecamera.mp4",
            Some(&"0".repeat(64)),
            None,
            size as u64,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap_err();
    assert_eq!(code(changed), err(ErrorCode::Mismatch, "local_changed"));
    assert_eq!(e.fake.ranges(mark).len(), 2);
    assert!(b.find(ID, "ecamera.mp4").await.unwrap().is_empty());

    // An empty file is one status request.
    let (empty, _, sha0, _) = e.media(0, 1);
    let mark = e.fake.mark();
    let r0 = b
        .upload(
            &empty,
            ID,
            "rlog.zst",
            Some(&sha0),
            None,
            0,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap();
    assert_eq!(
        (r0.bytes, e.fake.ranges(mark)),
        (0, vec!["bytes */0".to_owned()])
    );
}

#[tokio::test]
async fn upload_resumes_after_connection_drop_from_offset() {
    let e = env().await;
    let (_auth, b) = ready(&e).await;
    let size = 2 * C + 100;
    let (path, data, sha, md5) = e.media(size, 3);
    e.fake.st().drop_chunk_after_store = 1;
    let mark = e.fake.mark();
    let r = b
        .upload(
            &path,
            ID,
            KIND,
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap();
    assert_eq!(
        e.fake.ranges(mark),
        vec![
            format!("bytes 0-{}/{size}", C - 1),
            format!("bytes */{size}"),
            format!("bytes {C}-{}/{size}", 2 * C - 1),
            format!("bytes {}-{}/{size}", 2 * C, size - 1)
        ],
        "the dropped chunk had arrived: the session is asked and the next chunk follows"
    );
    assert_eq!(
        e.fake.file(r.file_id.as_deref().unwrap()).unwrap().content,
        data
    );
    assert!(e.sleeps().is_empty());

    // A chunk answered 503 is not resent blindly either: asked first, then sent again after one backoff.
    e.fake.fail_next("upload_id", 503, None, 1, &[]);
    let mark = e.fake.mark();
    let (path, data, sha, md5) = e.media(C + C / 2, 4);
    let r = b
        .upload(
            &path,
            ID,
            "ecamera.mp4",
            Some(&sha),
            Some(&md5),
            (C + C / 2) as u64,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap();
    let n = C + C / 2;
    assert_eq!(
        e.fake.ranges(mark),
        vec![
            format!("bytes 0-{}/{n}", C - 1),
            format!("bytes */{n}"),
            format!("bytes 0-{}/{n}", C - 1),
            format!("bytes {C}-{}/{n}", n - 1)
        ]
    );
    assert_eq!(e.sleeps(), vec![1.5]);
    assert_eq!(
        e.fake.file(r.file_id.as_deref().unwrap()).unwrap().content,
        data
    );

    // The completion answer lost: the session is asked instead of the last chunk being sent again.
    e.fake.st().lose_completion = true;
    let mark = e.fake.mark();
    let (path, small, sha, md5) = e.media(1000, 5);
    let r = b
        .upload(
            &path,
            ID,
            "camera.mp4",
            Some(&sha),
            Some(&md5),
            1000,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap();
    assert_eq!(
        e.fake.ranges(mark),
        vec!["bytes 0-999/1000".to_owned(), "bytes */1000".to_owned()]
    );
    assert_eq!(
        e.fake.file(r.file_id.as_deref().unwrap()).unwrap().content,
        small
    );
    assert!(
        r.completed,
        "this attempt sent the last chunk (Python: the status answer after a lost completion)"
    );
}

#[tokio::test]
async fn persisted_session_resumes_without_resending() {
    let e = env().await;
    let (_auth, b) = ready(&e).await;
    let size = 3 * C + 100;
    let (path, data, sha, md5) = e.media(size, 9);
    let mut resume = UploadResume::default();
    let stop = b
        .upload(
            &path,
            ID,
            KIND,
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut resume,
            &mut |n, _: &UploadResume| {
                if n == 2 * C as u64 {
                    Err(BackendError::new(ErrorCode::Io, "stopped"))
                } else {
                    Ok(())
                }
            },
        )
        .await
        .unwrap_err();
    assert_eq!(stop.reason(), Some("stopped"));
    assert_eq!(resume.bytes_sent, 2 * C as u64);
    // The index persists the resume as JSON; it comes back as the same record.
    let persisted: UploadResume =
        serde_json::from_value(serde_json::to_value(&resume).unwrap()).unwrap();
    assert_eq!(persisted, resume);
    assert_eq!(
        b.session_status(&persisted).await.unwrap(),
        SessionState::Incomplete
    );
    let mark = e.fake.mark();
    let mut progress = Vec::new();
    let r = b
        .upload(
            &path,
            ID,
            KIND,
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut persisted.clone(),
            &mut |n, _: &UploadResume| {
                progress.push(n);
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(
        e.fake.ranges(mark),
        vec![
            format!("bytes */{size}"),
            format!("bytes {}-{}/{size}", 2 * C, 3 * C - 1),
            format!("bytes {}-{}/{size}", 3 * C, size - 1)
        ]
    );
    assert_eq!(e.fake.count(mark, "POST", ""), 0, "same session");
    assert_eq!(progress, vec![3 * C as u64, size as u64]);
    assert_eq!(
        e.fake.file(r.file_id.as_deref().unwrap()).unwrap().content,
        data
    );
    // Completed while we were away: adopted with one status request.
    let mark = e.fake.mark();
    let again = b
        .upload(
            &path,
            ID,
            KIND,
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut persisted.clone(),
            &mut no_progress(),
        )
        .await
        .unwrap();
    assert_eq!(
        (again.file_id.clone(), e.fake.ranges(mark)),
        (r.file_id.clone(), vec![format!("bytes */{size}")])
    );
    assert_eq!(
        b.session_status(&persisted).await.unwrap(),
        SessionState::Done
    );
    // A record made for other content is never continued.
    let mut stale = UploadResume {
        sha256: Some("0".repeat(64)),
        ..persisted.clone()
    };
    let mark = e.fake.mark();
    b.upload(
        &path,
        ID,
        "qcamera.ts",
        Some(&sha),
        Some(&md5),
        size as u64,
        &mut stale,
        &mut no_progress(),
    )
    .await
    .unwrap();
    assert_eq!(e.fake.count(mark, "POST", "/upload"), 1);
    assert_ne!(stale.session, persisted.session);
    assert_eq!(
        b.session_status(&UploadResume::default()).await.unwrap(),
        SessionState::Expired
    );
}

#[tokio::test]
async fn range_answers_that_do_not_fit_are_refused() {
    let e = env().await;
    let (_auth, b) = ready(&e).await;
    let size = 2 * C;
    let (path, data, sha, md5) = e.media(size, 11);
    // Google kept only part of a chunk: the rest is sent again from where it stopped.
    e.fake.st().partial_accept = Some(1000);
    let mark = e.fake.mark();
    let r = b
        .upload(
            &path,
            ID,
            KIND,
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap();
    assert_eq!(
        e.fake.ranges(mark),
        vec![
            format!("bytes 0-{}/{size}", C - 1),
            format!("bytes 1000-{}/{size}", C + 999),
            format!("bytes {}-{}/{size}", C + 1000, size - 1)
        ]
    );
    assert_eq!(
        e.fake.file(r.file_id.as_deref().unwrap()).unwrap().content,
        data
    );
    // A 308 claiming more than was sent stops the upload; nothing is created.
    e.fake.fail_next(
        "upload_id",
        308,
        None,
        1,
        &[("range", &format!("bytes=0-{}", size * 4))],
    );
    let failed = b
        .upload(
            &path,
            ID,
            "ecamera.mp4",
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await;
    assert_eq!(code(failed.unwrap_err()), err(ErrorCode::Io, "bad_range"));
    assert!(b.find(ID, "ecamera.mp4").await.unwrap().is_empty());
    // Going backwards after a confirmed chunk.
    let (path, _, sha, md5) = e.media(3 * C, 12);
    let fake = e.fake.clone();
    let failed = b
        .upload(
            &path,
            ID,
            "qcamera.ts",
            Some(&sha),
            Some(&md5),
            3 * C as u64,
            &mut UploadResume::default(),
            &mut |n, _: &UploadResume| {
                if n == C as u64 {
                    fake.fail_next("upload_id", 308, None, 1, &[("range", "bytes=0-5")]);
                }
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(failed), err(ErrorCode::Io, "bad_range"));
}

#[tokio::test]
async fn expired_session_restarts_from_zero() {
    let e = env().await;
    let (_auth, b) = ready(&e).await;
    let size = 2 * C + 5;
    let (path, data, sha, md5) = e.media(size, 13);
    let old = archive::SessionUri(format!(
        "{}/upload/drive/v3/files?uploadType=resumable&upload_id=gone-secretsession",
        e.fake.url
    ));
    let mut resume = UploadResume {
        session: Some(old.clone()),
        bytes_sent: C as u64,
        sha256: Some(sha.clone()),
        size: Some(size as u64),
    };
    let mark = e.fake.mark();
    let r = b
        .upload(
            &path,
            ID,
            KIND,
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut resume,
            &mut no_progress(),
        )
        .await
        .unwrap();
    assert_eq!(
        e.fake.file(r.file_id.as_deref().unwrap()).unwrap().content,
        data
    );
    assert_ne!(resume.session, Some(old));
    assert_eq!(
        e.fake.ranges(mark)[..2],
        [
            format!("bytes */{size}"),
            format!("bytes 0-{}/{size}", C - 1)
        ]
    );

    // Expiring part-way (410 here): one new session from byte 0.
    let fake = e.fake.clone();
    let mut expired = false;
    let mark = e.fake.mark();
    let r = b
        .upload(
            &path,
            ID,
            "ecamera.mp4",
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut UploadResume::default(),
            &mut |n, _: &UploadResume| {
                if n == C as u64 && !expired {
                    expired = true;
                    fake.fail_next("upload_id", 410, None, 1, &[]);
                }
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(
        e.fake.file(r.file_id.as_deref().unwrap()).unwrap().content,
        data
    );
    assert_eq!(
        e.fake
            .ranges(mark)
            .iter()
            .filter(|r| **r == format!("bytes 0-{}/{size}", C - 1))
            .count(),
        2
    );
    assert_eq!(e.fake.count(mark, "POST", "/upload"), 2);

    // Expiring every time gives up, without the session URI in the message.
    let fake = e.fake.clone();
    let gone = b
        .upload(
            &path,
            ID,
            "camera.mp4",
            Some(&sha),
            Some(&md5),
            size as u64,
            &mut UploadResume::default(),
            &mut |n, _: &UploadResume| {
                if n == C as u64 {
                    fake.st().sessions.clear();
                }
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        code(gone.clone()),
        err(ErrorCode::Unavailable, "session_expired")
    );
    assert!(!format!("{gone} {gone:?}").contains("secretsession"));
}

#[tokio::test]
async fn completion_answer_must_match_what_was_sent() {
    let e = env().await;
    let (_auth, b) = ready(&e).await;
    let (path, _, sha, md5) = e.media(5000, 21);
    e.fake.st().completion_override = Some(json!({"size": "4999"}));
    assert_eq!(
        code(
            b.upload(
                &path,
                ID,
                KIND,
                Some(&sha),
                Some(&md5),
                5000,
                &mut UploadResume::default(),
                &mut no_progress()
            )
            .await
            .unwrap_err()
        ),
        err(ErrorCode::Mismatch, "size")
    );
    // No sha256 from Drive and an md5 other than the one sent: never taken as verified from the answer.
    e.fake.st().completion_override =
        Some(json!({"sha256Checksum": null, "md5Checksum": "0".repeat(32)}));
    let r = b
        .upload(
            &path,
            ID,
            "ecamera.mp4",
            Some(&sha),
            None,
            5000,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap();
    assert_eq!(r.sent_md5.as_deref(), Some(md5.as_str()));
    assert!(!archive::completion_matches(
        &r,
        Some(&sha),
        Some(5000),
        None
    ));
    e.fake.st().completion_override = Some(json!({"sha256Checksum": null}));
    let r = b
        .upload(
            &path,
            ID,
            "camera.mp4",
            Some(&sha),
            None,
            5000,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap();
    assert!(
        archive::completion_matches(&r, Some(&sha), Some(5000), None),
        "the md5 of what was sent matches"
    );
    // verify(): sha256, then md5, else unverifiable; gone is not_found.
    let id = r.file_id.clone().unwrap();
    e.fake.st().files.get_mut(&id).unwrap().sha256 = None;
    assert_eq!(
        b.verify(&r, Some(&sha), Some(5000), Some(&md5))
            .await
            .unwrap(),
        Verdict::Ok
    );
    assert_eq!(
        b.verify(&r, Some(&sha), Some(5000), Some(&"0".repeat(32)))
            .await
            .unwrap(),
        Verdict::Mismatch
    );
    assert_eq!(
        b.verify(&r, Some(&sha), Some(5000), None).await.unwrap(),
        Verdict::Unverifiable
    );
    e.fake.st().files.remove(&id);
    assert_eq!(
        b.verify(&r, Some(&sha), Some(5000), None)
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
}

// --- Reads ---

#[tokio::test]
async fn download_whole_and_ranges() {
    let e = env().await;
    let (_auth, b) = ready(&e).await;
    let (r, data, sha, _) = upload(&e, &b, 3 * C, KIND).await;
    let mark = e.fake.mark();
    assert_eq!(
        b.open(&r, 10, Some(109))
            .await
            .unwrap()
            .read_all()
            .await
            .unwrap(),
        data[10..110]
    );
    assert_eq!(
        e.fake
            .requests(mark)
            .last()
            .unwrap()
            .headers
            .get("range")
            .map(String::as_str),
        Some("bytes=10-109")
    );
    assert_eq!(
        b.open(&r, 0, None).await.unwrap().read_all().await.unwrap(),
        data
    );
    assert!(
        !e.fake
            .requests(mark)
            .last()
            .unwrap()
            .headers
            .contains_key("range")
    );
    assert_eq!(
        b.open(&r, 2 * C as u64, None)
            .await
            .unwrap()
            .read_all()
            .await
            .unwrap(),
        data[2 * C..]
    );
    assert_eq!(
        code(b.open(&r, data.len() as u64 + 5, None).await.err().unwrap()),
        err(ErrorCode::Io, "range_not_satisfiable")
    );
    assert_eq!(b.last_state(), None, "a failed read probes again");
    assert_eq!(b.probe(false).await, Probe::connected());
    let missing = RemoteRef::new(BackendKind::Gdrive, "nope", 1, None, None);
    assert_eq!(
        b.open(&missing, 0, None).await.err().unwrap().code,
        ErrorCode::NotFound
    );
    assert!(
        b.last_state().is_some(),
        "a missing file says nothing about the connection"
    );
    e.fake.fail_next(
        &format!("/drive/v3/files/{}", r.file_id.as_deref().unwrap()),
        200,
        None,
        1,
        &[],
    );
    assert_eq!(
        code(b.open(&r, 10, Some(109)).await.err().unwrap()),
        err(ErrorCode::Io, "range_ignored")
    );
    assert_eq!(b.last_state(), None, "any other failure probes again");

    let out = e.root.join("out");
    std::fs::create_dir(&out).unwrap();
    b.download_to(&r, &out.join("fcamera.mp4"), Some(&sha))
        .await
        .unwrap();
    assert_eq!(std::fs::read(out.join("fcamera.mp4")).unwrap(), data);
    b.download_to(
        &RemoteRef::new(
            BackendKind::Gdrive,
            r.file_id.clone().unwrap(),
            r.bytes,
            None,
            None,
        ),
        &out.join("count.mp4"),
        None,
    )
    .await
    .unwrap();
    let short = RemoteRef::new(
        BackendKind::Gdrive,
        r.file_id.clone().unwrap(),
        r.bytes + 1,
        None,
        None,
    );
    assert_eq!(
        code(
            b.download_to(&short, &out.join("short.mp4"), None)
                .await
                .unwrap_err()
        ),
        err(ErrorCode::Mismatch, "no_checksum")
    );
    let mut names: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|d| d.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["count.mp4", "fcamera.mp4"]);
}

#[tokio::test]
async fn corrupted_remote_copy_fails_verification() {
    let e = env().await;
    let (_auth, b) = ready(&e).await;
    let (r, data, sha, md5) = upload(&e, &b, C + 7, KIND).await;
    let id = r.file_id.clone().unwrap();
    // Changed on Drive, Drive's checksums describing the new bytes.
    let mut bad = data.clone();
    bad[100] ^= 0xff;
    e.fake.put(
        &id,
        FFile {
            content: bad.clone(),
            ..FFile::media(&bad, &[], &[])
        },
    );
    assert_eq!(
        b.verify(&r, Some(&sha), Some(data.len() as u64), Some(&md5))
            .await
            .unwrap(),
        Verdict::Mismatch
    );
    // Bytes changed under stale metadata: only reading them back shows it, and nothing reaches its final name.
    {
        let mut st = e.fake.st();
        let f = st.files.get_mut(&id).unwrap();
        f.sha256 = Some(sha.clone());
        f.md5 = Some(md5.clone());
    }
    assert_eq!(
        b.verify(&r, Some(&sha), Some(data.len() as u64), Some(&md5))
            .await
            .unwrap(),
        Verdict::Ok
    );
    let dest = e.root.join("restored.mp4");
    std::fs::write(&dest, b"previous").unwrap();
    assert_eq!(
        code(b.download_to(&r, &dest, Some(&sha)).await.unwrap_err()),
        err(ErrorCode::Mismatch, "sha256")
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"previous");
    assert!(!e.root.join("restored.mp4.part").exists());
}

#[tokio::test]
async fn usage_and_reliability() {
    let e = env().await;
    let (_auth, b) = ready(&e).await;
    upload(&e, &b, 1000, KIND).await;
    assert_eq!(
        b.usage().await.unwrap(),
        Usage {
            used: 1000,
            free: None,
            limit: None,
            reliable: false
        }
    );
    e.fake.st().quota_limit = Some(1_000_000);
    assert_eq!(
        b.usage().await.unwrap().limit,
        None,
        "reused for 10 seconds"
    );
    *e.mono.lock().unwrap() += 10.0;
    assert_eq!(
        b.usage().await.unwrap(),
        Usage {
            used: 1000,
            free: Some(999_000),
            limit: Some(1_000_000),
            reliable: true
        }
    );
    assert_eq!(b.last_usage().map(|u| u.used), Some(1000));
}

// --- Deletes and ownership ---

#[tokio::test]
async fn delete_owned_file() {
    let e = env().await;
    let (_auth, b) = ready(&e).await;
    let (r, ..) = upload(&e, &b, 100, KIND).await;
    assert!(b.delete(&r).await.unwrap());
    assert!(e.fake.file(r.file_id.as_deref().unwrap()).is_none());
    assert!(b.delete(&r).await.unwrap(), "already gone is confirmed");
    let (r, ..) = upload(&e, &b, 100, KIND).await;
    e.fake.fail_next(
        &format!("/drive/v3/files/{}", r.file_id.as_deref().unwrap()),
        503,
        None,
        5,
        &[],
    );
    assert!(!b.delete(&r).await.unwrap(), "unsure: kept for later");
    {
        let mut st = e.fake.st();
        st.access.clear();
        st.refresh.clear();
    }
    assert!(!b.delete(&r).await.unwrap());
    assert!(e.fake.file(r.file_id.as_deref().unwrap()).is_some());
}

#[tokio::test]
async fn delete_refuses_anything_not_owned() {
    let e = env().await;
    let (auth, b) = ready(&e).await;
    let (folder, ours) = (auth.folder_id().unwrap(), e.local_id());
    let owner = ("hars_owner", "roadviewer-hars");
    // Python's media file in Python's folder; Python's file moved into ours; a file of ours without the owner mark;
    // the owner mark under another storage id; ours but not media; ours but a folder; ours but outside the folder.
    e.fake.put(
        "py1",
        FFile::media(
            b"x",
            &["pyfolder"],
            &[
                ("rv_id", ID),
                ("rv_kind", KIND),
                ("rv_storage_id", "python-sid"),
            ],
        ),
    );
    e.fake.put(
        "py2",
        FFile::media(
            b"x",
            &[&folder],
            &[
                ("rv_id", ID),
                ("rv_kind", KIND),
                ("rv_storage_id", "python-sid"),
            ],
        ),
    );
    e.fake.put(
        "x1",
        FFile::media(
            b"x",
            &[&folder],
            &[("rv_id", ID), ("rv_kind", KIND), ("rv_storage_id", &ours)],
        ),
    );
    e.fake.put(
        "x2",
        FFile::media(
            b"x",
            &[&folder],
            &[
                ("rv_id", ID),
                ("rv_kind", KIND),
                ("rv_storage_id", "other"),
                owner,
            ],
        ),
    );
    e.fake.put(
        "x3",
        FFile::media(
            b"x",
            &[&folder],
            &[("rv_kind", KIND), ("rv_storage_id", &ours), owner],
        ),
    );
    let mut dir = FFile::media(
        b"",
        &[&folder],
        &[
            ("rv_id", ID),
            ("rv_kind", KIND),
            ("rv_storage_id", &ours),
            owner,
        ],
    );
    dir.mime = FOLDER.into();
    e.fake.put("x4", dir);
    e.fake.put(
        "x5",
        FFile::media(
            b"x",
            &["elsewhere"],
            &[
                ("rv_id", ID),
                ("rv_kind", KIND),
                ("rv_storage_id", &ours),
                owner,
            ],
        ),
    );
    for id in [
        "py1",
        "py2",
        "x1",
        "x2",
        "x3",
        "x4",
        "x5",
        folder.as_str(),
        "",
    ] {
        let refused = b
            .delete(&RemoteRef::new(BackendKind::Gdrive, id, 1, None, None))
            .await
            .unwrap_err();
        assert_eq!(code(refused), err(ErrorCode::Io, "foreign_file"), "{id}");
        assert!(id.is_empty() || e.fake.file(id).is_some());
    }
    assert_eq!(e.fake.count(0, "DELETE", ""), 0);
    // Python's files are not found as ours either.
    let ids: Vec<_> = b
        .find(ID, KIND)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|r| r.file_id)
        .collect();
    assert!(
        ids.iter()
            .all(|i| !["py1", "py2", "x1", "x2", "x5"].contains(&i.as_str())),
        "{ids:?}"
    );
}

// --- Python's data, read only ---

fn python_layout(e: &Env, sid: &str) -> (PathBuf, Vec<u8>) {
    let data = bytes_of(3000, 42);
    e.fake.put(
        "pyfolder",
        FFile::folder(
            "Road Viewer",
            &[("rv_storage_id", sid)],
            "2025-06-01T00:00:00.000Z",
        ),
    );
    let sha = sha_hex(&data);
    let mut f = FFile::media(
        &data,
        &["pyfolder"],
        &[
            ("rv_id", ID),
            ("rv_kind", KIND),
            ("rv_sha256", &sha),
            ("rv_storage_id", sid),
        ],
    );
    f.name = format!("{ID}--{KIND}");
    e.fake.put("pyfile1", f);
    let mut log = FFile::media(
        b"zstd",
        &["pyfolder"],
        &[
            ("rv_id", ID),
            ("rv_kind", "rlog.zst"),
            ("rv_sha256", &sha_hex(b"zstd")),
            ("rv_storage_id", sid),
        ],
    );
    log.name = format!("{ID}--rlog.zst");
    e.fake.put("pyfile2", log);
    let python_root = e.root.parent().unwrap().join("python");
    std::fs::create_dir(&python_root).unwrap();
    std::fs::write(
        python_root.join(".archive.json"),
        format!(r#"{{"storage_id": "nas-era", "gdrive_storage_id": "{sid}"}}"#),
    )
    .unwrap();
    (python_root, data)
}

#[tokio::test]
async fn python_drive_layout_is_read_without_changes() {
    let e = env().await;
    let auth = e.connected().await;
    let (python_root, data) = python_layout(&e, "pysid");
    let sid = DriveLegacy::python_storage_id(&python_root).unwrap();
    assert_eq!(sid, "pysid");
    let mark = e.fake.mark();
    let legacy = DriveLegacy::new(e.client(&auth), &sid, None, clock_of(&e.mono));
    assert_eq!(
        legacy.probe(false).await,
        Probe::unavailable("folder_missing")
    );
    assert_eq!(legacy.locate().await.unwrap().as_deref(), Some("pyfolder"));
    assert_eq!(legacy.probe(false).await, Probe::connected());
    let found = legacy.find(ID, KIND).await.unwrap();
    assert_eq!(
        found,
        vec![RemoteRef::new(
            BackendKind::Gdrive,
            "pyfile1",
            3000,
            Some(sha_hex(&data)),
            Some(md5_hex(&data))
        )]
    );
    assert_eq!(legacy.find_all().await.unwrap().len(), 2);
    let r = &found[0];
    assert_eq!(
        legacy
            .verify(r, Some(&sha_hex(&data)), Some(3000), None)
            .await
            .unwrap(),
        Verdict::Ok
    );
    assert_eq!(
        legacy
            .open(r, 100, Some(199))
            .await
            .unwrap()
            .read_all()
            .await
            .unwrap(),
        data[100..200]
    );
    let dest = e.root.join("restored");
    legacy.download_to(r, &dest, None).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    let writes = e
        .fake
        .requests(mark)
        .into_iter()
        .filter(|r| r.method != "GET")
        .count();
    assert_eq!(writes, 0, "the Python folder is only ever read");

    // HARS on the same account makes its own folder and never lists Python's files as its own.
    let b = e.backend(&auth);
    b.activate().await;
    assert_ne!(auth.folder_id().as_deref(), Some("pyfolder"));
    assert!(b.find(ID, KIND).await.unwrap().is_empty());
    assert_eq!(
        code(b.delete(r).await.unwrap_err()),
        err(ErrorCode::Io, "foreign_file")
    );
    let f = e.fake.file("pyfile1").unwrap();
    assert_eq!(
        (f.content, f.parents, f.name),
        (data, vec!["pyfolder".to_owned()], format!("{ID}--{KIND}"))
    );
    assert_eq!(e.fake.file("pyfolder").unwrap().name, "Road Viewer");
}

#[tokio::test]
async fn device_flow_connection_sets_up_the_folder() {
    let e = env().await;
    e.fake.st().device_poll.extend(["authorization_pending"]);
    let auth = e.auth();
    let backend = Arc::new(e.backend(&auth));
    let changes = Arc::new(Mutex::new(Vec::new()));
    let seen = changes.clone();
    let conn = DriveConnection::with_interval_unit(
        backend.clone(),
        Some(Arc::new(move |c| seen.lock().unwrap().push(c))),
        Duration::from_millis(2),
    );
    let code = conn.start().await.unwrap();
    assert_eq!(code.user_code, "ABCD-EFGH");
    assert_eq!(conn.start().await.unwrap(), code, "one code at a time");
    for _ in 0..500 {
        if !changes.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(*changes.lock().unwrap(), vec![true]);
    assert!(conn.pending().is_none() && conn.last_error().is_none());
    assert_eq!(backend.last_state(), Some(Avail::Connected));
    assert_eq!(e.fake.folders().len(), 1);
    let refresh = serde_json::from_slice::<Value>(&std::fs::read(e.root.join(TOKEN_FILE)).unwrap())
        .unwrap()["refresh_token"]
        .as_str()
        .unwrap()
        .to_owned();
    conn.disconnect();
    assert!(!e.root.join(TOKEN_FILE).exists() && !auth.connected());
    for _ in 0..200 {
        if e.fake.st().revoked.contains(&refresh) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(e.fake.st().revoked.contains(&refresh));
    assert_eq!(*changes.lock().unwrap(), vec![true, false]);
}

// --- Secrets ---

#[tokio::test]
async fn secrets_never_reach_messages() {
    let e = env().await;
    let auth = e.connected().await;
    let saved: Value =
        serde_json::from_slice(&std::fs::read(e.root.join(TOKEN_FILE)).unwrap()).unwrap();
    let (refresh, access) = (
        saved["refresh_token"].as_str().unwrap().to_owned(),
        saved["access_token"].as_str().unwrap().to_owned(),
    );
    let b = e.backend(&auth);
    let mut texts = vec![
        format!("{auth:?}"),
        format!("{:?}", e.client(&auth)),
        format!("{:?}", e.http),
        format!("{b:?}"),
    ];
    e.fake.st().refresh.clear();
    *e.wall.lock().unwrap() += 3600.0;
    for fail in [None, Some(500), Some(429)] {
        if let Some(status) = fail {
            e.fake.fail_next("/token", status, None, 5, &[]);
        }
        let x = auth.access_token(false).await.unwrap_err();
        texts.extend([x.to_string(), format!("{x:?}")]);
    }
    // A session PUT that fails at the socket: the URI and the token stay out of the error.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    let session = format!(
        "http://127.0.0.1:{port}/upload/drive/v3/files?uploadType=resumable&upload_id=up1-secretsession"
    );
    let x = e
        .http
        .request(
            Request::new(reqwest::Method::PUT, &session)
                .header("authorization", format!("Bearer {access}"))
                .body(&b"x"[..]),
        )
        .await
        .unwrap_err();
    texts.extend([x.to_string(), format!("{x:?}")]);
    let uri = archive::SessionUri(session.clone());
    texts.push(format!(
        "{:?} {:?}",
        uri,
        UploadResume {
            session: Some(uri.clone()),
            ..UploadResume::default()
        }
    ));
    texts.push(format!("{:?}", Secret(SECRET.into())));
    for text in &texts {
        for secret in [refresh.as_str(), access.as_str(), SECRET, "secretsession"] {
            assert!(!text.contains(secret), "{text}");
        }
    }
    assert_eq!(
        archive::redact(
            &format!("a {refresh} b {session}"),
            &[Some(&refresh), Some(&session), Some(""), None]
        ),
        "a <secret> b <secret>"
    );
    assert_eq!(
        archive::redact("abcabc", &[Some("abc"), Some("abcabc")]),
        "<secret>",
        "longest first"
    );
    // Error reasons are tokens only: free text is dropped.
    assert_eq!(
        BackendError::new(ErrorCode::Io, "https://x/?upload_id=1").reason(),
        None
    );
}

#[test]
fn base_urls_follow_python_variables() {
    // The only test touching these variables (process-wide); every other test passes its bases explicitly.
    unsafe {
        std::env::set_var("RV_GDRIVE_API_BASE", "http://127.0.0.1:18130");
        std::env::set_var("RV_GDRIVE_UPLOAD_BASE", "http://127.0.0.1:18130/upload");
        std::env::set_var("RV_GDRIVE_OAUTH_BASE", "http://127.0.0.1:18131");
    }
    assert_eq!(
        (
            http_client::api_base_url(),
            http_client::upload_base_url(),
            http_client::oauth_base_url()
        ),
        (
            "http://127.0.0.1:18130".to_owned(),
            "http://127.0.0.1:18130/upload".to_owned(),
            "http://127.0.0.1:18131".to_owned()
        )
    );
    unsafe {
        for k in [
            "RV_GDRIVE_API_BASE",
            "RV_GDRIVE_UPLOAD_BASE",
            "RV_GDRIVE_OAUTH_BASE",
        ] {
            std::env::remove_var(k);
        }
    }
    assert_eq!(http_client::api_base_url(), "https://www.googleapis.com");
    assert_eq!(
        http_client::upload_base_url(),
        "https://www.googleapis.com/upload"
    );
    assert_eq!(
        http_client::oauth_base_url(),
        "https://oauth2.googleapis.com"
    );
}

// --- NAS ---

fn nas(root: &Path, share: &Path, allow_any_path: bool, clock: &Arc<Mutex<f64>>) -> NasBackend {
    NasBackend::new(
        NasSettings {
            enabled: true,
            error: None,
            path: Some(share.to_path_buf()),
            allow_any_path,
        },
        root,
        clock_of(clock),
    )
}
fn sentinel(share: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(share.join(".roadviewer-storage.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn nas_unmounted_and_dropped_share_states() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, share) = (tmp.path().join("data"), tmp.path().join("share"));
    std::fs::create_dir(&root).unwrap();
    let clock = Arc::new(Mutex::new(0.0));
    let b = nas(&root, &share, false, &clock);
    assert_eq!(b.probe(false).await, Probe::unavailable("path_missing"));
    std::fs::create_dir(&share).unwrap();
    b.invalidate();
    assert_eq!(b.probe(false).await, Probe::unavailable("sentinel_missing"));
    // Not a mount (a plain folder outside /share/<name>): never claimed.
    assert!(!b.mounted());
    b.activate().await;
    assert!(!share.join(".roadviewer-storage.json").exists());
    assert_eq!(b.probe(false).await, Probe::unavailable("sentinel_missing"));
    let disabled = NasBackend::new(
        NasSettings {
            enabled: false,
            error: Some("invalid_archive_path".into()),
            ..NasSettings::default()
        },
        &root,
        clock_of(&clock),
    );
    assert_eq!(
        disabled.probe(false).await,
        Probe::disabled(Some("invalid_archive_path".into()))
    );
    assert!(archive::nas::valid_path(
        "/share/nas/roadviewer-hars",
        false
    ));
    assert!(
        !archive::nas::valid_path("/share/nas", false)
            && !archive::nas::valid_path("/share/nas/../x", false)
    );
    assert!(!archive::nas::valid_path("/tmp/x", false) && archive::nas::valid_path("/tmp/x", true));

    let b = nas(&root, &share, true, &clock);
    b.activate().await;
    let s = sentinel(&share);
    assert_eq!(s["owner"], "roadviewer-hars");
    assert_eq!(s["storage_id"].as_str(), b.storage_id().as_deref());
    assert_eq!(b.probe(false).await, Probe::connected());
    // The share drops: the folder below the mount point is an empty local directory.
    std::fs::remove_file(share.join(".roadviewer-storage.json")).unwrap();
    assert_eq!(
        b.probe(false).await,
        Probe::connected(),
        "reused for 10 seconds"
    );
    *clock.lock().unwrap() += 10.0;
    assert_eq!(b.last_state(), None);
    assert_eq!(b.probe(false).await, Probe::unavailable("sentinel_missing"));
    b.activate().await;
    assert!(
        !share.join(".roadviewer-storage.json").exists(),
        "a known id never claims an empty folder"
    );
    let r = RemoteRef::new(BackendKind::Smb, format!("{ID}/{KIND}"), 1, None, None);
    assert_eq!(
        code(b.stat(&r).await.unwrap_err()),
        err(ErrorCode::Unavailable, "share")
    );
    assert!(
        !b.delete(&r).await.unwrap(),
        "unreachable: unsure, kept for later"
    );
    std::fs::remove_dir(&share).unwrap();
    b.invalidate();
    assert_eq!(b.probe(false).await, Probe::unavailable("path_missing"));
    std::fs::create_dir(&share).unwrap();
    std::fs::write(share.join(".roadviewer-storage.json"), "{broken").unwrap();
    b.invalidate();
    assert_eq!(b.probe(false).await, Probe::unavailable("sentinel_invalid"));
}

#[tokio::test]
async fn nas_round_trip_and_failed_write_keeps_existing_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, share) = (tmp.path().join("data"), tmp.path().join("share"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&share).unwrap();
    let clock = Arc::new(Mutex::new(0.0));
    let b = nas(&root, &share, true, &clock);
    b.activate().await;
    let v1 = bytes_of(archive::CHUNK + 3, 1);
    let src = tmp.path().join("src");
    std::fs::write(&src, &v1).unwrap();
    let sha1 = sha_hex(&v1);
    let mut progress = Vec::new();
    let r = b
        .upload(
            &src,
            ID,
            KIND,
            Some(&sha1),
            None,
            v1.len() as u64,
            &mut UploadResume::default(),
            &mut |n, _: &UploadResume| {
                progress.push(n);
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (r.file_id.as_deref(), progress),
        (
            Some(format!("{ID}/{KIND}").as_str()),
            vec![archive::CHUNK as u64, v1.len() as u64]
        )
    );
    let target = share.join(ID).join(KIND);
    assert_eq!(std::fs::read(&target).unwrap(), v1);
    assert_eq!(
        b.stat(&r).await.unwrap(),
        Some(RemoteStat {
            exists: true,
            bytes: v1.len() as u64,
            sha256: None,
            md5: None,
            trashed: false
        })
    );
    assert_eq!(b.find(ID, KIND).await.unwrap().len(), 1);
    assert_eq!(
        b.verify(&r, Some(&sha1), Some(v1.len() as u64), None)
            .await
            .unwrap(),
        Verdict::Ok
    );
    assert_eq!(
        b.verify(&r, Some(&"0".repeat(64)), Some(v1.len() as u64), None)
            .await
            .unwrap(),
        Verdict::Mismatch
    );
    assert_eq!(
        b.verify(&r, None, Some(v1.len() as u64), None)
            .await
            .unwrap(),
        Verdict::Unverifiable
    );
    assert_eq!(
        b.open(&r, 5, Some(9))
            .await
            .unwrap()
            .read_all()
            .await
            .unwrap(),
        v1[5..10]
    );
    b.download_to(&r, &tmp.path().join("copy"), Some(&sha1))
        .await
        .unwrap();
    assert_eq!(std::fs::read(tmp.path().join("copy")).unwrap(), v1);
    let usage = b.usage().await.unwrap();
    assert!(usage.used >= v1.len() as u64 && usage.limit.is_none() && usage.reliable);

    // A second version that fails on the way: the part is removed, the archived copy stays as it was.
    let v2 = bytes_of(2 * archive::CHUNK, 2);
    std::fs::write(&src, &v2).unwrap();
    let cases: Vec<(Option<String>, u64, bool, Failure)> = vec![
        (
            Some("0".repeat(64)),
            v2.len() as u64,
            false,
            err(ErrorCode::Mismatch, "sha256"),
        ),
        (
            Some(sha_hex(&v2)),
            v2.len() as u64 + 1,
            false,
            err(ErrorCode::Mismatch, "size"),
        ),
        (
            Some(sha_hex(&v2)),
            v2.len() as u64,
            true,
            err(ErrorCode::Io, "stopped"),
        ),
    ];
    for (sha, size, stop, expected) in cases {
        let failed = b
            .upload(
                &src,
                ID,
                KIND,
                sha.as_deref(),
                None,
                size,
                &mut UploadResume::default(),
                &mut |n, _: &UploadResume| {
                    if stop && n >= archive::CHUNK as u64 {
                        Err(BackendError::new(ErrorCode::Io, "stopped"))
                    } else {
                        Ok(())
                    }
                },
            )
            .await
            .unwrap_err();
        assert_eq!(code(failed), expected);
        assert_eq!(std::fs::read(&target).unwrap(), v1);
        assert!(!share.join(ID).join(format!("{KIND}.part")).exists());
    }
    // Unreadable source: nothing changes either.
    let missing = b
        .upload(
            &tmp.path().join("gone"),
            ID,
            KIND,
            None,
            None,
            1,
            &mut UploadResume::default(),
            &mut no_progress(),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::Io);
    assert_eq!(std::fs::read(&target).unwrap(), v1);

    assert!(b.delete(&r).await.unwrap());
    assert!(!share.join(ID).exists());
    assert!(b.find(ID, KIND).await.unwrap().is_empty());
    assert_eq!(
        code(
            b.delete(&RemoteRef::new(
                BackendKind::Smb,
                format!("../{KIND}"),
                1,
                None,
                None
            ))
            .await
            .unwrap_err()
        ),
        err(ErrorCode::Io, "bad_ref")
    );
    assert_eq!(
        code(
            b.delete(&RemoteRef::new(
                BackendKind::Smb,
                format!("{ID}/meta.json"),
                1,
                None,
                None
            ))
            .await
            .unwrap_err()
        ),
        err(ErrorCode::Io, "bad_ref")
    );
}

#[tokio::test]
async fn nas_python_share_is_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, share, python_root) = (
        tmp.path().join("data"),
        tmp.path().join("share"),
        tmp.path().join("python"),
    );
    for d in [&root, &share, &python_root] {
        std::fs::create_dir_all(d).unwrap();
    }
    // Python's layout: sentinel, <id>/<kind>, and its data root's .archive.json.
    let py_sentinel =
        r#"{"storage_id": "0011223344556677", "created_at": 1700000000.5, "owner": "roadviewer"}"#;
    std::fs::write(share.join(".roadviewer-storage.json"), py_sentinel).unwrap();
    std::fs::write(
        python_root.join(".archive.json"),
        r#"{"storage_id": "0011223344556677"}"#,
    )
    .unwrap();
    let data = bytes_of(5000, 3);
    std::fs::create_dir(share.join(ID)).unwrap();
    std::fs::write(share.join(ID).join(KIND), &data).unwrap();
    let clock = Arc::new(Mutex::new(0.0));

    let b = nas(&root, &share, true, &clock);
    assert_eq!(b.probe(false).await, Probe::unavailable("not_owned"));
    b.activate().await;
    assert_eq!(
        std::fs::read_to_string(share.join(".roadviewer-storage.json")).unwrap(),
        py_sentinel,
        "never claimed"
    );
    let src = tmp.path().join("src");
    std::fs::write(&src, b"new").unwrap();
    assert!(
        b.upload(
            &src,
            ID,
            KIND,
            None,
            None,
            3,
            &mut UploadResume::default(),
            &mut no_progress()
        )
        .await
        .unwrap_err()
        .is_foreign()
    );
    let r = RemoteRef::new(
        BackendKind::Smb,
        format!("{ID}/{KIND}"),
        data.len() as u64,
        None,
        None,
    );
    assert!(b.delete(&r).await.unwrap_err().is_foreign());
    assert_eq!(std::fs::read(share.join(ID).join(KIND)).unwrap(), data);

    let legacy = NasLegacy::new(
        &share,
        &NasLegacy::python_storage_id(&python_root).unwrap(),
        clock_of(&clock),
    );
    assert_eq!(legacy.probe(false).await, Probe::connected());
    let found = legacy.find(ID, KIND).await.unwrap();
    assert_eq!(found, vec![r.clone()]);
    assert_eq!(
        legacy
            .verify(&r, Some(&sha_hex(&data)), Some(5000), None)
            .await
            .unwrap(),
        Verdict::Ok
    );
    assert_eq!(
        legacy
            .open(&r, 10, Some(19))
            .await
            .unwrap()
            .read_all()
            .await
            .unwrap(),
        data[10..20]
    );
    legacy
        .download_to(&r, &tmp.path().join("restored"), Some(&sha_hex(&data)))
        .await
        .unwrap();
    assert_eq!(std::fs::read(tmp.path().join("restored")).unwrap(), data);
    assert!(legacy.usage().await.unwrap().used >= 5000);
    let other = NasLegacy::new(&share, "someone-else", clock_of(&clock));
    assert_eq!(
        other.probe(false).await,
        Probe::unavailable("storage_id_mismatch")
    );
    let mut names: Vec<_> = std::fs::read_dir(&share)
        .unwrap()
        .map(|d| d.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, [".roadviewer-storage.json", ID]);
}

#[test]
fn shared_types_match_python() {
    assert_eq!(
        archive::DRIVE_CAPS,
        archive::Capabilities {
            kind: BackendKind::Gdrive,
            local_paths: false,
            checksum: &["sha256", "md5"],
            range_reads: true,
            atomic_rename: false,
            remote_cleanup: false
        }
    );
    assert_eq!(archive::SMB_CAPS.kind.as_str(), "smb");
    assert!(archive::SMB_CAPS.remote_cleanup && archive::SMB_CAPS.checksum.is_empty());
    let codes: Vec<&str> = [
        ErrorCode::Unavailable,
        ErrorCode::AuthExpired,
        ErrorCode::RateLimited,
        ErrorCode::QuotaExceeded,
        ErrorCode::NotFound,
        ErrorCode::Mismatch,
        ErrorCode::Io,
    ]
    .iter()
    .map(|c| c.as_str())
    .collect();
    assert_eq!(
        codes,
        [
            "unavailable",
            "auth_expired",
            "rate_limited",
            "quota_exceeded",
            "not_found",
            "mismatch",
            "io"
        ]
    );
    assert_eq!(
        BackendError::new(ErrorCode::Io, "foreign_file").to_string(),
        "io: foreign_file"
    );
    assert_eq!(
        BackendError::bare(ErrorCode::NotFound).to_string(),
        "not_found"
    );
    let mut r = RemoteRef::new(
        BackendKind::Gdrive,
        "f1",
        10,
        Some("s".into()),
        Some("m".into()),
    );
    let plain = r.clone();
    r.completed = true;
    r.sent_md5 = Some("m".into());
    assert_eq!(r, plain, "completed and sent_md5 are not part of equality");
    assert!(
        !archive::completion_matches(&plain, Some("s"), Some(10), None),
        "only a fresh upload's answer counts"
    );
    assert!(archive::completion_matches(&r, Some("s"), Some(10), None));
    assert!(!archive::completion_matches(&r, Some("t"), Some(10), None));
    assert!(!archive::completion_matches(&r, Some("s"), Some(11), None));
    assert_ne!(
        archive::DEFAULT_DRIVE_FOLDER,
        archive::PYTHON_DEFAULT_FOLDER
    );
    assert_eq!(archive::drive::q("a'b\\c"), "a\\'b\\\\c");
}

/// A Drive query whose pages add up past its bound is refused as a whole, not cut to a partial listing.
#[tokio::test]
async fn a_drive_listing_past_its_bound_is_refused_not_cut() {
    let e = env().await;
    let auth = e.connected().await;
    let client = e.client(&auth);
    for n in 0..12 {
        e.fake
            .put(&format!("listed-{n}"), FFile::media(b"x", &["parent"], &[]));
    }
    let query = "'parent' in parents and trashed=false";
    assert_eq!(
        client
            .list_upto(query, "files(id)", 12)
            .await
            .unwrap()
            .len(),
        12
    );
    let refused = client.list_upto(query, "files(id)", 11).await.unwrap_err();
    assert_eq!(refused.reason(), Some("too_many_files"));
}
