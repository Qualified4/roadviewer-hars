//! A share that goes away between two files of one cycle while its probe is still cached as connected.
use super::super::{
    Archive, Host,
    archiver::Outcome,
    index,
    settings::{self, Mode},
};
use crate::{http::App, store::sha};
use serde_json::{Map, Value, json};
use std::{path::Path, sync::Arc};

struct Env {
    _tmp: tempfile::TempDir,
    app: App,
    archive: Arc<Archive>,
}

async fn env(share: &Path, tmp: tempfile::TempDir) -> Env {
    let app = App::open(tmp.path().join("hars"), tmp.path().join("web"), false, 512)
        .await
        .unwrap();
    let archive = Archive::open(
        Host {
            store: app.store.clone(),
            uploads: app.uploads.clone(),
            jobs: app.jobs.clone(),
        },
        Map::new(),
        true,
        None,
    );
    let mut data = settings::values(&archive.settings());
    data.extend(
        json!({"archive_enabled": true, "archive_backend": "smb", "archive_path": share.to_string_lossy(),
            "archive_nas_margin_mb": 0, "archive_batch": 64, "archive_mode": Mode::Move.as_str()})
        .as_object()
        .unwrap()
        .clone(),
    );
    archive.swap_settings(settings::parse(&data, &Map::new(), true));
    Env {
        _tmp: tmp,
        app,
        archive,
    }
}

async fn record(e: &Env, c: char, data: &[u8]) -> (String, Value) {
    let id = c.to_string().repeat(32);
    let folder = e.app.store.root.join(&id);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("rlog.zst"), data).unwrap();
    let meta = json!({"id": id, "name": "rec", "uploaded": 1.0, "status": "ready",
        "files": {"rlog.zst": "a--rlog.zst"}, "content_hashes": {}, "error": null});
    e.app.store.save_meta(meta.clone()).await.unwrap();
    (id, meta)
}

/// Archives `first` (activating and probing the share), unmounts it with `unmount`, then archives `second` within
/// the probe's cache window: the second file must stay local.
async fn second_file_after_unmount(e: &Env, unmount: impl FnOnce()) -> (String, Outcome) {
    let (first, _) = record(e, 'a', b"first file").await;
    e.archive.run_cycle().await;
    assert_eq!(
        index::location(&e.app.store.meta(&first).await.unwrap(), "rlog.zst"),
        "nas"
    );
    let ctx = e.archive.ctx();
    assert!(e.archive.nas_available(ctx.nas().unwrap()).await);
    unmount();
    let (second, meta) = record(e, 'b', b"second file").await;
    let outcome = e
        .archive
        .archive_file(&ctx, &second, &meta, "rlog.zst")
        .await;
    let after = e.app.store.meta(&second).await.unwrap();
    assert_eq!(index::location(&after, "rlog.zst"), "local");
    assert_eq!(
        sha(std::fs::read(e.app.store.root.join(&second).join("rlog.zst")).unwrap()),
        sha(b"second file"),
        "the only copy stays"
    );
    (second, outcome)
}

#[tokio::test]
async fn an_unmounted_share_root_never_receives_a_move() {
    let tmp = tempfile::tempdir().unwrap();
    let share = tmp.path().join("nas");
    std::fs::create_dir_all(&share).unwrap();
    let e = env(&share, tmp).await;
    let away = share.with_file_name("nas-away");
    let (second, outcome) = second_file_after_unmount(&e, || {
        // The mount point stays behind as an empty local directory.
        std::fs::rename(&share, &away).unwrap();
        std::fs::create_dir(&share).unwrap();
    })
    .await;
    assert!(matches!(outcome, Outcome::Failed("storage_unavailable")));
    assert!(
        !share.join(&second).exists(),
        "nothing is left on the local mount point"
    );
}

