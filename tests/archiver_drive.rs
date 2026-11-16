//! The archiver's Drive side (Python `test_gdrive_archiver.py`, `test_manual_archive.py`, `test_gdrive_settings.py`,
//! `test_drive_capacity.py`) against the loopback Drive fake. Nothing here reaches Google.
mod common;
use axum::http::StatusCode;
use common::{archive_env::*, fake_drive::*};
use roadviewer_hars::archive::{
    ArchiveRead, index,
    pending::{PENDING_DELETES, PendingDeletes},
    resolve::Resolved,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

type Case = (&'static str, fn(&Shared));

// --- Drive

async fn drive_env(extra: Value) -> Env {
    let e = Env::new().await;
    e.use_drive(extra).await;
    e
}
fn sessions(e: &Env) -> u32 {
    e.fake.st().session_count
}

#[tokio::test]
async fn drive_copy_mode_uploads_verifies_and_keeps_local_files() {
    let e = drive_env(json!({})).await;
    let (video, log) = (data(600_000, 1), data(1000, 2));
    let id = e
        .record(
            'a',
            &[("fcamera.mp4", video.clone()), ("rlog.zst", log.clone())],
            OLD,
        )
        .await;
    let r = e.cycle().await;
    assert_eq!((r.archived, r.stopped.clone()), (2, None), "{r:?}");
    let m = e.meta(&id).await;
    for (kind, content) in [("fcamera.mp4", &video), ("rlog.zst", &log)] {
        let entry = index::entry(&m, kind);
        assert_eq!(entry["location"], "local");
        let block = &entry["remote"];
        assert_eq!(block["backend"], "gdrive");
        assert_eq!(block["bytes"], content.len());
        assert_eq!(block["sha256"], sha_hex(content));
        assert!(
            block["verified_by"].is_string()
                && block["uploaded_at"].is_f64()
                && block["md5"].is_string()
        );
        assert!(entry.get("remote_pending").is_none());
        assert!(e.local(&id, kind), "copy mode never removes the local file");
        let (file_id, f) = e.drive_file(&id, kind).unwrap();
        assert_eq!(block["file_id"], file_id);
        assert_eq!(f.content, *content);
        assert_eq!(f.props["hars_owner"], "roadviewer-hars");
    }
    // A verified copy is done: the next cycle uploads nothing.
    let before = sessions(&e);
    assert_eq!(e.cycle().await.archived, 0);
    assert_eq!(sessions(&e), before);
    let folders = e.fake.folders();
    assert!(
        folders
            .values()
            .any(|f| f.name == "Road Viewer HARS" && f.props["hars_owner"] == "roadviewer-hars")
    );
    // The list shows the copies, never the index or file ids.
    let (_, list) = e.req("GET", "/api/logs", Value::Null).await;
    let row = &list["logs"][0];
    assert_eq!(
        row["remote_copies"],
        json!({"rlog": "gdrive", "front": "gdrive"})
    );
    assert!(row.get("storage").is_none() && !list.to_string().contains(block_id(&m).as_str()));
    let (_, storage) = e.req("GET", "/api/settings/storage", Value::Null).await;
    let block = &storage["archive"];
    assert_eq!(
        (block["backend"].clone(), block["effective_mode"].clone()),
        (json!("gdrive"), json!("copy"))
    );
    assert_eq!(block["remote_copies_files"], 2);
    assert_eq!(block["gdrive"]["connected"], true);
}

fn block_id(m: &Value) -> String {
    index::remote(m, "rlog.zst").unwrap()["file_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn drive_move_requires_the_explicit_local_delete_permission() {
    let e = drive_env(json!({"archive_mode": "move"})).await;
    let id = e.record('a', &[("rlog.zst", data(1000, 1))], OLD).await;
    e.cycle().await;
    assert!(
        e.local(&id, "rlog.zst"),
        "move without allow_local_delete is a copy"
    );
    assert_eq!(e.loc(&id, "rlog.zst").await, "local");
    e.configure(json!({"archive_allow_local_delete": true}))
        .await;
    let b = e
        .record(
            'b',
            &[
                ("fcamera.mp4", data(300_000, 2)),
                ("rlog.zst", data(500, 3)),
            ],
            OLD,
        )
        .await;
    let r = e.cycle().await;
    assert_eq!(r.stopped, None);
    for kind in ["fcamera.mp4", "rlog.zst"] {
        let entry = index::entry(&e.meta(&b).await, kind);
        assert_eq!(entry["location"], "gdrive");
        assert!(entry["file_id"].is_string() && entry["sha256"].is_string());
        assert!(!e.local(&b, kind));
    }
    // The earlier copy is moved too: its verified remote copy is found and committed as the stored file.
    assert_eq!(e.loc(&id, "rlog.zst").await, "gdrive");
    assert!(!e.local(&id, "rlog.zst"));
    let m = e.meta(&b).await;
    let (found, state) = e.a().resolve(&b, &m, "rlog.zst").await;
    assert_eq!(state, "available");
    assert!(matches!(found, Resolved::Remote(f) if f.size == 500));
}

#[tokio::test]
async fn an_upload_cut_off_resumes_from_its_offset_next_cycle() {
    let e = drive_env(json!({})).await;
    let video = data(3 * CHUNK + 100, 7);
    let id = e.record('a', &[("fcamera.mp4", video.clone())], OLD).await;
    // The line dies once the first chunk is stored: nothing more reaches the session, so the cycle fails.
    e.fake.st().cut_after = Some(CHUNK);
    let r = e.cycle().await;
    assert_eq!(r.stopped.as_deref(), Some("storage_unavailable"), "{r:?}");
    let pending =
        index::remote_pending(&e.meta(&id).await, "fcamera.mp4").expect("session recorded");
    assert_eq!(pending["backend"], "gdrive");
    assert_eq!(
        (pending["size"].clone(), pending["sha256"].clone()),
        (json!(video.len()), json!(sha_hex(&video)))
    );
    assert!(e.local(&id, "fcamera.mp4"));
    e.fake.st().cut_after = None;
    let mark = e.fake.mark();
    let r = e.cycle().await;
    assert_eq!((r.archived, r.stopped), (1, None));
    let ranges = e.fake.ranges(mark);
    assert_eq!(
        ranges.first().map(String::as_str),
        Some(format!("bytes */{}", video.len()).as_str())
    );
    assert!(
        ranges[1].starts_with(&format!("bytes {CHUNK}-")),
        "resumed at the offset Drive holds: {ranges:?}"
    );
    assert_eq!(sessions(&e), 1, "the same session");
    let m = e.meta(&id).await;
    assert!(index::remote_pending(&m, "fcamera.mp4").is_none());
    assert_eq!(e.drive_file(&id, "fcamera.mp4").unwrap().1.content, video);
}

#[tokio::test]
async fn an_expired_session_after_a_restart_starts_again_from_zero() {
    let mut e = drive_env(json!({})).await;
    let video = data(2 * CHUNK + 10, 8);
    let id = e.record('a', &[("fcamera.mp4", video.clone())], OLD).await;
    e.fake.st().cut_after = Some(CHUNK);
    e.cycle().await;
    assert!(index::remote_pending(&e.meta(&id).await, "fcamera.mp4").is_some());
    // Google forgets the session (about a week later); HARS restarts meanwhile.
    e.fake.st().cut_after = None;
    e.fake.st().sessions.clear();
    e.restart();
    e.recover().await;
    assert!(
        index::remote_pending(&e.meta(&id).await, "fcamera.mp4").is_none(),
        "an expired session is cleared"
    );
    let mark = e.fake.mark();
    let r = e.cycle().await;
    assert_eq!((r.archived, r.stopped), (1, None));
    assert!(e.fake.ranges(mark)[0].starts_with("bytes 0-"));
    assert_eq!(e.drive_file(&id, "fcamera.mp4").unwrap().1.content, video);
}

#[tokio::test]
async fn a_copy_that_does_not_verify_is_marked_and_retried_on_request() {
    let e = drive_env(json!({"archive_mode": "move", "archive_allow_local_delete": true})).await;
    let log = data(1000, 9);
    let id = e.record('a', &[("rlog.zst", log.clone())], OLD).await;
    e.fake.st().completion_override = Some(json!({"size": "1"}));
    let r = e.cycle().await;
    assert_eq!(r.stopped.as_deref(), Some("hash_mismatch"));
    let entry = index::entry(&e.meta(&id).await, "rlog.zst");
    assert_eq!(
        entry,
        json!({"location": "local", "error": "remote_mismatch"})
    );
    assert!(e.local(&id, "rlog.zst"), "the local file stays");
    // Waits for attention: the next cycles leave it alone.
    let before = sessions(&e);
    e.cycle().await;
    assert_eq!(sessions(&e), before);
    let (_, list) = e.req("GET", "/api/logs", Value::Null).await;
    assert_eq!(
        list["logs"][0]["archive_error"],
        json!({"rlog.zst": "remote_mismatch"})
    );
    let (status, body) = e
        .req(
            "POST",
            &format!("/api/logs/{id}/archive/retry"),
            json!({"kinds": ["fcamera.mp4"]}),
        )
        .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("no_archive_error"))
    );
    let (status, _) = e
        .req(
            "POST",
            &format!("/api/logs/{id}/archive/retry"),
            json!({"kinds": ["nope"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = e
        .req(
            "POST",
            &format!("/api/logs/{id}/archive/retry"),
            Value::Null,
        )
        .await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({"retried": ["rlog.zst"]}))
    );
    assert!(
        e.meta(&id)
            .await
            .get("storage")
            .and_then(|s| s.get("rlog.zst"))
            .is_none()
    );
    let r = e.cycle().await;
    assert_eq!((r.archived, r.stopped), (1, None));
    assert_eq!(e.loc(&id, "rlog.zst").await, "gdrive");
}

#[tokio::test]
async fn remote_failures_keep_local_files_and_back_off() {
    let cases: [Case; 4] = [
        ("storage_unavailable", |f| {
            f.fail_next("/drive/v3", 503, None, 10_000, &[])
        }),
        ("auth_expired", |f| {
            let mut st = f.st();
            st.access.clear();
            st.refresh.clear();
        }),
        ("rate_limited", |f| {
            f.fail_next("/drive/v3", 429, Some("rateLimitExceeded"), 10_000, &[])
        }),
        ("remote_full", |f| f.st().quota_limit = Some(10)),
    ];
    for (expected, fail) in cases {
        let e =
            drive_env(json!({"archive_mode": "move", "archive_allow_local_delete": true})).await;
        e.cycle().await; // The HARS folder exists; then Drive starts failing.
        let id = e.record('a', &[("rlog.zst", data(1000, 1))], OLD).await;
        let before = e.meta(&id).await;
        fail(&e.fake);
        e.a().backend().invalidate();
        for n in 1..=2u64 {
            let r = e.cycle().await;
            assert_eq!(r.stopped.as_deref(), Some(expected), "{expected}");
            assert_eq!(e.a().status()["consecutive_failures"], n, "{expected}");
        }
        assert!(e.local(&id, "rlog.zst"), "{expected}");
        let after = e.meta(&id).await;
        assert_eq!(index::location(&after, "rlog.zst"), "local", "{expected}");
        assert!(index::remote(&after, "rlog.zst").is_none(), "{expected}");
        assert_eq!(after["content_hashes"], before["content_hashes"]);
        if expected == "auth_expired" {
            assert_eq!(e.a().status()["last_auth_error"], "auth_expired");
        }
        assert!(
            e.drive_files().is_empty(),
            "{expected}: nothing is left half-written on Drive"
        );
    }
}

#[tokio::test]
async fn an_identical_copy_already_on_drive_is_adopted_without_uploading() {
    let e = drive_env(json!({})).await;
    e.cycle().await; // Sets up the HARS folder.
    let log = data(2000, 4);
    let id = e.record('a', &[("rlog.zst", log.clone())], OLD).await;
    let folder = e
        .fake
        .folders()
        .into_iter()
        .find(|(_, f)| f.name == "Road Viewer HARS")
        .unwrap()
        .0;
    let sid = e.a().backend().storage_id().unwrap();
    let sha = sha_hex(&log);
    e.fake.put(
        "adopt-me",
        FFile::media(
            &log,
            &[&folder],
            &[
                ("rv_id", &id),
                ("rv_kind", "rlog.zst"),
                ("rv_sha256", &sha),
                ("rv_storage_id", &sid),
                ("hars_owner", "roadviewer-hars"),
            ],
        ),
    );
    // Same name, other content: not this file's copy.
    let other = data(2000, 5);
    e.fake.put(
        "other",
        FFile::media(
            &other,
            &[&folder],
            &[
                ("rv_id", &id),
                ("rv_kind", "rlog.zst"),
                ("rv_sha256", &sha_hex(&other)),
                ("rv_storage_id", &sid),
                ("hars_owner", "roadviewer-hars"),
            ],
        ),
    );
    let before = sessions(&e);
    assert_eq!(e.cycle().await.archived, 1);
    assert_eq!(sessions(&e), before, "nothing uploaded");
    assert_eq!(
        index::remote(&e.meta(&id).await, "rlog.zst").unwrap()["file_id"],
        "adopt-me"
    );
    assert_eq!(
        e.fake.file("other").unwrap().content,
        other,
        "an orphan is never deleted"
    );
}

#[tokio::test]
async fn deleting_a_recording_queues_its_drive_files_until_drive_answers() {
    let e = drive_env(json!({"archive_mode": "move", "archive_allow_local_delete": true})).await;
    let id = e
        .record(
            'a',
            &[
                ("fcamera.mp4", data(300_000, 1)),
                ("rlog.zst", data(100, 2)),
            ],
            OLD,
        )
        .await;
    let keep = e.record('b', &[("rlog.zst", data(100, 3))], OLD).await;
    e.cycle().await;
    let files: Vec<String> = ["fcamera.mp4", "rlog.zst"]
        .iter()
        .map(|k| e.drive_file(&id, k).unwrap().0)
        .collect();
    let (status, _) = e
        .req("DELETE", &format!("/api/logs/{id}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!e.root.join(&id).exists());
    let rows = PendingDeletes::new(&e.root).rows().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["explicit"], true);
    assert_eq!(rows[0]["refs"].as_array().unwrap().len(), 2);
    // Only queued by the request: the archiver cycle removes them, and keeps the row while Drive fails.
    assert!(files.iter().all(|f| e.fake.file(f).is_some()));
    for f in &files {
        e.fake
            .fail_next(&format!("/drive/v3/files/{f}"), 503, None, 10_000, &[]);
    }
    e.cycle().await;
    assert!(files.iter().all(|f| e.fake.file(f).is_some()));
    assert_eq!(PendingDeletes::new(&e.root).count().await.unwrap(), 2);
    e.fake.st().failures.clear();
    e.cycle().await;
    assert!(files.iter().all(|f| e.fake.file(f).is_none()));
    assert!(!e.root.join(PENDING_DELETES).exists());
    assert!(
        e.drive_file(&keep, "rlog.zst").is_some(),
        "other recordings' files stay"
    );
}

#[tokio::test]
async fn automatic_cleanup_never_deletes_a_recording_with_drive_files() {
    let e = drive_env(json!({})).await;
    let id = e.record('a', &[("rlog.zst", data(1000, 1))], OLD).await;
    e.cycle().await;
    let plan = roadviewer_hars::storage::eviction_plan(&e.app.store, &[], &Default::default())
        .await
        .unwrap();
    assert_eq!(
        plan.kept.get(&id),
        Some(&roadviewer_hars::storage::Protection::RemoteCopy)
    );
    let registration = e.app.store.registration.lock().await;
    let deleted = roadviewer_hars::storage::delete_records(
        &e.app.store,
        &registration,
        std::slice::from_ref(&id),
        &[],
        "storage_limit",
        |_, _| roadviewer_hars::storage::Judgement::Delete,
    )
    .await
    .unwrap();
    assert!(
        deleted.is_empty(),
        "only an explicit deletion may remove Drive files"
    );
    assert!(e.root.join(&id).exists());
    assert!(!e.root.join(PENDING_DELETES).exists());
}

/// Python data on the same Drive account: a folder named like Python's, carrying Python's storage id, with media
/// files for the same recording ids and contents. Nothing HARS does may touch it.
#[tokio::test]
async fn python_drive_data_is_never_written_moved_or_deleted() {
    let e = Env::new().await;
    let log = data(1500, 11);
    let id = e
        .record(
            'a',
            &[
                ("rlog.zst", log.clone()),
                ("fcamera.mp4", data(300_000, 12)),
            ],
            OLD,
        )
        .await;
    e.fake.put(
        "py-folder",
        FFile::folder(
            "Road Viewer",
            &[("rv_storage_id", "py-sid")],
            "2026-01-01T00:00:00Z",
        ),
    );
    let sha = sha_hex(&log);
    e.fake.put(
        "py-file",
        FFile::media(
            &log,
            &["py-folder"],
            &[
                ("rv_id", &id),
                ("rv_kind", "rlog.zst"),
                ("rv_sha256", &sha),
                ("rv_storage_id", "py-sid"),
            ],
        ),
    );
    // Even a file wrongly carrying the HARS owner mark, but in Python's folder, is not HARS's.
    e.fake.put(
        "py-marked",
        FFile::media(
            b"x",
            &["py-folder"],
            &[
                ("rv_id", &id),
                ("rv_kind", "camera.mp4"),
                ("rv_storage_id", "py-sid"),
                ("hars_owner", "roadviewer-hars"),
            ],
        ),
    );
    let snapshot = |e: &Env| -> BTreeMap<String, String> {
        let st = e.fake.st();
        ["py-folder", "py-file", "py-marked"]
            .iter()
            .map(|k| (k.to_string(), format!("{:?}", st.files.get(*k))))
            .collect()
    };
    let before = snapshot(&e);
    e.use_drive(json!({})).await;
    for extra in [
        json!({}),
        json!({"archive_mode": "move", "archive_allow_local_delete": true}),
        json!({"archive_gdrive_folder_name": "Road Viewer"}),
    ] {
        e.configure(extra).await;
        e.cycle().await;
        e.a().manual_plan().await;
    }
    assert_ne!(
        index::remote_refs(&e.meta(&id).await, true)
            .first()
            .map(|r| r.file_id.as_str()),
        Some("py-file"),
        "never adopted"
    );
    // A forged pending deletion pointing at Python's files is dropped as foreign, never executed.
    let forged = index::Ref {
        kind: "rlog.zst".into(),
        backend: "gdrive".into(),
        file_id: "py-file".into(),
        bytes: 1500,
        sha256: Some(sha.clone()),
    };
    let folder = index::Ref {
        file_id: "py-folder".into(),
        ..forged.clone()
    };
    let marked = index::Ref {
        file_id: "py-marked".into(),
        ..forged.clone()
    };
    PendingDeletes::new(&e.root)
        .add(&"d".repeat(32), &[], &[forged, folder, marked], true)
        .await
        .unwrap();
    let (status, _) = e
        .req("DELETE", &format!("/api/logs/{id}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    e.cycle().await;
    assert!(
        !e.root.join(PENDING_DELETES).exists(),
        "foreign targets are not tried again"
    );
    let (status, _) = e
        .req(
            "POST",
            "/api/settings/archive/gdrive/disconnect",
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot(&e), before);
    assert!(
        e.drive_files().iter().all(|(k, _)| k.starts_with("py-")),
        "HARS's own files went with the recording"
    );
}

#[tokio::test]
async fn manual_run_archives_everything_past_the_batch_and_survives_a_restart() {
    let mut e = drive_env(json!({"archive_batch": 1, "archive_local_threshold_mb": 4096})).await;
    for (i, c) in ['a', 'b', 'c'].into_iter().enumerate() {
        e.record(c, &[("rlog.zst", data(1000 + i, i as u64))], OLD)
            .await;
    }
    let young = e
        .record(
            'd',
            &[("rlog.zst", data(10, 9))],
            roadviewer_hars::store::now(),
        )
        .await;
    let (status, body) = e.req("GET", "/api/settings/archive/run", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let plan = &body["plan"];
    assert_eq!(
        (
            plan["files"].clone(),
            plan["recordings"].clone(),
            plan["mode"].clone()
        ),
        (json!(3), json!(3), json!("copy"))
    );
    assert_eq!(plan["excluded"], json!({"min_age": 1, "not_ready": 0}));
    assert_eq!(
        plan["local_delete_blocked"], true,
        "move is configured, deleting is not allowed"
    );
    let (status, body) = e
        .req("POST", "/api/settings/archive/run", Value::Null)
        .await;
    assert_eq!(
        (status, body["started"].clone()),
        (StatusCode::ACCEPTED, json!(true))
    );
    wait(|| !e.a().manual_active()).await;
    let job = e.a().status()["manual"].clone();
    assert_eq!(
        (
            job["state"].clone(),
            job["succeeded"].clone(),
            job["failed"].clone()
        ),
        (json!("done"), json!(3), json!(0))
    );
    assert_eq!(index::location(&e.meta(&young).await, "rlog.zst"), "local");
    assert!(
        index::remote(&e.meta(&young).await, "rlog.zst").is_none(),
        "the minimum age stays"
    );
    // A run stopped by a restart is reported as interrupted.
    let path = e.root.join(".archive-manual.json");
    let mut saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved["state"] = json!("running");
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    e.restart();
    assert_eq!(e.a().status()["manual"]["state"], "interrupted");
}

#[tokio::test]
async fn while_a_manual_run_works_settings_are_locked() {
    let e = drive_env(json!({})).await;
    let id = e
        .record('a', &[("fcamera.mp4", data(3 * CHUNK, 1))], OLD)
        .await;
    // The run's index commit waits on this recording's lock, so the run stays active meanwhile.
    let lock = e.app.store.record_lock(&id).await;
    let guard = lock.lock().await;
    let (status, _) = e
        .req("POST", "/api/settings/archive/run", Value::Null)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(e.a().manual_active());
    let (status, body) = e
        .req(
            "POST",
            "/api/settings/storage",
            json!({"max_bytes": 0, "policy": "reject_new"}),
        )
        .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("archive_running"))
    );
    let (status, _) = e
        .req("POST", "/api/settings/archive", json!({"archive_batch": 2}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = e
        .req("POST", "/api/settings/archive/run", Value::Null)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, storage) = e.req("GET", "/api/settings/storage", Value::Null).await;
    assert_eq!(storage["archive"]["manual_active"], true);
    drop(guard);
    wait(|| !e.a().manual_active()).await;
    assert_eq!(e.a().status()["manual"]["state"], "done");
    assert!(index::remote(&e.meta(&id).await, "fcamera.mp4").is_some());
}

async fn wait(done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
}

#[tokio::test]
async fn drive_sign_in_routes_report_codes_never_tokens() {
    let e = Env::new().await;
    let (status, body) = e
        .req("POST", "/api/settings/archive/gdrive/connect", Value::Null)
        .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("gdrive_not_selected"))
    );
    e.configure(json!({"archive_backend": "gdrive"})).await;
    e.a().set_connection_interval(Duration::from_millis(1));
    let (status, body) = e
        .req("POST", "/api/settings/archive/gdrive/connect", Value::Null)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["user_code"], "ABCD-EFGH");
    assert!(body["expires_in"].as_u64().unwrap() > 0);
    wait(|| roadviewer_hars::archive::DriveAuth::connected(&drive_auth(&e))).await;
    let (status, body) = e
        .req("POST", "/api/settings/archive/gdrive/connect", Value::Null)
        .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("gdrive_connected"))
    );
    let (_, storage) = e.req("GET", "/api/settings/storage", Value::Null).await;
    assert_eq!(storage["archive"]["gdrive"]["connected"], true);
    assert!(!storage.to_string().contains("secret"));
    let (status, body) = e
        .req(
            "POST",
            "/api/settings/archive/gdrive/disconnect",
            Value::Null,
        )
        .await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({"connected": false}))
    );
    assert!(!drive_auth(&e).connected());
    // Range: the next work item's routes answer 501, never a fake success.
    let (status, _) = e
        .req("POST", "/api/settings/archive/tracks/run", Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}

