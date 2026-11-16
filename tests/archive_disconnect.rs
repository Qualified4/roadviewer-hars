//! A client that goes away while a Drive-only file is copied for it (an original-format export, a ZIP of originals)
//! leaves nothing behind: the staged copy and the export are removed without waiting for a restart. Loopback Drive
//! fake; the client is a real TCP connection that is closed while Drive is still answering.
mod common;
use common::archive_env::*;
use roadviewer_hars::http::ingress;
use serde_json::json;
use std::{
    net::SocketAddr,
    path::Path,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

async fn serve(e: &Env) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = ingress(e.app.clone()).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Polls up to ; Drive requests time out after 10 s, so a removal must come well before that.
async fn until(what: &str, limit: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(limit);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A recording whose front MP4 only Drive holds; its content is answered after a minute.
async fn slow_drive_video(e: &Env) -> String {
    e.use_drive(json!({"archive_mode": "move", "archive_allow_local_delete": true}))
        .await;
    let id = e
        .record(
            'a',
            &[
                ("fcamera.mp4", data(300_000, 1)),
                ("rlog.zst", data(5000, 2)),
            ],
            OLD,
        )
        .await;
    e.cycle().await;
    assert!(!e.local(&id, "fcamera.mp4"));
    e.set(&id, |m| m["video_codecs"] = json!({"front": "hevc"}))
        .await;
    let (file_id, _) = e.drive_file(&id, "fcamera.mp4").unwrap();
    e.fake
        .st()
        .media_delay
        .insert(file_id, Duration::from_secs(60));
    id
}

async fn cut_off_during_staging(e: &Env, id: &str, path: &str) {
    let addr = serve(e).await;
    let held = e.fake.st().media_held;
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let staging = e.root.join(id).join(".staging");
    until("the staged copy is being received", 10, || {
        e.fake.st().media_held > held && staging.is_dir()
    })
    .await;
    drop(client);
    until(
        &format!("{path}: the staged copy goes with the request"),
        3,
        || !staging.exists(),
    )
    .await;
    assert!(
        entries(&e.root.join(".exports")).is_empty(),
        "{:?}",
        entries(&e.root.join(".exports"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_original_export_cut_off_leaves_no_staged_copy() {
    let e = Env::new().await;
    let id = slow_drive_video(&e).await;
    cut_off_during_staging(&e, &id, &format!("/api/logs/{id}/original/fcamera.hevc")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zip_of_originals_cut_off_leaves_no_staged_copy() {
    let e = Env::new().await;
    let id = slow_drive_video(&e).await;
    cut_off_during_staging(
        &e,
        &id,
        &format!("/api/logs/download.zip?ids={id}&parts=front&format=original"),
    )
    .await;
}
