use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use roadviewer_hars::{
    http::{App, ingress},
    storage::{
        self, Guards, Limits, MARGIN, Protection, RecordFacts, Space, StorageError, Verdict,
    },
    store::{atomic_json, read_json},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};
use tower::ServiceExt;

fn web() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("reference/python/roadviewer/app/web")
}
async fn app(root: &Path) -> App {
    App::open(root.to_path_buf(), web(), false, 512)
        .await
        .unwrap()
}
async fn req(app: &App, method: &str, path: &str, body: Vec<u8>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-roadviewer-request", "1")
        .body(Body::from(body))
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(20),
        ingress(app.clone()).oneshot(request),
    )
    .await
    .expect("HTTP handler deadlock")
    .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn post(app: &App, path: &str, body: Value) -> (StatusCode, Value) {
    req(app, "POST", path, serde_json::to_vec(&body).unwrap()).await
}
fn rid(c: char) -> String {
    c.to_string().repeat(32)
}
/// A recording like Python's `make_log`: one flat-named segment of `route`.
async fn make_log(
    a: &App,
    c: char,
    route: &str,
    segment: u32,
    uploaded: f64,
    size: usize,
) -> PathBuf {
    let id = rid(c);
    let folder = a.store.root.join(&id);
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("rlog.zst"), vec![b'x'; size]).unwrap();
    a.store
        .save_meta(json!({"id":id,"name":format!("{route} / 구간 {segment}"),"uploaded":uploaded,"status":"unconverted","files":{"rlog.zst":format!("{route}--{segment}--rlog.zst")}}))
        .await
        .unwrap();
    folder
}
async fn set_status(a: &App, c: char, status: &str) {
    let mut meta = a.store.meta(&rid(c)).await.unwrap();
    meta["status"] = json!(status);
    a.store.save_meta(meta).await.unwrap();
}
async fn configure(a: &App, max_bytes: u64, policy: &str) {
    let (status, _) = post(
        a,
        "/api/settings/storage",
        json!({"max_bytes":max_bytes,"policy":policy}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
async fn pin(a: &App, c: char, pinned: bool) {
    let (status, body) = post(
        a,
        &format!("/api/logs/{}/pin", rid(c)),
        json!({"pinned":pinned}),
    )
    .await;
    assert_eq!((status, body), (StatusCode::OK, json!({"pinned":pinned})));
}
async fn used(a: &App) -> u64 {
    a.store.used().await.unwrap()
}
async fn listed(a: &App) -> BTreeSet<String> {
    let (status, body) = req(a, "GET", "/api/logs", vec![]).await;
    assert_eq!(status, StatusCode::OK);
    body["logs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}
fn hidden_dirs(root: &Path) -> Vec<String> {
    std::fs::read_dir(root)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.starts_with(storage::HIDDEN_PREFIX).then_some(name)
        })
        .collect()
}
/// Commit-time admission exactly as an upload finish runs it.
async fn commit(
    a: &App,
    total: u64,
    incoming: &[&str],
) -> Result<u64, roadviewer_hars::error::ApiError> {
    let registration = a.store.registration.lock().await;
    let names = incoming.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    storage::admit(
        &a.store,
        total,
        &names,
        true,
        &BTreeSet::new(),
        Some(&registration),
    )
    .await
}
async fn begin(a: &App, name: &str, size: u64) -> (StatusCode, Value) {
    post(
        a,
        "/api/uploads",
        json!({"files":[{"name":name,"size":size}]}),
    )
    .await
}
async fn upload(a: &App, name: &str, data: &[u8]) -> (StatusCode, Value) {
    let (status, session) = begin(a, name, data.len() as u64).await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let id = session["id"].as_str().unwrap();
    let (status, _) = req(
        a,
        "PUT",
        &format!("/api/uploads/{id}/files/0?offset=0"),
        data.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    req(a, "POST", &format!("/api/uploads/{id}/finish"), vec![]).await
}

// ------------------------------------------------------------ admission arithmetic

fn limits(max_bytes: u64, delete_oldest: bool) -> Limits {
    Limits {
        max_bytes,
        delete_oldest,
    }
}
#[test]
fn needed_doubles_staging_and_reserves_metadata() {
    assert_eq!(storage::needed(10000, true), 20000 + 65536);
    assert_eq!(storage::needed(10000, false), 10000 + 65536);
    assert_eq!(storage::needed(0, true), 65536);
    assert_eq!(storage::needed(u64::MAX, true), u64::MAX);
}
#[test]
fn admission_boundaries_match_python() {
    let free = 10 * MARGIN;
    let at = |used, reserved| Space {
        used,
        reserved,
        free,
    };
    // Limit 0 means no limit.
    assert_eq!(
        storage::check(limits(0, false), at(u64::MAX / 4, 0), 1),
        Ok(Verdict::Fits)
    );
    // Exactly at the limit still fits; one byte more is refused or evicts.
    assert_eq!(
        storage::check(limits(1000, false), at(600, 300), 100),
        Ok(Verdict::Fits)
    );
    assert_eq!(
        storage::check(limits(1000, false), at(601, 300), 100),
        Err(StorageError::LimitExceeded)
    );
    assert_eq!(
        storage::check(limits(1000, true), at(601, 300), 100),
        Ok(Verdict::NeedsEviction)
    );
    // A request larger than the limit itself never evicts.
    assert_eq!(
        storage::check(limits(1000, true), at(0, 0), 1001),
        Err(StorageError::LimitExceeded)
    );
    // Physical space: exactly reserved + needed + MARGIN fits; one byte less is refused even
    // when old recordings could be deleted.
    let tight = Space {
        used: 0,
        reserved: 7,
        free: 7 + 100 + MARGIN,
    };
    assert_eq!(
        storage::check(limits(0, true), tight, 100),
        Ok(Verdict::Fits)
    );
    let short = Space {
        free: tight.free - 1,
        ..tight
    };
    assert_eq!(
        storage::check(limits(0, true), short, 100),
        Err(StorageError::InsufficientDisk)
    );
    assert_eq!(
        storage::check(limits(5000, true), short, 100),
        Err(StorageError::InsufficientDisk)
    );
    // Preflight: all candidates together must bring usage back under the limit.
    assert_eq!(
        storage::preflight(limits(1000, true), at(1500, 0), 100, 600),
        Ok(())
    );
    assert_eq!(
        storage::preflight(limits(1000, true), at(1500, 0), 100, 599),
        Err(StorageError::NoDeletable)
    );
    assert!(storage::enough(limits(1000, true), at(900, 0), 100));
    assert!(!storage::enough(limits(1000, true), at(901, 0), 100));
}

// ------------------------------------------------------------ pure eviction plan

fn fact(
    id: &str,
    route: &str,
    uploaded: f64,
    status: &str,
    pinned: bool,
    bytes: u64,
) -> RecordFacts {
    RecordFacts {
        id: id.into(),
        route: route.into(),
        uploaded,
        status: status.into(),
        pinned,
        bytes,
        remote_refs: false,
        archived: false,
    }
}
#[test]
fn plan_keeps_pins_per_segment_and_orders_routes_by_their_oldest_segment() {
    let records = [
        fact("a", "r1", 1., "ready", true, 10),
        fact("b", "r1", 5., "ready", false, 20),
        fact("c", "r2", 2., "ready", false, 30),
        fact("d", "r3", 0.5, "ready", true, 40),
        fact("e", "r3", 0.7, "ready", true, 50),
        fact("f", "r4", 3., "unconverted", false, 60),
    ];
    let plan = storage::plan_eviction(&records, &Guards::default());
    let order = plan
        .routes
        .iter()
        .map(|r| {
            (
                r.route.as_str(),
                r.records
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    // r1 sorts by its pinned segment's upload time (1) but only b is deleted; r3 is all pinned.
    assert_eq!(
        order,
        vec![("r1", vec!["b"]), ("r2", vec!["c"]), ("r4", vec!["f"])]
    );
    assert_eq!(plan.reclaim, 20 + 30 + 60);
    for id in ["a", "d", "e"] {
        assert_eq!(plan.kept[id], Protection::Pinned);
    }
}
#[test]
fn plan_protects_whole_routes_that_are_received_registered_or_converting() {
    let records = [
        fact("a", "recv", 1., "ready", false, 1),
        fact("b", "conv", 1., "ready", false, 1),
        fact("c", "conv", 2., "queued", false, 1),
        fact("d", "busy", 1., "ready", false, 1),
        fact("e", "busy", 2., "ready", false, 1),
        fact("f", "free", 9., "ready", false, 1),
        fact("g", "proc", 1., "processing", true, 1),
    ];
    let guards = Guards {
        receiving_routes: ["recv".to_owned()].into(),
        busy_ids: ["e".to_owned()].into(),
        archive_reachable: false,
    };
    let plan = storage::plan_eviction(&records, &guards);
    assert_eq!(plan.ids(), vec!["f"]);
    assert_eq!(plan.kept["a"], Protection::RouteReceiving);
    assert_eq!(plan.kept["b"], Protection::Converting);
    assert_eq!(plan.kept["c"], Protection::Converting);
    assert_eq!(plan.kept["d"], Protection::Uploading);
    assert_eq!(plan.kept["g"], Protection::Converting);
}
#[test]
fn remote_and_archived_files_block_eviction() {
    let mut remote = fact("a", "r", 1., "ready", false, 1);
    remote.remote_refs = true;
    let mut archived = fact("b", "s", 1., "ready", false, 1);
    archived.archived = true;
    let group_a = [remote.clone()];
    let group_b = [archived.clone()];
    let mut guards = Guards::default();
    assert_eq!(
        storage::eviction_protection(&remote, &group_a, &guards),
        Some(Protection::RemoteCopy)
    );
    assert_eq!(
        storage::eviction_protection(&archived, &group_b, &guards),
        Some(Protection::ArchiveUnreachable)
    );
    guards.archive_reachable = true;
    assert_eq!(
        storage::eviction_protection(&archived, &group_b, &guards),
        None
    );
    // Python media_store.remote_refs / archived on real index shapes.
    let drive = json!({"storage":{"rlog.zst":{"location":"gdrive","file_id":"abc"}}});
    let copy = json!({"storage":{"fcamera.hevc":{"location":"local","remote":{"backend":"gdrive","file_id":"x"}}}});
    let nas = json!({"storage":{"qcamera.ts":{"location":"nas"}}});
    let local = json!({"storage":{"rlog.zst":{"location":"local"}}});
    assert!(storage::has_remote_refs(&drive) && storage::archived(&drive));
    assert!(storage::has_remote_refs(&copy) && !storage::archived(&copy));
    assert!(!storage::has_remote_refs(&nas) && storage::archived(&nas));
    assert!(!storage::has_remote_refs(&local) && !storage::archived(&local));
}
#[test]
fn route_key_follows_flat_names_then_labels() {
    assert_eq!(
        storage::route_key(
            &json!({"id":"x","files":{"rlog.zst":"00000001--aaaaaaaaaa--3--rlog.zst"}})
        ),
        "00000001--aaaaaaaaaa"
    );
    assert_eq!(
        storage::route_key(
            &json!({"id":"x","name":"2024-01-02--03-04-05 / 구간 7","files":{"rlog.zst":"rlog.zst"}})
        ),
        "2024-01-02--03-04-05"
    );
    assert_eq!(
        storage::route_key(
            &json!({"id":"x","name":"업로드한 로그","files":{"rlog.zst":"rlog.zst"}})
        ),
        "log:x"
    );
}

// ------------------------------------------------------------ admission through the app

#[tokio::test]
async fn reject_new_returns_the_three_python_507_codes() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 200000).await;
    configure(&a, used(&a).await + 20000, "reject_new").await;
    let (status, body) = begin(&a, "rlog.zst", 10000).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(body["error"], "storage_limit_exceeded");
    assert_eq!(body["message"], "설정한 저장공간 제한을 초과합니다.");
    // Everything deletable is pinned.
    pin(&a, 'a', true).await;
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    let (status, body) = begin(&a, "rlog.zst", 10000).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(body["error"], "no_deletable_logs");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("확보할 공간이 없습니다")
    );
    assert!(temp.path().join(rid('a')).join("rlog.zst").is_file());
    // Physical space is measured, never configurable: the arithmetic proves the third code.
    let free = fs2::available_space(temp.path()).unwrap();
    assert_eq!(
        storage::check(
            limits(0, true),
            Space {
                used: 0,
                reserved: free,
                free
            },
            1
        ),
        Err(StorageError::InsufficientDisk)
    );
    assert_eq!(
        roadviewer_hars::error::ApiError::from(StorageError::InsufficientDisk).1,
        "insufficient_disk_space"
    );
}
#[tokio::test]
async fn segment_pins_and_partial_route_eviction() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let pa = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 50000).await;
    let pb = make_log(&a, 'b', "00000001--aaaaaaaaaa", 1, 2., 100000).await;
    let pc = make_log(&a, 'c', "00000002--bbbbbbbbbb", 0, 3., 50000).await;
    pin(&a, 'a', true).await;
    let saved: Value = read_json(&temp.path().join(storage::SETTINGS))
        .await
        .unwrap();
    assert_eq!(saved["pinned_logs"], json!([rid('a')]));
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    let plan = storage::eviction_plan(&a.store, &[], &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(plan.ids(), vec![rid('b'), rid('c')]);
    assert_eq!(plan.kept[&rid('a')], Protection::Pinned);
    commit(&a, 10000, &[]).await.unwrap();
    assert!(pa.exists() && !pb.exists() && pc.exists());
    assert_eq!(listed(&a).await, [rid('a'), rid('c')].into());
    pin(&a, 'a', false).await;
    let (_, body) = req(&a, "GET", "/api/logs", vec![]).await;
    assert!(
        body["logs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["pinned"] == false)
    );
}
#[tokio::test]
async fn pinned_capacity_is_excluded_from_preflight() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let pa = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 100000).await;
    let pb = make_log(&a, 'b', "00000001--aaaaaaaaaa", 1, 2., 10000).await;
    let pc = make_log(&a, 'c', "00000002--bbbbbbbbbb", 0, 3., 10000).await;
    pin(&a, 'a', true).await;
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    let error = commit(&a, 10000, &[]).await.unwrap_err();
    assert_eq!(error.1, "no_deletable_logs");
    assert!(pa.exists() && pb.exists() && pc.exists());
    assert!(!temp.path().join(storage::JOURNAL).exists());
}
#[tokio::test]
async fn fully_pinned_routes_survive_every_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    // Route 1 fully pinned, route 2 partly pinned, route 3 unpinned; all large.
    for (c, route, segment, uploaded) in [
        ('a', "00000001--aaaaaaaaaa", 0, 1.),
        ('b', "00000001--aaaaaaaaaa", 1, 2.),
        ('c', "00000002--bbbbbbbbbb", 0, 3.),
        ('d', "00000002--bbbbbbbbbb", 1, 4.),
        ('e', "00000003--cccccccccc", 0, 5.),
    ] {
        make_log(&a, c, route, segment, uploaded, 40000).await;
    }
    for c in ['a', 'b', 'c'] {
        pin(&a, c, true).await;
    }
    // A limit no deletion can satisfy: preflight refuses and nothing is touched.
    configure(&a, 1, "delete_oldest").await;
    assert_eq!(
        commit(&a, 0, &[]).await.unwrap_err().1,
        "storage_limit_exceeded"
    );
    configure(&a, used(&a).await - 50000, "delete_oldest").await;
    assert_eq!(commit(&a, 0, &[]).await.unwrap_err().1, "no_deletable_logs");
    assert_eq!(listed(&a).await.len(), 5);
    // Enough room once every unpinned segment is gone: exactly those are deleted.
    configure(&a, used(&a).await - 10000, "delete_oldest").await;
    commit(&a, 0, &[]).await.unwrap();
    assert_eq!(listed(&a).await, [rid('a'), rid('b'), rid('c')].into());
    assert!(hidden_dirs(temp.path()).is_empty());
}
#[tokio::test]
async fn incoming_and_converting_routes_are_protected() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let pa = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 100000).await;
    let pb = make_log(&a, 'b', "00000002--bbbbbbbbbb", 0, 2., 100000).await;
    let pc = make_log(&a, 'c', "00000003--cccccccccc", 0, 3., 100000).await;
    let pd = make_log(&a, 'd', "00000004--dddddddddd", 0, 4., 100000).await;
    set_status(&a, 'b', "processing").await;
    set_status(&a, 'c', "queued").await;
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    commit(&a, 10000, &["00000001--aaaaaaaaaa--1--rlog.zst"])
        .await
        .unwrap();
    assert!(pa.exists() && pb.exists() && pc.exists() && !pd.exists());
}
#[tokio::test]
async fn open_upload_session_protects_its_route() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let pa = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 200000).await;
    let pb = make_log(&a, 'b', "00000002--bbbbbbbbbb", 0, 2., 200000).await;
    // Another client is still sending the next segment of route 1.
    let (status, _) = begin(&a, "00000001--aaaaaaaaaa--1--rlog.zst", 10).await;
    assert_eq!(status, StatusCode::CREATED);
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    let plan = storage::eviction_plan(&a.store, &[], &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(plan.kept[&rid('a')], Protection::RouteReceiving);
    let (status, body) = upload(&a, "00000005--eeeeeeeeee--0--rlog.zst", b"new log").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(pa.exists() && !pb.exists());
}
#[tokio::test]
async fn verified_upload_evicts_only_at_finish() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let old = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 200000).await;
    let pinned = make_log(&a, 'b', "00000002--bbbbbbbbbb", 0, 0., 50000).await;
    pin(&a, 'b', true).await;
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    let (status, session) = begin(&a, "rlog.zst", 4).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = session["id"].as_str().unwrap();
    req(
        &a,
        "PUT",
        &format!("/api/uploads/{id}/files/0?offset=0"),
        b"log\0".to_vec(),
    )
    .await;
    assert!(old.exists());
    let (status, body) = req(&a, "POST", &format!("/api/uploads/{id}/finish"), vec![]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(!old.exists() && pinned.exists());
    assert_eq!(storage::reserved(temp.path()).await.unwrap(), 0);
}
#[tokio::test]
async fn reservations_and_duplicates_never_delete() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let old = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 200000).await;
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    for _ in 0..4 {
        let (status, session) = begin(&a, "rlog.zst", 3).await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(old.exists());
        let (status, _) = req(
            &a,
            "DELETE",
            &format!("/api/uploads/{}", session["id"].as_str().unwrap()),
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(storage::reserved(temp.path()).await.unwrap(), 0);
    }
    // A duplicate of an existing log registers no record and evicts nothing.
    let payload = std::fs::read(old.join("rlog.zst")).unwrap();
    configure(&a, 0, "delete_oldest").await;
    let (status, session) = begin(&a, "rlog.zst", payload.len() as u64).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = session["id"].as_str().unwrap();
    // Content hashes are what the duplicate check compares.
    let mut meta = a.store.meta(&rid('a')).await.unwrap();
    meta["content_hashes"] = json!({"rlog.zst":{"sha256":roadviewer_hars::store::sha(&payload)}});
    a.store.save_meta(meta).await.unwrap();
    for (i, chunk) in payload.chunks(2 * 1024 * 1024).enumerate() {
        let (status, _) = req(
            &a,
            "PUT",
            &format!("/api/uploads/{id}/files/0?offset={}", i * 2 * 1024 * 1024),
            chunk.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    configure(&a, 210000, "delete_oldest").await;
    let (status, body) = req(&a, "POST", &format!("/api/uploads/{id}/finish"), vec![]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["duplicates"].as_array().unwrap().len(), 1);
    assert!(old.exists());
}
#[tokio::test]
async fn concurrent_delete_oldest_reservations_are_bounded_without_deletion() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let old = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 200000).await;
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    let body = json!({"files":[{"name":"rlog.zst","size":10000}]});
    let results = futures_join(&a, body, 5).await;
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 2, "{results:?}");
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| *e == "no_deletable_logs")
    );
    assert!(old.exists());
}
async fn futures_join(a: &App, body: Value, n: usize) -> Vec<Result<Value, &'static str>> {
    let tasks = (0..n)
        .map(|_| {
            let a = a.clone();
            let body = body.clone();
            tokio::spawn(async move { a.uploads.begin_browser(body).await.map_err(|e| e.1) })
        })
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap());
    }
    results
}

