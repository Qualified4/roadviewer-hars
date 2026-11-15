//! The archiver (Python `media_store.Archiver`, `archive_settings.py`, the archive routes) against a temporary NAS
//! folder and the loopback Drive fake. Nothing here reaches Google or a real share.
mod common;
use axum::http::StatusCode;
use common::{archive_env::*, fake_drive::*};
use roadviewer_hars::archive::{
    ArchiveRead, MEDIA_KINDS, State as Avail, index,
    pending::{PENDING_DELETES, PendingDeletes},
    resolve::Resolved,
    settings::{self, Mode},
};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, time::Duration};

// --- Settings

#[test]
fn validation_reports_every_field_code() {
    let v = |body: Value| settings::validate(&body, false);
    let code = |body: Value, key: &str| v(body).get(key).copied();
    assert_eq!(code(json!([]), "_body"), Some("expected_object"));
    assert_eq!(code(json!({"bogus": 1}), "bogus"), Some("unknown_field"));
    assert_eq!(
        code(json!({"archive_batch": "4"}), "archive_batch"),
        Some("expected_integer")
    );
    assert_eq!(
        code(json!({"archive_batch": 4.5}), "archive_batch"),
        Some("expected_integer")
    );
    assert_eq!(
        code(json!({"archive_batch": true}), "archive_batch"),
        Some("expected_integer")
    );
    assert_eq!(
        code(json!({"archive_batch": 0}), "archive_batch"),
        Some("out_of_range")
    );
    assert_eq!(
        code(json!({"archive_batch": 65}), "archive_batch"),
        Some("out_of_range")
    );
    assert_eq!(
        code(
            json!({"archive_min_age_hours": 8761}),
            "archive_min_age_hours"
        ),
        Some("out_of_range")
    );
    assert_eq!(
        code(
            json!({"archive_interval_minutes": 0}),
            "archive_interval_minutes"
        ),
        Some("out_of_range")
    );
    assert_eq!(
        code(json!({"archive_nas_max_mb": -1}), "archive_nas_max_mb"),
        Some("out_of_range")
    );
    assert_eq!(
        code(json!({"archive_enabled": "true"}), "archive_enabled"),
        Some("expected_boolean")
    );
    assert_eq!(
        code(
            json!({"archive_allow_local_delete": 1}),
            "archive_allow_local_delete"
        ),
        Some("expected_boolean")
    );
    assert_eq!(
        code(json!({"archive_path": 5}), "archive_path"),
        Some("expected_string")
    );
    assert_eq!(
        code(json!({"archive_backend": "ftp"}), "archive_backend"),
        Some("invalid_enum")
    );
    assert_eq!(
        code(json!({"archive_mode": "sync"}), "archive_mode"),
        Some("invalid_enum")
    );
    assert_eq!(
        code(
            json!({"archive_gdrive_folder_name": "  "}),
            "archive_gdrive_folder_name"
        ),
        Some("invalid_length")
    );
    assert_eq!(
        code(
            json!({"archive_gdrive_folder_name": "x".repeat(256)}),
            "archive_gdrive_folder_name"
        ),
        Some("invalid_length")
    );
    // Only when the types are right does the parser judge the path (Python `validate` + `from_options`).
    assert_eq!(
        code(
            json!({"archive_enabled": true, "archive_path": "/tmp/x"}),
            "archive_path"
        ),
        Some("invalid_archive_path")
    );
    assert_eq!(
        code(json!({"archive_path": "/share/a/../b"}), "archive_path"),
        Some("invalid_archive_path")
    );
    assert!(
        v(json!({"archive_enabled": true, "archive_path": "/share/nas/roadviewer-hars"}))
            .is_empty()
    );
    assert!(v(json!({"archive_enabled": true, "archive_backend": "gdrive"})).is_empty());
    assert!(
        settings::validate(
            &json!({"archive_enabled": true, "archive_path": "/tmp/x"}),
            true
        )
        .is_empty()
    );
    // Every key the panel sends is known (Python `KEYS`).
    let all = settings::values(&settings::ArchiveSettings::default());
    assert_eq!(all.len(), settings::KEYS.len());
    assert!(v(Value::Object(all)).is_empty());
}

