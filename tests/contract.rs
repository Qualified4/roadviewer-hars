use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use hmac::{Hmac, Mac};
use roadviewer_hars::{
    auth::{derive_secret, random_token},
    http::{App, device, ingress},
    store::{atomic_json, sha},
};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{path::PathBuf, time::Duration};
use tower::ServiceExt;

fn web() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web")
}
async fn app(root: &std::path::Path) -> App {
    App::open(root.to_path_buf(), web(), false, 512)
        .await
        .unwrap()
}
async fn req(
    router: Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        router.oneshot(builder.body(Body::from(body)).unwrap()),
    )
    .await
    .expect("HTTP handler deadlock")
    .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn credentials(app: &App) -> Value {
    let (status, p) = req(
        ingress(app.clone()),
        "POST",
        "/api/settings/devices/pairing",
        vec![],
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, creds) = req(
        device(app.clone()),
        "POST",
        "/api/device/pair",
        serde_json::to_vec(&json!({"code":p["code"],"metadata":{"name":"Test device"}})).unwrap(),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    creds
}

#[tokio::test]
async fn prepared_mp4_download_uses_existing_ui_routes_and_range() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let record = roadviewer_hars::store::id();
    let folder = temp.path().join(&record);
    tokio::fs::create_dir_all(folder.join("prepared"))
        .await
        .unwrap();
    tokio::fs::write(folder.join("prepared/fcamera.mp4"), b"0123456789")
        .await
        .unwrap();
    a.store.save_meta(json!({"id":record,"name":"test","status":"ready","files":{"fcamera.hevc":"route--0--fcamera.hevc"},"video_downloads":[{"source":"front","kind":"fcamera.mp4","bytes":10}]})).await.unwrap();
    for suffix in [
        "original/fcamera.mp4",
        "download/video",
        "download?source=front&format=mp4",
    ] {
        let request = Request::builder()
            .uri(format!("/api/logs/{record}/{suffix}"))
            .header("range", "bytes=2-5")
            .body(Body::empty())
            .unwrap();
        let response = ingress(a.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["content-range"], "bytes 2-5/10");
        assert!(
            response.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .contains("route--0--fcamera.mp4")
        );
        assert_eq!(
            to_bytes(response.into_body(), 100).await.unwrap(),
            &b"2345"[..]
        );
    }
}