// ------------------------------------------------------------ pins

#[tokio::test]
async fn legacy_route_pins_migrate_once_and_later_segments_stay_unpinned() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 10).await;
    make_log(&a, 'b', "00000001--aaaaaaaaaa", 1, 2., 10).await;
    make_log(&a, 'c', "00000002--bbbbbbbbbb", 0, 3., 10).await;
    atomic_json(
        &temp.path().join(storage::SETTINGS),
        &json!({"max_bytes":123456,"policy":"delete_oldest","pinned_routes":["00000001--aaaaaaaaaa"]}),
    )
    .await
    .unwrap();
    drop(a);
    let a = app(temp.path()).await;
    let saved: Value = read_json(&temp.path().join(storage::SETTINGS))
        .await
        .unwrap();
    assert!(saved.get("pinned_routes").is_none());
    assert_eq!(saved["pinned_logs"], json!([rid('a'), rid('b')]));
    assert_eq!(
        (saved["max_bytes"].clone(), saved["policy"].clone()),
        (json!(123456), json!("delete_oldest"))
    );
    make_log(&a, 'd', "00000001--aaaaaaaaaa", 2, 4., 10).await;
    drop(a);
    let a = app(temp.path()).await;
    let (_, body) = req(&a, "GET", "/api/logs", vec![]).await;
    for m in body["logs"].as_array().unwrap() {
        let id = m["id"].as_str().unwrap();
        assert_eq!(
            m["pinned"] == true,
            id == rid('a') || id == rid('b'),
            "{id}"
        );
    }
}
#[tokio::test]
async fn unpin_leaves_other_segments_and_pin_validates() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 10).await;
    make_log(&a, 'b', "00000001--aaaaaaaaaa", 1, 2., 10).await;
    pin(&a, 'a', true).await;
    pin(&a, 'b', true).await;
    pin(&a, 'a', false).await;
    let saved: Value = read_json(&temp.path().join(storage::SETTINGS))
        .await
        .unwrap();
    assert_eq!(saved["pinned_logs"], json!([rid('b')]));
    for body in [json!({}), json!({"pinned":1}), json!([])] {
        let (status, body) = post(&a, &format!("/api/logs/{}/pin", rid('a')), body).await;
        assert_eq!(
            (status, body["error"].clone()),
            (StatusCode::BAD_REQUEST, json!("invalid_pin"))
        );
    }
    let (status, _) = post(
        &a,
        &format!("/api/logs/{}/pin", rid('f')),
        json!({"pinned":true}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------ settings and cleanup

#[tokio::test]
async fn storage_settings_schema_and_strict_validation() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (status, body) = req(&a, "GET", "/api/settings/storage", vec![]).await;
    assert_eq!(status, StatusCode::OK);
    let keys = body
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        [
            "archive",
            "free_bytes",
            "max_bytes",
            "policy",
            "reserved_bytes",
            "used_bytes"
        ]
        .map(String::from)
        .into()
    );
    assert_eq!(
        (body["max_bytes"].clone(), body["policy"].clone()),
        (json!(0), json!("reject_new"))
    );
    for bad in [
        json!({"max_bytes":1}),
        json!({"max_bytes":1,"policy":"reject_new","x":1}),
        json!({"max_bytes":1.0,"policy":"reject_new"}),
        json!({"max_bytes":true,"policy":"reject_new"}),
        json!({"max_bytes":-1,"policy":"reject_new"}),
        json!({"max_bytes":9007199254740992u64,"policy":"reject_new"}),
        json!({"max_bytes":1,"policy":"delete_all"}),
        json!([1]),
    ] {
        let (status, body) = post(&a, "/api/settings/storage", bad.clone()).await;
        assert_eq!(
            (status, body),
            (
                StatusCode::BAD_REQUEST,
                json!({"error":"invalid_storage_settings"})
            ),
            "{bad}"
        );
    }
    let (status, _) = req(&a, "POST", "/api/settings/storage", b"not json".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    pin_target(&a).await;
    let (status, body) = post(
        &a,
        "/api/settings/storage",
        json!({"max_bytes":9007199254740991u64,"policy":"delete_oldest"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max_bytes"], json!(9007199254740991u64));
    let saved: Value = read_json(&temp.path().join(storage::SETTINGS))
        .await
        .unwrap();
    assert_eq!(saved["policy"], "delete_oldest");
    assert_eq!(
        saved["pinned_logs"],
        json!([rid('a')]),
        "pins survive a settings change"
    );
}
async fn pin_target(a: &App) {
    make_log(a, 'a', "00000001--aaaaaaaaaa", 0, 1., 10).await;
    pin(a, 'a', true).await;
}
#[tokio::test]
async fn cleanup_discards_incomplete_uploads_and_keeps_logs() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let log = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 10).await;
    let (_, session) = begin(&a, "rlog.zst", 100).await;
    let id = session["id"].as_str().unwrap().to_owned();
    req(
        &a,
        "PUT",
        &format!("/api/uploads/{id}/files/0?offset=0"),
        vec![b'y'; 40],
    )
    .await;
    assert!(storage::reserved(temp.path()).await.unwrap() > 0);
    let (status, body) = post(&a, "/api/storage/cleanup", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["removed_bytes"].as_u64().unwrap() >= 40, "{body}");
    assert!(!temp.path().join(".uploads").join(&id).exists());
    assert_eq!(storage::reserved(temp.path()).await.unwrap(), 0);
    assert!(log.join("rlog.zst").is_file());
    let (_, body) = post(&a, "/api/storage/cleanup", json!({})).await;
    assert_eq!(body, json!({"removed_bytes":0}));
}

// ------------------------------------------------------------ manual deletion

#[tokio::test]
async fn manual_delete_routes_and_protection() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let pa = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 10).await;
    let pb = make_log(&a, 'b', "00000001--aaaaaaaaaa", 1, 2., 10).await;
    std::fs::create_dir_all(pa.join("prepared")).unwrap();
    std::fs::write(pa.join("prepared/data.json"), b"{}").unwrap();
    pin(&a, 'a', true).await;
    let path = format!("/api/logs/{}", rid('a'));
    let (status, body) = req(&a, "DELETE", &format!("{path}?skip_pinned=1"), vec![]).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({"skipped":"pinned"}))
    );
    assert!(pa.exists());
    // An explicit deletion removes a pinned recording too, as in Python.
    let (status, body) = req(&a, "DELETE", &path, vec![]).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({"deleted":rid('a')}))
    );
    assert!(!pa.exists() && pb.exists());
    assert_eq!(listed(&a).await, [rid('b')].into());
    let (status, body) = req(&a, "DELETE", &path, vec![]).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::NOT_FOUND, json!("not_found"))
    );
    let (status, _) = req(&a, "DELETE", "/api/logs/not-an-id", vec![]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A recording still named by a registration journal waits for it.
    let (_, session) = begin(&a, "rlog.zst", 3).await;
    atomic_json(
        &temp
            .path()
            .join(".uploads")
            .join(session["id"].as_str().unwrap())
            .join("commit.json"),
        &json!({"records":[{"id":rid('b')}],"moves":[],"result":{}}),
    )
    .await
    .unwrap();
    let (status, body) = req(&a, "DELETE", &format!("/api/logs/{}", rid('b')), vec![]).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("upload_busy"))
    );
    assert!(pb.join("rlog.zst").is_file());
    std::fs::remove_file(
        temp.path()
            .join(".uploads")
            .join(session["id"].as_str().unwrap())
            .join("commit.json"),
    )
    .unwrap();
    // A conversion that is still running is never deleted under it.
    set_status(&a, 'b', "processing").await;
    let (status, body) = req(&a, "DELETE", &format!("/api/logs/{}", rid('b')), vec![]).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("conversion_busy"))
    );
    assert!(pb.join("rlog.zst").is_file());
    set_status(&a, 'b', "queued").await;
    let (status, _) = req(&a, "DELETE", &format!("/api/logs/{}", rid('b')), vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!temp.path().join(storage::JOURNAL).exists());
    assert!(hidden_dirs(temp.path()).is_empty());
}

