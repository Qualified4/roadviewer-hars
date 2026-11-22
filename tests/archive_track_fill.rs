//! Drive maps backfill (Python `gps_track.TrackBackfill`, `run_track_fill`, `stop_track_fill`) against the loopback
//! Drive fake. The fixture is a recording imported from the Python Road Viewer before it made routes: converted
//! (prepared/data.json.gz, a summary without `track_v`) with its rlog only on Drive. The rlog is read by a stand-in
//! for the child process; the original drive data is tested separately.
mod common;
use axum::http::StatusCode;
use common::archive_env::*;
use roadviewer_hars::archive::{
    ArchiveRead,
    track_fill::{Reader, Timing},
};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const FIELDS: &str = r#"{"track_v":2,"has_track":true,"track_distance_m":12,"track_bbox":[1.0,2.0,3.0,4.0],"track_read":true}"#;

fn gzip(v: &Value) -> Vec<u8> {
    let mut g = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    g.write_all(v.to_string().as_bytes()).unwrap();
    g.finish().unwrap()
}

async fn env() -> Env {
    let e = Env::new().await;
    e.use_drive(json!({"archive_mode": "move", "archive_allow_local_delete": true, "archive_min_age_hours": 0}))
        .await;
    e
}

/// Converted by Python before routes existed; the rlog was then moved to Drive.
async fn imported(e: &Env, c: char) -> (String, Vec<u8>) {
    let log = data(5000, c as u64);
    let id = e.record(c, &[("rlog.zst", log.clone())], OLD).await;
    e.cycle().await;
    assert_eq!(e.loc(&id, "rlog.zst").await, "gdrive");
    let prepared = e.root.join(&id).join("prepared");
    std::fs::create_dir_all(&prepared).unwrap();
    std::fs::write(
        prepared.join("data.json.gz"),
        gzip(&json!({"frames": [{"id": 7, "t": 0.0}]})),
    )
    .unwrap();
    std::fs::write(prepared.join("summary.json"), br#"{"duration":60.0}"#).unwrap();
    (id, log)
}

#[derive(Default)]
struct Calls {
    rlogs: Vec<(PathBuf, Vec<u8>)>,
}

/// Records what it was handed and stages a route, as the child does.
fn reader(calls: Arc<Mutex<Calls>>, gate: Option<Arc<tokio::sync::Semaphore>>) -> Reader {
    Arc::new(move |rlog: PathBuf, prepared: PathBuf| {
        let (calls, gate) = (calls.clone(), gate.clone());
        Box::pin(async move {
            let bytes = std::fs::read(&rlog).ok()?;
            calls.lock().unwrap().rlogs.push((rlog, bytes));
            if let Some(g) = gate {
                g.acquire().await.ok()?.forget();
            }
            std::fs::write(
                prepared.join("track.json.gz.migrating"),
                gzip(&json!({"v": 2, "source": "gpsLocation", "points": [[0.0, 1.0, 2.0, null, null]]})),
            )
            .ok()?;
            serde_json::from_str(FIELDS).ok()
        })
    })
}

fn timing() -> Timing {
    Timing {
        start_delay: Duration::from_secs(3600),
        interval: Duration::from_secs(3600),
        busy_retry: Duration::from_millis(50),
        pause: Duration::ZERO,
    }
}

async fn status(e: &Env) -> Value {
    let (s, body) = e
        .req("GET", "/api/settings/archive/tracks/run", Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK);
    body["track_fill"].clone()
}

async fn until(e: &Env, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = status(e).await;
        if f(&s) {
            return s;
        }
        assert!(tokio::time::Instant::now() < deadline, "{what}: {s}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn summary(e: &Env, id: &str) -> Value {
    serde_json::from_slice(&std::fs::read(e.root.join(id).join("prepared/summary.json")).unwrap())
        .unwrap()
}

fn temps(e: &Env) -> Vec<String> {
    std::fs::read_dir(&e.root)
        .unwrap()
        .filter_map(|d| d.ok())
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".track-"))
        .collect()
}

/// Only reads reach Drive: no upload, change, move or delete.
fn only_reads(e: &Env, since: usize) {
    for r in e.fake.requests(since) {
        if r.path.starts_with("/drive") || r.path.starts_with("/upload") {
            assert_eq!(r.method, "GET", "{} {}", r.method, r.path);
        }
    }
}

