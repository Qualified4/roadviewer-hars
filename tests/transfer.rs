//! Export/import, backward compatibility with a Road Viewer 1.x export, and ZIP downloads.
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use roadviewer_hars::{
    http::{App, ingress},
    store::{atomic_json, id, sha},
};
use serde_json::{Value, json};
use std::{io::Read, path::PathBuf, time::Duration};
use tower::ServiceExt;

fn web() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web")
}
async fn app(root: &std::path::Path) -> App {
    let a = App::open(root.to_path_buf(), web(), false, 512)
        .await
        .unwrap();
    a.store.processing.write().await["auto_convert"] = json!(false);
    a
}
async fn raw(
    router: Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-roadviewer-request", "1")
        .body(Body::from(body))
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(30), router.oneshot(request))
        .await
        .expect("handler deadlock")
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 256 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}
async fn call(a: &App, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let body = if body.is_null() {
        vec![]
    } else {
        serde_json::to_vec(&body).unwrap()
    };
    let (status, _, bytes) = raw(ingress(a.clone()), method, path, body).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn recording(a: &App, name: &str, files: &[(&str, &[u8])], extra: Value) -> String {
    let record = id();
    let folder = a.store.root.join(&record);
    tokio::fs::create_dir_all(&folder).await.unwrap();
    let mut names = serde_json::Map::new();
    let mut hashes = serde_json::Map::new();
    for (kind, data) in files {
        tokio::fs::write(folder.join(kind), data).await.unwrap();
        names.insert(
            (*kind).into(),
            json!(format!("000003fe--d524dc1e0b--3--{kind}")),
        );
        hashes.insert(
            (*kind).into(),
            json!({"size":data.len(),"sha256":sha(data)}),
        );
    }
    let mut meta = json!({"id":record,"name":name,"uploaded":1700000000.5,"status":"unconverted",
        "files":names,"content_hashes":hashes,"error":null});
    meta.as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    a.store.save_meta(meta).await.unwrap();
    record
}
struct Member {
    name: String,
    data: Vec<u8>,
}
fn members(tar_bytes: &[u8]) -> Vec<Member> {
    let mut archive = tar::Archive::new(tar_bytes);
    archive
        .entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            assert_eq!(e.header().entry_type(), tar::EntryType::Regular);
            let name = e.path().unwrap().to_string_lossy().into_owned();
            let mut data = vec![];
            e.read_to_end(&mut data).unwrap();
            Member { name, data }
        })
        .collect()
}
/// What `log-transfer.js` does with an export file: one import session per recording.
async fn browser_import(a: &App, tar_bytes: &[u8]) -> Vec<Value> {
    let members = members(tar_bytes);
    assert_eq!(members[0].name, "manifest.json");
    let manifest: Value = serde_json::from_slice(&members[0].data).unwrap();
    assert_eq!(manifest["format"], "roadviewer-export");
    assert_eq!(manifest["version"], 1);
    let mut groups: Vec<(String, Vec<&Member>)> = vec![];
    for m in &members[1..] {
        let (id, _) = m.name.split_once('/').unwrap();
        match groups.iter_mut().find(|g| g.0 == id) {
            Some(g) => g.1.push(m),
            None => groups.push((id.into(), vec![m])),
        }
    }
    let mut results = vec![];
    for (_, group) in groups {
        let record: Value = serde_json::from_slice(
            &group
                .iter()
                .find(|m| m.name.ends_with("/record.json"))
                .unwrap()
                .data,
        )
        .unwrap();
        let (status, mut result) = call(
            a,
            "POST",
            "/api/logs/import",
            json!({"format":manifest["format"],"version":manifest["version"],"record":record}),
        )
        .await;
        assert!(status.is_success(), "{status} {result}");
        if let Some(session) = result["session"].as_str().map(str::to_owned) {
            for (i, kind) in result["files"].as_array().unwrap().iter().enumerate() {
                let member = group
                    .iter()
                    .find(|m| m.name.ends_with(&format!("/{}", kind.as_str().unwrap())))
                    .unwrap();
                let chunk = result["chunk_size"].as_u64().unwrap() as usize;
                for (n, part) in member.data.chunks(chunk).enumerate() {
                    let (status, _, _) = raw(
                        ingress(a.clone()),
                        "PUT",
                        &format!("/api/uploads/{session}/files/{i}?offset={}", n * chunk),
                        part.to_vec(),
                    )
                    .await;
                    assert_eq!(status, StatusCode::OK);
                }
            }
            let (status, finished) = call(
                a,
                "POST",
                &format!("/api/logs/import/{session}/finish"),
                Value::Null,
            )
            .await;
            assert!(status.is_success(), "{status} {finished}");
            result = finished;
        }
        results.push(result);
    }
    results
}
#[tokio::test]
async fn export_import_round_trip_preserves_files_and_record() {
    let source = tempfile::tempdir().unwrap();
    let a = app(source.path()).await;
    let first = recording(
        &a,
        "000003fe--d524dc1e0b / 구간 3",
        &[("rlog.zst", b"log-three"), ("qcamera.ts", b"ts-bytes")],
        json!({"tags":[{"name":"출근","color":"red"}],"bookmarks":[{"id":"b1","time":2,"title":"t","note":"n"}],
            "video_codecs":{"qcamera":"h264"},"video_origins":{"qcamera":1.25}}),
    )
    .await;
    let second = recording(&a, "custom name", &[("rlog.zst", b"other-log")], json!({})).await;
    let (status, _) = call(
        &a,
        "POST",
        &format!("/api/logs/{first}/pin"),
        json!({"pinned":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, tar_bytes) =
        raw(ingress(a.clone()), "GET", "/api/logs/export.tar", vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/x-tar");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(
        headers["content-length"]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        tar_bytes.len()
    );
    let disposition = headers["content-disposition"].to_str().unwrap();
    assert!(
        disposition.starts_with("attachment; filename=\"roadviewer-export-")
            && disposition.ends_with(".tar\"")
    );
    let names = members(&tar_bytes)
        .into_iter()
        .map(|m| m.name)
        .collect::<Vec<_>>();
    // recording_order: custom names first, then numbered routes; record.json after its files.
    assert_eq!(
        names,
        [
            "manifest.json".to_owned(),
            format!("{second}/rlog.zst"),
            format!("{second}/record.json"),
            format!("{first}/rlog.zst"),
            format!("{first}/qcamera.ts"),
            format!("{first}/record.json"),
        ]
    );
    let target = tempfile::tempdir().unwrap();
    let b = app(target.path()).await;
    let results = browser_import(&b, &tar_bytes).await;
    assert!(
        results
            .iter()
            .all(|r| r["status"] == "unconverted" && r["imported"].is_string())
    );
    let meta = b.store.meta(&first).await.unwrap();
    assert_eq!(meta["name"], "000003fe--d524dc1e0b / 구간 3");
    assert_eq!(meta["uploaded"], json!(1700000000.5));
    assert_eq!(meta["tags"], json!([{"name":"출근","color":"red"}]));
    assert_eq!(
        meta["bookmarks"],
        json!([{"id":"b1","time":2,"title":"t","note":"n"}])
    );
    assert_eq!(meta["video_origins"], json!({"qcamera":1.25}));
    assert_eq!(
        meta["files"]["qcamera.ts"],
        "000003fe--d524dc1e0b--3--qcamera.ts"
    );
    assert_eq!(meta["video"], true);
    assert_eq!(meta["bytes"], 17);
    assert_eq!(
        std::fs::read(target.path().join(&first).join("qcamera.ts")).unwrap(),
        b"ts-bytes"
    );
    assert_eq!(
        meta["content_hashes"]["rlog.zst"]["sha256"],
        sha(b"log-three")
    );
    let settings: Value = serde_json::from_slice(
        &std::fs::read(target.path().join(".storage-settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["pinned_logs"], json!([first]));
    // The same export again: every record is skipped (its id exists), nothing is duplicated.
    let again = browser_import(&b, &tar_bytes).await;
    assert!(again.iter().all(|r| r["skipped"] == "exists"));
    assert!(
        std::fs::read_dir(target.path().join(".uploads"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn legacy_export_imports_and_duplicates_are_skipped() {
    // Synthetic Road Viewer 1.3.16 export with two records sharing identical log bytes.
    let tar_bytes = include_bytes!("fixtures/legacy-export.tar");
    let one = "11111111111111111111111111111111";
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    let results = browser_import(&a, tar_bytes).await;
    assert_eq!(results[0]["imported"], one);
    // The second record has another id but the same rlog.zst content.
    assert_eq!(results[1]["skipped"], "duplicate");
    let meta = a.store.meta(one).await.unwrap();
    assert_eq!(meta["tags"], json!([{"name":"Trip","color":"blue"}]));
    assert_eq!(
        std::fs::read(root.path().join(one).join("rlog.zst")).unwrap(),
        b"python-log"
    );
    let (_, list) = call(&a, "GET", "/api/logs", Value::Null).await;
    assert_eq!(list["logs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn import_rejects_unsafe_records_and_cleans_interrupted_work() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    let record = id();
    let good =
        json!({"id":record,"name":"n","originals":{"rlog.zst":{"bytes":4,"sha256":sha(b"data")}}});
    let cases = [
        json!({"id":"../../etc","name":"n","originals":{"rlog.zst":{"bytes":4,"sha256":sha(b"data")}}}),
        json!({"id":record,"name":"n","files":{"rlog.zst":"../../x"},"originals":{"rlog.zst":{"bytes":4,"sha256":sha(b"data")}}}),
        json!({"id":record,"name":"n","files":{"rlog.zst":"/etc/passwd"},"originals":{"rlog.zst":{"bytes":4,"sha256":sha(b"data")}}}),
        json!({"id":record,"name":"n","originals":{"../meta.json":{"bytes":4,"sha256":sha(b"data")}}}),
        json!({"id":record,"name":"n","originals":{"meta.json":{"bytes":4,"sha256":sha(b"data")}}}),
        json!({"id":record,"name":"n","originals":{}}),
        json!({"id":record,"name":"\u{0007}","originals":{"rlog.zst":{"bytes":4,"sha256":sha(b"data")}}}),
    ];
    for bad in cases {
        let (status, body) = call(
            &a,
            "POST",
            "/api/logs/import",
            json!({"format":"roadviewer-export","version":1,"record":bad}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_import");
        assert!(body["message"].is_string());
    }
    let (status, body) = call(
        &a,
        "POST",
        "/api/logs/import",
        json!({"format":"roadviewer-export","version":2,"record":good}),
    )
    .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_import"))
    );
    assert!(body["message"].as_str().unwrap().contains("(2)"));

    // A checksum mismatch removes the session and registers nothing.
    let (status, begun) = call(
        &a,
        "POST",
        "/api/logs/import",
        json!({"format":"roadviewer-export","version":1,"record":good}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session = begun["session"].as_str().unwrap().to_owned();
    assert_eq!(begun["files"], json!(["rlog.zst"]));
    let (status, body) = call(
        &a,
        "POST",
        &format!("/api/uploads/{session}/finish"),
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("invalid_upload"))
    );
    let (status, body) = call(
        &a,
        "POST",
        &format!("/api/logs/import/{session}/finish"),
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("incomplete_upload"))
    );
    let (status, _, _) = raw(
        ingress(a.clone()),
        "PUT",
        &format!("/api/uploads/{session}/files/0?offset=0"),
        b"DATA".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &a,
        "POST",
        &format!("/api/logs/import/{session}/finish"),
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::UNPROCESSABLE_ENTITY, json!("checksum_mismatch"))
    );
    assert!(!root.path().join(".uploads").join(&session).exists());
    assert!(a.store.meta(&record).await.is_err());
    assert!(!root.path().join(&record).exists());

    // Interrupted mid-upload and mid-registration: after restart the staging copy is gone,
    // the record is not listed and the half-sent session can still be cancelled.
    let (_, begun) = call(
        &a,
        "POST",
        "/api/logs/import",
        json!({"format":"roadviewer-export","version":1,"record":good}),
    )
    .await;
    let session = begun["session"].as_str().unwrap().to_owned();
    raw(
        ingress(a.clone()),
        "PUT",
        &format!("/api/uploads/{session}/files/0?offset=0"),
        b"da".to_vec(),
    )
    .await;
    let crashed = root.path().join(".import-0123").join(&record);
    std::fs::create_dir_all(&crashed).unwrap();
    std::fs::write(crashed.join("rlog.zst"), b"data").unwrap();
    atomic_json(&crashed.join("meta.json"), &json!({"id":record}))
        .await
        .unwrap();
    drop(a);
    let a = app(root.path()).await;
    assert!(!root.path().join(".import-0123").exists());
    let (_, list) = call(&a, "GET", "/api/logs", Value::Null).await;
    assert!(list["logs"].as_array().unwrap().is_empty());
    let (status, _) = call(
        &a,
        "DELETE",
        &format!("/api/uploads/{session}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &a,
        "POST",
        "/api/logs/import/0123456789abcdef0123456789abcdef/finish",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn zip_downloads_follow_python_names_and_are_valid_zip64() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    let one = recording(
        &a,
        "000003fe--d524dc1e0b / 구간 3",
        &[("rlog.zst", b"log-a"), ("fcamera.hevc", b"raw-front")],
        json!({}),
    )
    .await;
    let two = recording(
        &a,
        "000003fe--d524dc1e0b / 구간 3",
        &[("rlog.zst", b"log-b")],
        json!({}),
    )
    .await;
    std::fs::create_dir_all(root.path().join(&one).join("prepared")).unwrap();
    std::fs::write(
        root.path().join(&one).join("prepared/fcamera.mp4"),
        b"mp4-front",
    )
    .unwrap();
    let check = |bytes: &[u8]| -> Value {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.zip");
        std::fs::write(&path, bytes).unwrap();
        let out = std::process::Command::new("python3")
            .args(["-I", "-c", "import zipfile,sys,json,hashlib\nz=zipfile.ZipFile(sys.argv[1]);assert z.testzip() is None\nprint(json.dumps({i.filename:hashlib.sha256(z.read(i)).hexdigest() for i in z.infolist()}))"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let (status, headers, body) = raw(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/download.zip?ids={one},{two},{one}&parts=rlog,front"),
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/zip");
    assert!(
        headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"roadviewer-2-segments-")
    );
    let files = check(&body);
    let mut expected = serde_json::Map::new();
    expected.insert(
        "000003fe--d524dc1e0b--3--rlog.zst".into(),
        json!(sha(b"log-a")),
    );
    expected.insert(
        "000003fe--d524dc1e0b--3--fcamera.hevc".into(),
        json!(sha(b"raw-front")),
    );
    expected.insert(
        format!("{}-000003fe--d524dc1e0b--3--rlog.zst", &two[..8]),
        json!(sha(b"log-b")),
    );
    assert_eq!(files, Value::Object(expected));
    let (status, headers, body) = raw(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/download.zip?ids={one}&parts=front&format=mp4"),
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"roadviewer-1-segments-mp4.zip\""
    );
    assert_eq!(
        check(&body),
        json!({"000003fe--d524dc1e0b--3--fcamera.mp4":sha(b"mp4-front")})
    );
    for bad in [
        "/api/logs/download.zip".to_owned(),
        format!("/api/logs/download.zip?ids={one}&parts=meta"),
        format!("/api/logs/download.zip?ids={one}&format=avi"),
    ] {
        assert_eq!(
            raw(ingress(a.clone()), "GET", &bad, vec![]).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let missing = "0123456789abcdef0123456789abcdef";
    assert_eq!(
        raw(
            ingress(a.clone()),
            "GET",
            &format!("/api/logs/download.zip?ids={missing}"),
            vec![]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        raw(
            ingress(a.clone()),
            "GET",
            &format!("/api/logs/download.zip?ids={two}&parts=wide"),
            vec![]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // A single log download is the rlog by default, named as uploaded.
    let (status, headers, body) = raw(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/{one}/download"),
        vec![],
    )
    .await;
    assert_eq!((status, body), (StatusCode::OK, b"log-a".to_vec()));
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"000003fe--d524dc1e0b--3--rlog.zst\""
    );
    assert_eq!(headers["cache-control"], "no-store");
    let (_, headers, _) = raw(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/{one}/original/rlog.zst"),
        vec![],
    )
    .await;
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=000003fe--d524dc1e0b--3--rlog.zst"
    );
    assert_eq!(headers["cache-control"], "no-cache");
}

#[tokio::test]
async fn zip_members_are_opened_while_streaming_and_vanished_ones_are_left_out() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    // Larger than the response channel holds, so the stream is still inside this member.
    let big = (0..8 * 1024 * 1024u32)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let one = recording(&a, "a / 구간 3", &[("rlog.zst", &big)], json!({})).await;
    let two = recording(&a, "b / 구간 4", &[("rlog.zst", b"log-b")], json!({})).await;
    let request = Request::builder()
        .uri(format!("/api/logs/download.zip?ids={one},{two}&parts=rlog"))
        .body(Body::empty())
        .unwrap();
    let response = ingress(a.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Removed after the request was planned (Python `zip_stream`: "Removed since the request started").
    std::fs::remove_file(root.path().join(&two).join("rlog.zst")).unwrap();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.zip");
    std::fs::write(&path, &bytes).unwrap();
    let out = std::process::Command::new("python3")
        .args(["-I", "-c", "import zipfile,sys,json,hashlib\nz=zipfile.ZipFile(sys.argv[1]);assert z.testzip() is None\nprint(json.dumps({i.filename:hashlib.sha256(z.read(i)).hexdigest() for i in z.infolist()}))"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let files: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        files,
        json!({"000003fe--d524dc1e0b--3--rlog.zst":sha(&big)}),
        "a member opened up front would still have been sent"
    );
}