// ------------------------------------------------------------ journal and restart

async fn three_logs(a: &App) -> (PathBuf, PathBuf, PathBuf) {
    let pa = make_log(a, 'a', "00000001--aaaaaaaaaa", 0, 1., 1000).await;
    let pb = make_log(a, 'b', "00000002--bbbbbbbbbb", 0, 2., 1000).await;
    let pc = make_log(a, 'c', "00000003--cccccccccc", 0, 3., 1000).await;
    for p in [&pa, &pb, &pc] {
        std::fs::create_dir_all(p.join("prepared")).unwrap();
        std::fs::write(p.join("prepared/data.json"), vec![b'z'; 500]).unwrap();
        std::fs::write(p.join("qcamera.ts"), vec![b'v'; 500]).unwrap();
    }
    (pa, pb, pc)
}
async fn assert_converged(root: &Path, gone: &[char], kept: &[&PathBuf]) {
    let a = app(root).await;
    for c in gone {
        assert!(!root.join(rid(*c)).exists());
        assert!(a.store.meta(&rid(*c)).await.is_err());
    }
    for p in kept {
        assert!(p.join("rlog.zst").is_file() && p.join("prepared/data.json").is_file());
    }
    assert!(!root.join(storage::JOURNAL).exists());
    assert!(hidden_dirs(root).is_empty());
    assert_eq!(listed(&a).await.len(), kept.len());
}
/// The rename is the commit point: a crash after the journal write but before the rename leaves
/// the recording whole, and nothing re-decides its deletion after the restart (as in Python,
/// where nothing is removed at this point).
#[tokio::test]
async fn restart_after_journal_write_keeps_the_recording() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (pa, pb, pc) = three_logs(&a).await;
    storage::journal_add(temp.path(), &[rid('a')], "manual")
        .await
        .unwrap();
    let journal: Value = read_json(&temp.path().join(storage::JOURNAL))
        .await
        .unwrap();
    assert_eq!(journal["version"], 1);
    assert_eq!(journal["deletes"][0]["id"], rid('a'));
    assert_eq!(journal["deletes"][0]["reason"], "manual");
    drop(a);
    assert_converged(temp.path(), &[], &[&pa, &pb, &pc]).await;
}
#[tokio::test]
async fn periodic_retry_discards_a_stale_journal_entry_and_keeps_the_recording() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (pa, pb, pc) = three_logs(&a).await;
    // As left by a failed hide whose journal cleanup failed too.
    storage::journal_add(temp.path(), &[rid('a')], "storage_limit")
        .await
        .unwrap();
    {
        let registration = a.store.registration.lock().await;
        assert_eq!(storage::recover(&a.store, &registration).await.unwrap(), 0);
    }
    assert_eq!(listed(&a).await.len(), 3);
    assert!(!storage::pending(temp.path()).await.unwrap());
    drop(a);
    assert_converged(temp.path(), &[], &[&pa, &pb, &pc]).await;
}
#[tokio::test]
async fn restart_after_partial_removal_finishes_only_hidden_deletions() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (_, _, pc) = three_logs(&a).await;
    let ids = [rid('a'), rid('b')];
    storage::journal_add(temp.path(), &ids, "storage_limit")
        .await
        .unwrap();
    // a: hidden and half removed; b: crash before it was hidden, with a file already missing.
    storage::hide(&a.store, &ids[0]).await.unwrap();
    assert!(!listed(&a).await.contains(&ids[0]));
    let hidden = storage::hidden_path(temp.path(), &ids[0]);
    std::fs::remove_file(hidden.join("rlog.zst")).unwrap();
    std::fs::remove_dir_all(hidden.join("prepared")).unwrap();
    std::fs::remove_file(temp.path().join(&ids[1]).join("qcamera.ts")).unwrap();
    let pb = temp.path().join(&ids[1]);
    drop(a);
    assert_converged(temp.path(), &['a'], &[&pb, &pc]).await;
}
#[tokio::test]
async fn restart_before_journal_removal_clears_it() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (pa, pb, _) = three_logs(&a).await;
    storage::journal_add(temp.path(), &[rid('c')], "manual")
        .await
        .unwrap();
    storage::hide(&a.store, &rid('c')).await.unwrap();
    storage::purge(temp.path(), &rid('c')).await.unwrap();
    drop(a);
    assert_converged(temp.path(), &['c'], &[&pa, &pb]).await;
}
#[tokio::test]
async fn corrupt_journal_is_kept_aside_without_deleting() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (pa, pb, pc) = three_logs(&a).await;
    drop(a);
    std::fs::write(
        temp.path().join(storage::JOURNAL),
        br#"{"version":1,"deletes":[{"id":"../x","reason":"manual","at":0}]}"#,
    )
    .unwrap();
    let a = app(temp.path()).await;
    assert_eq!(listed(&a).await.len(), 3);
    assert!(pa.exists() && pb.exists() && pc.exists());
    assert!(!temp.path().join(storage::JOURNAL).exists());
    assert!(std::fs::read_dir(temp.path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".hars-delete-journal.corrupt-")
    }));
}
#[tokio::test]
async fn corrupt_journals_are_never_overwritten_and_unknown_versions_are_kept_aside() {
    let temp = tempfile::tempdir().unwrap();
    let journal = temp.path().join(storage::JOURNAL);
    let bodies: [&[u8]; 3] = [
        b"not json",
        br#"{"version":1,"deletes":[{"id":"../x","reason":"manual","at":0}]}"#,
        br#"{"version":2,"deletes":[]}"#,
    ];
    // Within the same second.
    for body in bodies {
        std::fs::write(&journal, body).unwrap();
        assert!(
            storage::journal_entries(temp.path())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!journal.exists());
    }
    let mut aside = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".hars-delete-journal.corrupt-")
        })
        .map(|p| std::fs::read(p).unwrap())
        .collect::<Vec<_>>();
    aside.sort();
    let mut expected = bodies.map(|b| b.to_vec()).to_vec();
    expected.sort();
    assert_eq!(aside, expected);
}