#[test]
fn defaults_match_python_except_the_hars_drive_folder() {
    let s = settings::ArchiveSettings::default();
    assert!(!s.enabled);
    assert_eq!(
        (s.min_age_seconds, s.interval_seconds, s.batch),
        (86400, 86400, 4)
    );
    assert_eq!((s.nas_max_bytes, s.nas_margin_bytes), (0, 1024 << 20));
    assert_eq!((s.mode, s.effective_mode()), (Mode::Move, Mode::Move));
    // Never Python's own folder name, so the two are told apart and never share a write root.
    assert_eq!(s.gdrive_folder_name, "Road Viewer HARS");
    let mut drive = s.clone();
    drive.backend = settings::Backend::Gdrive;
    assert_eq!(
        drive.effective_mode(),
        Mode::Copy,
        "Drive keeps local files unless deleting is allowed"
    );
    drive.allow_local_delete = true;
    assert_eq!(drive.effective_mode(), Mode::Move);
}

#[test]
fn options_are_migrated_once_and_a_saved_file_is_authoritative() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut options = options();
    options.insert("archive_enabled".into(), json!(true));
    options.insert("archive_backend".into(), json!("gdrive"));
    options.insert("archive_batch".into(), json!(8));
    let loaded = settings::load(root, &options, false);
    assert_eq!(
        (loaded.migrated_from.as_str(), loaded.error),
        ("options.json", None)
    );
    assert_eq!(loaded.settings.batch, 8);
    assert_eq!(loaded.settings.gdrive_client_id, "client-id");
    let saved: Value =
        serde_json::from_slice(&std::fs::read(root.join(settings::FILENAME)).unwrap()).unwrap();
    assert_eq!(
        (saved["version"].clone(), saved["migrated_from"].clone()),
        (json!(1), json!("options.json"))
    );
    let text = serde_json::to_string(&saved).unwrap();
    assert!(
        !text.contains(SECRET) && !text.contains("client-id"),
        "credentials stay in the add-on options"
    );
    // Stale options never override the file.
    options.insert("archive_batch".into(), json!(2));
    let again = settings::load(root, &options, false);
    assert_eq!(
        (again.settings.batch, again.migrated_from.as_str()),
        (8, "options.json")
    );

    // Nothing to migrate: the defaults apply and nothing is written.
    let empty = tempfile::tempdir().unwrap();
    let d = settings::load(empty.path(), &Map::new(), false);
    assert_eq!(d.migrated_from, "defaults");
    assert!(!empty.path().join(settings::FILENAME).exists());

    // A file that cannot be read safely disables archiving (copy mode) and is never written over.
    for bad in [
        &b"{not json"[..],
        br#"{"version": 2}"#,
        br#"{"archive_batch": "x"}"#,
        br#"{"migrated_from": "x"}"#,
    ] {
        std::fs::write(root.join(settings::FILENAME), bad).unwrap();
        let e = settings::load(root, &options, false);
        assert_eq!(
            (e.error, e.migrated_from.as_str()),
            (Some("settings_error"), "error")
        );
        assert!(!e.settings.enabled);
        assert_eq!(e.settings.mode, Mode::Copy);
        assert_eq!(std::fs::read(root.join(settings::FILENAME)).unwrap(), bad);
    }
}

#[tokio::test]
async fn settings_route_validates_atomically_and_hides_secrets() {
    let e = Env::new().await;
    let (status, body) = e.req("GET", "/api/settings/archive", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["client_configured"], true);
    assert_eq!(body["archive_gdrive_folder_name"], "Road Viewer HARS");
    let (status, body) = e
        .req(
            "POST",
            "/api/settings/archive",
            json!({"archive_batch": 0, "archive_mode": "x", "archive_enabled": true}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_archive_settings");
    assert_eq!(
        body["fields"],
        json!({"archive_batch": "out_of_range", "archive_mode": "invalid_enum"})
    );
    assert!(
        !e.root.join(settings::FILENAME).exists(),
        "nothing is saved from a refused request"
    );
    let (status, body) = e.req("POST", "/api/settings/archive", json!([1])).await;
    assert_eq!(
        (status, body["fields"].clone()),
        (StatusCode::BAD_REQUEST, json!({"_body": "expected_object"}))
    );
    // A partial save keeps the other values.
    e.configure(json!({"archive_batch": 7})).await;
    let saved = e.configure(json!({"archive_min_age_hours": 3})).await;
    assert_eq!(
        (
            saved["archive_batch"].clone(),
            saved["archive_min_age_hours"].clone()
        ),
        (json!(7), json!(3))
    );
    let file = std::fs::read_to_string(e.root.join(settings::FILENAME)).unwrap();
    assert!(!file.contains(SECRET));
    let (_, storage) = e.req("GET", "/api/settings/storage", Value::Null).await;
    let text = storage.to_string();
    assert!(
        !text.contains(SECRET) && !text.contains("secretrefresh") && !text.contains("secretaccess")
    );
    // A cycle in progress keeps its configuration: saving waits for it (409), changing nothing.
    let guard = e.a().cycle.lock().await;
    let (status, body) = e
        .req("POST", "/api/settings/archive", json!({"archive_batch": 9}))
        .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("archive_running"))
    );
    drop(guard);
    assert_eq!(e.a().settings().batch, 7);
}

