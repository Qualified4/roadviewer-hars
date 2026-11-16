//! Crashes between the archiver's steps (Python `test_archiver_crash.py`, `test_gdrive_crash.py`): a new archiver
//! over the same data folder, share and Drive must converge to the same end state, and at every point each file of
//! every recording is local or on a verified copy the index points at.
mod common;
use axum::http::StatusCode;
use common::{archive_env::*, fake_drive::*};
use roadviewer_hars::archive::{
    ArchiveWrite, UploadResume, WriteBackend, index, pending::PENDING_DELETES,
};
use serde_json::{Value, json};
use std::time::Duration;

/// Every file a recording registered is local (with its registered content) or on the archive the index names,
/// with the indexed content.
async fn invariant(e: &Env, id: &str) {
    let m = e.meta(id).await;
    for (kind, h) in m["content_hashes"].as_object().unwrap() {
        let sha = h["sha256"].as_str().unwrap();
        let entry = index::entry(&m, kind);
        let local = std::fs::read(e.root.join(id).join(kind)).ok();
        match entry["location"].as_str().unwrap() {
            "local" => {
                let ok = local.as_deref().is_some_and(|d| sha_hex(d) == sha)
                    || index::remote(&m, kind).is_some_and(|b| {
                        e.fake
                            .file(b["file_id"].as_str().unwrap())
                            .is_some_and(|f| sha_hex(&f.content) == sha)
                    });
                assert!(ok, "{id}/{kind}: local entry without its content");
            }
            "gdrive" => {
                let f = e
                    .fake
                    .file(entry["file_id"].as_str().unwrap())
                    .expect("moved file on Drive");
                assert_eq!(
                    sha_hex(&f.content),
                    entry["sha256"].as_str().unwrap(),
                    "{id}/{kind}"
                );
                assert_eq!(entry["sha256"], sha);
            }
            "nas" => {
                let d = e.on_nas(id, kind).expect("archived file on the share");
                assert_eq!(sha_hex(&d), sha, "{id}/{kind}");
            }
            other => panic!("{id}/{kind}: unexpected location {other}"),
        }
    }
}

async fn nas_env() -> Env {
    let e = Env::new().await;
    // Activation first, with nothing old enough to move.
    e.use_nas(json!({"archive_nas_margin_mb": 0, "archive_min_age_hours": 8760}))
        .await;
    e.cycle().await;
    assert!(e.nas.join(".roadviewer-storage.json").exists());
    e
}
async fn drive_env(extra: Value) -> Env {
    let e = Env::new().await;
    let mut body = json!({"archive_min_age_hours": 8760});
    merge(&mut body, extra);
    e.use_drive(body).await;
    e.cycle().await;
    e
}
async fn go(e: &Env) {
    e.configure(json!({"archive_min_age_hours": 0})).await;
}