// ------------------------------------------------------------ filesystem failures

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn unwritable_data_root_refuses_deletion_and_keeps_the_recording() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (pa, pb, pc) = three_logs(&a).await;
    chmod(temp.path(), 0o555);
    let (status, body) = req(&a, "DELETE", &format!("/api/logs/{}", rid('a')), vec![]).await;
    chmod(temp.path(), 0o755);
    assert_eq!(
        (status, body),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"storage_error"})
        )
    );
    assert_eq!(listed(&a).await.len(), 3);
    for p in [&pa, &pb, &pc] {
        assert!(p.join("rlog.zst").is_file() && p.join("prepared/data.json").is_file());
    }
    assert!(!temp.path().join(storage::JOURNAL).exists());
}
#[cfg(unix)]
#[tokio::test]
async fn partial_deletion_failure_stays_journaled_and_is_retried() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (pa, pb, pc) = three_logs(&a).await;
    chmod(&pa.join("prepared"), 0o555);
    let (status, body) = req(&a, "DELETE", &format!("/api/logs/{}", rid('a')), vec![]).await;
    assert_eq!(
        (status, body),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"storage_error"})
        )
    );
    // Never half-listed: hidden, journaled, the other recordings untouched.
    assert_eq!(listed(&a).await, [rid('b'), rid('c')].into());
    assert!(temp.path().join(storage::JOURNAL).exists());
    let hidden = storage::hidden_path(temp.path(), &rid('a'));
    assert!(hidden.join("prepared/data.json").is_file());
    for p in [&pb, &pc] {
        assert!(p.join("rlog.zst").is_file() && p.join("prepared/data.json").is_file());
    }
    {
        let registration = a.store.registration.lock().await;
        assert!(storage::recover(&a.store, &registration).await.is_err());
    }
    assert!(storage::pending(temp.path()).await.unwrap());
    chmod(&hidden.join("prepared"), 0o755);
    {
        let registration = a.store.registration.lock().await;
        assert_eq!(storage::recover(&a.store, &registration).await.unwrap(), 1);
    }
    assert!(!storage::pending(temp.path()).await.unwrap());
    drop(a);
    assert_converged(temp.path(), &['a'], &[&pb, &pc]).await;
}
#[cfg(unix)]
#[tokio::test]
async fn eviction_failure_reports_and_preserves_other_recordings() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (pa, pb, pc) = three_logs(&a).await;
    chmod(&pa.join("prepared"), 0o555);
    configure(&a, used(&a).await + 65536 - 1500, "delete_oldest").await;
    let error = commit(&a, 0, &[]).await.unwrap_err();
    assert_eq!(error.1, "storage_error");
    chmod(
        &storage::hidden_path(temp.path(), &rid('a')).join("prepared"),
        0o755,
    );
    assert!(pb.join("rlog.zst").is_file() && pc.join("rlog.zst").is_file());
    drop(a);
    assert_converged(temp.path(), &['a'], &[&pb, &pc]).await;
}
#[cfg(unix)]
#[tokio::test]
async fn hide_that_fails_after_the_rename_still_unlists_the_recording() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let (_, pb, pc) = three_logs(&a).await;
    // Write and search only: the rename succeeds, the directory sync cannot open the root.
    chmod(temp.path(), 0o333);
    let outcome = storage::hide(&a.store, &rid('a')).await;
    chmod(temp.path(), 0o755);
    assert!(outcome.is_err());
    assert!(storage::hidden_path(temp.path(), &rid('a')).is_dir());
    assert_eq!(listed(&a).await, [rid('b'), rid('c')].into());
    assert!(a.store.meta(&rid('a')).await.is_err());
    {
        let registration = a.store.registration.lock().await;
        assert_eq!(storage::recover(&a.store, &registration).await.unwrap(), 1);
    }
    drop(a);
    assert_converged(temp.path(), &['a'], &[&pb, &pc]).await;
}