fn drive_auth(e: &Env) -> std::sync::Arc<roadviewer_hars::archive::DriveAuth> {
    match &*e.a().backend() {
        roadviewer_hars::archive::WriteBackend::Drive(d) => d.auth.clone(),
        _ => panic!("drive"),
    }
}

/// Upload admission's capacity goal (Python `request_reclaim` / `_reclaim`): pinned recordings move too, but only
/// after every other candidate, and the batch still bounds one run.
#[tokio::test]
async fn capacity_moves_take_pinned_recordings_last() {
    let e = drive_env(json!({"archive_mode": "move", "archive_allow_local_delete": true, "archive_batch": 1, "archive_min_age_hours": 0})).await;
    let pinned = e.record('a', &[("rlog.zst", data(1000, 1))], OLD).await;
    let other = e.record('b', &[("rlog.zst", data(1000, 2))], 2.0).await;
    let (status, _) = e
        .req(
            "POST",
            &format!("/api/logs/{pinned}/pin"),
            json!({"pinned": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = e
        .req(
            "POST",
            "/api/settings/storage",
            json!({"max_bytes": 1, "policy": "reject_new"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(e.a().drive_move(), Some(0));
    e.a().request_reclaim(0, &[], &Default::default());
    wait(|| !e.a().running()).await;
    assert_eq!(
        e.loc(&other, "rlog.zst").await,
        "gdrive",
        "the unpinned recording first"
    );
    assert_eq!(
        e.loc(&pinned, "rlog.zst").await,
        "local",
        "the batch bounds one run"
    );
    e.a().request_reclaim(0, &[], &Default::default());
    wait(|| !e.a().running()).await;
    assert_eq!(
        e.loc(&pinned, "rlog.zst").await,
        "gdrive",
        "a pin protects from deletion, not from moving"
    );
    assert!(
        e.root.join(&pinned).exists(),
        "moving never deletes the recording"
    );
}