#[tokio::test]
async fn startup_fills_download_metadata_without_reconverting_existing_output() {
    let temp = tempfile::tempdir().unwrap();
    let record = roadviewer_hars::store::id();
    let folder = temp.path().join(&record);
    tokio::fs::create_dir_all(folder.join("prepared"))
        .await
        .unwrap();
    tokio::fs::write(folder.join("prepared/ecamera.mp4"), b"retained-wide")
        .await
        .unwrap();
    atomic_json(
        &folder.join("meta.json"),
        &json!({"id":record,"name":"existing","status":"ready","video":true}),
    )
    .await
    .unwrap();
    let a = app(temp.path()).await;
    let meta = a.store.meta(&record).await.unwrap();
    assert_eq!(meta["status"], "ready");
    assert_eq!(
        meta["video_downloads"],
        json!([{"source":"wide","kind":"ecamera.mp4","bytes":13}])
    );
    assert_eq!(
        tokio::fs::read(folder.join("prepared/ecamera.mp4"))
            .await
            .unwrap(),
        b"retained-wide"
    );
}
#[tokio::test]
async fn startup_removes_the_staging_of_a_conversion_a_killed_server_left() {
    let temp = tempfile::tempdir().unwrap();
    let record = roadviewer_hars::store::id();
    let folder = temp.path().join(&record);
    let staging = folder.join(format!(".convert-{}", roadviewer_hars::store::id()));
    tokio::fs::create_dir_all(staging.join("prepared"))
        .await
        .unwrap();
    tokio::fs::write(staging.join("prepared/fcamera.mp4"), b"partial")
        .await
        .unwrap();
    tokio::fs::write(folder.join("rlog.zst"), b"original")
        .await
        .unwrap();
    atomic_json(
        &folder.join("meta.json"),
        &json!({"id":record,"name":"killed","status":"processing","conversion_id":"x"}),
    )
    .await
    .unwrap();
    let a = app(temp.path()).await;
    assert_eq!(a.store.meta(&record).await.unwrap()["status"], "queued");
    assert!(!staging.exists());
    assert_eq!(
        tokio::fs::read(folder.join("rlog.zst")).await.unwrap(),
        b"original"
    );
}
fn manifest(data: &[u8], streaming: bool) -> Value {
    json!({"batch_id":"test_batch_123456789","segments":[{"route":"2026-11-15--12-00-00","segment":0,"files":[{"kind":"rlog.zst","size":data.len(),"sha256":if streaming {Value::Null}else{json!(sha(data))}}]}]})
}
fn signed(creds: &Value, body: &[u8], nonce: &str, stamp: Option<u64>) -> HeaderMap {
    let id = creds["device_id"].as_str().unwrap();
    let stamp = stamp
        .unwrap_or_else(|| roadviewer_hars::store::now() as u64)
        .to_string();
    let canonical = format!(
        "RV1\n{id}\n{stamp}\n{nonce}\nPOST\n/api/device/uploads\n{}",
        sha(body)
    );
    let mut mac =
        Hmac::<Sha256>::new_from_slice(creds["device_secret"].as_str().unwrap().as_bytes())
            .unwrap();
    mac.update(canonical.as_bytes());
    let mut headers = HeaderMap::new();
    for (k, v) in [
        ("x-rv-device", id.to_owned()),
        ("x-rv-timestamp", stamp),
        ("x-rv-nonce", nonce.to_owned()),
        ("x-rv-signature", hex::encode(mac.finalize().into_bytes())),
        ("x-rv-max-chunk", "8388608".to_owned()),
        ("x-rv-checksum", "sha256-stream".to_owned()),
    ] {
        headers.insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    headers
}
async fn begin(app: &App, creds: &Value, body: &Value) -> Value {
    let body = serde_json::to_vec(body).unwrap();
    let headers = signed(creds, &body, &random_token(), None);
    let (status, value) = tokio::time::timeout(
        Duration::from_secs(5),
        app.uploads.begin_device(&headers, &body, false),
    )
    .await
    .expect("begin deadlock")
    .unwrap();
    assert!(status == StatusCode::CREATED || status == StatusCode::OK);
    value
}
fn bearer(session: &Value) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {}", session["token"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    h
}

