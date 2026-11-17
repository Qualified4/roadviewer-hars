use axum::http::HeaderMap;
use roadviewer_hars::{
    http::App,
    store::{atomic_json, id, read_json, sha},
};
use serde_json::{Value, json};
use std::path::PathBuf;

async fn app(root: &std::path::Path) -> App {
    App::open(
        root.to_path_buf(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web"),
        false,
        512,
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn startup_recovers_journal_after_file_move_before_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let session = a
        .uploads
        .begin_browser(json!({"files":[{"name":"rlog.zst","size":3}]}))
        .await
        .unwrap();
    let name = session["id"].as_str().unwrap().to_owned();
    a.uploads
        .chunk(&name, 0, Some(0), b"abc", false, &HeaderMap::new())
        .await
        .unwrap();
    let record = id();
    let folder = temp.path().join(&record);
    std::fs::create_dir(&folder).unwrap();
    let meta = json!({"id":record,"name":"test","status":"unconverted","files":{"rlog.zst":"rlog.zst"},"content_hashes":{"rlog.zst":{"size":3,"sha256":sha(b"abc")}}});
    let plan = json!({"moves":[{"index":0,"id":record,"kind":"rlog.zst","sha256":sha(b"abc")}],"records":[meta],"result":{"logs":[meta],"duplicates":[],"updated":[]}});
    atomic_json(
        &temp.path().join(".uploads").join(&name).join("commit.json"),
        &plan,
    )
    .await
    .unwrap();
    std::fs::rename(
        temp.path().join(".uploads").join(&name).join("0"),
        folder.join("rlog.zst"),
    )
    .unwrap();
    drop(a);
    let a = app(temp.path()).await;
    assert_eq!(
        a.store.meta(&record).await.unwrap()["status"],
        "unconverted"
    );
    assert!(std::fs::read(folder.join("rlog.zst")).unwrap() == b"abc");
    assert!(!temp.path().join(".uploads").join(&name).exists());
    drop(a);
    let a = app(temp.path()).await;
    assert_eq!(a.store.records.read().await.len(), 1);
    let disk: Value = read_json(&folder.join("meta.json")).await.unwrap();
    assert_eq!(disk["id"], record);
}
#[tokio::test]
async fn concurrent_registration_deduplicates_without_blocking_list() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let body = json!({"files":[{"name":"rlog.zst","size":3}]});
    let first = a.uploads.begin_browser(body.clone()).await.unwrap();
    let second = a.uploads.begin_browser(body).await.unwrap();
    for s in [&first, &second] {
        a.uploads
            .chunk(
                s["id"].as_str().unwrap(),
                0,
                Some(0),
                b"abc",
                false,
                &HeaderMap::new(),
            )
            .await
            .unwrap();
    }
    let h = HeaderMap::new();
    let work = async {
        tokio::join!(
            a.uploads
                .finish(first["id"].as_str().unwrap(), false, &h, Value::Null),
            a.uploads
                .finish(second["id"].as_str().unwrap(), false, &h, Value::Null)
        )
    };
    let (x, y) = tokio::time::timeout(std::time::Duration::from_secs(5), work)
        .await
        .unwrap();
    let x = x.unwrap().1;
    let y = y.unwrap().1;
    assert_eq!(
        x["logs"].as_array().unwrap().len() + y["logs"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        x["duplicates"].as_array().unwrap().len() + y["duplicates"].as_array().unwrap().len(),
        1
    );
    assert_eq!(a.store.records.read().await.len(), 1);
}

#[tokio::test]
async fn conversion_publication_recovers_before_and_after_directory_rename() {
    for already_installed in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let record = id();
        let generation = id();
        let folder = temp.path().join(&record);
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("rlog.zst"), b"original").unwrap();
        atomic_json(&folder.join("meta.json"),&json!({"id":record,"name":"recovery","status":"processing","conversion_id":generation})).await.unwrap();
        let staging = folder.join(format!(".convert-{generation}"));
        std::fs::create_dir(&staging).unwrap();
        let summary =
            json!({"duration":1.,"model_frames":2,"video":null,"videos":{},"has_track":false});
        atomic_json(&staging.join("summary.json"), &summary)
            .await
            .unwrap();
        atomic_json(&staging.join(".generation"), &generation)
            .await
            .unwrap();
        if already_installed {
            std::fs::rename(&staging, folder.join("prepared")).unwrap();
        }
        atomic_json(
            &folder.join(".conversion.json"),
            &json!({"generation":generation}),
        )
        .await
        .unwrap();
        let a = app(temp.path()).await;
        assert_eq!(a.store.meta(&record).await.unwrap()["status"], "ready");
        assert_eq!(a.store.meta(&record).await.unwrap()["model_frames"], 2);
        assert_eq!(std::fs::read(folder.join("rlog.zst")).unwrap(), b"original");
        assert!(!folder.join(".conversion.json").exists());
        drop(a);
        let a = app(temp.path()).await;
        assert_eq!(a.store.meta(&record).await.unwrap()["status"], "ready");
    }
}