#[tokio::test]
async fn a_pass_by_hand_reads_each_drive_only_log_once_and_completes() {
    let e = env().await;
    let (a, log_a) = imported(&e, 'a').await;
    let (b, log_b) = imported(&e, 'b').await;
    let calls = Arc::new(Mutex::new(Calls::default()));
    e.a().track_fill().set_reader(reader(calls.clone(), None));
    e.a().track_fill().set_timing(timing());
    e.a().start();
    let before = until(&e, "counted", |s| s["total"] == 2).await;
    assert_eq!(
        (
            before["done"].clone(),
            before["pending"].clone(),
            before["enabled"].clone()
        ),
        (json!(0), json!(2), json!(false))
    );
    let mark = e.fake.mark();
    let (s, body) = e
        .req("POST", "/api/settings/archive/tracks/run", Value::Null)
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["started"], true);
    let done = until(&e, "pass ended", |s| s["finished_at"].is_f64()).await;
    assert_eq!(
        (
            done["done"].clone(),
            done["pending"].clone(),
            done["running"].clone(),
            done["manual"].clone(),
            done["last_error"].clone()
        ),
        (json!(2), json!(0), json!(false), json!(false), Value::Null)
    );
    let read = calls.lock().unwrap().rlogs.clone();
    assert_eq!(read.len(), 2, "one read per log");
    let mut contents: Vec<Vec<u8>> = read.iter().map(|(_, b)| b.clone()).collect();
    contents.sort();
    let mut want = vec![log_a, log_b];
    want.sort();
    assert_eq!(contents, want, "the verified Drive copies were read");
    assert!(read.iter().all(|(p, _)| {
        p.starts_with(&e.root)
            && p.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".track-")
    }));
    assert!(temps(&e).is_empty(), "downloaded copies are removed");
    for id in [&a, &b] {
        let s = summary(&e, id);
        assert_eq!(
            (s["track_v"].clone(), s["has_track"].clone()),
            (json!(2), json!(true))
        );
        assert_eq!(s["duration"], 60.0, "the rest of the summary is kept");
        assert!(e.root.join(id).join("prepared/track.json.gz").is_file());
        assert!(
            !e.root
                .join(id)
                .join("prepared/track.json.gz.migrating")
                .exists()
        );
        assert_eq!(
            e.meta(id).await["has_track"],
            true,
            "the list shows the route"
        );
        assert_eq!(
            e.loc(id, "rlog.zst").await,
            "gdrive",
            "the index is unchanged"
        );
    }
    only_reads(&e, mark);
    // Read once: a second pass reads nothing.
    let (s, _) = e
        .req("POST", "/api/settings/archive/tracks/run", Value::Null)
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    until(&e, "second pass ended", |s| s["manual"] == false).await;
    assert_eq!(calls.lock().unwrap().rlogs.len(), 2);
}