// --- NAS

async fn nas_env() -> Env {
    let e = Env::new().await;
    e.use_nas(json!({"archive_nas_margin_mb": 0})).await;
    e
}

#[tokio::test]
async fn nas_move_archives_oldest_first_videos_before_the_log() {
    let e = nas_env().await;
    let (v1, l1, v2, l2) = (data(5000, 1), data(3000, 2), data(4000, 3), data(2000, 4));
    let a = e
        .record(
            'a',
            &[("fcamera.hevc", v1.clone()), ("rlog.zst", l1.clone())],
            1.0,
        )
        .await;
    let b = e
        .record(
            'b',
            &[("fcamera.hevc", v2.clone()), ("rlog.zst", l2.clone())],
            2.0,
        )
        .await;
    e.configure(json!({"archive_batch": 1})).await;
    let r = e.cycle().await;
    assert_eq!(r.archived, 1);
    assert_eq!(
        e.loc(&a, "fcamera.hevc").await,
        "nas",
        "the oldest recording's video goes first"
    );
    assert_eq!(e.loc(&a, "rlog.zst").await, "local");
    e.configure(json!({"archive_batch": 64})).await;
    let r = e.cycle().await;
    assert_eq!(r.archived, 3);
    for (id, kind, content) in [
        (&a, "fcamera.hevc", &v1),
        (&a, "rlog.zst", &l1),
        (&b, "fcamera.hevc", &v2),
        (&b, "rlog.zst", &l2),
    ] {
        let entry = index::entry(&e.meta(id).await, kind);
        assert_eq!(entry["location"], "nas");
        assert_eq!(entry["bytes"], content.len());
        assert_eq!(entry["sha256"], sha_hex(content));
        assert!(entry["archived_at"].is_f64());
        assert!(
            !e.local(id, kind),
            "moved: the local copy goes once the index is committed"
        );
        assert_eq!(e.on_nas(id, kind).as_deref(), Some(content.as_slice()));
    }
    let sentinel: Value =
        serde_json::from_slice(&std::fs::read(e.nas.join(".roadviewer-storage.json")).unwrap())
            .unwrap();
    assert_eq!(sentinel["owner"], "roadviewer-hars");
    let (_, storage) = e.req("GET", "/api/settings/storage", Value::Null).await;
    assert_eq!(storage["archive"]["state"], "connected");
    assert_eq!(storage["archive"]["archived_files"], 4);
    assert_eq!(storage["archive"]["archived_bytes"], 14000);
    // Archived files resolve to the share.
    let (found, state) = e.a().resolve(&a, &e.meta(&a).await, "rlog.zst").await;
    assert_eq!(
        (found, state),
        (
            Resolved::Local(e.nas.join(&a).join("rlog.zst")),
            "available"
        )
    );
}

#[tokio::test]
async fn only_ready_idle_recordings_past_the_minimum_age_and_threshold_are_archived() {
    let e = nas_env().await;
    let young = e
        .record(
            'a',
            &[("rlog.zst", data(100, 1))],
            roadviewer_hars::store::now(),
        )
        .await;
    let queued = e.record('b', &[("rlog.zst", data(100, 2))], OLD).await;
    e.set(&queued, |m| m["status"] = json!("queued")).await;
    let unconverted = e.record('c', &[("rlog.zst", data(100, 3))], OLD).await;
    e.set(&unconverted, |m| m["status"] = json!("unconverted"))
        .await;
    let r = e.cycle().await;
    assert_eq!(r.archived, 0);
    for id in [&young, &queued, &unconverted] {
        assert_eq!(e.loc(id, "rlog.zst").await, "local");
    }
    // The minimum age is a boundary, not a suggestion.
    e.configure(json!({"archive_min_age_hours": 0})).await;
    assert_eq!(e.cycle().await.archived, 1);
    assert_eq!(e.loc(&young, "rlog.zst").await, "nas");
    // Below the local threshold nothing moves.
    e.record('d', &[("rlog.zst", data(100, 4))], OLD).await;
    e.configure(json!({"archive_local_threshold_mb": 1024}))
        .await;
    assert_eq!(e.cycle().await.stopped.as_deref(), Some("below_threshold"));
}