#[tokio::test]
async fn a_share_copy_left_before_its_index_commit_is_reused() {
    let e = nas_env().await;
    let log = data(5000, 1);
    let id = e.record('a', &[("rlog.zst", log.clone())], OLD).await;
    std::fs::create_dir_all(e.nas.join(&id)).unwrap();
    std::fs::write(e.nas.join(&id).join("rlog.zst"), &log).unwrap();
    let before = std::fs::metadata(e.nas.join(&id).join("rlog.zst"))
        .unwrap()
        .modified()
        .unwrap();
    go(&e).await;
    assert_eq!(e.cycle().await.archived, 1);
    assert_eq!(e.loc(&id, "rlog.zst").await, "nas");
    assert!(!e.local(&id, "rlog.zst"));
    let after = std::fs::metadata(e.nas.join(&id).join("rlog.zst"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        before, after,
        "the identical copy was kept, not written again"
    );
    invariant(&e, &id).await;
}

#[tokio::test]
async fn share_recovery_finishes_or_undoes_each_interrupted_step() {
    let mut e = nas_env().await;
    let (l1, l2, l3, l4) = (data(100, 1), data(200, 2), data(300, 3), data(400, 4));
    let committed = e.record('a', &[("rlog.zst", l1.clone())], OLD).await;
    let lost_index = e.record('b', &[("rlog.zst", l2.clone())], OLD).await;
    let differs = e.record('c', &[("rlog.zst", l3.clone())], OLD).await;
    let intent = e
        .record(
            'd',
            &[("rlog.zst", l4.clone()), ("fcamera.hevc", data(500, 5))],
            OLD,
        )
        .await;
    go(&e).await;
    assert_eq!(e.cycle().await.archived, 5);
    // a: committed, the local unlink lost (power cut): the identical local file goes.
    std::fs::write(e.root.join(&committed).join("rlog.zst"), &l1).unwrap();
    // b: the share holds the copy, the index commit was lost and the local file is gone: the copy is adopted.
    e.set(&lost_index, |m| {
        m["storage"].as_object_mut().unwrap().remove("rlog.zst");
    })
    .await;
    // c: the share copy differs from the index while the local file is the indexed content: local again.
    std::fs::write(e.nas.join(&differs).join("rlog.zst"), b"damaged").unwrap();
    std::fs::write(e.root.join(&differs).join("rlog.zst"), &l3).unwrap();
    // d: a cleanup that recorded its intent: before its unlink (file there) and after it (file gone).
    e.set(&intent, |m| {
        for kind in ["rlog.zst", "fcamera.hevc"] {
            m["storage"][kind]["removing"] = json!(true);
        }
    })
    .await;
    std::fs::remove_file(e.nas.join(&intent).join("fcamera.hevc")).unwrap();
    std::fs::write(e.nas.join(&intent).join("rlog.zst.part"), b"half").unwrap();
    e.restart();
    e.recover().await;
    assert!(!e.local(&committed, "rlog.zst"));
    assert_eq!(e.loc(&lost_index, "rlog.zst").await, "nas");
    assert_eq!(e.loc(&differs, "rlog.zst").await, "local");
    assert!(e.local(&differs, "rlog.zst"));
    let m = e.meta(&intent).await;
    assert_eq!(
        index::entry(&m, "rlog.zst").get("removing"),
        None,
        "withdrawn: the file is still there"
    );
    assert_eq!(index::entry(&m, "fcamera.hevc")["location"], "removed");
    assert!(!e.nas.join(&intent).join("rlog.zst.part").exists());
    for id in [&committed, &lost_index, &differs] {
        invariant(&e, id).await;
    }
    // The next cycle replaces the damaged share copy from the good local file.
    assert_eq!(e.cycle().await.archived, 1);
    assert_eq!(
        e.on_nas(&differs, "rlog.zst").as_deref(),
        Some(l3.as_slice())
    );
}

#[tokio::test]
async fn a_missing_share_copy_keeps_its_index_and_is_counted() {
    let mut e = nas_env().await;
    let id = e.record('a', &[("rlog.zst", data(100, 1))], OLD).await;
    go(&e).await;
    e.cycle().await;
    let before = e.meta(&id).await;
    std::fs::remove_file(e.nas.join(&id).join("rlog.zst")).unwrap();
    e.restart();
    e.recover().await;
    assert_eq!(
        e.meta(&id).await,
        before,
        "a lookup that missed proves nothing"
    );
    assert_eq!(e.a().status()["missing_archived"], 1);
    // Visible again: no longer counted.
    std::fs::write(e.nas.join(&id).join("rlog.zst"), data(100, 1)).unwrap();
    e.cycle().await;
    assert_eq!(e.a().status()["missing_archived"], 0);
}

fn drive(e: &Env) -> std::sync::Arc<roadviewer_hars::archive::DriveBackend> {
    match &*e.a().backend() {
        WriteBackend::Drive(d) => d.clone(),
        _ => panic!("drive"),
    }
}

#[tokio::test]
async fn a_completed_upload_whose_commit_was_lost_is_adopted_without_resending() {
    let mut e = drive_env(json!({})).await;
    let video = data(2 * CHUNK + 5, 3);
    let id = e.record('a', &[("fcamera.mp4", video.clone())], OLD).await;
    // The upload completed and the process died before the index commit: only the session record is there.
    let mut resume = UploadResume::default();
    let sha = sha_hex(&video);
    drive(&e)
        .upload(
            &e.root.join(&id).join("fcamera.mp4"),
            &id,
            "fcamera.mp4",
            Some(&sha),
            None,
            video.len() as u64,
            &mut resume,
            &mut |_, _| Ok(()),
        )
        .await
        .unwrap();
    let session = resume.session.clone().unwrap();
    e.set(&id, |m| {
        index::set_remote_pending(
            m,
            "fcamera.mp4",
            json!({"backend": "gdrive", "session": session.expose(), "started_at": 1.0,
            "bytes_sent": CHUNK, "sha256": sha, "size": video.len()}),
        );
    })
    .await;
    let sessions = e.fake.st().session_count;
    let mark = e.fake.mark();
    e.restart();
    e.recover().await;
    let block = index::remote(&e.meta(&id).await, "fcamera.mp4").expect("committed by recovery");
    assert_eq!(block["sha256"], sha);
    assert!(index::remote_pending(&e.meta(&id).await, "fcamera.mp4").is_none());
    assert_eq!(e.fake.st().session_count, sessions);
    assert!(
        e.fake
            .ranges(mark)
            .iter()
            .all(|r| r.starts_with("bytes */")),
        "recovery never uploads"
    );
    assert_eq!(e.drive_files().len(), 1);
    invariant(&e, &id).await;
}

#[tokio::test]
async fn a_move_stopped_before_its_local_unlink_is_finished_only_after_verification() {
    let mut e =
        drive_env(json!({"archive_mode": "move", "archive_allow_local_delete": true})).await;
    let (ok, bad) = (data(1000, 1), data(1200, 2));
    let a = e.record('a', &[("rlog.zst", ok.clone())], OLD).await;
    let b = e.record('b', &[("rlog.zst", bad.clone())], OLD).await;
    go(&e).await;
    e.cycle().await;
    assert_eq!(
        (e.loc(&a, "rlog.zst").await, e.loc(&b, "rlog.zst").await),
        ("gdrive".into(), "gdrive".into())
    );
    // Both local files are back (the unlink after the commit was lost).
    std::fs::write(e.root.join(&a).join("rlog.zst"), &ok).unwrap();
    std::fs::write(e.root.join(&b).join("rlog.zst"), &bad).unwrap();
    // b's Drive copy is damaged meanwhile.
    let (fid, _) = e.drive_file(&b, "rlog.zst").unwrap();
    {
        let mut st = e.fake.st();
        let f = st.files.get_mut(&fid).unwrap();
        f.content = b"damaged".to_vec();
        f.sha256 = Some(sha_hex(b"damaged"));
        f.md5 = None;
    }
    e.restart();
    e.recover().await;
    assert!(!e.local(&a, "rlog.zst"), "the verified move is finished");
    assert_eq!(e.loc(&a, "rlog.zst").await, "gdrive");
    assert!(
        e.local(&b, "rlog.zst"),
        "a copy that does not verify never costs the local file"
    );
    assert_eq!(e.loc(&b, "rlog.zst").await, "local");
    assert!(
        e.fake.file(&fid).is_some(),
        "the bad copy stays on Drive (never deleted automatically)"
    );
    invariant(&e, &a).await;
    invariant(&e, &b).await;
}

#[tokio::test]
async fn a_copy_of_replaced_content_is_dropped_and_copied_again() {
    let mut e = drive_env(json!({})).await;
    let id = e.record('a', &[("rlog.zst", data(1000, 1))], OLD).await;
    go(&e).await;
    e.cycle().await;
    let old = index::remote(&e.meta(&id).await, "rlog.zst").unwrap();
    // A conversion replaced the file and stopped before its index commit.
    std::fs::write(e.root.join(&id).join("rlog.zst"), data(1000, 2)).unwrap();
    e.restart();
    e.recover().await;
    assert!(index::remote(&e.meta(&id).await, "rlog.zst").is_none());
    assert!(
        e.fake.file(old["file_id"].as_str().unwrap()).is_some(),
        "nothing is deleted"
    );
    e.cycle().await;
    let new = index::remote(&e.meta(&id).await, "rlog.zst").unwrap();
    assert_eq!(new["sha256"], sha_hex(&data(1000, 2)));
}

/// The process dies at an arbitrary await point of a cycle (the task is dropped mid-request); a new archiver must
/// converge, never leaving a local file removed without its verified, committed copy.
#[tokio::test]
async fn killed_at_any_point_of_a_drive_move_the_next_run_converges() {
    for k in [1usize, 2, 3, 4, 5, 6, 7, 9, 12] {
        let mut e =
            drive_env(json!({"archive_mode": "move", "archive_allow_local_delete": true})).await;
        let (video, log) = (data(3 * CHUNK + 11, k as u64), data(900, 100 + k as u64));
        let id = e
            .record(
                'a',
                &[("fcamera.mp4", video.clone()), ("rlog.zst", log.clone())],
                OLD,
            )
            .await;
        go(&e).await;
        let base = e.fake.mark();
        let archive = e.a().clone();
        let task = tokio::spawn(async move { archive.run_cycle().await });
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while e.fake.mark() < base + k && !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        task.abort();
        let _ = task.await;
        invariant(&e, &id).await;
        e.restart();
        e.recover().await;
        invariant(&e, &id).await;
        for _ in 0..2 {
            e.cycle().await;
        }
        for (kind, content) in [("fcamera.mp4", &video), ("rlog.zst", &log)] {
            let entry = index::entry(&e.meta(&id).await, kind);
            assert_eq!(entry["location"], "gdrive", "k={k} {kind}");
            assert!(!e.local(&id, kind), "k={k} {kind}");
            assert_eq!(
                e.fake
                    .file(entry["file_id"].as_str().unwrap())
                    .unwrap()
                    .content,
                *content
            );
            assert!(entry.get("remote_pending").is_none());
        }
    }
}

#[tokio::test]
async fn killed_at_any_point_of_a_share_move_the_next_run_converges() {
    for delay in [0u64, 1, 2, 5, 10, 20, 50] {
        let mut e = nas_env().await;
        let mut ids = Vec::new();
        for (i, c) in ['a', 'b', 'c'].into_iter().enumerate() {
            ids.push(
                e.record(
                    c,
                    &[
                        ("fcamera.hevc", data(200_000 + i, i as u64)),
                        ("rlog.zst", data(100, 9 + i as u64)),
                    ],
                    OLD,
                )
                .await,
            );
        }
        go(&e).await;
        let archive = e.a().clone();
        let task = tokio::spawn(async move { archive.run_cycle().await });
        tokio::time::sleep(Duration::from_millis(delay)).await;
        task.abort();
        let _ = task.await;
        for id in &ids {
            invariant(&e, id).await;
        }
        e.restart();
        e.recover().await;
        e.cycle().await;
        for id in &ids {
            invariant(&e, id).await;
            for kind in ["fcamera.hevc", "rlog.zst"] {
                assert_eq!(e.loc(id, kind).await, "nas", "delay={delay}");
                assert!(!e.local(id, kind));
            }
        }
        let parts = walk_parts(&e.nas);
        assert!(
            parts.is_empty(),
            "no partial copy survives recovery: {parts:?}"
        );
    }
}

fn walk_parts(p: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(p).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk_parts(&path));
        } else if path.to_string_lossy().ends_with(".part") {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn uploads_and_conversions_come_first() {
    let e = drive_env(json!({})).await;
    let id = e.record('a', &[("rlog.zst", data(1000, 1))], OLD).await;
    let busy = e.record('b', &[("rlog.zst", data(1000, 2))], OLD).await;
    e.set(&busy, |m| m["status"] = json!("processing")).await;
    go(&e).await;
    // A browser upload is receiving: the cycle waits for it.
    let name = "2026-01-01--10-00-00--0--rlog.zst";
    let payload = data(4096, 3);
    let (status, session) = e
        .req(
            "POST",
            "/api/uploads",
            json!({"files": [{"name": name, "size": payload.len()}]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let upload = session["id"].as_str().unwrap().to_owned();
    let (status, _) = put(
        &e,
        &format!("/api/uploads/{upload}/files/0?offset=0"),
        payload[..2048].to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(e.cycle().await.stopped.as_deref(), Some("uploading"));
    assert!(index::remote(&e.meta(&id).await, "rlog.zst").is_none());
    let (status, _) = e
        .req("DELETE", &format!("/api/uploads/{upload}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    let r = e.cycle().await;
    assert_eq!((r.archived, r.stopped), (1, None));
    assert!(index::remote(&e.meta(&id).await, "rlog.zst").is_some());
    assert!(
        index::remote(&e.meta(&busy).await, "rlog.zst").is_none(),
        "a converting recording is left alone"
    );
}

async fn put(e: &Env, path: &str, body: Vec<u8>) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method("PUT")
        .uri(path)
        .header("x-roadviewer-request", "1")
        .body(axum::body::Body::from(body))
        .unwrap();
    let response = roadviewer_hars::http::ingress(e.app.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// An archive cycle, a manual run, explicit deletions, automatic cleanup and an upload at the same time: afterwards
/// every remaining recording keeps each file locally or on a verified copy, and the deleted ones' Drive files go.
#[tokio::test]
async fn concurrent_archiving_uploads_cleanup_and_deletions_keep_every_file() {
    let e = drive_env(json!({"archive_mode": "move", "archive_allow_local_delete": true})).await;
    let mut ids = Vec::new();
    for (i, c) in ['a', 'b', 'c', 'd', 'e', 'f'].into_iter().enumerate() {
        ids.push(
            e.record(
                c,
                &[
                    ("fcamera.mp4", data(CHUNK + 100 * i, i as u64)),
                    ("rlog.zst", data(500 + i, 50 + i as u64)),
                ],
                OLD + i as f64,
            )
            .await,
        );
    }
    go(&e).await;
    let (status, _) = e
        .req(
            "POST",
            "/api/settings/storage",
            json!({"max_bytes": 1, "policy": "delete_oldest"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let archive = e.a().clone();
    let cycle = tokio::spawn(async move { archive.run_cycle().await });
    let deletes = async {
        for id in &ids[..2] {
            tokio::task::yield_now().await;
            let (status, _) = e
                .req("DELETE", &format!("/api/logs/{id}"), Value::Null)
                .await;
            assert_eq!(status, StatusCode::OK);
        }
    };
    let upload = async {
        let name = "2026-02-01--10-00-00--0--rlog.zst";
        let payload = data(3000, 77);
        let (status, s) = e
            .req(
                "POST",
                "/api/uploads",
                json!({"files": [{"name": name, "size": payload.len()}]}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{s}");
        let id = s["id"].as_str().unwrap().to_owned();
        put(&e, &format!("/api/uploads/{id}/files/0?offset=0"), payload).await;
        e.req("POST", &format!("/api/uploads/{id}/finish"), Value::Null)
            .await
    };
    let ((), (finished, _)) = tokio::join!(deletes, upload);
    assert_eq!(
        finished,
        StatusCode::CREATED,
        "admission with Drive in move mode never refuses a safe upload"
    );
    let _ = cycle.await;
    // Drain: further cycles finish the moves and the queued deletions.
    for _ in 0..3 {
        e.cycle().await;
    }
    let (status, _) = e.req("GET", "/api/settings/storage", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    for id in &ids[2..] {
        if e.app.store.meta(id).await.is_ok() {
            invariant(&e, id).await;
        }
    }
    for id in &ids[..2] {
        assert!(e.app.store.meta(id).await.is_err());
        assert!(!e.root.join(id).exists());
    }
    // Every Drive file the remaining indexes refer to exists; the deleted recordings' referenced files are gone
    // once the journal is empty.
    assert!(
        !e.root.join(PENDING_DELETES).exists(),
        "every queued deletion finished"
    );
    let referenced: Vec<String> = {
        let mut out = Vec::new();
        for m in e.app.store.records.read().await.values() {
            out.extend(index::remote_refs(m, true).into_iter().map(|r| r.file_id));
        }
        out
    };
    for f in &referenced {
        assert!(e.fake.file(f).is_some());
    }
    for (fid, f) in e.drive_files() {
        let owner = f.props.get("rv_id").cloned().unwrap_or_default();
        if ids[..2].contains(&owner) {
            // Only an upload still in flight when its recording was deleted can leave an orphan (as Python).
            assert!(!referenced.contains(&fid));
        }
    }
}
