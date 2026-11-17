//! Route-level parity with Python `server.py`, `device_network.py` and `log_transfer.py`.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
};
use roadviewer_hars::{
    http::{App, device, ingress},
    network::Network,
    store::{id, sha},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
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
async fn send(
    router: Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        router.oneshot(builder.body(Body::from(body)).unwrap()),
    )
    .await
    .expect("handler deadlock")
    .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn call(a: &App, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let body = if body.is_null() {
        vec![]
    } else {
        serde_json::to_vec(&body).unwrap()
    };
    let (s, _, v) = send(
        ingress(a.clone()),
        method,
        path,
        body,
        &[
            ("x-roadviewer-request", "1"),
            ("content-type", "application/json"),
        ],
    )
    .await;
    (s, v)
}
async fn recording(a: &App, name: &str, meta: Value) -> String {
    let record = id();
    std::fs::create_dir_all(a.store.root.join(&record)).unwrap();
    let mut m = json!({"id":record,"name":name,"uploaded":1700000000.0,"status":"unconverted","files":{},"error":null});
    m.as_object_mut()
        .unwrap()
        .extend(meta.as_object().cloned().unwrap_or_default());
    a.store.save_meta(m).await.unwrap();
    record
}

#[tokio::test]
async fn list_rows_carry_python_derived_fields_and_order() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    let ready = recording(&a, "000003fe--d524dc1e0b / 구간 2", json!({"status":"ready","video_codecs":{"front":"hevc"},
        "warnings":["지원하지 않는 새 로그 이벤트: x","real"],"bookmarks":[{"id":"b","time":1,"title":"t","note":""}],
        "content_hashes":{},"queued_at":5,"conversion_id":"x","video_downloads":[],"drive_summary":{"stale":true},"replay_v":2,
        "files":{"rlog.zst":"000003fe--d524dc1e0b--2--rlog.zst"}})).await;
    let folder = root.path().join(&ready);
    std::fs::write(folder.join("rlog.zst"), b"12345").unwrap();
    std::fs::write(folder.join("fcamera.hevc"), b"1234567").unwrap();
    std::fs::create_dir_all(folder.join("prepared")).unwrap();
    std::fs::write(folder.join("prepared/fcamera.mp4"), b"123").unwrap();
    std::fs::write(folder.join("prepared/thumbnail.jpg"), b"j").unwrap();
    std::fs::write(
        folder.join("prepared/thumbnail.json"),
        br#"{"version":2,"score":0.5}"#,
    )
    .unwrap();
    std::fs::write(
        folder.join("prepared/summary.json"),
        serde_json::to_vec(&json!({"drive_summary":{"km":1},"has_track":false,
        "first_mono":2_000_000_000u64,"clock_offset":1.7e9,"started_at":null}))
        .unwrap(),
    )
    .unwrap();
    let earlier = recording(&a, "3FE--D524DC1E0B--1", json!({"status":"ready"})).await;
    std::fs::create_dir_all(root.path().join(&earlier).join("prepared")).unwrap();
    std::fs::write(
        root.path().join(&earlier).join("prepared/summary.json"),
        serde_json::to_vec(&json!({"first_mono":1_000_000_000u64})).unwrap(),
    )
    .unwrap();
    let custom = recording(&a, "custom", json!({"drive_summary":{"stale":true}})).await;
    let (status, headers, list) = send(ingress(a.clone()), "GET", "/api/logs", vec![], &[]).await;
    assert_eq!(status, StatusCode::OK);
    let ids = list["logs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, [ready.as_str(), earlier.as_str(), custom.as_str()]);
    let row = &list["logs"][0];
    for key in [
        "bookmarks",
        "content_hashes",
        "queued_at",
        "conversion_id",
        "replay_v",
        "first_mono",
        "clock_offset",
    ] {
        assert!(row.get(key).is_none(), "{key}");
    }
    assert_eq!(row["warnings"], json!(["real"]));
    assert_eq!(row["bytes"], 12);
    assert_eq!(row["log_bytes"], 5);
    assert_eq!(
        row["prepared_bytes"],
        json!(
            3 + 1
                + 25
                + std::fs::metadata(folder.join("prepared/summary.json"))
                    .unwrap()
                    .len()
        )
    );
    assert_eq!(row["video"], true);
    assert_eq!(row["video_download"], "fcamera.hevc");
    assert_eq!(
        row["video_downloads"],
        json!([{"source":"front","kind":"fcamera.hevc","bytes":7}])
    );
    assert_eq!(
        row["download_options"],
        json!([{"source":"front","mp4":true,"original":true,"bytes":3,"original_kind":"fcamera.hevc"}])
    );
    assert_eq!(
        (
            row["thumbnail"].clone(),
            row["thumbnail_version"].clone(),
            row["thumbnail_score"].clone()
        ),
        (json!(true), json!(2), json!(0.5))
    );
    assert_eq!(row["drive_summary"], json!({"km":1}));
    assert_eq!(row["has_track"], false);
    assert_eq!(row["pinned"], false);
    assert!(row["progress"].is_null());
    // The other segment of the drive is dated from the latest segment's clock offset.
    assert_eq!(list["logs"][1]["started_at"], json!(1.7e9 + 1.));
    assert_eq!(list["logs"][1]["start_source"], "clock");
    assert!(list["logs"][2].get("drive_summary").is_none());
    let etag = headers["etag"].to_str().unwrap().to_owned();
    let (status, _, _) = send(
        ingress(a.clone()),
        "GET",
        "/api/logs",
        vec![],
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn tracks_use_python_route_keys_and_revalidate() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    let mut ids = vec![];
    for name in ["000003FE--D524DC1E0B / 구간 1", "3fe--d524dc1e0b--0"] {
        let record = recording(&a, name, json!({"status":"ready"})).await;
        let prepared = root.path().join(&record).join("prepared");
        std::fs::create_dir_all(&prepared).unwrap();
        std::fs::write(
            prepared.join("summary.json"),
            br#"{"has_track":true,"track_distance_m":10,"duration":60}"#,
        )
        .unwrap();
        roadviewer_hars::logs::write_gzip(
            &prepared.join("track.json.gz"),
            &json!({"points":[[0,37.5,127.0],[1,37.6,127.1]]}),
        )
        .unwrap();
        ids.push(record);
    }
    let (status, headers, body) = send(ingress(a.clone()), "GET", "/api/tracks", vec![], &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["routes"].as_array().unwrap().len(), 1);
    assert_eq!(body["routes"][0]["key"], "3fe--d524dc1e0b");
    assert_eq!(body["routes"][0]["segments"], json!([ids[1], ids[0]]));
    assert_eq!(headers["cache-control"], "private, no-cache");
    let etag = headers["etag"].to_str().unwrap().to_owned();
    assert_eq!(
        send(
            ingress(a.clone()),
            "GET",
            "/api/tracks",
            vec![],
            &[("if-none-match", &etag)]
        )
        .await
        .0,
        StatusCode::NOT_MODIFIED
    );
    let (_, _, track) = send(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/{}/track", ids[0]),
        vec![],
        &[],
    )
    .await;
    assert_eq!(
        track["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["segment"].clone())
            .collect::<Vec<_>>(),
        [json!(0), json!(1)]
    );
    let (_, _, none) = send(
        ingress(a.clone()),
        "GET",
        "/api/tracks?ids=nothex",
        vec![],
        &[],
    )
    .await;
    assert_eq!(none["routes"], json!([]));
}

#[tokio::test]
async fn bookmark_and_tag_rules_and_messages_match_python() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    let record = recording(&a, "x", json!({"duration":10})).await;
    let path = format!("/api/logs/{record}/bookmarks");
    let cases = [
        (
            json!({"time":1,"title":"a"}),
            "북마크 요청이 올바르지 않습니다.",
        ),
        (
            json!({"time":true,"title":"a","note":""}),
            "북마크 시간이 올바르지 않습니다.",
        ),
        (
            json!({"time":1,"title":3,"note":""}),
            "제목과 메모는 문자열이어야 합니다.",
        ),
        (
            json!({"time":1,"title":"a\u{200b}","note":""}),
            "제목은 80자, 메모는 1000자 이내로 입력해 주세요.",
        ),
        (
            json!({"time":1,"title":"  \n ","note":""}),
            "북마크 제목을 입력해 주세요.",
        ),
        (
            json!({"time":11,"title":"a","note":""}),
            "구간 길이를 벗어난 북마크입니다.",
        ),
    ];
    for (body, message) in cases {
        let (status, v) = call(&a, "POST", &path, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            (v["error"].as_str(), v["message"].as_str()),
            (Some("invalid_bookmark"), Some(message))
        );
    }
    let (status, v) = call(
        &a,
        "POST",
        &path,
        json!({"time":2,"title":" a\tb ","note":"line\nnext"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["bookmarks"][0]["title"], "a b");
    assert_eq!(v["bookmarks"][0]["time"], json!(2));
    let (status, v) = call(
        &a,
        "PUT",
        &format!("{path}/missing"),
        json!({"time":1,"title":"a","note":""}),
    )
    .await;
    assert_eq!(
        (status, v["message"].as_str()),
        (
            StatusCode::NOT_FOUND,
            Some("로그 또는 파일을 찾을 수 없습니다.")
        )
    );
    let (status, _) = call(
        &a,
        "POST",
        &path,
        json!({"time":1,"title":"a","note":"x".repeat(9000)}),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let missing = format!("/api/logs/{}/bookmarks", id());
    assert_eq!(
        call(&a, "POST", &missing, json!({"time":1})).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&a, "GET", &missing, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );

    let tags = format!("/api/logs/{record}/tags");
    let (status, v) = call(&a, "POST", &tags, json!({"tags":[{"name":"Zeta"},{"name":"alpha","color":"red"},{"name":"ZETA","color":"blue"}]})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["tags"],
        json!([{"name":"ZETA","color":"blue"},{"name":"alpha","color":"red"}])
    );
    let (_, v) = call(
        &a,
        "POST",
        &tags,
        json!({"action":"add","tags":[{"name":"beta"},{"name":"alpha","color":"cyan"}]}),
    )
    .await;
    assert_eq!(
        v["tags"],
        json!([{"name":"ZETA","color":"blue"},{"name":"alpha","color":"red"},{"name":"beta","color":"mint"}])
    );
    let (_, v) = call(&a, "POST", &tags, json!({"tags":[{"name":"line\nbreak"}]})).await;
    assert_eq!(v["tags"], json!([{"name":"line break","color":"mint"}]));
    for (body, message) in [
        (
            json!({"action":"toggle","tags":[]}),
            "태그 요청이 올바르지 않습니다.",
        ),
        (json!({"tags":"x"}), "태그는 구간당 최대 100개입니다."),
        (
            json!({"tags":[{"name":1}]}),
            "태그 이름이 올바르지 않습니다.",
        ),
        (
            json!({"tags":[{"name":"x".repeat(33)}]}),
            "태그 이름은 1~32자의 표시 가능한 문자로 입력해 주세요.",
        ),
        (
            json!({"tags":[{"name":"x","color":"black"}]}),
            "태그 색상이 올바르지 않습니다.",
        ),
    ] {
        let (status, v) = call(&a, "POST", &tags, body).await;
        assert_eq!(
            (status, v["error"].as_str(), v["message"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_tags"), Some(message))
        );
    }
    // Not JSON at all: Python's get_json(silent=True) answers the same 400.
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        &tags,
        b"nope".to_vec(),
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_tags"))
    );
}

#[tokio::test]
async fn unknown_methods_paths_and_limits_answer_json() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    let (status, _, v) = send(ingress(a.clone()), "GET", "/api/nothing", vec![], &[]).await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    assert_eq!(v["message"], "로그 또는 파일을 찾을 수 없습니다.");
    // Ported with the storage policy: answered by their handlers, not the unported fallback.
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        "/api/storage/cleanup",
        vec![],
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!((status, &v), (StatusCode::OK, &json!({"removed_bytes":0})));
    let (status, _, v) = send(
        ingress(a.clone()),
        "DELETE",
        "/api/logs/0123456789abcdef0123456789abcdef",
        vec![],
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    // Ported with the archiver: answered by their handlers.
    let (status, _, v) = send(
        ingress(a.clone()),
        "GET",
        "/api/settings/archive",
        vec![],
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(
        (status, &v["archive_enabled"]),
        (StatusCode::OK, &json!(false))
    );
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        "/api/logs/0123456789abcdef0123456789abcdef/archive/retry",
        vec![],
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    let (status, _, v) = send(
        ingress(a.clone()),
        "PUT",
        "/api/logs",
        vec![],
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::METHOD_NOT_ALLOWED, Some("method_not_allowed"))
    );
    let (status, _, v) = send(device(a.clone()), "GET", "/api/device/pair", vec![], &[]).await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::METHOD_NOT_ALLOWED, Some("method_not_allowed"))
    );
    let (status, _, v) = send(ingress(a.clone()), "POST", "/api/logs/import", vec![], &[]).await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        "/api/settings/processing",
        vec![b' '; 9 * 1024 * 1024],
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, Some("request_too_large"))
    );
    assert_eq!(v["message"], "업로드 용량 제한을 초과했습니다.");
    let (status, v) = call(&a, "POST", "/api/uploads", json!(["x"])).await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_upload"))
    );
}

#[tokio::test]
async fn upload_failure_report_and_legacy_multipart_upload() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    let session = id();
    let (status, v) = call(
        &a,
        "POST",
        &format!("/api/uploads/{session}/failure"),
        json!({"stage":"chunk","attempt":2,"name":"secret.zst"}),
    )
    .await;
    assert_eq!((status, v), (StatusCode::OK, json!({"recorded":true})));
    assert_eq!(
        call(&a, "POST", "/api/uploads/bad/failure", json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &a,
            "POST",
            &format!("/api/uploads/{session}/failure"),
            json!(["x"])
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &a,
            "POST",
            &format!("/api/uploads/{session}/failure"),
            json!({"x":"y".repeat(3000)})
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let multipart = |files: &[(&str, &[u8])]| {
        let mut body = vec![];
        for (name, data) in files {
            body.extend(format!("--XBOUNDARY\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
            body.extend(*data);
            body.extend(b"\r\n");
        }
        body.extend(b"--XBOUNDARY--\r\n");
        body
    };
    let headers = [
        ("x-roadviewer-request", "1"),
        ("content-type", "multipart/form-data; boundary=XBOUNDARY"),
    ];
    let body = multipart(&[
        ("1a2b3c4d--0123456789--5--rlog.zst", b"legacy-log"),
        ("1a2b3c4d--0123456789--5--qcamera.ts", b"legacy-ts"),
    ]);
    let length = body.len().to_string();
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        "/api/upload",
        body.clone(),
        &[headers[0], headers[1], ("content-length", &length)],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["logs"][0]["name"], "1a2b3c4d--0123456789 / 구간 5");
    let record = v["logs"][0]["id"].as_str().unwrap();
    assert_eq!(
        std::fs::read(root.path().join(record).join("qcamera.ts")).unwrap(),
        b"legacy-ts"
    );
    assert_eq!(
        a.store.meta(record).await.unwrap()["content_hashes"]["rlog.zst"]["sha256"],
        sha(b"legacy-log")
    );
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        "/api/upload",
        body.clone(),
        &[headers[0], headers[1], ("content-length", &length)],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["duplicates"][0]["id"], record);
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        "/api/upload",
        body,
        &[headers[0], headers[1]],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::LENGTH_REQUIRED, Some("content_length_required"))
    );
    let bad = multipart(&[("../rlog.zst", b"x")]);
    let length = bad.len().to_string();
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        "/api/upload",
        bad,
        &[headers[0], headers[1], ("content-length", &length)],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_file_path"))
    );
    let leftovers = std::fs::read_dir(root.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".legacy-"))
        .count();
    assert_eq!(leftovers, 0);
    assert!(
        std::fs::read_dir(root.path().join(".uploads"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[derive(Clone, Default)]
struct Supervisor {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    network: Arc<Mutex<Value>>,
}
async fn supervisor_info(State(s): State<Supervisor>, headers: HeaderMap) -> Json<Value> {
    if headers["authorization"] != "Bearer test-token" {
        return Json(json!({"result":"error"}));
    }
    s.calls.lock().unwrap().push(("info".into(), Value::Null));
    Json(
        json!({"result":"ok","data":{"network":s.network.lock().unwrap().clone(),"options":{"device_certfile":"a.pem","device_keyfile":"b.pem"}}}),
    )
}
async fn supervisor_options(State(s): State<Supervisor>, Json(body): Json<Value>) -> Json<Value> {
    *s.network.lock().unwrap() = body["network"].clone();
    s.calls.lock().unwrap().push(("options".into(), body));
    Json(json!({"result":"ok","data":{}}))
}
async fn supervisor_restart(State(s): State<Supervisor>, Json(body): Json<Value>) -> Json<Value> {
    s.calls.lock().unwrap().push(("restart".into(), body));
    Json(json!({"result":"ok"}))
}

#[tokio::test]
async fn device_network_uses_supervisor_contract() {
    let root = tempfile::tempdir().unwrap();
    let mut a = app(root.path()).await;
    // No Supervisor (development): the same 502 as Python without SUPERVISOR_TOKEN.
    a.network = Arc::new(Network::new(
        "http://127.0.0.1:18145",
        None,
        "/bin/true",
        None,
        "",
    ));
    let (status, v) = call(&a, "GET", "/api/settings/device-network", Value::Null).await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::BAD_GATEWAY, Some("supervisor_unavailable"))
    );
    assert_eq!(
        call(
            &a,
            "POST",
            "/api/settings/device-network/restart",
            json!({})
        )
        .await
        .0,
        StatusCode::BAD_GATEWAY
    );

    let mock = Supervisor::default();
    *mock.network.lock().unwrap() = json!({"8443/tcp":null,"8099/tcp":null});
    let router = Router::new()
        .route("/addons/self/info", get(supervisor_info))
        .route("/addons/self/options", post(supervisor_options))
        .route("/addons/self/restart", post(supervisor_restart))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18145")
        .await
        .unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    a.network = Arc::new(Network::new(
        "http://127.0.0.1:18145",
        Some("test-token".into()),
        "/bin/false",
        None,
        "",
    ));
    let (status, v) = call(&a, "GET", "/api/settings/device-network", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (
            v["configured_port"].clone(),
            v["active_port"].clone(),
            v["restart_required"].clone()
        ),
        (Value::Null, Value::Null, json!(false))
    );
    for (body, code) in [
        (json!({"enabled":true}), "invalid_network_settings"),
        (
            json!({"enabled":"yes","port":1}),
            "invalid_network_settings",
        ),
        (json!({"enabled":true,"port":0}), "invalid_port"),
        (json!({"enabled":true,"port":8443.0}), "invalid_port"),
        (json!({"enabled":false,"port":8443}), "invalid_port"),
    ] {
        let (status, v) = call(&a, "POST", "/api/settings/device-network", body).await;
        assert_eq!(
            (status, v["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some(code))
        );
    }
    // TLS that nginx rejects keeps the mapping unchanged.
    let (status, v) = call(
        &a,
        "POST",
        "/api/settings/device-network",
        json!({"enabled":true,"port":18443}),
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_tls"))
    );
    assert!(mock.calls.lock().unwrap().iter().all(|c| c.0 == "info"));
    a.network = Arc::new(Network::new(
        "http://127.0.0.1:18145",
        Some("test-token".into()),
        "/bin/true",
        None,
        "",
    ));
    let (status, v) = call(
        &a,
        "POST",
        "/api/settings/device-network",
        json!({"enabled":true,"port":18443}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        (v["configured_port"].clone(), v["restart_required"].clone()),
        (json!(18443), json!(true))
    );
    assert_eq!(
        *mock.network.lock().unwrap(),
        json!({"8443/tcp":18443,"8099/tcp":null})
    );
    let (status, v) = call(
        &a,
        "POST",
        "/api/settings/device-network/restart",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(v["restarting"], true);
    let (status, v) = call(
        &a,
        "POST",
        "/api/settings/device-network",
        json!({"enabled":false,"port":null}),
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("restarting"))
    );
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        mock.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.0 == "restart" && c.1 == json!({}))
    );
}

async fn import_session(a: &App, begin: &Value) -> String {
    let (status, v) = call(a, "POST", "/api/logs/import", begin.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let session = v["session"].as_str().unwrap().to_owned();
    let (status, _, _) = send(
        ingress(a.clone()),
        "PUT",
        &format!("/api/uploads/{session}/files/0?offset=0"),
        b"data".to_vec(),
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    session
}
async fn storage_limit(a: &App, max: u64, policy: &str) {
    let (status, v) = call(
        a,
        "POST",
        "/api/settings/storage",
        json!({"max_bytes":max,"policy":policy}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn import_and_legacy_upload_use_storage_admission_and_settings() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;
    let leftovers = |prefix: &str| {
        std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .count()
    };
    let record = id();
    let begin = json!({"format":"roadviewer-export","version":1,"record":{"id":record,"name":"n","pinned":true,
        "originals":{"rlog.zst":{"bytes":4,"sha256":sha(b"data")}}}});

    // Admitted when it began; the commit that publishes the recording is admitted again.
    let session = import_session(&a, &begin).await;
    storage_limit(&a, 1, "reject_new").await;
    let finish = format!("/api/logs/import/{session}/finish");
    let (status, v) = call(&a, "POST", &finish, Value::Null).await;
    assert_eq!(
        (status, v["error"].as_str()),
        (
            StatusCode::INSUFFICIENT_STORAGE,
            Some("storage_limit_exceeded")
        )
    );
    assert!(!root.path().join(&record).exists());
    assert_eq!(leftovers(".import-"), 0);
    assert!(
        std::fs::read_dir(root.path().join(".uploads"))
            .unwrap()
            .next()
            .is_none()
    );

    // The legacy upload is refused before its body lands in the data root.
    let body = b"--XBOUNDARY\r\nContent-Disposition: form-data; name=\"files\"; filename=\"1a2b3c4d--0123456789--5--rlog.zst\"\r\nContent-Type: application/octet-stream\r\n\r\nlegacy-log\r\n--XBOUNDARY--\r\n".to_vec();
    let length = body.len().to_string();
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        "/api/upload",
        body,
        &[
            ("x-roadviewer-request", "1"),
            ("content-type", "multipart/form-data; boundary=XBOUNDARY"),
            ("content-length", &length),
        ],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (
            StatusCode::INSUFFICIENT_STORAGE,
            Some("storage_limit_exceeded")
        )
    );
    assert_eq!(leftovers(".legacy-"), 0);
    assert!(a.store.records.read().await.is_empty());

    // Without a limit the import registers, and its pin joins the saved settings.
    storage_limit(&a, 0, "delete_oldest").await;
    let session = import_session(&a, &begin).await;
    let (status, v) = call(
        &a,
        "POST",
        &format!("/api/logs/import/{session}/finish"),
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, v["imported"].as_str()),
        (StatusCode::CREATED, Some(record.as_str()))
    );
    let saved: Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".storage-settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        (&saved["pinned_logs"], &saved["policy"], &saved["max_bytes"]),
        (&json!([record]), &json!("delete_oldest"), &json!(0))
    );
    let (_, list) = call(&a, "GET", "/api/logs", Value::Null).await;
    assert_eq!(list["logs"][0]["pinned"], true);

    // Bodies that are not JSON get Python's error codes, not a framework rejection.
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        &format!("/api/logs/{record}/pin"),
        b"pinned".to_vec(),
        &[
            ("x-roadviewer-request", "1"),
            ("content-type", "text/plain"),
        ],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_pin"))
    );
    let (status, _, v) = send(
        ingress(a.clone()),
        "POST",
        "/api/settings/storage",
        b"max_bytes=0".to_vec(),
        &[
            ("x-roadviewer-request", "1"),
            ("content-type", "text/plain"),
        ],
    )
    .await;
    assert_eq!(
        (status, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_storage_settings"))
    );
    // Flask's `get_json(silent=True)` ignores even valid JSON sent with another type.
    let pin = format!("/api/logs/{record}/pin");
    for (path, body, code) in [
        (
            "/api/settings/storage",
            br#"{"max_bytes":0,"policy":"reject_new"}"#.to_vec(),
            "invalid_storage_settings",
        ),
        (pin.as_str(), br#"{"pinned":false}"#.to_vec(), "invalid_pin"),
    ] {
        let (status, _, v) = send(
            ingress(a.clone()),
            "POST",
            path,
            body,
            &[
                ("x-roadviewer-request", "1"),
                ("content-type", "text/plain"),
            ],
        )
        .await;
        assert_eq!(
            (status, v["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some(code)),
            "{path}"
        );
    }
    let (_, list) = call(&a, "GET", "/api/logs", Value::Null).await;
    assert_eq!(list["logs"][0]["pinned"], true);
}