// ------------------------------------------------------------ races

/// Polls `done` until it holds; ordering is decided by observed state, never by a sleep length.
async fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "state never reached"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_creation_never_straddles_a_commit_eviction() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let route = "00000001--aaaaaaaaaa";
    let segments = [
        make_log(&a, 'a', route, 0, 1., 100000).await,
        make_log(&a, 'b', route, 1, 2., 100000).await,
        make_log(&a, 'c', route, 2, 3., 100000).await,
    ];
    let other = make_log(&a, 'd', "00000002--bbbbbbbbbb", 0, 4., 300000).await;
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    // A finish holds the registration lock through its guard read and eviction.
    let registration = a.store.registration.lock().await;
    let begin = {
        let a = a.clone();
        let body = json!({"files":[{"name":format!("{route}--3--rlog.zst"),"size":10}]});
        tokio::spawn(async move { a.uploads.begin_browser(body).await.map_err(|e| e.1) })
    };
    // An admitted session must not exist without its manifest while the eviction can read guards.
    let uploads = temp.path().join(".uploads");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < deadline {
        assert_eq!(
            std::fs::read_dir(&uploads).unwrap().count(),
            0,
            "a session was created while a commit held the registration lock"
        );
        assert!(!begin.is_finished());
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    drop(registration);
    begin.await.unwrap().unwrap();
    let (status, body) = upload(&a, "00000009--ffffffffff--0--rlog.zst", b"fresh").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(segments.iter().all(|p| p.join("rlog.zst").is_file()));
    assert!(!other.exists());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_skips_a_whole_route_queued_after_planning() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let route = "00000001--aaaaaaaaaa";
    let pa = make_log(&a, 'a', route, 0, 1., 100000).await;
    let pb = make_log(&a, 'b', route, 1, 2., 100000).await;
    let pc = make_log(&a, 'c', "00000002--bbbbbbbbbb", 0, 3., 100000).await;
    configure(&a, used(&a).await + 65536 - 1000, "delete_oldest").await;
    // Hold segment a's record lock the way `convert` does; the eviction plans without it.
    let lock = a.store.record_lock(&rid('a')).await;
    let guard = lock.lock().await;
    let eviction = {
        let a = a.clone();
        tokio::spawn(async move { commit(&a, 0, &[]).await.map_err(|e| e.1) })
    };
    // The planned eviction now waits for this lock: queue the segment as `convert` would.
    wait_until(|| std::sync::Arc::strong_count(&lock) > 2).await;
    set_status(&a, 'a', "queued").await;
    drop(guard);
    eviction.await.unwrap().unwrap();
    assert!(pa.join("rlog.zst").is_file() && pb.join("rlog.zst").is_file());
    assert!(!pc.exists());
    assert_eq!(listed(&a).await, [rid('a'), rid('b')].into());
}