/// A copy already on the share (left by a crash before its index was written) is reused, not written: when the
/// share then looks unmounted, that copy is not this call's to remove.
#[tokio::test]
async fn a_reused_copy_is_never_removed_by_its_confirmation() {
    let tmp = tempfile::tempdir().unwrap();
    let share = tmp.path().join("nas");
    std::fs::create_dir_all(&share).unwrap();
    let e = env(&share, tmp).await;
    let (first, _) = record(&e, 'a', b"first file").await;
    e.archive.run_cycle().await;
    assert_eq!(
        index::location(&e.app.store.meta(&first).await.unwrap(), "rlog.zst"),
        "nas"
    );
    let ctx = e.archive.ctx();
    assert!(e.archive.nas_available(ctx.nas().unwrap()).await);
    let (second, meta) = record(&e, 'b', b"second file").await;
    let orphan = share.join(&second).join("rlog.zst");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"second file").unwrap();
    std::fs::remove_file(share.join(super::super::SENTINEL)).unwrap();
    let outcome = e
        .archive
        .archive_file(&ctx, &second, &meta, "rlog.zst")
        .await;
    assert!(matches!(outcome, Outcome::Failed("storage_unavailable")));
    assert_eq!(std::fs::read(&orphan).unwrap(), b"second file");
    assert_eq!(
        index::location(&e.app.store.meta(&second).await.unwrap(), "rlog.zst"),
        "local"
    );
    assert!(e.app.store.root.join(&second).join("rlog.zst").is_file());
}

#[tokio::test]
async fn a_missing_share_folder_is_never_recreated() {
    let tmp = tempfile::tempdir().unwrap();
    let share = tmp.path().join("mnt").join("share");
    std::fs::create_dir_all(&share).unwrap();
    let e = env(&share, tmp).await;
    let away = share.with_file_name("away");
    let (_, outcome) =
        second_file_after_unmount(&e, || std::fs::rename(&share, &away).unwrap()).await;
    assert!(matches!(outcome, Outcome::Failed(_)));
    assert!(!share.exists(), "the share folder is not created locally");
}

/// Within the probe's cache window the share path becomes a Python Road Viewer folder holding the same
/// `<id>/<kind>` and a `.part` of its own: the cleanup of an archived file, the removal of an unindexed copy and the
/// recovery's part sweep all read the sentinel again and touch none of it.
#[tokio::test]
async fn removals_within_the_probe_cache_never_reach_a_root_that_became_python_s() {
    let tmp = tempfile::tempdir().unwrap();
    let share = tmp.path().join("nas");
    std::fs::create_dir_all(&share).unwrap();
    let e = env(&share, tmp).await;
    let (id, _) = record(&e, 'a', b"archived file").await;
    e.archive.run_cycle().await;
    let entry = index::entry(&e.app.store.meta(&id).await.unwrap(), "rlog.zst");
    assert_eq!(entry["location"], "nas");
    let ctx = e.archive.ctx();
    let nas = ctx.nas().unwrap();
    assert!(e.archive.nas_available(nas).await);
    let sentinel = share.join(super::super::SENTINEL);
    let mut marks: Value = serde_json::from_slice(&std::fs::read(&sentinel).unwrap()).unwrap();
    marks["owner"] = json!("roadviewer");
    std::fs::write(&sentinel, marks.to_string()).unwrap();
    let python = share.join(&id).join("rlog.zst");
    std::fs::write(&python, b"python's file").unwrap();
    let python_part = share.join(&id).join("fcamera.hevc.part");
    std::fs::write(&python_part, b"python's part").unwrap();

    let cleaned = e
        .archive
        .remove_archived(nas, &id, "rlog.zst", &entry)
        .await;
    assert_ne!(cleaned, Some(true));
    e.archive.remove_nas_copy(nas, &id, "rlog.zst").await;
    e.archive.recover_nas(&ctx).await;
    assert_eq!(std::fs::read(&python).unwrap(), b"python's file");
    assert_eq!(std::fs::read(&python_part).unwrap(), b"python's part");
    let after = e.app.store.meta(&id).await.unwrap();
    assert_eq!(index::location(&after, "rlog.zst"), "nas", "{after}");
    assert!(index::entry(&after, "rlog.zst").get("removing").is_none());
}