#[tokio::test]
async fn pinned_recordings_are_archived_but_never_cleaned_from_the_share() {
    let e = nas_env().await;
    let id = e
        .record(
            'a',
            &[
                ("fcamera.hevc", data(600_000, 1)),
                ("rlog.zst", data(100, 2)),
            ],
            OLD,
        )
        .await;
    let (status, _) = e
        .req(
            "POST",
            &format!("/api/logs/{id}/pin"),
            json!({"pinned": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        e.cycle().await.archived,
        2,
        "a pin protects from deletion, not from moving"
    );
    let other = e
        .record(
            'b',
            &[
                ("fcamera.hevc", data(600_000, 3)),
                ("rlog.zst", data(100, 4)),
            ],
            2.0,
        )
        .await;
    assert_eq!(e.cycle().await.archived, 2);
    assert!(walk(&e.nas) > 1 << 20);
    // Over its limit the share drops unpinned archived videos first; one is enough here, so every log stays.
    e.configure(json!({"archive_nas_max_mb": 1})).await;
    let r = e.cycle().await;
    assert_eq!(r.stopped, None, "{r:?}");
    let removed = index::entry(&e.meta(&other).await, "fcamera.hevc");
    assert_eq!(
        (removed["location"].clone(), removed["reason"].clone()),
        (json!("removed"), json!("nas_full"))
    );
    assert!(
        removed["sha256"].is_string() && removed["bytes"] == 600_000,
        "what identified the copy is kept"
    );
    assert!(e.on_nas(&other, "fcamera.hevc").is_none());
    assert_eq!(
        e.loc(&other, "rlog.zst").await,
        "nas",
        "logs go only after every video"
    );
    assert_eq!(e.loc(&id, "fcamera.hevc").await, "nas");
    assert!(e.on_nas(&id, "fcamera.hevc").is_some(), "pinned");
    assert!(
        !e.local(&other, "fcamera.hevc"),
        "local files are never touched by share cleanup"
    );
    // A limit no removal can meet: the remaining unpinned files go, the pinned ones stay, and the cycle says so.
    e.configure(json!({"archive_nas_max_mb": 0, "archive_nas_margin_mb": 1u64 << 30}))
        .await;
    let r = e.cycle().await;
    assert_eq!(r.stopped.as_deref(), Some("nas_full_no_candidates"));
    assert_eq!(e.loc(&other, "rlog.zst").await, "removed");
    assert_eq!(e.loc(&id, "rlog.zst").await, "nas");
    assert_eq!(e.loc(&id, "fcamera.hevc").await, "nas");
}

fn walk(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                walk(&e.path())
            } else {
                e.metadata().unwrap().len()
            }
        })
        .sum()
}

