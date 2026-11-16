//! Upload fault injection over real loopback TCP against an in-process server.
mod common;
use common::*;
use roadviewer_hars::store::{atomic_json, now, read_json, sha};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{io::AsyncWriteExt, net::TcpStream};

const KIB: usize = 1024;

async fn fixture() -> (tempfile::TempDir, Server, Value) {
    let temp = tempfile::tempdir().unwrap();
    let server = serve(open(temp.path()).await).await;
    let creds = pair(server.ui, server.device).await;
    (temp, server, creds)
}
async fn slots_free(server: &Server) {
    let app = server.app.clone();
    poll("transfer slots to return", || {
        let app = app.clone();
        async move { app.transfer_slots_available() == 8 }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_drop_mid_chunk_keeps_only_acknowledged_bytes_and_resumes() {
    let (temp, server, creds) = fixture().await;
    let (log, video) = segment(1, 300 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "tcp_drop_batch_0001",
        1,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let session = begin(server.device, &creds, &manifest).await;
    assert_eq!(
        put(server.device, &session, 0, 0, &log[..100 * KIB])
            .await
            .0,
        200
    );
    let id = session["id"].as_str().unwrap();
    for reset in [false, true] {
        let path = format!("/api/device/uploads/{id}/files/0?offset={}", 100 * KIB);
        let request = request_bytes("PUT", &path, &bearer(&session), &log[100 * KIB..200 * KIB]);
        let head = request.len() - 100 * KIB;
        let mut stream = TcpStream::connect(server.device).await.unwrap();
        stream.write_all(&request[..head + 37 * KIB]).await.unwrap();
        stream.flush().await.unwrap();
        if reset {
            stream.set_zero_linger().unwrap();
        } else {
            stream.shutdown().await.unwrap();
        }
        drop(stream);
        slots_free(&server).await;
        let (code, state) = status(server.device, &session).await;
        assert_eq!(code, 200);
        assert_eq!(
            state["files"][0]["received"],
            100 * KIB,
            "a partial body must not be stored"
        );
        assert_eq!(
            std::fs::metadata(temp.path().join(".uploads").join(id).join("0"))
                .unwrap()
                .len(),
            100 * KIB as u64
        );
    }
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 1,
        },
    )
    .await;
    let resumed = begin(server.device, &creds, &manifest).await;
    assert_eq!(resumed["id"], session["id"]);
    assert_eq!(resumed["files"][0]["received"], 100 * KIB);
    upload_all(server.device, &resumed, &files, 64 * KIB).await;
    let (code, result) = finish(server.device, &resumed, &files).await;
    assert_eq!(code, 201, "{result}");
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_tcp_drop_mid_chunk_resumes_from_disk_length() {
    let (temp, server, _) = fixture().await;
    let (log, _) = segment(2, 200 * KIB);
    let body =
        serde_json::to_vec(&json!({"files":[{"name":"rlog.zst","size":log.len()}]})).unwrap();
    let mut headers = csrf();
    headers.push(("Content-Type", "application/json".into()));
    let (code, session) = http(server.ui, "POST", "/api/uploads", &headers, &body).await;
    assert_eq!(code, 201);
    let id = session["id"].as_str().unwrap();
    let path = |offset: usize| format!("/api/uploads/{id}/files/0?offset={offset}");
    assert_eq!(
        http(server.ui, "PUT", &path(0), &csrf(), &log[..50 * KIB])
            .await
            .0,
        200
    );
    let request = request_bytes("PUT", &path(50 * KIB), &csrf(), &log[50 * KIB..]);
    let mut stream = TcpStream::connect(server.ui).await.unwrap();
    stream
        .write_all(&request[..request.len() - 10])
        .await
        .unwrap();
    stream.set_zero_linger().unwrap();
    drop(stream);
    slots_free(&server).await;
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 1,
        },
    )
    .await;
    assert_eq!(
        http(server.ui, "PUT", &path(50 * KIB), &csrf(), &log[50 * KIB..])
            .await
            .0,
        200
    );
    let (code, result) = http(
        server.ui,
        "POST",
        &format!("/api/uploads/{id}/finish"),
        &csrf(),
        b"",
    )
    .await;
    assert_eq!(code, 201, "{result}");
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&[&log]],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chunk_replay_and_offset_conflicts_keep_the_stored_prefix() {
    let (temp, server, creds) = fixture().await;
    let (log, video) = segment(3, 64 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "replay_batch_000001",
        3,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let session = begin(server.device, &creds, &manifest).await;
    let dev = server.device;
    assert_eq!(
        put(dev, &session, 0, 0, &log[..16 * KIB]).await.1["received"],
        16 * KIB
    );
    assert_eq!(
        put(dev, &session, 0, 0, &log[..16 * KIB]).await.1["received"],
        16 * KIB
    );
    assert_eq!(
        put(dev, &session, 0, 8 * KIB, &log[8 * KIB..16 * KIB])
            .await
            .1["received"],
        16 * KIB
    );
    let mut tampered = log[..16 * KIB].to_vec();
    tampered[100] ^= 1;
    for (offset, data, expected) in [
        (0, &tampered[..], "offset_conflict"),
        (32 * KIB, &log[32 * KIB..40 * KIB], "offset_conflict"),
        (8 * KIB, &log[8 * KIB..24 * KIB], "offset_conflict"),
    ] {
        let (code, body) = put(dev, &session, 0, offset, data).await;
        assert_eq!((code, body["error"].as_str().unwrap()), (409, expected));
    }
    let (code, body) = put(dev, &session, 0, 60 * KIB, &log[56 * KIB..]).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (400, "invalid_chunk")
    );
    let (code, body) = finish(dev, &session, &files).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (409, "incomplete_upload")
    );
    assert_eq!(
        status(dev, &session).await.1["files"][0]["received"],
        16 * KIB
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 1,
        },
    )
    .await;
    let session = begin(dev, &creds, &manifest).await;
    upload_all(dev, &session, &files, 16 * KIB).await;
    assert_eq!(finish(dev, &session, &files).await.0, 201);
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_finish_response_and_batch_receipt_are_reused() {
    let (temp, server, creds) = fixture().await;
    let dev = server.device;
    let (log, video) = segment(4, 96 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "receipt_batch_00001",
        4,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let session = begin(dev, &creds, &manifest).await;
    upload_all(dev, &session, &files, 32 * KIB).await;
    let (a, b) = tokio::join!(finish(dev, &session, &files), finish(dev, &session, &files));
    let mut codes = [a.0, b.0];
    codes.sort();
    // One finish registers; the overlapping one is told to retry (transient) or sees the receipt.
    assert!(
        codes == [201, 409] || codes == [200, 201],
        "{codes:?} {a:?} {b:?}"
    );
    let result = if a.0 == 201 { a.1 } else { b.1 };
    for _ in 0..2 {
        let (code, again) = finish(dev, &session, &files).await;
        assert_eq!((code, &again), (200, &result));
    }
    let (code, state) = status(dev, &session).await;
    assert_eq!((code, &state["state"]), (200, &json!("completed")));
    let completed = begin(dev, &creds, &manifest).await;
    assert_eq!(completed, json!({"state":"completed","result":result}));
    let (code, body) = put(dev, &session, 0, 0, &log[..KIB]).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (409, "session_completed")
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
    let root = temp.path().to_path_buf();
    drop(server);
    let server = serve(open(&root).await).await;
    assert_eq!(
        finish(server.device, &session, &files).await,
        (200, result.clone())
    );
    invariants(
        &root,
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_batches_and_reuploaded_segments_register_once() {
    let (temp, server, creds) = fixture().await;
    let dev = server.device;
    let (log, video) = segment(5, 64 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "dup_batch_000000001",
        5,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let (a, b) = tokio::join!(
        begin_raw(dev, &creds, &manifest),
        begin_raw(dev, &creds, &manifest)
    );
    let mut codes = [a.0, b.0];
    codes.sort();
    assert_eq!(codes, [200, 201]);
    assert_eq!(a.1["id"], b.1["id"]);
    let valid = [status(dev, &a.1).await.0, status(dev, &b.1).await.0];
    // The later issue rotates the token, exactly like a renewed begin.
    assert_eq!(valid.iter().filter(|c| **c == 200).count(), 1, "{valid:?}");
    let live = if valid[0] == 200 { a.1 } else { b.1 };
    upload_all(dev, &live, &files, 32 * KIB).await;
    let (code, first) = finish(dev, &live, &files).await;
    assert_eq!(code, 201);
    let record = first["logs"][0]["id"].clone();
    let disk_before =
        std::fs::read(temp.path().join(record.as_str().unwrap()).join("meta.json")).unwrap();

    for (n, set) in [
        (1, vec![("rlog.zst", &log[..]), ("qcamera.ts", &video[..])]),
        (2, vec![("rlog.zst", &log[..])]),
    ] {
        let again = batch(&format!("dup_batch_reupload_{n}"), 5, &set);
        let session = begin(dev, &creds, &again).await;
        assert_eq!(session["files"][0]["received"], 0);
        let data = set.iter().map(|(_, d)| *d).collect::<Vec<_>>();
        upload_all(dev, &session, &data, 32 * KIB).await;
        let (code, result) = finish(dev, &session, &data).await;
        assert_eq!(code, 201, "{result}");
        assert_eq!(result["logs"], json!([]));
        assert_eq!(result["updated"], json!([]));
        assert_eq!(result["duplicates"][0]["id"], record);
        assert_eq!(result["duplicates"][0]["video_differs"], false);
    }
    assert_eq!(
        std::fs::read(temp.path().join(record.as_str().unwrap()).join("meta.json")).unwrap(),
        disk_before
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
}

async fn manual_processing(server: &Server) {
    let body = serde_json::to_vec(&json!({"auto_convert":false})).unwrap();
    let mut headers = csrf();
    headers.push(("Content-Type", "application/json".into()));
    assert_eq!(
        http(
            server.ui,
            "POST",
            "/api/settings/processing",
            &headers,
            &body
        )
        .await
        .0,
        200
    );
}
async fn send_batch(
    server: &Server,
    creds: &Value,
    id: &str,
    number: u32,
    files: &[(&str, &[u8])],
) -> (u16, Value) {
    let session = begin(server.device, creds, &batch(id, number, files)).await;
    let data = files.iter().map(|(_, d)| *d).collect::<Vec<_>>();
    upload_all(server.device, &session, &data, 32 * KIB).await;
    finish(server.device, &session, &data).await
}

#[tokio::test(flavor = "multi_thread")]
async fn camera_added_to_registered_segment_is_attached_and_reconverted() {
    let (temp, server, creds) = fixture().await;
    manual_processing(&server).await;
    let (log, video) = segment(6, 32 * KIB);
    let (code, result) = send_batch(
        &server,
        &creds,
        "camera_batch_000001",
        6,
        &[("rlog.zst", &log)],
    )
    .await;
    assert_eq!(code, 201);
    let record = result["logs"][0]["id"].as_str().unwrap().to_owned();
    let folder = temp.path().join(&record);
    // A finished conversion of the log alone.
    std::fs::create_dir(folder.join("prepared")).unwrap();
    std::fs::write(folder.join("prepared/summary.json"), b"{}").unwrap();
    let mut meta = server.app.store.meta(&record).await.unwrap();
    meta["status"] = json!("ready");
    meta["duration"] = json!(60.0);
    meta["warnings"] = json!(["old"]);
    server.app.store.save_meta(meta).await.unwrap();

    let (code, result) = send_batch(
        &server,
        &creds,
        "camera_batch_000002",
        6,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    )
    .await;
    assert_eq!(code, 201, "{result}");
    assert_eq!(
        (&result["logs"], &result["duplicates"]),
        (&json!([]), &json!([]))
    );
    let updated = &result["updated"][0];
    assert_eq!(result["updated"].as_array().unwrap().len(), 1);
    assert_eq!(updated["id"], record.as_str());
    assert_eq!(
        updated["files"]["qcamera.ts"],
        "2026-11-15--12-00-00--6--qcamera.ts"
    );
    assert_eq!(
        updated["content_hashes"]["qcamera.ts"]["sha256"],
        sha(&video)
    );
    // Python `clear_prepared`, then `unconverted` because automatic conversion is off.
    assert_eq!(
        (
            &updated["status"],
            &updated["duration"],
            &updated["warnings"],
            &updated["conversion_revision"],
            &updated["video"]
        ),
        (
            &json!("unconverted"),
            &Value::Null,
            &json!([]),
            &json!(1),
            &json!(true)
        )
    );
    assert_eq!(updated.get("original_restored"), None);
    assert!(!folder.join("prepared").exists());
    assert_eq!(std::fs::read(folder.join("qcamera.ts")).unwrap(), video);
    let saved: Value = read_json(&folder.join("meta.json")).await.unwrap();
    assert_eq!(approx(&saved), approx(updated));
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&[&log, &video]],
            sessions: 0,
        },
    )
    .await;

    // The same files again, and a different video for a camera it has: duplicates, untouched.
    let before = std::fs::read(folder.join("meta.json")).unwrap();
    let (_, other) = segment(60, 32 * KIB);
    for (n, camera, differs) in [(3, &video, false), (4, &other, true)] {
        let (code, result) = send_batch(
            &server,
            &creds,
            &format!("camera_batch_00000{n}"),
            6,
            &[("rlog.zst", &log), ("qcamera.ts", camera)],
        )
        .await;
        assert_eq!(code, 201, "{result}");
        assert_eq!(
            (&result["logs"], &result["updated"]),
            (&json!([]), &json!([]))
        );
        assert_eq!(
            result["duplicates"],
            json!([{"id":record,"name":"2026-11-15--12-00-00 / 구간 6","video_differs":differs}])
        );
    }
    assert_eq!(std::fs::read(folder.join("meta.json")).unwrap(), before);
    assert_eq!(std::fs::read(folder.join("qcamera.ts")).unwrap(), video);
}

#[tokio::test(flavor = "multi_thread")]
async fn camera_addition_waits_for_an_archive_and_needs_the_log() {
    let (temp, server, creds) = fixture().await;
    manual_processing(&server).await;
    let (log, video) = segment(61, 32 * KIB);
    let (code, result) = send_batch(
        &server,
        &creds,
        "camera_refused_0001",
        7,
        &[("rlog.zst", &log)],
    )
    .await;
    assert_eq!(code, 201);
    let record = result["logs"][0]["id"].as_str().unwrap().to_owned();
    let folder = temp.path().join(&record);
    for (n, storage, expected) in [
        (
            2,
            json!({"rlog.zst":{"location":"nas","bytes":log.len(),"sha256":sha(&log)}}),
            "storage_unavailable",
        ),
        (
            3,
            json!({"rlog.zst":{"location":"removed","removed_at":1.0,"reason":"cleanup"}}),
            "log_unavailable",
        ),
    ] {
        let mut meta = server.app.store.meta(&record).await.unwrap();
        meta["storage"] = storage;
        server.app.store.save_meta(meta).await.unwrap();
        let before = std::fs::read(folder.join("meta.json")).unwrap();
        let (code, body) = send_batch(
            &server,
            &creds,
            &format!("camera_refused_000{n}"),
            7,
            &[("rlog.zst", &log), ("qcamera.ts", &video)],
        )
        .await;
        assert_eq!((code, body["error"].as_str().unwrap()), (409, expected));
        assert!(body["message"].as_str().is_some_and(|m| !m.is_empty()));
        assert_eq!(std::fs::read(folder.join("meta.json")).unwrap(), before);
        assert!(!folder.join("qcamera.ts").exists());
        invariants(
            temp.path(),
            server.ui,
            Expect {
                registered: &[&[&log]],
                sessions: 0,
            },
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn camera_addition_admission_never_evicts_its_recording() {
    let (temp, server, creds) = fixture().await;
    manual_processing(&server).await;
    let (log, video) = segment(62, 64 * KIB);
    let (code, result) = send_batch(
        &server,
        &creds,
        "camera_evict_000001",
        8,
        &[("rlog.zst", &log)],
    )
    .await;
    assert_eq!(code, 201);
    let target = result["logs"][0]["id"].as_str().unwrap().to_owned();
    // A newer recording of another route: the only one cleanup may take.
    let (other, _) = segment(63, 64 * KIB);
    let manifest = json!({"batch_id":"camera_evict_000002","segments":[{"route":"2026-11-14--08-00-00","segment":1,"files":[{"kind":"rlog.zst","size":other.len(),"sha256":null}]}]});
    let session = begin(server.device, &creds, &manifest).await;
    upload_all(server.device, &session, &[&other], 32 * KIB).await;
    let (code, result) = finish(server.device, &session, &[&other]).await;
    assert_eq!(code, 201);
    let newer = result["logs"][0]["id"].as_str().unwrap().to_owned();
    let mut meta = server.app.store.meta(&target).await.unwrap();
    meta["uploaded"] = json!(1.0);
    server.app.store.save_meta(meta).await.unwrap();

    let session = begin(
        server.device,
        &creds,
        &batch(
            "camera_evict_000003",
            8,
            &[("rlog.zst", &log), ("qcamera.ts", &video)],
        ),
    )
    .await;
    upload_all(server.device, &session, &[&log, &video], 32 * KIB).await;
    let used = server.app.store.used().await.unwrap();
    let body = serde_json::to_vec(&json!({"max_bytes":used + 65536 - 1,"policy":"delete_oldest"}))
        .unwrap();
    let mut headers = csrf();
    headers.push(("Content-Type", "application/json".into()));
    assert_eq!(
        http(server.ui, "POST", "/api/settings/storage", &headers, &body)
            .await
            .0,
        200
    );
    let (code, result) = finish(server.device, &session, &[&log, &video]).await;
    assert_eq!(code, 201, "{result}");
    assert_eq!(result["updated"][0]["id"], target.as_str());
    assert!(
        !temp.path().join(&newer).exists(),
        "the other recording made room"
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&[&log, &video]],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_and_legacy_uploads_attach_cameras_like_device_uploads() {
    let (temp, server, _) = fixture().await;
    manual_processing(&server).await;
    let (log, video) = segment(64, 32 * KIB);
    let name = |kind: &str| format!("2026-11-15--12-00-00--9--{kind}");
    let mut stream = TcpStream::connect(server.ui).await.unwrap();
    stream
        .write_all(&legacy_request(&name("rlog.zst"), &log))
        .await
        .unwrap();
    let (code, result) = parse_response(&read_all(&mut stream).await);
    assert_eq!(code, 201, "{result}");
    let record = result["logs"][0]["id"].clone();
    // Browser chunked upload of both files.
    let body = serde_json::to_vec(&json!({"files":[
        {"name":name("rlog.zst"),"size":log.len()},{"name":name("qcamera.ts"),"size":video.len()}]}))
    .unwrap();
    let mut headers = csrf();
    headers.push(("Content-Type", "application/json".into()));
    let (code, session) = http(server.ui, "POST", "/api/uploads", &headers, &body).await;
    assert_eq!(code, 201);
    let id = session["id"].as_str().unwrap();
    for (i, data) in [&log, &video].iter().enumerate() {
        let path = format!("/api/uploads/{id}/files/{i}?offset=0");
        assert_eq!(http(server.ui, "PUT", &path, &csrf(), data).await.0, 200);
    }
    let (code, result) = http(
        server.ui,
        "POST",
        &format!("/api/uploads/{id}/finish"),
        &csrf(),
        b"",
    )
    .await;
    assert_eq!(code, 201, "{result}");
    assert_eq!(result["updated"][0]["id"], record);
    assert_eq!(
        result["updated"][0]["files"]["qcamera.ts"],
        name("qcamera.ts")
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&[&log, &video]],
            sessions: 0,
        },
    )
    .await;
    // A second segment: browser upload of the log, then the legacy form with log and camera.
    let (first_log, first_video) = (log, video);
    let name = |kind: &str| format!("2026-11-15--12-00-00--10--{kind}");
    let (log, video) = segment(65, 32 * KIB);
    let body =
        serde_json::to_vec(&json!({"files":[{"name":name("rlog.zst"),"size":log.len()}]})).unwrap();
    let (code, session) = http(server.ui, "POST", "/api/uploads", &headers, &body).await;
    assert_eq!(code, 201);
    let id = session["id"].as_str().unwrap();
    let path = format!("/api/uploads/{id}/files/0?offset=0");
    assert_eq!(http(server.ui, "PUT", &path, &csrf(), &log).await.0, 200);
    let (code, result) = http(
        server.ui,
        "POST",
        &format!("/api/uploads/{id}/finish"),
        &csrf(),
        b"",
    )
    .await;
    assert_eq!(code, 201, "{result}");
    let second = result["logs"][0]["id"].clone();
    let mut stream = TcpStream::connect(server.ui).await.unwrap();
    stream
        .write_all(&legacy_files(&[
            (&name("rlog.zst"), &log),
            (&name("qcamera.ts"), &video),
        ]))
        .await
        .unwrap();
    let (code, result) = parse_response(&read_all(&mut stream).await);
    assert_eq!(code, 201, "{result}");
    assert_eq!(result["updated"][0]["id"], second);
    assert_eq!(
        (&result["logs"], &result["duplicates"]),
        (&json!([]), &json!([]))
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&[&first_log, &first_video], &[&log, &video]],
            sessions: 0,
        },
    )
    .await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn disk_full_write_releases_session_and_a_new_session_completes() {
    let (temp, server, creds) = fixture().await;
    let dev = server.device;
    let (log, video) = segment(7, 64 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "enospc_batch_000001",
        7,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let session = begin(dev, &creds, &manifest).await;
    assert_eq!(put(dev, &session, 0, 0, &log[..16 * KIB]).await.0, 200);
    let folder = temp
        .path()
        .join(".uploads")
        .join(session["id"].as_str().unwrap());
    // The kernel's /dev/full returns a genuine ENOSPC from write(2) without privileges.
    std::os::unix::fs::symlink("/dev/full", folder.join("1")).unwrap();
    let (code, body) = put(dev, &session, 1, 0, &video[..16 * KIB]).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (507, "insufficient_disk_space")
    );
    slots_free(&server).await;
    assert!(
        !folder.exists(),
        "a failed storage write releases the session like Python"
    );
    let (code, body) = status(dev, &session).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (404, "session_not_found")
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 0,
        },
    )
    .await;
    let retry = begin(dev, &creds, &manifest).await;
    assert_ne!(retry["id"], session["id"]);
    upload_all(dev, &retry, &files, 16 * KIB).await;
    assert_eq!(finish(dev, &retry, &files).await.0, 201);
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_limit_rejects_admission_until_space_is_freed() {
    let (temp, server, creds) = fixture().await;
    let dev = server.device;
    let (log, video) = segment(8, 128 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "limit_batch_0000001",
        8,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let set = |max: u64| {
        let body = serde_json::to_vec(&json!({"max_bytes":max,"policy":"reject_new"})).unwrap();
        let mut headers = csrf();
        headers.push(("Content-Type", "application/json".into()));
        async move { http(server.ui, "POST", "/api/settings/storage", &headers, &body).await }
    };
    assert_eq!(set(1).await.0, 200);
    let (code, body) = begin_raw(dev, &creds, &manifest).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (507, "storage_limit_exceeded")
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 0,
        },
    )
    .await;
    assert_eq!(set(0).await.0, 200);
    let session = begin(dev, &creds, &manifest).await;
    upload_all(dev, &session, &files, 64 * KIB).await;
    assert_eq!(finish(dev, &session, &files).await.0, 201);
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_bodies_return_transfer_slots_and_exhaustion_is_reported() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = open(temp.path()).await;
    app.slot_wait = Duration::from_millis(300);
    app.body_idle = Duration::from_secs(600);
    let server = serve(app).await;
    let creds = pair(server.ui, server.device).await;
    let dev = server.device;
    let (log, video) = segment(9, 64 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "stall_batch_0000001",
        9,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let session = begin(dev, &creds, &manifest).await;
    let id = session["id"].as_str().unwrap();
    let mut stalled = Vec::new();
    for n in 0..8 {
        let request = request_bytes(
            "PUT",
            &format!("/api/device/uploads/{id}/files/0?offset=0"),
            &bearer(&session),
            &log[..32 * KIB],
        );
        let head = request.len() - 32 * KIB;
        let mut stream = TcpStream::connect(dev).await.unwrap();
        // Half of the senders stop after the headers, the rest in the middle of the body.
        stream
            .write_all(&request[..head + if n % 2 == 0 { 0 } else { 5 * KIB }])
            .await
            .unwrap();
        stalled.push(stream);
    }
    let app = server.app.clone();
    poll("all slots held", || {
        let app = app.clone();
        async move { app.transfer_slots_available() == 0 }
    })
    .await;
    let (code, body) = put(dev, &session, 1, 0, &video[..KIB]).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (503, "upload_busy")
    );
    drop(stalled);
    slots_free(&server).await;
    upload_all(dev, &session, &files, 32 * KIB).await;
    assert_eq!(finish(dev, &session, &files).await.0, 201);
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;

    // A sender that stays connected but silent is timed out and its slot returned.
    let temp = tempfile::tempdir().unwrap();
    let mut app = open(temp.path()).await;
    app.body_idle = Duration::from_millis(400);
    let server = serve(app).await;
    let creds = pair(server.ui, server.device).await;
    let session = begin(server.device, &creds, &manifest).await;
    let id = session["id"].as_str().unwrap();
    let request = request_bytes(
        "PUT",
        &format!("/api/device/uploads/{id}/files/0?offset=0"),
        &bearer(&session),
        &log[..32 * KIB],
    );
    let mut streams = Vec::new();
    for cut in [request.len() - 32 * KIB, request.len() - 1] {
        let mut stream = TcpStream::connect(server.device).await.unwrap();
        stream.write_all(&request[..cut]).await.unwrap();
        streams.push(stream);
    }
    for mut stream in streams {
        let (code, body) = parse_response(&read_all(&mut stream).await);
        // `upload_busy` is in the RV1 uploader's TRANSIENT set, so the chunk is retried.
        assert_eq!(
            (code, body["error"].as_str().unwrap()),
            (503, "upload_busy")
        );
    }
    slots_free(&server).await;
    assert_eq!(
        status(server.device, &session).await.1["files"][0]["received"],
        0
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 1,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_session_is_replaced_by_a_new_begin() {
    let (temp, server, creds) = fixture().await;
    let (log, video) = segment(10, 32 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "expired_batch_00001",
        10,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let session = begin(server.device, &creds, &manifest).await;
    assert_eq!(put(server.device, &session, 0, 0, &log[..KIB]).await.0, 200);
    let root = temp.path().to_path_buf();
    drop(server);
    let device = root
        .join(".uploads")
        .join(session["id"].as_str().unwrap())
        .join("device.json");
    let mut meta: Value = read_json(&device).await.unwrap();
    meta["expires_at"] = json!(now() - 1.);
    atomic_json(&device, &meta).await.unwrap();
    let server = serve(open(&root).await).await;
    let (code, body) = status(server.device, &session).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (410, "session_expired")
    );
    // Python cleans expired sessions before matching the batch, so the uploader's retry starts over.
    let (code, fresh) = begin_raw(server.device, &creds, &manifest).await;
    assert_eq!(code, 201, "{fresh}");
    assert_ne!(fresh["id"], session["id"]);
    invariants(
        &root,
        server.ui,
        Expect {
            registered: &[],
            sessions: 1,
        },
    )
    .await;
    upload_all(server.device, &fresh, &files, 16 * KIB).await;
    assert_eq!(finish(server.device, &fresh, &files).await.0, 201);
    invariants(
        &root,
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn renewed_revoked_and_removed_device_credentials_fail_closed() {
    let (temp, server, creds) = fixture().await;
    let dev = server.device;
    let (log, video) = segment(11, 32 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "revoke_batch_000001",
        11,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let old = begin(dev, &creds, &manifest).await;
    assert_eq!(put(dev, &old, 0, 0, &log[..KIB]).await.0, 200);
    let renewed = begin(dev, &creds, &manifest).await;
    assert_eq!(renewed["files"][0]["received"], KIB);
    for (code, body) in [
        status(dev, &old).await,
        put(dev, &old, 0, KIB, &log[KIB..2 * KIB]).await,
    ] {
        assert_eq!(
            (code, body["error"].as_str().unwrap()),
            (401, "invalid_upload_token")
        );
    }
    upload_all(dev, &renewed, &files, 16 * KIB).await;
    let (code, result) = finish(dev, &renewed, &files).await;
    assert_eq!(code, 201);
    let pending = begin(
        dev,
        &creds,
        &batch("revoke_batch_000002", 12, &[("rlog.zst", &log[..KIB])]),
    )
    .await;
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 1,
        },
    )
    .await;

    let device = creds["device_id"].as_str().unwrap();
    let path = format!("/api/settings/devices/{device}");
    assert_eq!(
        http(server.ui, "POST", &format!("{path}/revoke"), &csrf(), b"")
            .await
            .0,
        200
    );
    for (code, body) in [
        status(dev, &renewed).await,
        finish(dev, &renewed, &files).await,
        begin_raw(dev, &creds, &manifest).await,
    ] {
        assert_eq!(
            (code, body["error"].as_str().unwrap()),
            (401, "invalid_device")
        );
    }
    // Python removes a revoked device's idle sessions at once, releasing their reservation,
    // so the unfinished session is no longer known at all.
    let (code, body) = put(dev, &pending, 0, 0, &log[..KIB]).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (404, "session_not_found")
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
    assert_eq!(http(server.ui, "DELETE", &path, &csrf(), b"").await.0, 200);
    let root = temp.path().to_path_buf();
    drop(server);
    let server = serve(open(&root).await).await;
    let (code, body) = finish(server.device, &renewed, &files).await;
    assert!(code == 401 || code == 404, "{code} {body}");
    invariants(
        &root,
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
    let fresh = pair(server.ui, server.device).await;
    assert_ne!(fresh["device_id"], creds["device_id"]);
    assert_eq!(
        begin(server.device, &fresh, &manifest).await["state"],
        Value::Null
    );
    let _ = result;
}

#[tokio::test(flavor = "multi_thread")]
async fn nonce_replay_and_clock_window_survive_restart() {
    let (temp, server, creds) = fixture().await;
    let (log, _) = segment(13, KIB);
    let manifest = batch("nonce_batch_0000001", 13, &[("rlog.zst", &log)]);
    let body = serde_json::to_vec(&manifest).unwrap();
    let stamp = now() as u64;
    let replayed = signed(&creds, &body, "fixed_nonce_value_0001", stamp);
    let send = |headers: Vec<(&'static str, String)>, dev| {
        let body = body.clone();
        async move { http(dev, "POST", "/api/device/uploads", &headers, &body).await }
    };
    let (a, b) = tokio::join!(
        send(replayed.clone(), server.device),
        send(replayed.clone(), server.device)
    );
    let mut codes = [a.0, b.0];
    codes.sort();
    assert_eq!(codes, [201, 409]);
    let error = if a.0 == 409 { &a.1 } else { &b.1 };
    assert_eq!(error["error"], "nonce_replayed");
    let root = temp.path().to_path_buf();
    drop(server);
    let server = serve(open(&root).await).await;
    let (code, body_) = send(replayed, server.device).await;
    assert_eq!(
        (code, body_["error"].as_str().unwrap()),
        (409, "nonce_replayed")
    );
    // Real clock: a margin beyond the 120 s window keeps the boundary check deterministic.
    for skew in [-125i64, 125] {
        let stamp = (now() as i64 + skew) as u64;
        let headers = signed(
            &creds,
            &body,
            &format!("skewed_nonce_{}", skew + 1000),
            stamp,
        );
        let (code, body_) = send(headers, server.device).await;
        assert_eq!(
            (code, body_["error"].as_str().unwrap()),
            (401, "stale_timestamp")
        );
    }
    let headers = signed(&creds, &body, "future_nonce_000001", now() as u64 + 100);
    assert_eq!(send(headers, server.device).await.0, 200);
    let mut forged = signed(&creds, &body, "forged_nonce_000001", now() as u64);
    forged[4].1 = sha(b"forged");
    let (code, body_) = send(forged, server.device).await;
    assert_eq!(
        (code, body_["error"].as_str().unwrap()),
        (401, "invalid_signature")
    );
    invariants(
        &root,
        server.ui,
        Expect {
            registered: &[],
            sessions: 1,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_session_directory_changes_do_not_block_startup() {
    let temp = tempfile::tempdir().unwrap();
    let uploads = temp.path().join(".uploads");
    std::fs::create_dir_all(&uploads).unwrap();
    // A crash while an older build created or deleted a session left a folder without a manifest.
    let partial = uploads.join(roadviewer_hars::store::id());
    std::fs::create_dir(&partial).unwrap();
    std::fs::write(
        partial.join("reservation.json"),
        b"{\"bytes\":1,\"expires_at\":0}",
    )
    .unwrap();
    for suffix in ["creating", "removing"] {
        let leftover = uploads.join(format!("{}.{suffix}", roadviewer_hars::store::id()));
        std::fs::create_dir(&leftover).unwrap();
        std::fs::write(leftover.join("manifest.json"), b"[]").unwrap();
    }
    let server = serve(open(temp.path()).await).await;
    assert_eq!(std::fs::read_dir(&uploads).unwrap().count(), 0);
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 0,
        },
    )
    .await;
}

fn legacy_request(name: &str, data: &[u8]) -> Vec<u8> {
    legacy_files(&[(name, data)])
}
fn legacy_files(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = vec![];
    for (name, data) in files {
        body.extend(format!("--XBOUNDARY\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--XBOUNDARY--\r\n");
    let mut headers = csrf();
    headers.push((
        "Content-Type",
        "multipart/form-data; boundary=XBOUNDARY".into(),
    ));
    request_bytes("POST", "/api/upload", &headers, &body)
}
fn files_under(root: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, u64> {
    let mut found = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                found.insert(entry.path(), entry.metadata().unwrap().len());
            }
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_upload_refused_by_admission_writes_nothing() {
    let (temp, server, _) = fixture().await;
    let body = serde_json::to_vec(&json!({"max_bytes":1,"policy":"reject_new"})).unwrap();
    let mut headers = csrf();
    headers.push(("Content-Type", "application/json".into()));
    assert_eq!(
        http(server.ui, "POST", "/api/settings/storage", &headers, &body)
            .await
            .0,
        200
    );
    let before = files_under(temp.path());
    let (log, _) = segment(31, 8 * KIB);
    let mut stream = TcpStream::connect(server.ui).await.unwrap();
    stream
        .write_all(&legacy_request("2026-11-15--12-00-00--31--rlog.zst", &log))
        .await
        .unwrap();
    let (code, body) = parse_response(&read_all(&mut stream).await);
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (507, "storage_limit_exceeded")
    );
    assert_eq!(
        files_under(temp.path()),
        before,
        "a refused upload must not leave a single byte in the data folder"
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 0,
        },
    )
    .await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn legacy_upload_disk_full_is_reported_as_507() {
    let (temp, server, _) = fixture().await;
    let (log, _) = segment(32, 4 * KIB);
    let request = legacy_request("2026-11-15--12-00-00--32--rlog.zst", &log);
    let head = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let mut stream = TcpStream::connect(server.ui).await.unwrap();
    stream.write_all(&request[..head]).await.unwrap();
    let root = temp.path().to_path_buf();
    let stage = std::sync::Arc::new(std::sync::Mutex::new(None));
    let found = stage.clone();
    poll("the multipart staging folder", || {
        let root = root.clone();
        let found = found.clone();
        async move {
            let dir = std::fs::read_dir(&root)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".legacy-")
                });
            let hit = dir.is_some();
            *found.lock().unwrap() = dir;
            hit
        }
    })
    .await;
    let stage = stage.lock().unwrap().clone().unwrap();
    // One small field is a single buffered write: only flush can report its ENOSPC.
    std::os::unix::fs::symlink("/dev/full", stage.join("0")).unwrap();
    stream.write_all(&request[head..]).await.unwrap();
    let (code, body) = parse_response(&read_all(&mut stream).await);
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (507, "insufficient_disk_space")
    );
    assert!(!stage.exists(), "the staging folder is removed");
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 0,
        },
    )
    .await;
    let mut stream = TcpStream::connect(server.ui).await.unwrap();
    stream.write_all(&request).await.unwrap();
    let (code, body) = parse_response(&read_all(&mut stream).await);
    assert_eq!(code, 201, "{body}");
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&[&log]],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_finish_releases_the_session_so_the_uploader_starts_over() {
    let (temp, server, creds) = fixture().await;
    let dev = server.device;
    let (log, video) = segment(33, 64 * KIB);
    let files: [&[u8]; 2] = [&log, &video];
    let manifest = batch(
        "finish_fail_0000001",
        33,
        &[("rlog.zst", &log), ("qcamera.ts", &video)],
    );
    let session = begin(dev, &creds, &manifest).await;
    upload_all(dev, &session, &files, 32 * KIB).await;
    // The uploader's streamed checksum disagrees with the stored bytes.
    let (code, body) = finish(dev, &session, &[&log, &log]).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (422, "checksum_mismatch")
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 0,
        },
    )
    .await;
    // `uploader.py` keeps its checkpoint; a retried job begins the same batch again. Python then
    // has no session for it, so every file is sent again rather than finishing the same bytes.
    let retry = begin(dev, &creds, &manifest).await;
    assert_ne!(retry["id"], session["id"]);
    assert_eq!(retry["files"][0]["received"], 0);
    assert_eq!(retry["files"][1]["received"], 0);
    upload_all(dev, &retry, &files, 32 * KIB).await;
    let (code, result) = finish(dev, &retry, &files).await;
    assert_eq!(code, 201, "{result}");

    // A camera file that is not media goes the same way (Python's ValueError inside `finally`).
    let (other, _) = segment(34, 32 * KIB);
    let bad = b"not an hevc stream".repeat(64);
    let parts: [&[u8]; 2] = [&other, &bad];
    let manifest = batch(
        "finish_fail_0000002",
        34,
        &[("rlog.zst", &other), ("fcamera.hevc", &bad)],
    );
    let session = begin(dev, &creds, &manifest).await;
    upload_all(dev, &session, &parts, 32 * KIB).await;
    let (code, body) = finish(dev, &session, &parts).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (400, "invalid_file")
    );
    assert_eq!(status(dev, &session).await.0, 404);
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[&files],
            sessions: 0,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn finish_refused_before_registration_keeps_the_session() {
    let (temp, server, creds) = fixture().await;
    let dev = server.device;
    let (log, _) = segment(35, 64 * KIB);
    let manifest = batch("finish_keep_0000001", 35, &[("rlog.zst", &log)]);
    let session = begin(dev, &creds, &manifest).await;
    assert_eq!(put(dev, &session, 0, 0, &log[..32 * KIB]).await.0, 200);
    let (code, body) = finish(dev, &session, &[&log]).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (409, "incomplete_upload")
    );
    let mut headers = bearer(&session);
    headers.push(("Content-Type", "application/json".into()));
    let path = format!(
        "/api/device/uploads/{}/finish",
        session["id"].as_str().unwrap()
    );
    let (code, body) = http(dev, "POST", &path, &headers, br#"{"sha256":["x"]}"#).await;
    assert_eq!(
        (code, body["error"].as_str().unwrap()),
        (400, "invalid_file")
    );
    invariants(
        temp.path(),
        server.ui,
        Expect {
            registered: &[],
            sessions: 1,
        },
    )
    .await;
    assert_eq!(
        put(dev, &session, 0, 32 * KIB, &log[32 * KIB..]).await.0,
        200
    );
    assert_eq!(finish(dev, &session, &[&log]).await.0, 201);
}