#[tokio::test]
async fn listeners_enforce_ingress_csrf_and_public_separation() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    assert_eq!(
        req(
            ingress(a.clone()),
            "POST",
            "/api/settings/devices/pairing",
            vec![],
            &[]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        req(
            ingress(a.clone()),
            "POST",
            "/api/device/pair",
            b"{}".to_vec(),
            &[("x-roadviewer-request", "1")]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        req(
            device(a.clone()),
            "GET",
            "/api/settings/devices",
            vec![],
            &[]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let mut restricted = a.clone();
    restricted.ingress_only = true;
    assert_eq!(
        req(ingress(restricted), "GET", "/api/logs", vec![], &[])
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn streaming_upload_retry_finish_and_lost_response_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let creds = credentials(&a).await;
    let data = b"abc123";
    let batch = manifest(data, true);
    let session = begin(&a, &creds, &batch).await;
    let id = session["id"].as_str().unwrap();
    let headers = bearer(&session);
    let public = req(
        device(a.clone()),
        "PUT",
        &format!("/api/device/uploads/{id}/files/0?offset=0"),
        data[..3].to_vec(),
        &[("authorization", headers["authorization"].to_str().unwrap())],
    )
    .await;
    assert_eq!(public.0, StatusCode::OK);
    assert_eq!(public.1["received"], 3);
    assert_eq!(
        a.uploads
            .chunk(id, 0, Some(0), &data[..3], true, &headers)
            .await
            .unwrap()["received"],
        3
    );
    assert_eq!(
        a.uploads
            .chunk(id, 0, Some(0), b"bad", true, &headers)
            .await
            .unwrap_err()
            .1,
        "offset_conflict"
    );
    assert_eq!(
        a.uploads
            .finish(id, true, &headers, json!({"sha256":[sha(data)]}))
            .await
            .unwrap_err()
            .1,
        "incomplete_upload"
    );
    a.uploads
        .chunk(id, 0, Some(3), &data[3..], true, &headers)
        .await
        .unwrap();
    assert_eq!(
        a.uploads
            .finish(id, true, &headers, json!({"sha256":[sha(b"bad")]}))
            .await
            .unwrap_err()
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Python's `finish_upload` removes the session after a failed check (its `finally`), so the
    // uploader's next begin of the same batch starts over instead of meeting the same bytes.
    assert_eq!(
        a.uploads
            .finish(id, true, &headers, json!({"sha256":[sha(data)]}))
            .await
            .unwrap_err()
            .1,
        "session_not_found"
    );
    assert!(!temp.path().join(".uploads").join(id).exists());
    let session = begin(&a, &creds, &batch).await;
    assert_ne!(session["id"].as_str().unwrap(), id);
    assert_eq!(session["files"][0]["received"], 0);
    let id = session["id"].as_str().unwrap();
    let headers = bearer(&session);
    a.uploads
        .chunk(id, 0, Some(0), data, true, &headers)
        .await
        .unwrap();
    let (status, result) = a
        .uploads
        .finish(id, true, &headers, json!({"sha256":[sha(data)]}))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(result["logs"].as_array().unwrap().len(), 1);
    assert_eq!(
        a.uploads
            .finish(id, true, &headers, Value::Null)
            .await
            .unwrap()
            .1,
        result
    );
    assert_eq!(
        a.uploads.status(id, &headers).await.unwrap()["state"],
        "completed"
    );
    assert_eq!(
        a.uploads
            .chunk(id, 0, Some(0), data, true, &headers)
            .await
            .unwrap_err()
            .1,
        "session_completed"
    );
    assert_eq!(begin(&a, &creds, &batch).await["state"], "completed");
    let record = result["logs"][0]["id"].as_str().unwrap();
    assert!(
        tokio::fs::read(temp.path().join(record).join("rlog.zst"))
            .await
            .unwrap()
            == data
    );
}
#[tokio::test]
async fn nonce_and_offsets_survive_restart_and_token_rotation() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let creds = credentials(&a).await;
    let batch = manifest(b"abcd", false);
    let bytes = serde_json::to_vec(&batch).unwrap();
    let signed = signed(&creds, &bytes, "unique_nonce_123456", None);
    let (_, session) = a
        .uploads
        .begin_device(&signed, &bytes, false)
        .await
        .unwrap();
    let name = session["id"].as_str().unwrap().to_owned();
    let old = bearer(&session);
    a.uploads
        .chunk(&name, 0, Some(0), b"ab", true, &old)
        .await
        .unwrap();
    drop(a);
    let a = app(temp.path()).await;
    assert_eq!(
        a.uploads
            .begin_device(&signed, &bytes, false)
            .await
            .unwrap_err()
            .1,
        "nonce_replayed"
    );
    assert_eq!(
        a.uploads.status(&name, &old).await.unwrap()["files"][0]["received"],
        2
    );
    let renewed = begin(&a, &creds, &batch).await;
    let new = bearer(&renewed);
    assert_eq!(
        a.uploads.status(&name, &old).await.unwrap_err().1,
        "invalid_upload_token"
    );
    a.uploads
        .chunk(&name, 0, Some(2), b"cd", true, &new)
        .await
        .unwrap();
    a.uploads
        .finish(&name, true, &new, Value::Null)
        .await
        .unwrap();
}
#[tokio::test]
async fn revoked_device_cannot_resume_or_finish() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let creds = credentials(&a).await;
    let session = begin(&a, &creds, &manifest(b"abc", false)).await;
    let headers = bearer(&session);
    let name = session["id"].as_str().unwrap();
    a.auth
        .device_action(creds["device_id"].as_str().unwrap(), false)
        .await
        .unwrap();
    assert_eq!(
        a.uploads
            .chunk(name, 0, Some(0), b"abc", true, &headers)
            .await
            .unwrap_err()
            .1,
        "invalid_device"
    );
    assert_eq!(
        a.uploads
            .finish(name, true, &headers, Value::Null)
            .await
            .unwrap_err()
            .1,
        "invalid_device"
    );
}
#[tokio::test]
async fn malformed_batches_and_stale_authentication_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let creds = credentials(&a).await;
    let body = serde_json::to_vec(&manifest(b"abc", false)).unwrap();
    let headers = signed(&creds, &body, &random_token(), Some(1700000000));
    assert_eq!(
        a.uploads
            .begin_device(&headers, &body, false)
            .await
            .unwrap_err()
            .1,
        "stale_timestamp"
    );
    let mut batch = manifest(b"abc", false);
    batch["segments"][0]["route"] = json!("../../outside");
    assert_eq!(
        roadviewer_hars::upload::flatten_batch(&batch, false, 512 * 1024 * 1024)
            .unwrap_err()
            .1,
        "invalid_segment"
    );
    batch = manifest(b"abc", false);
    batch["segments"][0]["segment"] = json!(true);
    assert_eq!(
        roadviewer_hars::upload::flatten_batch(&batch, false, 512 * 1024 * 1024)
            .unwrap_err()
            .1,
        "invalid_segment"
    );
    batch = manifest(b"abc", false);
    batch["segments"][0]["files"][0]["sha256"] = Value::Null;
    assert_eq!(
        roadviewer_hars::upload::flatten_batch(&batch, false, 512 * 1024 * 1024)
            .unwrap_err()
            .1,
        "invalid_file"
    );
}
#[tokio::test]
async fn browser_cannot_access_device_sessions_and_path_traversal_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let creds = credentials(&a).await;
    let session = begin(&a, &creds, &manifest(b"abc", false)).await;
    let name = session["id"].as_str().unwrap();
    assert_eq!(
        a.uploads
            .chunk(name, 0, Some(0), b"abc", false, &HeaderMap::new())
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    for name in ["../rlog.zst", "/rlog.zst", "C:/rlog.zst"] {
        assert_eq!(
            a.uploads
                .begin_browser(json!({"files":[{"name":name,"size":1}]}))
                .await
                .unwrap_err()
                .1,
            "invalid_file_path"
        );
    }
}
#[tokio::test]
async fn disk_quota_reserves_unreceived_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let used = a.store.used().await.unwrap();
    atomic_json(
        &temp.path().join(".storage-settings.json"),
        &json!({"max_bytes":used+100000,"policy":"reject_new"}),
    )
    .await
    .unwrap();
    a.uploads
        .begin_browser(json!({"files":[{"name":"rlog.zst","size":50000}]}))
        .await
        .unwrap_err();
    atomic_json(
        &temp.path().join(".storage-settings.json"),
        &json!({"max_bytes":used+200000,"policy":"reject_new"}),
    )
    .await
    .unwrap();
    a.uploads
        .begin_browser(json!({"files":[{"name":"rlog.zst","size":80000}]}))
        .await
        .unwrap();
    assert_eq!(
        a.uploads
            .begin_browser(json!({"files":[{"name":"rlog.zst","size":80000}]}))
            .await
            .unwrap_err()
            .1,
        "storage_limit_exceeded"
    );
}
#[tokio::test]
async fn metadata_is_persistent_and_bookmarks_are_validated() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let name = roadviewer_hars::store::id();
    tokio::fs::create_dir(temp.path().join(&name))
        .await
        .unwrap();
    a.store
        .save_meta(json!({"id":name,"status":"ready","name":"test","duration":10,"files":{}}))
        .await
        .unwrap();
    let (status, result) = req(
        ingress(a.clone()),
        "POST",
        &format!("/api/logs/{name}/bookmarks"),
        serde_json::to_vec(&json!({"time":1,"title":"  한글   북마크  ","note":"메모"})).unwrap(),
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(result["bookmarks"][0]["title"], "한글 북마크");
    assert_eq!(
        req(
            ingress(a.clone()),
            "POST",
            &format!("/api/logs/{name}/bookmarks"),
            serde_json::to_vec(&json!({"time":11,"title":"x","note":""})).unwrap(),
            &[("x-roadviewer-request", "1")]
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    drop(a);
    let a = app(temp.path()).await;
    assert_eq!(
        a.store.meta(&name).await.unwrap()["bookmarks"][0]["note"],
        "메모"
    );
}
#[tokio::test]
async fn same_data_directory_cannot_have_two_writers() {
    let temp = tempfile::tempdir().unwrap();
    let _a = app(temp.path()).await;
    assert!(
        App::open(temp.path().to_path_buf(), web(), false, 512)
            .await
            .is_err()
    );
}
#[cfg(unix)]
#[tokio::test]
async fn data_directory_lock_ends_with_its_app_while_a_child_is_starting() {
    use std::{io::Read, os::fd::AsRawFd, os::unix::process::CommandExt};
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (mut started, signal) = std::io::pipe().unwrap();
    let signal_fd = signal.as_raw_fd();
    // A child between fork and exec holds a copy of every descriptor of this process, the lock's among them.
    let child = std::thread::spawn(move || {
        let mut command = std::process::Command::new("true");
        // SAFETY: write(2) and nanosleep(2) are async-signal-safe; the closure only reads the copied descriptor.
        unsafe {
            command.pre_exec(move || {
                libc::write(signal_fd, b"x".as_ptr().cast(), 1);
                std::thread::sleep(Duration::from_millis(800));
                Ok(())
            });
        }
        let status = command.status();
        drop(signal);
        status
    });
    started.read_exact(&mut [0]).unwrap();
    drop(a);
    let reopened = App::open(temp.path().to_path_buf(), web(), false, 512).await;
    assert!(child.join().unwrap().unwrap().success());
    reopened.unwrap();
}
#[test]
fn root_secret_derivation_matches_python_hmac() {
    // Public, synthetic test vector. Comparison avoids displaying secrets on failure.
    let master = "00".repeat(32);
    let device = "1234567890abcdef1234567890abcdef";
    let expected=std::process::Command::new("python3").args(["-c","import hmac,hashlib; print(hmac.new(bytes(32), b'roadviewer-device-v1:1234567890abcdef1234567890abcdef', hashlib.sha256).hexdigest())"]).output().unwrap();
    assert!(
        derive_secret(&master, device).unwrap()
            == String::from_utf8(expected.stdout).unwrap().trim()
    );
}

#[tokio::test]
async fn proxy_request_id_is_correlated_and_progress_keeps_the_python_schema() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let correlation = "0123456789abcdef0123456789abcdef";
    let response = ingress(a.clone())
        .oneshot(
            Request::builder()
                .uri("/api/progress")
                .header("x-request-id", correlation)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["x-request-id"], correlation);
    let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value, json!({"progress":{}}));
    let response = ingress(a)
        .oneshot(
            Request::builder()
                .uri("/api/progress")
                .header("x-request-id", "invalid-log-input")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(roadviewer_hars::store::valid_id(
        response.headers()["x-request-id"].to_str().unwrap()
    ));
}
