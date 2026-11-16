//! Playing and downloading recordings whose files the archiver moved to the share or to Drive (Python `video`,
//! `original`, `download_one`, `download_many`, `remote_response`, `media_file`). Loopback Drive fake and a
//! temporary share only.
mod common;
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use common::archive_env::*;
use roadviewer_hars::{archive::ArchiveRead, http::ingress, store::sha};
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;

async fn get(e: &Env, path: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut b = Request::builder().method("GET").uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        ingress(e.app.clone()).oneshot(b.body(Body::empty()).unwrap()),
    )
    .await
    .expect("HTTP handler deadlock")
    .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 256 << 20)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}
fn json_of(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}
fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).map(|v| v.to_str().unwrap())
}

/// A ready recording with a kept front MP4 and its log, moved off the local disk by one cycle.
async fn moved(e: &Env, video: &[u8], log: &[u8]) -> String {
    let id = e
        .record(
            'a',
            &[("fcamera.mp4", video.to_vec()), ("rlog.zst", log.to_vec())],
            OLD,
        )
        .await;
    e.cycle().await;
    assert!(!e.local(&id, "fcamera.mp4") && !e.local(&id, "rlog.zst"));
    id
}

/// What every archive must answer for a moved recording: playback ranges, whole downloads and the ZIP.
async fn reads_like_local(e: &Env, id: &str, video: &[u8], log: &[u8]) {
    let (status, h, body) = get(
        e,
        &format!("/api/logs/{id}/video?source=front"),
        &[("range", "bytes=100-199")],
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, video[100..200]);
    assert_eq!(
        header(&h, "content-range"),
        Some(format!("bytes 100-199/{}", video.len()).as_str())
    );
    assert_eq!(header(&h, "accept-ranges"), Some("bytes"));
    let (status, _, body) = get(e, &format!("/api/logs/{id}/video?source=front"), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sha(&body), sha(video));
    let (status, h, body) = get(e, &format!("/api/logs/{id}/original/rlog.zst"), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sha(&body), sha(log));
    assert_eq!(
        header(&h, "content-length"),
        Some(log.len().to_string().as_str())
    );
    assert!(header(&h, "content-disposition").is_some_and(|v| v.starts_with("attachment")));
    let (status, _, body) = get(
        e,
        &format!("/api/logs/{id}/download?source=front&format=mp4"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sha(&body), sha(video));
    let (status, _, body) = get(e, &format!("/api/logs/{id}/download/video"), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sha(&body), sha(video));
    for query in ["parts=rlog,front", "parts=rlog,front&format=mp4"] {
        let (status, h, body) =
            get(e, &format!("/api/logs/download.zip?ids={id}&{query}"), &[]).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(header(&h, "content-type"), Some("application/zip"));
        let find = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
        assert!(
            find(log) && find(video),
            "{query}: the ZIP carries both files"
        );
    }
}

#[tokio::test]
async fn nas_moved_files_play_and_download() {
    let e = Env::new().await;
    e.use_nas(json!({"archive_nas_margin_mb": 0})).await;
    let (video, log) = (data(300_000, 1), data(5000, 2));
    let id = moved(&e, &video, &log).await;
    assert_eq!(e.loc(&id, "fcamera.mp4").await, "nas");
    reads_like_local(&e, &id, &video, &log).await;
}

#[tokio::test]
async fn drive_moved_files_stream_from_drive() {
    let e = Env::new().await;
    e.use_drive(json!({"archive_mode": "move", "archive_allow_local_delete": true}))
        .await;
    let (video, log) = (data(300_000, 3), data(5000, 4));
    let id = moved(&e, &video, &log).await;
    assert_eq!(e.loc(&id, "fcamera.mp4").await, "gdrive");
    reads_like_local(&e, &id, &video, &log).await;
    // Python `remote_response`: playback has no Content-Length and may be cached; an open range is bounded.
    let (status, h, _) = get(
        &e,
        &format!("/api/logs/{id}/video?source=front"),
        &[("range", "bytes=0-")],
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(header(&h, "content-length"), None);
    assert_eq!(header(&h, "cache-control"), Some("private, max-age=3600"));
    let etag = header(&h, "etag").unwrap().to_owned();
    assert_eq!(etag.len(), 34, "an opaque 32-hex ETag");
    assert!(!etag.contains(&e.drive_file(&id, "fcamera.mp4").unwrap().0));
    let (status, h, body) = get(
        &e,
        &format!("/api/logs/{id}/video?source=front"),
        &[("range", "bytes=-0")],
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(json_of(&body)["error"], "range_not_satisfiable");
    assert_eq!(
        header(&h, "content-range"),
        Some(format!("bytes */{}", video.len()).as_str())
    );
    // A download range is answered with its Content-Length.
    let (status, h, body) = get(
        &e,
        &format!("/api/logs/{id}/original/rlog.zst"),
        &[("range", "bytes=10-")],
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, log[10..]);
    assert_eq!(
        header(&h, "content-length"),
        Some((log.len() - 10).to_string().as_str())
    );
    assert_eq!(header(&h, "cache-control"), Some("no-store"));
}

#[tokio::test]
async fn an_unreachable_share_answers_storage_unavailable_not_missing() {
    let e = Env::new().await;
    e.use_nas(json!({"archive_nas_margin_mb": 0})).await;
    let id = moved(&e, &data(3000, 5), &data(500, 6)).await;
    std::fs::remove_file(e.nas.join(".roadviewer-storage.json")).unwrap();
    e.a().backend().invalidate();
    for path in [
        format!("/api/logs/{id}/video?source=front"),
        format!("/api/logs/{id}/original/rlog.zst"),
        format!("/api/logs/{id}/download"),
        format!("/api/logs/download.zip?ids={id}&parts=rlog"),
    ] {
        let (status, _, body) = get(&e, &path, &[]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        let body = json_of(&body);
        assert_eq!(body["error"], "storage_unavailable", "{path}");
        assert_eq!(
            body["message"],
            "영상 저장소에 연결할 수 없습니다. NAS 연결을 확인하세요."
        );
    }
}

#[tokio::test]
async fn a_disconnected_drive_names_google_drive() {
    let e = Env::new().await;
    e.use_drive(json!({"archive_mode": "move", "archive_allow_local_delete": true}))
        .await;
    let id = moved(&e, &data(3000, 7), &data(500, 8)).await;
    let (status, _) = e
        .req(
            "POST",
            "/api/settings/archive/gdrive/disconnect",
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = get(&e, &format!("/api/logs/{id}/original/rlog.zst"), &[]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body = json_of(&body);
    assert_eq!(body["error"], "storage_unavailable");
    assert_eq!(
        body["message"],
        "영상 저장소(Google Drive)에 연결할 수 없습니다. 인터넷과 Google Drive 연결을 확인하세요."
    );
}

#[tokio::test]
async fn a_drive_file_gone_meanwhile_is_missing_and_a_changed_one_corrupt() {
    let e = Env::new().await;
    e.use_drive(json!({"archive_mode": "move", "archive_allow_local_delete": true}))
        .await;
    let id = moved(&e, &data(3000, 9), &data(500, 10)).await;
    let (video_id, _) = e.drive_file(&id, "fcamera.mp4").unwrap();
    e.fake.st().files.get_mut(&video_id).unwrap().content = data(2999, 11);
    e.a().backend().invalidate();
    let (status, _, body) = get(&e, &format!("/api/logs/{id}/video?source=front"), &[]).await;
    assert_eq!(
        (status, json_of(&body)["error"].clone()),
        (StatusCode::CONFLICT, json!("media_corrupt"))
    );
    let (log_id, _) = e.drive_file(&id, "rlog.zst").unwrap();
    e.fake.st().files.remove(&log_id);
    let (status, _, _) = get(&e, &format!("/api/logs/{id}/original/rlog.zst"), &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
