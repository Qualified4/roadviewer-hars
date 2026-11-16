//! Upload admission with Drive in move mode (Python `admit`'s `drive_move` branch, `reserved(exclude=...)`): the
//! committing upload's own reservation is not room it still needs, while another session's reservation is.
mod common;
use axum::http::StatusCode;
use common::archive_env::*;
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;

const MB: u64 = 1 << 20;

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

/// A session with every byte received; its reservation file then claims `reserved` bytes, as a reservation still
/// outstanding would.
async fn received(e: &Env, name: &str, seed: u64, reserved: u64) -> String {
    let payload = data(4096, seed);
    let (status, session) = e
        .req(
            "POST",
            "/api/uploads",
            json!({"files": [{"name": name, "size": payload.len()}]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let id = session["id"].as_str().unwrap().to_owned();
    assert_eq!(
        put(e, &format!("/api/uploads/{id}/files/0?offset=0"), payload).await,
        StatusCode::OK
    );
    let file = e.root.join(".uploads").join(&id).join("reservation.json");
    let mut r: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    r["bytes"] = json!(reserved);
    std::fs::write(&file, serde_json::to_vec(&r).unwrap()).unwrap();
    id
}

async fn moving_env() -> (Env, String) {
    let e = Env::new().await;
    e.use_drive(
        json!({"archive_mode": "move", "archive_allow_local_delete": true,
        "archive_min_age_hours": 0}),
    )
    .await;
    let old = e.record('a', &[("rlog.zst", data(1000, 1))], OLD).await;
    (e, old)
}

/// Just under the limit by what is on disk; over it with `reserved` more.
async fn limit_below(e: &Env, reserved: u64) {
    let used = e.app.store.used().await.unwrap();
    let (status, body) = e
        .req(
            "POST",
            "/api/settings/storage",
            json!({"max_bytes": used + reserved / 2, "policy": "reject_new"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn settled(e: &Env) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while e.a().running() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the capacity run did not end");
}

#[tokio::test]
async fn the_committing_uploads_own_reservation_starts_no_move() {
    let (e, old) = moving_env().await;
    let session = received(&e, "2026-01-01--10-00-00--0--rlog.zst", 2, 64 * MB).await;
    limit_below(&e, 64 * MB).await;
    let (status, body) = e
        .req(
            "POST",
            &format!("/api/uploads/{session}/finish"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    settled(&e).await;
    assert_eq!(e.loc(&old, "rlog.zst").await, "local");
    assert!(e.drive_files().is_empty(), "nothing was moved for room");
}

#[tokio::test]
async fn another_sessions_reservation_moves_old_recordings_at_commit() {
    let (e, old) = moving_env().await;
    let session = received(&e, "2026-01-01--10-00-00--0--rlog.zst", 2, 0).await;
    received(&e, "2026-01-01--11-00-00--0--rlog.zst", 3, 64 * MB).await;
    limit_below(&e, 64 * MB).await;
    let (status, body) = e
        .req(
            "POST",
            &format!("/api/uploads/{session}/finish"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    settled(&e).await;
    assert_eq!(e.loc(&old, "rlog.zst").await, "gdrive");
    assert!(!e.local(&old, "rlog.zst"));
}