#[tokio::test]
async fn concurrent_finish_eviction_and_manual_delete_converge() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let px = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 200000).await;
    let py = make_log(&a, 'b', "00000002--bbbbbbbbbb", 0, 2., 200000).await;
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    let (_, session) = begin(&a, "00000009--ffffffffff--0--rlog.zst", 5).await;
    let id = session["id"].as_str().unwrap().to_owned();
    req(
        &a,
        "PUT",
        &format!("/api/uploads/{id}/files/0?offset=0"),
        b"fresh".to_vec(),
    )
    .await;
    let finish = {
        let a = a.clone();
        let id = id.clone();
        tokio::spawn(async move {
            a.uploads
                .finish(&id, false, &HeaderMap::new(), Value::Null)
                .await
                .map(|r| r.0)
                .map_err(|e| e.1)
        })
    };
    let delete = {
        let a = a.clone();
        tokio::spawn(async move {
            req(&a, "DELETE", &format!("/api/logs/{}", rid('a')), vec![])
                .await
                .0
        })
    };
    let (finish, delete) = (finish.await.unwrap(), delete.await.unwrap());
    assert_eq!(finish, Ok(StatusCode::CREATED));
    assert!(
        matches!(delete, StatusCode::OK | StatusCode::NOT_FOUND),
        "{delete}"
    );
    assert!(!px.exists());
    // Whichever ran first, removing x alone made room: y is never touched.
    assert!(py.exists());
    assert!(!temp.path().join(storage::JOURNAL).exists());
    assert!(hidden_dirs(temp.path()).is_empty());
    let on_disk = std::fs::read_dir(temp.path())
        .unwrap()
        .filter(|e| {
            roadviewer_hars::store::valid_id(&e.as_ref().unwrap().file_name().to_string_lossy())
        })
        .count();
    assert_eq!(on_disk, a.store.records.read().await.len());
}
/// Python  judges remote copies per recording: a route with one Drive-backed segment still gives up its
/// other segments, before any newer route.
#[tokio::test]
async fn a_drive_copy_keeps_only_its_own_segment() {
    let temp = tempfile::tempdir().unwrap();
    let a = app(temp.path()).await;
    let pa = make_log(&a, 'a', "00000001--aaaaaaaaaa", 0, 1., 50000).await;
    let pb = make_log(&a, 'b', "00000001--aaaaaaaaaa", 1, 2., 100000).await;
    let pc = make_log(&a, 'c', "00000002--bbbbbbbbbb", 0, 3., 50000).await;
    let mut meta = a.store.meta(&rid('a')).await.unwrap();
    meta["storage"] = json!({"rlog.zst":{"location":"local","remote":{"backend":"gdrive","file_id":"f1","bytes":50000,"sha256":"00"}}});
    a.store.save_meta(meta).await.unwrap();
    configure(&a, used(&a).await + 20000, "delete_oldest").await;
    let plan = storage::eviction_plan(&a.store, &[], &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(plan.ids(), vec![rid('b'), rid('c')]);
    assert_eq!(plan.kept[&rid('a')], Protection::RemoteCopy);
    commit(&a, 10000, &[]).await.unwrap();
    assert!(pa.exists(), "the Drive-backed segment stays");
    assert!(!pb.exists(), "its route's other segment goes first");
    assert!(pc.exists(), "the newer route is not touched");
}
