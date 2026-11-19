//! Adding a camera to a recording whose files the archiver moved to Drive. The upload resolves the archived files
//! (Drive trusts a "connected" answer for minutes), then registration must not refuse it only because the 10 s
//! probe cache ran out meanwhile.
mod common;
use axum::http::StatusCode;
use common::archive_env::*;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tower::ServiceExt;

const ROUTE: &str = "2026-01-02--10-00-00--0";

async fn put(e: &Env, path: &str, body: Vec<u8>) -> StatusCode {
    let request = axum::http::Request::builder()
        .method("PUT")
        .uri(path)
        .header("x-roadviewer-request", "1")
        .body(axum::body::Body::from(body))
        .unwrap();
    roadviewer_hars::http::ingress(e.app.clone())
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn a_wide_camera_joins_a_recording_moved_to_drive_after_the_probe_cache_expired() {
    let e = Env::new().await;
    e.use_drive(
        json!({"archive_mode": "move", "archive_allow_local_delete": true,
        "archive_min_age_hours": 0}),
    )
    .await;
    let seconds = Arc::new(AtomicU64::new(1_000));
    let now = seconds.clone();
    e.a().set_factory(factory_clocked(
        &e.fake.url,
        e.http.clone(),
        Arc::new(move || now.load(Ordering::SeqCst) as f64),
    ));
    let front = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/smoke/fcamera.hevc"
    ))
    .unwrap();
    let log = data(300, 4);
    let id = e
        .record(
            'a',
            &[("fcamera.hevc", front.clone()), ("rlog.zst", log.clone())],
            OLD,
        )
        .await;
    e.cycle().await;
    assert_eq!(e.loc(&id, "rlog.zst").await, "gdrive");
    assert_eq!(e.loc(&id, "fcamera.hevc").await, "gdrive");
    // Drive answered "connected" a minute ago: still trusted for reads, no longer a fresh probe result.
    seconds.fetch_add(60, Ordering::SeqCst);

    let wide = front;
    let files = [
        (format!("{ROUTE}--rlog.zst"), log),
        (format!("{ROUTE}--ecamera.hevc"), wide),
    ];
    let (status, session) = e
        .req(
            "POST",
            "/api/uploads",
            json!({"files": files.iter().map(|(n, b)| json!({"name": n, "size": b.len()})).collect::<Vec<_>>()}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let sid = session["id"].as_str().unwrap();
    for (i, (_, bytes)) in files.iter().enumerate() {
        assert_eq!(
            put(
                &e,
                &format!("/api/uploads/{sid}/files/{i}?offset=0"),
                bytes.clone()
            )
            .await,
            StatusCode::OK
        );
    }
    let (status, body) = e
        .req(
            "POST",
            &format!("/api/uploads/{sid}/finish"),
            serde_json::Value::Null,
        )
        .await;
    assert!(status.is_success(), "{status} {body}");
    let meta = e.meta(&id).await;
    assert!(meta["files"]["ecamera.hevc"].is_string(), "{meta}");
}
