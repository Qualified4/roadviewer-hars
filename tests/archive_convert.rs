//! Converting again a recording whose inputs the archiver moved (Python `run_job`, `convert`, `clear_prepared`,
//! `stage_remote`, `requeue_deferred`). The converter itself is not run here; integration/archive_e2e.py converts
//! real logs read from the share and from Drive.
mod common;
use axum::http::StatusCode;
use common::archive_env::*;
use roadviewer_hars::{archive::ArchiveRead, media_read};
use serde_json::{Value, json};
use std::time::Duration;

async fn moved(e: &Env, video: &[u8], log: &[u8]) -> String {
    let id = e
        .record(
            'a',
            &[("fcamera.hevc", video.to_vec()), ("rlog.zst", log.to_vec())],
            OLD,
        )
        .await;
    e.cycle().await;
    assert!(!e.local(&id, "fcamera.hevc") && !e.local(&id, "rlog.zst"));
    id
}

#[tokio::test]
async fn share_inputs_are_read_where_they_are() {
    let e = Env::new().await;
    e.use_nas(json!({"archive_nas_margin_mb": 0})).await;
    let id = moved(&e, &data(4000, 1), &data(300, 2)).await;
    let folder = e.root.join(&id);
    let inputs = media_read::conversion_inputs(e.a(), &folder, &id, &e.meta(&id).await).await;
    assert_eq!(inputs.deferred, None);
    assert!(inputs.staged.is_empty(), "nothing is copied from the share");
    assert_eq!(inputs.paths["rlog.zst"], e.nas.join(&id).join("rlog.zst"));
    assert_eq!(
        inputs.paths["fcamera.hevc"],
        e.nas.join(&id).join("fcamera.hevc")
    );
    assert_eq!(inputs.states["camera.mp4"], "missing");
}

#[tokio::test]
async fn drive_inputs_are_staged_verified_and_removed() {
    let e = Env::new().await;
    e.use_drive(json!({"archive_mode": "move", "archive_allow_local_delete": true}))
        .await;
    let (video, log) = (data(4000, 3), data(300, 4));
    let id = moved(&e, &video, &log).await;
    let folder = e.root.join(&id);
    let inputs = media_read::conversion_inputs(e.a(), &folder, &id, &e.meta(&id).await).await;
    assert_eq!(inputs.deferred, None);
    assert_eq!(inputs.staged.len(), 2);
    assert_eq!(std::fs::read(&inputs.paths["rlog.zst"]).unwrap(), log);
    assert_eq!(std::fs::read(&inputs.paths["fcamera.hevc"]).unwrap(), video);
    assert!(inputs.paths["rlog.zst"].starts_with(folder.join(".staging")));
    for path in &inputs.staged {
        media_read::unstage(path).await;
    }
    assert!(!folder.join(".staging").exists());
    // A copy that does not match its index is never handed to the converter.
    let (file_id, _) = e.drive_file(&id, "rlog.zst").unwrap();
    e.fake.st().files.get_mut(&file_id).unwrap().content = data(300, 5);
    e.a().backend().invalidate();
    let meta = e.meta(&id).await;
    let inputs = media_read::conversion_inputs(e.a(), &folder, &id, &meta).await;
    assert_ne!(inputs.states["rlog.zst"], "available");
    assert!(!inputs.paths["rlog.zst"].starts_with(folder.join(".staging")));
    assert_eq!(
        inputs.staged,
        vec![inputs.paths["fcamera.hevc"].clone()],
        "a failed staging leaves nothing"
    );
}

#[tokio::test]
async fn an_unreachable_archive_defers_the_job_until_it_answers() {
    let e = Env::new().await;
    e.use_nas(json!({"archive_nas_margin_mb": 0})).await;
    let id = moved(&e, &data(4000, 6), &data(300, 7)).await;
    let sentinel = std::fs::read(e.nas.join(".roadviewer-storage.json")).unwrap();
    std::fs::remove_file(e.nas.join(".roadviewer-storage.json")).unwrap();
    e.a().backend().invalidate();
    // The convert route refuses at once rather than queueing (Python `convert`).
    e.set(&id, |m| m["status"] = json!("unconverted")).await;
    let (status, body) = e
        .req("POST", &format!("/api/logs/{id}/convert"), Value::Null)
        .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::CONFLICT,
            json!({"error":"storage_unavailable","message":"저장소 연결 후 다시 시도하세요."})
        )
    );
    // A queued job finds the archive gone: it is deferred, never run without its inputs.
    e.set(&id, |m| {
        m["status"] = json!("queued");
        m["manual_conversion"] = json!(true);
    })
    .await;
    e.app.jobs.tick().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while e.meta(&id).await["status"] != "unconverted" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the job was not deferred"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let m = e.meta(&id).await;
    assert_eq!(m["deferred"], "storage");
    assert_eq!(m["error"], "저장소에 연결할 수 없어 변환을 미뤘습니다.");
    // The share is back: the archiver's cycle queues it again.
    std::fs::write(e.nas.join(".roadviewer-storage.json"), sentinel).unwrap();
    e.a().backend().invalidate();
    e.cycle().await;
    let m = e.meta(&id).await;
    assert_eq!(m["status"], "queued");
    assert!(m.get("deferred").is_none());
}

#[tokio::test]
async fn converting_again_keeps_the_only_copy_of_each_camera() {
    let e = Env::new().await;
    e.use_nas(json!({"archive_nas_margin_mb": 0})).await;
    let id = e
        .record(
            'a',
            &[
                ("fcamera.hevc", data(4000, 8)),
                ("ecamera.mp4", data(2000, 9)),
                ("rlog.zst", data(300, 10)),
            ],
            OLD,
        )
        .await;
    e.cycle().await;
    // A kept MP4 next to its archived original: the original is readable, so the conversion makes the MP4 again.
    std::fs::write(e.root.join(&id).join("fcamera.mp4"), data(1000, 11)).unwrap();
    std::fs::create_dir_all(e.root.join(&id).join("prepared")).unwrap();
    e.set(&id, |m| m["status"] = json!("error")).await;
    let (status, body) = e
        .req("POST", &format!("/api/logs/{id}/convert"), Value::Null)
        .await;
    assert_eq!((status, body), (StatusCode::OK, json!({"status":"queued"})));
    assert!(
        !e.local(&id, "fcamera.mp4"),
        "made again from the archived original"
    );
    assert!(!e.root.join(&id).join("prepared").exists());
    assert_eq!(
        e.loc(&id, "ecamera.mp4").await,
        "nas",
        "the wide camera's only copy stays"
    );
    let m = e.meta(&id).await;
    assert_eq!(m["conversion_revision"], 1);
    assert_eq!(m["manual_conversion"], true);
    // A corrupt archived camera could not be made again: refused, its result kept.
    e.set(&id, |m| {
        m["status"] = json!("error");
        m["storage"]["fcamera.hevc"]["error"] = json!("hash_mismatch");
    })
    .await;
    let (status, body) = e
        .req("POST", &format!("/api/logs/{id}/convert"), Value::Null)
        .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("media_corrupt"))
    );
}
