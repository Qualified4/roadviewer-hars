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
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("reference/python/roadviewer/app/web"),
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