#[tokio::test]
async fn an_unreachable_share_keeps_local_files_and_counts_failures() {
    let e = nas_env().await;
    let id = e.record('a', &[("rlog.zst", data(100, 1))], OLD).await;
    let before = e.meta(&id).await;
    // Unmounted: the folder is there but empty (no sentinel), so the share is never claimed again.
    std::fs::remove_dir_all(&e.nas).unwrap();
    std::fs::create_dir(&e.nas).unwrap();
    std::fs::write(e.root.join(".archive.json"), r#"{"storage_id":"aaaa"}"#).unwrap();
    for n in 1..=2 {
        let r = e.cycle().await;
        assert_eq!(r.stopped.as_deref(), Some("storage_unavailable"));
        let status = e.a().status();
        assert_eq!(status["consecutive_failures"], n);
        assert_eq!(status["last_error"], "storage_unavailable");
    }
    assert_eq!(e.meta(&id).await, before);
    assert!(e.local(&id, "rlog.zst"));
    assert!(
        !e.nas.join(".roadviewer-storage.json").exists(),
        "an unmounted share is never claimed"
    );
}

#[tokio::test]
async fn a_python_share_is_never_claimed_or_written() {
    let e = Env::new().await;
    let py = e.tmp.path().join("python-share");
    std::fs::create_dir_all(py.join("cccccccccccccccccccccccccccccccc")).unwrap();
    let sentinel = br#"{"storage_id": "py", "created_at": 1, "owner": "roadviewer"}"#;
    std::fs::write(py.join(".roadviewer-storage.json"), sentinel).unwrap();
    std::fs::write(
        py.join("cccccccccccccccccccccccccccccccc/rlog.zst"),
        b"python data",
    )
    .unwrap();
    let id = e.record('a', &[("rlog.zst", data(100, 1))], OLD).await;
    e.configure(json!({"archive_enabled": true, "archive_path": py.to_string_lossy(), "archive_nas_margin_mb": 0})).await;
    let r = e.cycle().await;
    assert_eq!(r.stopped.as_deref(), Some("storage_unavailable"));
    // The HARS data root's .archive.json carries no share id (never Python's).
    assert!(!e.root.join(".archive.json").exists());
    let (status, _) = e
        .req("DELETE", &format!("/api/logs/{id}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        std::fs::read(py.join(".roadviewer-storage.json")).unwrap(),
        sentinel
    );
    assert_eq!(
        std::fs::read(py.join("cccccccccccccccccccccccccccccccc/rlog.zst")).unwrap(),
        b"python data"
    );
    assert_eq!(std::fs::read_dir(&py).unwrap().count(), 2);
}

#[tokio::test]
async fn deleting_a_recording_removes_its_files_on_the_share() {
    let e = nas_env().await;
    let id = e
        .record(
            'a',
            &[("fcamera.hevc", data(4000, 1)), ("rlog.zst", data(100, 2))],
            OLD,
        )
        .await;
    e.cycle().await;
    assert!(e.on_nas(&id, "rlog.zst").is_some());
    let (status, _) = e
        .req("DELETE", &format!("/api/logs/{id}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !e.nas.join(&id).exists(),
        "the share's files and folder go with the recording"
    );
    assert!(!e.root.join(PENDING_DELETES).exists());
}

#[tokio::test]
async fn share_deletions_wait_in_the_journal_while_the_share_is_away() {
    let e = nas_env().await;
    let id = e.record('a', &[("rlog.zst", data(100, 2))], OLD).await;
    e.cycle().await;
    let sentinel = std::fs::read(e.nas.join(".roadviewer-storage.json")).unwrap();
    std::fs::remove_file(e.nas.join(".roadviewer-storage.json")).unwrap();
    e.a().backend().invalidate();
    let (status, _) = e
        .req("DELETE", &format!("/api/logs/{id}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    let rows = PendingDeletes::new(&e.root).rows().await.unwrap();
    assert_eq!(rows, vec![json!({"id": id, "kinds": ["rlog.zst"]})]);
    assert!(
        e.on_nas(&id, "rlog.zst").is_some(),
        "kept while the share does not answer"
    );
    std::fs::write(e.nas.join(".roadviewer-storage.json"), sentinel).unwrap();
    e.a().backend().invalidate();
    e.cycle().await;
    assert!(!e.nas.join(&id).exists());
    assert!(!e.root.join(PENDING_DELETES).exists());
}

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
    let cases: [(&str, fn(&Shared)); 4] = [
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
        |_| false,
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

// --- resolve and the index

#[tokio::test]
async fn resolve_follows_the_index_and_the_archive_state() {
    let e = nas_env().await;
    let id = e
        .record(
            'a',
            &[("rlog.zst", data(100, 1)), ("fcamera.hevc", data(200, 2))],
            OLD,
        )
        .await;
    let m = e.meta(&id).await;
    assert_eq!(
        e.a().resolve(&id, &m, "rlog.zst").await,
        (
            Resolved::Local(e.root.join(&id).join("rlog.zst")),
            "available"
        )
    );
    assert_eq!(e.a().resolve(&id, &m, "camera.mp4").await.1, "missing");
    e.cycle().await;
    let m = e.meta(&id).await;
    assert_eq!(e.a().resolve(&id, &m, "rlog.zst").await.1, "available");
    assert_eq!(e.a().media_size(&id, &m, "fcamera.hevc"), 200);
    assert_eq!(
        e.a().indexed_state(&id, &m, "rlog.zst", Avail::Unavailable),
        "storage_unavailable"
    );
    assert_eq!(
        e.a().indexed_state(&id, &m, "rlog.zst", Avail::Checking),
        "checking"
    );
    // A file gone from a reachable share is missing; an unreachable share says so instead.
    std::fs::remove_file(e.nas.join(&id).join("rlog.zst")).unwrap();
    assert_eq!(e.a().resolve(&id, &m, "rlog.zst").await.1, "missing");
    std::fs::remove_file(e.nas.join(".roadviewer-storage.json")).unwrap();
    e.a().backend().invalidate();
    assert_eq!(
        e.a().resolve(&id, &m, "fcamera.hevc").await.1,
        "storage_unavailable"
    );
    let mut removed = m.clone();
    index::set_location(
        &mut removed,
        "rlog.zst",
        json!({"location": "removed", "reason": "nas_full"}),
    );
    assert_eq!(
        e.a().resolve(&id, &removed, "rlog.zst").await,
        (Resolved::Nothing, "missing")
    );
}

#[test]
fn python_indexes_read_the_same() {
    // A recording imported from Python: every location, a remote copy with an upload in progress, a mismatch.
    let mut m = json!({"id": "a".repeat(32), "storage": {
        "rlog.zst": {"location": "nas", "bytes": 10, "sha256": "s1", "archived_at": 1.0},
        "fcamera.mp4": {"location": "gdrive", "file_id": "F1", "bytes": 20, "sha256": "s2", "md5": "m2", "archived_at": 2.0},
        "qcamera.ts": {"location": "local", "remote": {"backend": "gdrive", "file_id": "F2", "bytes": 30, "sha256": "s3", "md5": null, "uploaded_at": 3.0, "verified_by": "sha256"},
                        "remote_pending": {"backend": "gdrive", "session": "https://up/secret", "started_at": 1.0, "bytes_sent": 5, "sha256": "s4", "size": 40}},
        "ecamera.mp4": {"location": "removed", "removed_at": 4.0, "reason": "nas_full", "sha256": "s5", "bytes": 50},
        "camera.mp4": {"location": "local", "error": "remote_mismatch"},
        "fcamera.hevc": "legacy-garbage"
    }});
    assert_eq!(index::location(&m, "rlog.zst"), "nas");
    assert_eq!(index::location(&m, "fcamera.hevc"), "local");
    assert_eq!(index::archived_bytes(&m), 30);
    assert_eq!(index::archived_files(&m), 2);
    assert_eq!(index::remote_copy_totals(&m), (1, 30));
    assert_eq!(index::mismatch_count(&m), 1);
    let refs: Vec<(String, String)> = index::remote_refs(&m, true)
        .into_iter()
        .map(|r| (r.kind, r.file_id))
        .collect();
    assert_eq!(
        refs,
        vec![
            ("qcamera.ts".into(), "F2".into()),
            ("fcamera.mp4".into(), "F1".into())
        ]
    );
    assert_eq!(index::remote_refs(&m, false).len(), 1);
    let public = index::public_storage(&m["storage"]);
    let text = public.to_string();
    assert!(!text.contains("secret") && !text.contains("F1") && !text.contains("F2"));
    assert_eq!(public["qcamera.ts"]["remote"]["bytes"], 30);
    // drop_remote keeps NAS entries exactly as they are and removes an emptied entry.
    assert!(index::drop_remote(&mut m, "rlog.zst", None).is_none());
    assert_eq!(
        index::drop_remote(&mut m, "qcamera.ts", Some("s3")),
        None,
        "the same content keeps its copy"
    );
    assert!(index::drop_remote(&mut m, "qcamera.ts", Some("other")).is_some());
    assert!(m["storage"].get("qcamera.ts").is_none());
    assert!(index::drop_remote(&mut m, "camera.mp4", None).is_none());
    assert!(
        m["storage"].get("camera.mp4").is_none(),
        "a mismatch goes with the copy it described"
    );
    let mut p = json!({"storage": {"qcamera.ts": {"location": "local", "remote": {"backend": "gdrive", "file_id": "F", "bytes": 3, "sha256": "x", "uploaded_at": 9.0}}}});
    assert!(index::promote_remote(
        &mut p,
        "qcamera.ts",
        Some((3, "x")),
        1.0
    ));
    assert_eq!(
        p["storage"]["qcamera.ts"],
        json!({"location": "gdrive", "file_id": "F", "bytes": 3, "sha256": "x", "md5": null, "archived_at": 9.0})
    );
    for kind in MEDIA_KINDS {
        let _ = index::entry(&m, kind);
    }
}
