//! Reading the Python Road Viewer's archive and copying recordings into HARS (`/api/settings/archive/legacy/*`).
//! The Python side is a fixture in Python's own layout: a Drive folder `Road Viewer` (appProperties
//! `rv_storage_id`, files `<id>--<kind>` with `rv_id`/`rv_kind`/`rv_sha256`) on the loopback fake, and a share folder
//! `<id>/<kind>` with a `roadviewer`-owned sentinel. Every test ends by proving the Python data was only read.
#![cfg(unix)]
mod common;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use common::{archive_env::*, fake_drive::*};
use roadviewer_hars::{
    archive::{DriveAuth, Poll, Secret, drive_auth::TOKEN_FILE},
    http::ingress,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tower::ServiceExt;

const PY_FOLDER: &str = "pyfolder";
const PY_SID: &str = "pythonsid0001";
const NAS_SID: &str = "pythonnas0001";

fn pid(n: u32) -> String {
    format!("{:032x}", 0x00c0_ffee_0000u64 + u64::from(n))
}

// ---------------------------------------------------------------- Python's Drive layout

#[derive(Clone, Copy)]
enum Mark {
    Right,
    Wrong,
    Empty,
}

fn py_folder(env: &Env) {
    env.fake.put(
        PY_FOLDER,
        FFile::folder(
            "Road Viewer",
            &[("rv_storage_id", PY_SID)],
            "2025-01-01T00:00:00Z",
        ),
    );
}

fn py_file(env: &Env, file_id: &str, id: &str, kind: &str, content: &[u8], mark: Mark) {
    let sha = match mark {
        Mark::Right => sha_hex(content),
        Mark::Wrong => sha_hex(b"something else"),
        Mark::Empty => String::new(),
    };
    let mut f = FFile::media(
        content,
        &[PY_FOLDER],
        &[
            ("rv_id", id),
            ("rv_kind", kind),
            ("rv_sha256", &sha),
            ("rv_storage_id", PY_SID),
        ],
    );
    f.name = format!("{id}--{kind}");
    env.fake.put(file_id, f);
}

/// Python's folder and its files exactly as they are (name, parents, appProperties, bytes, trash state).
fn drive_snapshot(fake: &Fake) -> BTreeMap<String, String> {
    fake.st()
        .files
        .iter()
        .filter(|(id, f)| id.as_str() == PY_FOLDER || f.parents.iter().any(|p| p == PY_FOLDER))
        .map(|(id, f)| (id.clone(), format!("{f:?}")))
        .collect()
}

/// The proof: every request naming a Python file or folder was a GET, and Python's folder is unchanged.
fn assert_drive_untouched(fake: &Fake, since: usize, before: &BTreeMap<String, String>) {
    let mut named = 0;
    for r in fake.requests(since) {
        if before.keys().any(|id| r.path.contains(id.as_str())) {
            named += 1;
            assert_eq!(
                r.method, "GET",
                "a non-read request reached Python data: {r:?}"
            );
        }
    }
    assert!(named > 0, "the Python folder was never read");
    assert_eq!(&drive_snapshot(fake), before, "Python's Drive data changed");
}

// ---------------------------------------------------------------- Python's share layout

/// Records as (id, [(kind, bytes)]).
type ShareRecords<'a> = [(&'a str, &'a [(&'a str, Vec<u8>)])];

fn py_share(dir: &Path, records: &ShareRecords<'_>) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(".roadviewer-storage.json"),
        json!({"storage_id": NAS_SID, "created_at": 1.0, "owner": "roadviewer"}).to_string(),
    )
    .unwrap();
    for (id, files) in records {
        std::fs::create_dir_all(dir.join(id)).unwrap();
        for (kind, data) in *files {
            std::fs::write(dir.join(id).join(kind), data).unwrap();
        }
    }
}

/// Every entry of the share with its bytes, mtime and permissions.
fn tree(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, i64, i64, u32)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let m = std::fs::symlink_metadata(e.path()).unwrap();
            let bytes = if m.is_dir() {
                stack.push(e.path());
                Vec::new()
            } else {
                std::fs::read(e.path()).unwrap()
            };
            out.insert(
                e.path().strip_prefix(dir).unwrap().to_path_buf(),
                (bytes, m.mtime(), m.mtime_nsec(), m.mode()),
            );
        }
    }
    out
}

fn read_only(dir: &Path, on: bool) {
    let mut stack = vec![dir.to_path_buf()];
    let mut all = Vec::new();
    while let Some(d) = stack.pop() {
        all.push((d.clone(), true));
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                all.push((e.path(), false));
            }
        }
    }
    for (p, is_dir) in all {
        let mode = match (on, is_dir) {
            (true, true) => 0o555,
            (true, false) => 0o444,
            (false, true) => 0o755,
            (false, false) => 0o644,
        };
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

// ---------------------------------------------------------------- HARS side

fn drive_body() -> Value {
    json!({"backend": "gdrive", "folder_name": "Road Viewer"})
}
fn nas_body(dir: &Path) -> Value {
    json!({"backend": "smb", "path": dir.to_string_lossy()})
}
fn with_ids<S: AsRef<str>>(mut body: Value, ids: &[S]) -> Value {
    body["ids"] = json!(ids.iter().map(AsRef::as_ref).collect::<Vec<_>>());
    body
}

async fn status(env: &Env) -> Value {
    let (code, out) = env
        .req("GET", "/api/settings/archive/legacy/status", Value::Null)
        .await;
    assert_eq!(code, StatusCode::OK);
    out
}

async fn finished(env: &Env) -> Value {
    let start = tokio::time::Instant::now();
    loop {
        let s = status(env).await;
        if s["running"] == false && s["job"]["state"] != "running" {
            return s["job"].clone();
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "job never finished: {s}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn import(env: &Env, body: Value) -> Value {
    let (code, out) = env
        .req("POST", "/api/settings/archive/legacy/import", body)
        .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{out}");
    finished(env).await
}

fn record<'a>(job: &'a Value, id: &str) -> &'a Value {
    job["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no record {id} in {job}"))
}

