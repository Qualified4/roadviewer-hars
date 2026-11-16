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