#[tokio::test]
async fn cancelled_completed_conversion_is_not_published_on_restart() {
    let temp = tempfile::tempdir().unwrap();
    let record = id();
    let generation = id();
    let folder = temp.path().join(&record);
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("rlog.zst"), b"original").unwrap();
    atomic_json(&folder.join("meta.json"),&json!({"id":record,"name":"cancelled","status":"processing","conversion_id":generation,"cancel_requested":generation})).await.unwrap();
    let staging = folder.join(format!(".convert-{generation}"));
    std::fs::create_dir(&staging).unwrap();
    atomic_json(&staging.join("summary.json"), &json!({"duration":1.}))
        .await
        .unwrap();
    atomic_json(
        &folder.join(".conversion.json"),
        &json!({"generation":generation}),
    )
    .await
    .unwrap();
    let a = app(temp.path()).await;
    let meta = a.store.meta(&record).await.unwrap();
    assert_eq!(meta["status"], "unconverted");
    assert_eq!(meta["auto_excluded"], true);
    assert!(!folder.join("prepared").exists());
    assert!(staging.join("summary.json").is_file());
    assert_eq!(std::fs::read(folder.join("rlog.zst")).unwrap(), b"original");
}

#[cfg(unix)]
#[tokio::test]
async fn failed_temporary_sweep_and_leftover_exports_do_not_block_startup() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    drop(app(temp.path()).await);
    let leftover = temp.path().join(format!("settings.{}.tmp", id()));
    std::fs::write(&leftover, b"{}").unwrap();
    // A folder whose temporary cannot be deleted: no write permission on the folder.
    let locked = temp.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let stuck = locked.join(format!("meta.{}.tmp", id()));
    std::fs::write(&stuck, b"{}").unwrap();
    let exports = temp.path().join(".exports");
    std::fs::create_dir(&exports).unwrap();
    std::fs::write(exports.join(format!("{}.hevc", id())), b"partial export").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(locked.join("probe"))
            .is_err(),
        "the test needs a folder this user cannot change"
    );
    let opened = App::open(
        temp.path().to_path_buf(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web"),
        false,
        512,
    )
    .await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let a = opened.expect("a temporary that cannot be deleted is only logged");
    assert!(stuck.exists());
    assert!(!leftover.exists(), "other temporaries are still swept");
    assert!(
        !exports.exists(),
        "original-format exports of an interrupted download are removed at startup"
    );
    drop(a);
}

#[tokio::test]
async fn camera_addition_supersedes_an_interrupted_conversion_publication() {
    for installed in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let a = app(temp.path()).await;
        let names = [
            "2026-11-15--12-00-00--1--rlog.zst",
            "2026-11-15--12-00-00--1--qcamera.ts",
        ];
        let session = a
            .uploads
            .begin_browser(json!({"files":[{"name":names[0],"size":3},{"name":names[1],"size":5}]}))
            .await
            .unwrap();
        let name = session["id"].as_str().unwrap().to_owned();
        for (i, data) in [&b"abc"[..], b"video"].iter().enumerate() {
            a.uploads
                .chunk(&name, i, Some(0), data, false, &HeaderMap::new())
                .await
                .unwrap();
        }
        let record = id();
        let generation = id();
        let folder = temp.path().join(&record);
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("rlog.zst"), b"abc").unwrap();
        let meta = json!({"id":record,"name":"n","status":"processing","conversion_id":generation,
            "files":{"rlog.zst":names[0]},"content_hashes":{"rlog.zst":{"size":3,"sha256":sha(b"abc")}}});
        atomic_json(&folder.join("meta.json"), &meta).await.unwrap();
        // A conversion publication killed midway, and the camera addition journaled after it.
        let staging = folder.join(format!(".convert-{generation}"));
        std::fs::create_dir(&staging).unwrap();
        atomic_json(&staging.join("summary.json"), &json!({"duration":1.}))
            .await
            .unwrap();
        atomic_json(&staging.join(".generation"), &generation)
            .await
            .unwrap();
        let backup = folder.join(format!(".previous-{generation}"));
        if installed {
            std::fs::create_dir(&backup).unwrap();
            std::fs::rename(&staging, folder.join("prepared")).unwrap();
        }
        atomic_json(
            &folder.join(".conversion.json"),
            &json!({"generation":generation}),
        )
        .await
        .unwrap();
        let update = json!({"id":record,"restored":false,"conversion_revision":1,
            "kinds":[{"index":1,"kind":"qcamera.ts","name":names[1],"sha256":sha(b"video")}]});
        let plan = json!({"records":[],"updates":[update],
            "moves":[{"index":1,"id":record,"kind":"qcamera.ts","sha256":sha(b"video")}],
            "result":{"logs":[],"duplicates":[],"updated":[]}});
        atomic_json(
            &temp.path().join(".uploads").join(&name).join("commit.json"),
            &plan,
        )
        .await
        .unwrap();
        drop(a);
        let a = app(temp.path()).await;
        let meta = a.store.meta(&record).await.unwrap();
        assert_eq!(meta["status"], "queued", "{installed}");
        assert_eq!(meta.get("conversion_id"), None);
        assert_eq!(meta["files"]["qcamera.ts"], names[1]);
        assert_eq!(std::fs::read(folder.join("qcamera.ts")).unwrap(), b"video");
        for gone in [
            folder.join("prepared"),
            staging.clone(),
            backup.clone(),
            folder.join(".conversion.json"),
            temp.path().join(".uploads").join(&name),
        ] {
            assert!(!gone.exists(), "{installed}: {gone:?}");
        }
        let disk: Value = read_json(&folder.join("meta.json")).await.unwrap();
        drop(a);
        let a = app(temp.path()).await;
        assert_eq!(a.store.meta(&record).await.unwrap(), disk);
    }
}