/// Nothing of a copy is left in the data root: no session, no partial file, no reservation.
async fn no_leftovers(env: &Env) {
    let sessions: Vec<_> = std::fs::read_dir(env.root.join(".uploads"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(sessions.is_empty(), "sessions left: {sessions:?}");
    assert_eq!(
        roadviewer_hars::storage::reserved(&env.root).await.unwrap(),
        0
    );
    assert!(env.app.uploads.sessions.read().await.is_empty());
}

async fn records_with_origin(env: &Env) -> Vec<Value> {
    env.app
        .store
        .records
        .read()
        .await
        .values()
        .filter(|m| m.get("imported_from").is_some())
        .cloned()
        .collect()
}

async fn new_env() -> Env {
    let env = Env::new().await;
    env.connect_drive().await;
    env
}

fn hevc(path: &Path) -> Vec<u8> {
    let ok = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x96:rate=10",
            "-t",
            "1",
            "-an",
            "-threads",
            "1",
            "-c:v",
            "libx265",
            "-pix_fmt",
            "yuv420p",
            "-x265-params",
            "pools=1:frame-threads=1:bframes=0:keyint=10:log-level=error",
            "-f",
            "hevc",
        ])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "ffmpeg fixture");
    std::fs::read(path).unwrap()
}

// ---------------------------------------------------------------- scan