#[tokio::test]
async fn stop_ends_the_pass_after_the_log_being_read() {
    let e = env().await;
    imported(&e, 'a').await;
    imported(&e, 'b').await;
    let calls = Arc::new(Mutex::new(Calls::default()));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    e.a()
        .track_fill()
        .set_reader(reader(calls.clone(), Some(gate.clone())));
    e.a().track_fill().set_timing(timing());
    e.a().start();
    let (s, _) = e
        .req("POST", "/api/settings/archive/tracks/run", Value::Null)
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (s, body) = e
        .req("POST", "/api/settings/archive/tracks/run", Value::Null)
        .await;
    assert_eq!(
        (s, body["error"].clone()),
        (StatusCode::CONFLICT, json!("track_fill_running"))
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while calls.lock().unwrap().rlogs.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no log was read");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (s, body) = e
        .req("POST", "/api/settings/archive/tracks/stop", Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        (
            body["track_fill"]["stopping"].clone(),
            body["track_fill"]["manual"].clone()
        ),
        (json!(true), json!(false))
    );
    gate.add_permits(10);
    let end = until(&e, "pass ended", |s| s["running"] == false).await;
    assert_eq!(
        calls.lock().unwrap().rlogs.len(),
        1,
        "the next log is not read"
    );
    assert_eq!(
        (end["done"].clone(), end["pending"].clone()),
        (json!(1), json!(1))
    );
    assert!(end["finished_at"].is_f64());
    assert!(temps(&e).is_empty());
}

#[tokio::test]
async fn a_failing_drive_backs_off_and_marks_nothing() {
    let e = env().await;
    let (a, _) = imported(&e, 'a').await;
    let calls = Arc::new(Mutex::new(Calls::default()));
    e.a().track_fill().set_reader(reader(calls.clone(), None));
    e.a().track_fill().set_timing(timing());
    e.a().start();
    e.fake.fail_next("alt=media", 503, None, 100, &[]);
    let mark = e.fake.mark();
    let (s, _) = e
        .req("POST", "/api/settings/archive/tracks/run", Value::Null)
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let st = until(&e, "backing off", |s| s["last_error"] == "network").await;
    assert!(st["retry_in"].as_u64().is_some_and(|r| r > 0), "{st}");
    assert_eq!(
        (st["running"].clone(), st["pending"].clone()),
        (json!(true), json!(1))
    );
    assert!(calls.lock().unwrap().rlogs.is_empty());
    let s = summary(&e, &a);
    assert!(
        s.get("track_v").is_none() && s.get("track_failures").is_none(),
        "{s}"
    );
    assert!(temps(&e).is_empty());
    only_reads(&e, mark);
    // Stopped while backing off: the pass ends at once.
    let (_, body) = e
        .req("POST", "/api/settings/archive/tracks/stop", Value::Null)
        .await;
    assert_eq!(body["track_fill"]["manual"], false);
}

#[tokio::test]
async fn a_corrupt_drive_copy_counts_as_a_failure_until_skipped() {
    let e = env().await;
    let (a, _) = imported(&e, 'a').await;
    let (file_id, _) = e.drive_file(&a, "rlog.zst").unwrap();
    e.fake.st().files.get_mut(&file_id).unwrap().content = data(5000, 99);
    let calls = Arc::new(Mutex::new(Calls::default()));
    e.a().track_fill().set_reader(reader(calls.clone(), None));
    e.a().track_fill().set_timing(timing());
    for n in 1..=3u64 {
        assert!(e.a().track_fill().request_run());
        e.a().backend().invalidate();
        assert!(!e.a().track_run_once().await);
        assert_eq!(summary(&e, &a)["track_failures"], n);
    }
    let st = status(&e).await;
    assert_eq!(
        (st["failed"].clone(), st["pending"].clone()),
        (json!(1), json!(0))
    );
    let s = summary(&e, &a);
    assert_eq!(
        (s["track_v"].clone(), s["track_read"].clone()),
        (json!(2), json!(false))
    );
    assert!(
        calls.lock().unwrap().rlogs.is_empty(),
        "a copy that does not verify is never read"
    );
}

#[tokio::test]
async fn after_a_restart_the_counts_come_from_disk_and_leftover_downloads_go() {
    let mut e = env().await;
    imported(&e, 'a').await;
    imported(&e, 'b').await;
    let calls = Arc::new(Mutex::new(Calls::default()));
    e.a().track_fill().set_reader(reader(calls.clone(), None));
    assert!(e.a().track_fill().request_run());
    assert!(!e.a().track_run_once().await);
    assert_eq!(status(&e).await["done"], 2);
    // A restart in the middle of a read left its download behind.
    let stale = e.root.join(".track-0123456789abcdef");
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("rlog.zst"), b"partial").unwrap();
    e.restart();
    e.a().track_fill().set_reader(reader(calls.clone(), None));
    e.a().track_fill().set_timing(timing());
    let st = status(&e).await;
    assert_eq!(
        (
            st["done"].clone(),
            st["pending"].clone(),
            st["manual"].clone(),
            st["running"].clone(),
            st["finished_at"].clone()
        ),
        (json!(2), json!(0), json!(false), json!(false), Value::Null)
    );
    e.a().start();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while stale.exists() {
        assert!(tokio::time::Instant::now() < deadline, "leftover kept");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        calls.lock().unwrap().rlogs.len(),
        2,
        "nothing is read again"
    );
}

#[tokio::test]
async fn without_drive_selected_a_pass_is_refused() {
    let e = Env::new().await;
    e.use_nas(json!({})).await;
    let (s, body) = e
        .req("POST", "/api/settings/archive/tracks/run", Value::Null)
        .await;
    assert_eq!(
        (s, body["error"].clone()),
        (StatusCode::CONFLICT, json!("gdrive_not_selected"))
    );
}