#[tokio::test]
async fn scan_lists_python_records_and_what_hars_already_has() {
    let env = new_env().await;
    py_folder(&env);
    let (a, b, c) = (pid(1), pid(2), pid(3));
    let rlog_b = data(3000, 2);
    py_file(
        &env,
        "py-a-rlog",
        &a,
        "rlog.zst",
        &data(4000, 1),
        Mark::Right,
    );
    py_file(
        &env,
        "py-a-q",
        &a,
        "qcamera.ts",
        &data(5000, 11),
        Mark::Right,
    );
    py_file(
        &env,
        "py-a-qmp4",
        &a,
        "camera.mp4",
        &data(900, 12),
        Mark::Right,
    );
    py_file(&env, "py-b-rlog", &b, "rlog.zst", &rlog_b, Mark::Empty);
    py_file(
        &env,
        "py-c-mp4",
        &c,
        "camera.mp4",
        &data(700, 3),
        Mark::Right,
    );
    // A file of another Python data root in the same folder and one in the trash: neither is listed.
    let mut other = FFile::media(
        b"x",
        &[PY_FOLDER],
        &[
            ("rv_id", &pid(9)),
            ("rv_kind", "rlog.zst"),
            ("rv_storage_id", "other"),
        ],
    );
    other.name = "other".into();
    env.fake.put("py-other", other);
    let mut trashed = FFile::media(
        b"y",
        &[PY_FOLDER],
        &[
            ("rv_id", &pid(8)),
            ("rv_kind", "rlog.zst"),
            ("rv_storage_id", PY_SID),
        ],
    );
    trashed.trashed = true;
    env.fake.put("py-trashed", trashed);
    let have = env.record('b', &[("rlog.zst", rlog_b.clone())], OLD).await;
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();

    let (code, out) = env
        .req("POST", "/api/settings/archive/legacy/scan", drive_body())
        .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    assert_eq!(
        out["source"],
        json!({"backend": "gdrive", "storage_id": PY_SID, "folder_id": PY_FOLDER, "folder_name": "Road Viewer"})
    );
    assert_eq!(out["total"], 3);
    assert_eq!(out["importable"], 2);
    assert_eq!(out["metadata"], "not_imported");
    let rec = |id: &str| {
        out["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap()
            .clone()
    };
    let ra = rec(&a);
    assert_eq!(ra["importable"], true);
    assert_eq!(ra["import_kinds"], json!(["rlog.zst", "qcamera.ts"]));
    assert_eq!(ra["skipped_kinds"], json!(["camera.mp4"]));
    assert_eq!(ra["in_hars"], Value::Null);
    let rlog_a = ra["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "rlog.zst")
        .unwrap();
    assert_eq!(rlog_a["bytes"], 4000);
    assert_eq!(rlog_a["sha256"], sha_hex(&data(4000, 1)));
    assert_eq!(rlog_a["sha256_marked"], true);
    assert_eq!(rlog_a["copies"], 1);
    let rb = rec(&b);
    assert_eq!(rb["files"][0]["sha256_marked"], false);
    assert_eq!(rb["in_hars"], json!({"id": have, "match": "sha256"}));
    let rc = rec(&c);
    assert_eq!(rc["importable"], false);
    assert_eq!(rc["reason"], "rlog_required");
    assert_drive_untouched(&env.fake, mark, &before);
    // Nothing about the Python folder is remembered: the archive settings file is untouched.
    assert!(
        !env.root
            .join(roadviewer_hars::archive::settings::FILENAME)
            .exists()
    );
}

#[tokio::test]
async fn scan_refusals_name_the_problem() {
    let env = Env::new().await;
    let (code, out) = env
        .req("POST", "/api/settings/archive/legacy/scan", drive_body())
        .await;
    assert_eq!(
        (code, out),
        (
            StatusCode::CONFLICT,
            json!({"error": "gdrive_not_connected"})
        )
    );
    env.connect_drive().await;
    let (code, out) = env
        .req("POST", "/api/settings/archive/legacy/scan", drive_body())
        .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(out["error"], "legacy_not_found");
    // A HARS-owned folder of the same name is never taken for Python's.
    env.fake.put(
        "hars",
        FFile::folder(
            "Road Viewer",
            &[("rv_storage_id", "h"), ("hars_owner", "roadviewer-hars")],
            "2024-01-01T00:00:00Z",
        ),
    );
    let (code, _) = env
        .req("POST", "/api/settings/archive/legacy/scan", drive_body())
        .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    py_folder(&env);
    env.fake.put(
        "py2",
        FFile::folder(
            "Road Viewer",
            &[("rv_storage_id", "second")],
            "2025-02-01T00:00:00Z",
        ),
    );
    let (code, out) = env
        .req("POST", "/api/settings/archive/legacy/scan", drive_body())
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(out["error"], "legacy_ambiguous");
    assert_eq!(out["candidates"].as_array().unwrap().len(), 2);
    let mut body = drive_body();
    body["storage_id"] = json!(PY_SID);
    let (code, out) = env
        .req("POST", "/api/settings/archive/legacy/scan", body)
        .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    assert_eq!(out["source"]["folder_id"], PY_FOLDER);
    for bad in [
        json!({"backend": "ftp"}),
        json!({"backend": "smb", "path": "relative"}),
        json!({"backend": "gdrive", "folder_name": ""}),
        json!({"backend": "gdrive", "x": 1}),
    ] {
        let (code, out) = env
            .req("POST", "/api/settings/archive/legacy/scan", bad)
            .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(out["error"], "invalid_legacy_request");
    }
    // A share is Python's only with a `roadviewer` sentinel; HARS's own root and the data folder are refused.
    let share = env.tmp.path().join("hars-share");
    std::fs::create_dir_all(&share).unwrap();
    std::fs::write(
        share.join(".roadviewer-storage.json"),
        json!({"storage_id": "h", "owner": "roadviewer-hars"}).to_string(),
    )
    .unwrap();
    let (code, out) = env
        .req(
            "POST",
            "/api/settings/archive/legacy/scan",
            nas_body(&share),
        )
        .await;
    assert_eq!(
        (code, out["reason"].clone()),
        (StatusCode::CONFLICT, json!("hars_owned"))
    );
    let (code, out) = env
        .req(
            "POST",
            "/api/settings/archive/legacy/scan",
            nas_body(&env.root),
        )
        .await;
    assert_eq!(
        (code, out["fields"]["path"].clone()),
        (StatusCode::BAD_REQUEST, json!("data_folder"))
    );
    let (code, out) = env
        .req(
            "POST",
            "/api/settings/archive/legacy/scan",
            nas_body(&env.tmp.path().join("none")),
        )
        .await;
    assert_eq!(
        (code, out["reason"].clone()),
        (StatusCode::NOT_FOUND, json!("path_missing"))
    );
}

// ---------------------------------------------------------------- import from Drive

#[tokio::test]
async fn import_copies_registers_and_queues_conversion() {
    let env = new_env().await;
    py_folder(&env);
    let a = pid(1);
    let (rlog, cam) = (data(300_000, 1), data(200_000, 2));
    py_file(&env, "py-a-rlog", &a, "rlog.zst", &rlog, Mark::Right);
    py_file(&env, "py-a-q", &a, "qcamera.ts", &cam, Mark::Empty);
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();

    let job = import(&env, with_ids(drive_body(), &[&a])).await;
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(
        job["counts"],
        json!({"imported": 1, "duplicate": 0, "failed": 0})
    );
    assert_eq!(job["metadata"], "not_imported");
    let r = record(&job, &a);
    assert_eq!(
        (r["state"].clone(), r["mode"].clone()),
        (json!("imported"), json!("new")),
        "{r}"
    );
    let files = r["files"].as_array().unwrap();
    assert_eq!(
        files[0]["verified_by"],
        json!(["size", "rv_sha256", "sha256Checksum", "md5Checksum"])
    );
    assert_eq!(
        files[1]["verified_by"],
        json!(["size", "sha256Checksum", "md5Checksum"])
    );
    assert_eq!(files[1]["sha256"], sha_hex(&cam));
    let id = r["hars_id"].as_str().unwrap().to_owned();
    assert_ne!(id, a, "a copy is a new HARS recording");
    let meta = env.meta(&id).await;
    assert_eq!(
        std::fs::read(env.root.join(&id).join("rlog.zst")).unwrap(),
        rlog
    );
    assert_eq!(
        std::fs::read(env.root.join(&id).join("qcamera.ts")).unwrap(),
        cam
    );
    assert_eq!(meta["content_hashes"]["rlog.zst"]["sha256"], sha_hex(&rlog));
    assert_eq!(meta["status"], "unconverted");
    assert_eq!(meta["video"], true);
    assert_eq!(meta["imported_from"]["backend"], "gdrive");
    assert_eq!(meta["imported_from"]["storage_id"], PY_SID);
    assert_eq!(meta["imported_from"]["id"], a.as_str());
    assert!(
        meta.get("storage").is_none(),
        "the Python copy is never a HARS archive location: {meta}"
    );
    assert!(meta.get("tags").is_none() && meta.get("bookmarks").is_none());
    // The list answers as for any upload; the origin mark stays in meta.json.
    let plain = env.record('c', &[("rlog.zst", data(10, 9))], OLD).await;
    let (code, list) = env.req("GET", "/api/logs", Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    let row = |want: &str| {
        list["logs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == want)
            .unwrap()
            .clone()
    };
    let keys = |v: &Value| {
        v.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert!(row(&id).get("imported_from").is_none());
    let (mine, theirs) = (keys(&row(&id)), keys(&row(&plain)));
    assert!(
        mine.is_subset(&theirs) || mine == theirs,
        "row keys {mine:?} vs {theirs:?}"
    );
    // Waiting for conversion like any upload: the scheduler queues it.
    env.app.jobs.tick().await.unwrap();
    let queued = env.meta(&id).await;
    assert!(
        queued["queued_at"].is_number() && queued["status"] != "unconverted",
        "{queued}"
    );
    no_leftovers(&env).await;
    assert_drive_untouched(&env.fake, mark, &before);

    // Asking again converges: already here, nothing is copied.
    let mark = env.fake.mark();
    let job = import(&env, with_ids(drive_body(), &[&a])).await;
    let r = record(&job, &a);
    assert_eq!(
        (r["state"].clone(), r["hars_id"].clone()),
        (json!("duplicate"), json!(id))
    );
    assert_eq!(
        env.fake.count(mark, "GET", "/drive/v3/files/py-a"),
        0,
        "no download for a known copy"
    );
    let (_, scan) = env
        .req("POST", "/api/settings/archive/legacy/scan", drive_body())
        .await;
    assert_eq!(scan["records"][0]["in_hars"]["id"], id.as_str());
    assert_drive_untouched(&env.fake, mark, &before);
}

#[tokio::test]
async fn several_records_each_get_an_outcome() {
    let env = new_env().await;
    py_folder(&env);
    let (a, b, c, d, e) = (pid(1), pid(2), pid(3), pid(4), pid(5));
    py_file(
        &env,
        "py-a-rlog",
        &a,
        "rlog.zst",
        &data(1000, 1),
        Mark::Right,
    );
    py_file(
        &env,
        "py-b-rlog",
        &b,
        "rlog.zst",
        &data(1000, 2),
        Mark::Right,
    );
    py_file(
        &env,
        "py-b-q",
        &b,
        "qcamera.ts",
        &data(800, 22),
        Mark::Right,
    );
    py_file(
        &env,
        "py-c-mp4",
        &c,
        "camera.mp4",
        &data(1000, 3),
        Mark::Right,
    );
    py_file(
        &env,
        "py-d-rlog",
        &d,
        "rlog.zst",
        &data(1000, 4),
        Mark::Right,
    );
    let dup = env.record('d', &[("rlog.zst", data(1000, 4))], OLD).await;
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();
    let job = import(&env, with_ids(drive_body(), &[&a, &b, &c, &d, &e, &a])).await;
    assert_eq!(job["total"], 5, "repeated ids count once");
    assert_eq!(
        job["counts"],
        json!({"imported": 2, "duplicate": 1, "failed": 2})
    );
    assert_eq!(record(&job, &a)["state"], "imported");
    assert_eq!(record(&job, &b)["state"], "imported");
    assert_eq!(
        (
            record(&job, &c)["state"].clone(),
            record(&job, &c)["reason"].clone()
        ),
        (json!("failed"), json!("rlog_required"))
    );
    assert_eq!(
        (
            record(&job, &d)["state"].clone(),
            record(&job, &d)["hars_id"].clone()
        ),
        (json!("duplicate"), json!(dup))
    );
    assert_eq!(record(&job, &e)["reason"], "not_found");
    assert_eq!(records_with_origin(&env).await.len(), 2);
    no_leftovers(&env).await;
    assert_drive_untouched(&env.fake, mark, &before);
}

#[tokio::test]
async fn a_damaged_copy_fails_and_leaves_nothing() {
    let env = new_env().await;
    py_folder(&env);
    let (a, b) = (pid(1), pid(2));
    // A: Python's own mark disagrees with the content. B: Drive's checksum disagrees with what it sends.
    py_file(
        &env,
        "py-a-rlog",
        &a,
        "rlog.zst",
        &data(200_000, 1),
        Mark::Wrong,
    );
    py_file(
        &env,
        "py-b-rlog",
        &b,
        "rlog.zst",
        &data(200_000, 2),
        Mark::Empty,
    );
    env.fake.st().files.get_mut("py-b-rlog").unwrap().sha256 = Some(sha_hex(b"other"));
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();
    let job = import(&env, with_ids(drive_body(), &[&a, &b])).await;
    for id in [&a, &b] {
        let r = record(&job, id);
        assert_eq!(
            (r["state"].clone(), r["reason"].clone()),
            (json!("failed"), json!("checksum_mismatch")),
            "{r}"
        );
    }
    assert!(env.app.store.records.read().await.is_empty());
    no_leftovers(&env).await;
    assert_drive_untouched(&env.fake, mark, &before);
}

/// A download that goes on past the size the listing gave (the file grew since, or the server is hostile) is
/// stopped at that size, not written to the end and then compared: the space reserved for it bounds what lands.
#[tokio::test]
async fn a_download_longer_than_listed_stops_at_the_listed_size() {
    let env = new_env().await;
    py_folder(&env);
    let a = pid(1);
    let rlog = data(300_000, 1);
    py_file(&env, "py-a-rlog", &a, "rlog.zst", &rlog, Mark::Right);
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();
    let sent = {
        let mut st = env.fake.st();
        st.media_overrun = 64 << 20;
        st.media_sent.clone()
    };
    let job = import(&env, with_ids(drive_body(), &[&a])).await;
    let r = record(&job, &a);
    assert_eq!(
        (r["state"].clone(), r["reason"].clone()),
        (json!("failed"), json!("size_mismatch")),
        "{r}"
    );
    let attempts = env
        .fake
        .count(mark, "GET", "/drive/v3/files/py-a-rlog?alt=media");
    assert!(attempts >= 1);
    let sent = sent.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        sent < attempts * (rlog.len() + (16 << 20)),
        "{sent} bytes sent over {attempts} attempts"
    );
    assert!(env.app.store.records.read().await.is_empty());
    no_leftovers(&env).await;
    assert_drive_untouched(&env.fake, mark, &before);
}

#[tokio::test]
async fn a_new_camera_joins_the_recording_hars_has() {
    let env = new_env().await;
    py_folder(&env);
    let a = pid(1);
    let (rlog, cam) = (data(2000, 1), data(1500, 2));
    let front = hevc(&env.tmp.path().join("f.hevc"));
    let have = env
        .record(
            'a',
            &[("rlog.zst", rlog.clone()), ("qcamera.ts", cam.clone())],
            OLD,
        )
        .await;
    py_file(&env, "py-a-rlog", &a, "rlog.zst", &rlog, Mark::Right);
    py_file(&env, "py-a-q", &a, "qcamera.ts", &cam, Mark::Right);
    py_file(&env, "py-a-f", &a, "fcamera.hevc", &front, Mark::Right);
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();
    let job = import(&env, with_ids(drive_body(), &[&a])).await;
    let r = record(&job, &a);
    assert_eq!(
        (r["state"].clone(), r["mode"].clone(), r["hars_id"].clone()),
        (json!("imported"), json!("cameras_added"), json!(have)),
        "{r}"
    );
    let meta = env.meta(&have).await;
    assert_eq!(
        meta["content_hashes"]["fcamera.hevc"]["sha256"],
        sha_hex(&front)
    );
    assert_eq!(
        std::fs::read(env.root.join(&have).join("fcamera.hevc")).unwrap(),
        front
    );
    assert_eq!(env.app.store.records.read().await.len(), 1);
    no_leftovers(&env).await;
    assert_drive_untouched(&env.fake, mark, &before);
}

#[tokio::test]
async fn no_room_fails_with_the_storage_reason_and_releases_the_reservation() {
    let env = new_env().await;
    py_folder(&env);
    let a = pid(1);
    py_file(
        &env,
        "py-a-rlog",
        &a,
        "rlog.zst",
        &data(400_000, 1),
        Mark::Right,
    );
    let (code, _) = env
        .req(
            "POST",
            "/api/settings/storage",
            json!({"max_bytes": 100_000, "policy": "reject_new"}),
        )
        .await;
    assert_eq!(code, StatusCode::OK);
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();
    let job = import(&env, with_ids(drive_body(), &[&a])).await;
    let r = record(&job, &a);
    assert_eq!(
        (r["state"].clone(), r["reason"].clone(), r["status"].clone()),
        (json!("failed"), json!("storage_limit_exceeded"), json!(507)),
        "{r}"
    );
    assert!(r["message"].is_string());
    assert_eq!(
        env.fake.count(mark, "GET", "/drive/v3/files/py-a-rlog?alt"),
        0,
        "refused before downloading"
    );
    no_leftovers(&env).await;
    assert!(env.app.store.records.read().await.is_empty());
    assert_eq!(drive_snapshot(&env.fake), before);
}

#[tokio::test]
async fn drive_errors_and_a_broken_download_are_retried() {
    let env = new_env().await;
    py_folder(&env);
    let a = pid(1);
    let rlog = data(600_000, 1);
    py_file(&env, "py-a-rlog", &a, "rlog.zst", &rlog, Mark::Right);
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();
    env.fake.fail_next(
        "/files/py-a-rlog?alt=media",
        503,
        Some("backendError"),
        2,
        &[],
    );
    env.fake.st().cut_media = 1;
    let job = import(&env, with_ids(drive_body(), &[&a])).await;
    let r = record(&job, &a);
    assert_eq!(r["state"], "imported", "{r}");
    assert!(
        env.fake
            .count(mark, "GET", "/drive/v3/files/py-a-rlog?alt=media")
            >= 4
    );
    assert_eq!(
        std::fs::read(
            env.root
                .join(r["hars_id"].as_str().unwrap())
                .join("rlog.zst")
        )
        .unwrap(),
        rlog
    );
    no_leftovers(&env).await;
    assert_drive_untouched(&env.fake, mark, &before);

    // Drive down for good: the record fails, nothing stays.
    let b = pid(2);
    py_file(
        &env,
        "py-b-rlog",
        &b,
        "rlog.zst",
        &data(1000, 2),
        Mark::Right,
    );
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();
    env.fake.fail_next(
        "/files/py-b-rlog?alt=media",
        503,
        Some("backendError"),
        1000,
        &[],
    );
    let job = import(&env, with_ids(drive_body(), &[&b])).await;
    assert_eq!(record(&job, &b)["state"], "failed");
    no_leftovers(&env).await;
    assert_drive_untouched(&env.fake, mark, &before);
}

#[tokio::test]
async fn one_job_at_a_time_and_cancel_leaves_nothing() {
    let env = new_env().await;
    py_folder(&env);
    let (a, b) = (pid(1), pid(2));
    py_file(
        &env,
        "py-a-rlog",
        &a,
        "rlog.zst",
        &data(2 << 20, 1),
        Mark::Right,
    );
    py_file(
        &env,
        "py-b-rlog",
        &b,
        "rlog.zst",
        &data(1000, 2),
        Mark::Right,
    );
    env.fake.st().media_delay = Some(Duration::from_millis(100));
    let before = drive_snapshot(&env.fake);
    let mark = env.fake.mark();
    let (code, _) = env
        .req(
            "POST",
            "/api/settings/archive/legacy/import",
            with_ids(drive_body(), &[&a, &b]),
        )
        .await;
    assert_eq!(code, StatusCode::ACCEPTED);
    let (code, out) = env
        .req(
            "POST",
            "/api/settings/archive/legacy/import",
            with_ids(drive_body(), &[&b]),
        )
        .await;
    assert_eq!(
        (code, out),
        (
            StatusCode::CONFLICT,
            json!({"error": "legacy_import_running"})
        )
    );
    // Mid-download: a session holds part of the file.
    let start = tokio::time::Instant::now();
    while std::fs::read_dir(env.root.join(".uploads"))
        .unwrap()
        .count()
        == 0
    {
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (code, _) = env
        .req("POST", "/api/settings/archive/legacy/cancel", Value::Null)
        .await;
    assert_eq!(code, StatusCode::ACCEPTED);
    let job = finished(&env).await;
    assert_eq!(job["state"], "cancelled");
    for id in [&a, &b] {
        assert_eq!(record(&job, id)["reason"], "cancelled");
    }
    let (code, _) = env
        .req("POST", "/api/settings/archive/legacy/cancel", Value::Null)
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(env.app.store.records.read().await.is_empty());
    no_leftovers(&env).await;
    assert_drive_untouched(&env.fake, mark, &before);
}

// ---------------------------------------------------------------- import from a share

#[tokio::test]
async fn share_import_reads_a_read_only_mount_and_never_changes_it() {
    let env = Env::new().await;
    let share = env.tmp.path().join("python-share");
    let (a, b, c) = (pid(1), pid(2), pid(3));
    let (rlog_a, cam_a) = (data(300_000, 1), data(100_000, 2));
    py_share(
        &share,
        &[
            (
                &a,
                &[
                    ("rlog.zst", rlog_a.clone()),
                    ("qcamera.ts", cam_a.clone()),
                    ("camera.mp4", data(500, 5)),
                ],
            ),
            (&b, &[("rlog.zst", data(2000, 3))]),
            (&c, &[("ecamera.mp4", data(2000, 4))]),
        ],
    );
    let dup = env.record('b', &[("rlog.zst", data(2000, 3))], OLD).await;
    read_only(&share, true);
    let before = tree(&share);

    let (code, scan) = env
        .req(
            "POST",
            "/api/settings/archive/legacy/scan",
            nas_body(&share),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{scan}");
    assert_eq!(scan["source"]["storage_id"], NAS_SID);
    assert_eq!(scan["total"], 3);
    let job = import(&env, with_ids(nas_body(&share), &[&a, &b, &c])).await;
    let r = record(&job, &a);
    assert_eq!(r["state"], "imported", "{job}");
    assert_eq!(r["files"][0]["verified_by"], json!(["size", "readback"]));
    assert_eq!(r["skipped_kinds"], json!(["camera.mp4"]));
    let id = r["hars_id"].as_str().unwrap();
    assert_eq!(
        std::fs::read(env.root.join(id).join("rlog.zst")).unwrap(),
        rlog_a
    );
    assert_eq!(
        std::fs::read(env.root.join(id).join("qcamera.ts")).unwrap(),
        cam_a
    );
    assert_eq!(env.meta(id).await["imported_from"]["backend"], "smb");
    // The share carries no checksums: the duplicate is found by the upload's own rlog comparison.
    assert_eq!(
        (
            record(&job, &b)["state"].clone(),
            record(&job, &b)["hars_id"].clone()
        ),
        (json!("duplicate"), json!(dup))
    );
    assert_eq!(record(&job, &c)["reason"], "rlog_required");
    no_leftovers(&env).await;
    let job = import(&env, with_ids(nas_body(&share), &[&a])).await;
    assert_eq!(record(&job, &a)["state"], "duplicate");
    no_leftovers(&env).await;
    assert_eq!(tree(&share), before, "the Python share changed");
    read_only(&share, false);
}

// ---------------------------------------------------------------- HARS changes never reach Python's data

#[tokio::test]
async fn deleting_cleaning_archiving_and_disconnecting_leave_python_data_alone() {
    let mut env = new_env().await;
    py_folder(&env);
    let share = env.tmp.path().join("python-share");
    let (a, b) = (pid(1), pid(2));
    let rlog_a = data(5000, 1);
    py_file(&env, "py-a-rlog", &a, "rlog.zst", &rlog_a, Mark::Right);
    py_file(
        &env,
        "py-a-q",
        &a,
        "qcamera.ts",
        &data(3000, 2),
        Mark::Right,
    );
    py_share(
        &share,
        &[(
            &b,
            &[("rlog.zst", data(4000, 3)), ("qcamera.ts", data(3000, 4))],
        )],
    );
    let before = drive_snapshot(&env.fake);
    let share_before = tree(&share);
    let mark = env.fake.mark();
    let job = import(&env, with_ids(drive_body(), &[&a])).await;
    let from_drive = record(&job, &a)["hars_id"].as_str().unwrap().to_owned();
    let job = import(&env, with_ids(nas_body(&share), &[&b])).await;
    let from_share = record(&job, &b)["hars_id"].as_str().unwrap().to_owned();
    for id in [&from_drive, &from_share] {
        env.set(id, |m| m["status"] = json!("ready")).await;
    }

    // HARS archives the copies to its own Drive folder, then they are deleted, cleaned up and Drive disconnected.
    env.use_drive(json!({"archive_min_age_hours": 0})).await;
    env.cycle().await;
    env.restart();
    env.cycle().await;
    assert!(
        env.drive_file(&from_drive, "rlog.zst").is_some(),
        "HARS archived its copy to its own folder"
    );
    let (code, out) = env
        .req("DELETE", &format!("/api/logs/{from_drive}"), Value::Null)
        .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    let (code, _) = env.req("POST", "/api/storage/cleanup", json!({})).await;
    assert!(code.is_success() || code.is_client_error());
    env.cycle().await;
    assert!(
        env.drive_file(&from_drive, "rlog.zst").is_none(),
        "HARS deleted its own Drive copy"
    );
    assert!(env.fake.count(mark, "DELETE", "/drive/v3/files/") > 0);
    let (code, _) = env
        .req(
            "POST",
            "/api/settings/archive/gdrive/disconnect",
            Value::Null,
        )
        .await;
    assert_eq!(code, StatusCode::OK);
    // And with the NAS as HARS's archive: archive, delete.
    env.connect_drive().await;
    env.use_nas(json!({"archive_min_age_hours": 0})).await;
    env.cycle().await;
    assert!(
        env.on_nas(&from_share, "rlog.zst").is_some(),
        "HARS archived its copy to its own share"
    );
    let (code, _) = env
        .req("DELETE", &format!("/api/logs/{from_share}"), Value::Null)
        .await;
    assert_eq!(code, StatusCode::OK);
    env.cycle().await;
    assert!(
        env.on_nas(&from_share, "rlog.zst").is_none(),
        "HARS deleted its own share copy"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_drive_untouched(&env.fake, mark, &before);
    assert_eq!(tree(&share), share_before);
    // Scanning again still finds both Python records whole.
    let (code, scan) = env
        .req("POST", "/api/settings/archive/legacy/scan", drive_body())
        .await;
    assert_eq!(code, StatusCode::OK, "{scan}");
    assert_eq!(scan["records"][0]["files"].as_array().unwrap().len(), 2);
    let (code, scan) = env
        .req(
            "POST",
            "/api/settings/archive/legacy/scan",
            nas_body(&share),
        )
        .await;
    assert_eq!((code, scan["total"].clone()), (StatusCode::OK, json!(1)));
}

// ---------------------------------------------------------------- the routes

#[tokio::test]
async fn the_routes_need_the_ingress_and_its_request_header() {
    let env = new_env().await;
    for (method, path) in [("POST", "scan"), ("POST", "import"), ("POST", "cancel")] {
        let request = Request::builder()
            .method(method)
            .uri(format!("/api/settings/archive/legacy/{path}"))
            .header("content-type", "application/json")
            .body(Body::from(drive_body().to_string()))
            .unwrap();
        let response = ingress(env.app.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error": "forbidden"})
        );
    }
    let server = common::serve(env.app.clone()).await;
    for (method, path) in [
        ("POST", "scan"),
        ("POST", "import"),
        ("GET", "status"),
        ("POST", "cancel"),
    ] {
        let (code, _) = common::http(
            server.device,
            method,
            &format!("/api/settings/archive/legacy/{path}"),
            &common::csrf(),
            drive_body().to_string().as_bytes(),
        )
        .await;
        assert_eq!(code, 404, "device listener {path}");
    }
}

// ---------------------------------------------------------------- SIGKILL of the real executable

struct Proc {
    child: Child,
    ui: std::net::SocketAddr,
}
impl Drop for Proc {
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

async fn start(root: &Path, fake: &str, crash: Option<(&str, &Path)>) -> Proc {
    for _ in 0..5 {
        let ui: std::net::SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let device: std::net::SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_roadviewer-hars"));
        command
            .env("HARS_DATA", root)
            .env("HARS_WEB", common::web())
            .env("HARS_INGRESS_ONLY", "0")
            .env("HARS_BIND", ui.to_string())
            .env("HARS_DEVICE_BIND", device.to_string())
            .env("RUST_LOG", "off")
            .env("RV_ARCHIVE_ALLOW_ANY_PATH", "1")
            .env("RV_ARCHIVE_OPTIONS_JSON", json!({"archive_gdrive_client_id": "client-id", "archive_gdrive_client_secret": SECRET}).to_string())
            .env("RV_GDRIVE_API_BASE", fake)
            .env("RV_GDRIVE_UPLOAD_BASE", format!("{fake}/upload"))
            .env("RV_GDRIVE_OAUTH_BASE", fake)
            .env_remove("HARS_CRASH_POINT")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some((point, marker)) = crash {
            command
                .env("HARS_CRASH_POINT", point)
                .env("HARS_CRASH_MARKER", marker);
        }
        let mut proc = Proc {
            child: command.spawn().unwrap(),
            ui,
        };
        let begun = tokio::time::Instant::now();
        while begun.elapsed() < common::DEADLINE {
            if proc.child.try_wait().unwrap().is_some() {
                break;
            }
            if tokio::net::TcpStream::connect(ui).await.is_ok()
                && common::http(ui, "GET", "/health", &[], b"").await.0 == 200
            {
                return proc;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = proc.child.kill();
    }
    panic!("server did not start");
}

async fn call(p: &Proc, method: &str, path: &str, body: &Value) -> (u16, Value) {
    common::http(
        p.ui,
        method,
        path,
        &common::csrf(),
        body.to_string().as_bytes(),
    )
    .await
}

async fn done(p: &Proc) -> Value {
    let start = tokio::time::Instant::now();
    loop {
        let (_, s) = call(
            p,
            "GET",
            "/api/settings/archive/legacy/status",
            &Value::Null,
        )
        .await;
        if s["running"] == false && s["job"]["state"] != "running" {
            return s["job"].clone();
        }
        assert!(
            start.elapsed() < common::DEADLINE,
            "job never finished: {s}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn walk_temps(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        if e.file_type().unwrap().is_dir() {
            walk_temps(&e.path(), out);
        } else if name.ends_with(".part") || name.ends_with(".tmp") {
            out.push(e.path());
        }
    }
}

async fn killed_and_restarted(point: &str, drive: bool) {
    let fake = start_fake().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(".processing-settings.json"),
        br#"{"auto_convert":false}"#,
    )
    .unwrap();
    let share = tmp.path().join("python-share");
    let a = pid(1);
    let rlog = data(3 << 20, 1);
    let cam = data(1 << 20, 2);
    let body = if drive {
        let auth = DriveAuth::new(
            root.join(TOKEN_FILE),
            "client-id",
            Secret(SECRET.into()),
            http(),
            Some(&fake.url),
            roadviewer_hars::archive::wall_clock(),
        );
        auth.start_device_flow(None).await.unwrap();
        assert_eq!(auth.poll_once().await.unwrap(), Poll::Granted);
        fake.put(
            PY_FOLDER,
            FFile::folder(
                "Road Viewer",
                &[("rv_storage_id", PY_SID)],
                "2025-01-01T00:00:00Z",
            ),
        );
        for (fid, kind, content) in [
            ("py-a-rlog", "rlog.zst", &rlog),
            ("py-a-q", "qcamera.ts", &cam),
        ] {
            let sha = sha_hex(content);
            let mut f = FFile::media(
                content,
                &[PY_FOLDER],
                &[
                    ("rv_id", &a),
                    ("rv_kind", kind),
                    ("rv_sha256", &sha),
                    ("rv_storage_id", PY_SID),
                ],
            );
            f.name = format!("{a}--{kind}");
            fake.put(fid, f);
        }
        drive_body()
    } else {
        py_share(
            &share,
            &[(
                &a,
                &[("rlog.zst", rlog.clone()), ("qcamera.ts", cam.clone())],
            )],
        );
        read_only(&share, true);
        nas_body(&share)
    };
    let before = drive_snapshot(&fake);
    let share_before = (!drive).then(|| tree(&share));
    let mark = fake.mark();
    let marker = tmp.path().join("crashed");
    let p = start(&root, &fake.url, Some((point, &marker))).await;
    let (code, out) = call(
        &p,
        "POST",
        "/api/settings/archive/legacy/import",
        &with_ids(body.clone(), &[&a]),
    )
    .await;
    assert_eq!(code, 202, "{out}");
    common::poll(point, || async {
        std::fs::read_to_string(&marker).is_ok_and(|v| v == point)
    })
    .await;
    drop(p);
    assert_eq!(
        std::fs::read_dir(root.join(".uploads")).unwrap().count(),
        1,
        "killed mid-copy"
    );

    let p = start(&root, &fake.url, None).await;
    let (_, s) = call(
        &p,
        "GET",
        "/api/settings/archive/legacy/status",
        &Value::Null,
    )
    .await;
    assert_eq!(s["job"]["state"], "interrupted", "{s}");
    assert_eq!(s["job"]["records"][0]["reason"], "interrupted");
    assert_eq!(
        std::fs::read_dir(root.join(".uploads")).unwrap().count(),
        0,
        "the interrupted copy is gone"
    );
    let mut temps = Vec::new();
    walk_temps(&root, &mut temps);
    assert!(temps.is_empty(), "{temps:?}");
    assert_eq!(roadviewer_hars::storage::reserved(&root).await.unwrap(), 0);
    let (_, list) = call(&p, "GET", "/api/logs", &Value::Null).await;
    assert_eq!(list["logs"].as_array().unwrap().len(), 0);

    // The same request again: copied once; once more: a duplicate.
    let (code, _) = call(
        &p,
        "POST",
        "/api/settings/archive/legacy/import",
        &with_ids(body.clone(), &[&a]),
    )
    .await;
    assert_eq!(code, 202);
    let job = done(&p).await;
    assert_eq!(job["records"][0]["state"], "imported", "{job}");
    let id = job["records"][0]["hars_id"].as_str().unwrap().to_owned();
    assert_eq!(
        std::fs::read(root.join(&id).join("rlog.zst")).unwrap(),
        rlog
    );
    let (code, _) = call(
        &p,
        "POST",
        "/api/settings/archive/legacy/import",
        &with_ids(body, &[&a]),
    )
    .await;
    assert_eq!(code, 202);
    let job = done(&p).await;
    assert_eq!(
        (
            job["records"][0]["state"].clone(),
            job["records"][0]["hars_id"].clone()
        ),
        (json!("duplicate"), json!(id))
    );
    let (_, list) = call(&p, "GET", "/api/logs", &Value::Null).await;
    assert_eq!(list["logs"].as_array().unwrap().len(), 1);
    assert_eq!(std::fs::read_dir(root.join(".uploads")).unwrap().count(), 0);
    drop(p);
    if drive {
        assert_drive_untouched(&fake, mark, &before);
    } else {
        assert_eq!(Some(tree(&share)), share_before);
        read_only(&share, false);
    }
}

#[tokio::test]
async fn sigkill_mid_drive_download_then_restart_converges() {
    killed_and_restarted("legacy_import_mid_file", true).await;
}
#[tokio::test]
async fn sigkill_mid_share_copy_then_restart_converges() {
    killed_and_restarted("legacy_import_mid_file", false).await;
}
#[tokio::test]
async fn sigkill_before_registration_then_restart_converges() {
    killed_and_restarted("legacy_import_before_finish", true).await;
}

#[tokio::test]
async fn the_command_line_works_without_a_server_and_refuses_beside_one() {
    let fake = start_fake().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(".processing-settings.json"),
        br#"{"auto_convert":false}"#,
    )
    .unwrap();
    let share = tmp.path().join("python-share");
    let a = pid(1);
    let rlog = data(5000, 1);
    py_share(&share, &[(&a, &[("rlog.zst", rlog.clone())])]);
    read_only(&share, true);
    let before = tree(&share);
    let run = |verb: &str, body: Value| {
        let out = Command::new(env!("CARGO_BIN_EXE_roadviewer-hars"))
            .args(["legacy-archive", verb, &body.to_string()])
            .env("HARS_DATA", &root)
            .env("RV_ARCHIVE_ALLOW_ANY_PATH", "1")
            .env("RUST_LOG", "off")
            .stderr(Stdio::null())
            .output()
            .unwrap();
        (
            out.status.code(),
            serde_json::from_slice::<Value>(&out.stdout).unwrap_or(Value::Null),
        )
    };
    let (code, scan) = run("scan", nas_body(&share));
    assert_eq!(code, Some(0), "{scan}");
    assert_eq!(scan["records"][0]["id"], a.as_str());
    let (code, out) = run("import", with_ids(nas_body(&share), &[&a]));
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(out["job"]["records"][0]["state"], "imported", "{out}");
    let id = out["job"]["records"][0]["hars_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        std::fs::read(root.join(&id).join("rlog.zst")).unwrap(),
        rlog
    );
    let (code, out) = run("scan", json!({"backend": "ftp"}));
    assert_eq!(
        (code, out["error"].clone()),
        (Some(1), json!("invalid_legacy_request"))
    );
    // The server holds the folder: the command line does not open it.
    let p = start(&root, &fake.url, None).await;
    let (code, out) = run("import", with_ids(nas_body(&share), &[&a]));
    assert_ne!(code, Some(0), "{out}");
    let (_, list) = call(&p, "GET", "/api/logs", &Value::Null).await;
    assert_eq!(list["logs"].as_array().unwrap().len(), 1);
    drop(p);
    assert_eq!(tree(&share), before);
    read_only(&share, false);
}
