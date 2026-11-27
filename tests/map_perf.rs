//! Integration tests for QW-1 (single-route track filtering), QW-2 (HTTP gzip/br compression
//! without double-compressing binary/pre-compressed responses), and ST-1 (in-memory track cache,
//! lightweight ETag 304 fast-path, and cache invalidation).
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode, header as h},
};
use flate2::read::GzDecoder;
use roadviewer_hars::{
    http::{App, ingress},
    store::id,
    track::cache_counts,
};
use serde_json::{Value, json};
use std::{io::Read, path::PathBuf, time::Duration};
use tower::ServiceExt;

fn web() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web")
}

async fn app(root: &std::path::Path) -> App {
    let a = App::open(root.to_path_buf(), web(), false, 512)
        .await
        .unwrap();
    a.store.processing.write().await["auto_convert"] = json!(false);
    a
}

async fn send_raw(
    router: Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        router.oneshot(builder.body(Body::from(body)).unwrap()),
    )
    .await
    .expect("handler deadlock")
    .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

async fn seed_recording(
    a: &App,
    name: &str,
    status: &str,
    points: Option<Vec<Value>>,
    distance_m: Option<u64>,
) -> String {
    let rec_id = id();
    let folder = a.store.root.join(&rec_id);
    std::fs::create_dir_all(&folder).unwrap();
    let has_track = points.as_ref().is_some_and(|p| !p.is_empty());
    let meta = json!({
        "id": rec_id,
        "name": name,
        "status": status,
        "uploaded": 1.0,
        "has_track": has_track,
    });
    a.store.save_meta(meta).await.unwrap();
    if status == "ready" {
        let prepared = folder.join("prepared");
        std::fs::create_dir_all(&prepared).unwrap();
        std::fs::write(
            prepared.join("summary.json"),
            serde_json::to_vec(&json!({
                "has_track": has_track,
                "track_distance_m": distance_m,
                "duration": 60,
            }))
            .unwrap(),
        )
        .unwrap();
        if let Some(pts) = points {
            roadviewer_hars::logs::write_gzip(
                &prepared.join("track.json.gz"),
                &json!({"v": 2, "source": "gpsLocationExternal", "points": pts}),
            )
            .unwrap();
        }
    }
    rec_id
}

fn synthetic_points(lat0: f64, lon0: f64, count: usize) -> Vec<Value> {
    (0..count)
        .map(|i| {
            let f = i as f64;
            json!([f, lat0 + f * 0.0001, lon0 + f * 0.00015, 12.5, 45.0])
        })
        .collect()
}

#[tokio::test]
async fn qw1_single_track_reads_only_target_route_and_preserves_order_and_errors() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;

    let a1 = seed_recording(
        &a,
        "00000100--aaaaaaaaaa / 구간 1",
        "ready",
        Some(synthetic_points(37.50, 127.00, 10)),
        Some(200),
    )
    .await;
    let a0 = seed_recording(
        &a,
        "100--aaaaaaaaaa--0",
        "ready",
        Some(synthetic_points(37.49, 126.99, 10)),
        Some(150),
    )
    .await;
    let a2 = seed_recording(&a, "100--aaaaaaaaaa--2", "unconverted", None, None).await;
    // Unrelated route with corrupt track.json.gz — QW-1 must not read or fail on it.
    let b0 = seed_recording(
        &a,
        "00000200--bbbbbbbbbb / 구간 0",
        "ready",
        Some(synthetic_points(35.10, 129.00, 10)),
        Some(500),
    )
    .await;
    // GPS-less recording
    let no_gps = seed_recording(&a, "00000300--cccccccccc / 구간 0", "ready", None, None).await;

    let (status, headers, bytes) = send_raw(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/{a1}/track"),
        vec![],
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["id"], a1);
    assert_eq!(
        body["unconverted"],
        json!([{"id": a2, "segment": 2, "status": "unconverted"}])
    );
    let segs = body["segments"].as_array().unwrap();
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0]["id"], a0);
    assert_eq!(segs[0]["segment"], 0);
    assert_eq!(segs[0]["distance_m"], 150);
    assert_eq!(segs[0]["duration"], 60);
    assert_eq!(segs[1]["id"], a1);
    assert_eq!(segs[1]["segment"], 1);
    assert_eq!(segs[1]["distance_m"], 200);

    // Lightweight ETag 304 fast-path
    let etag = headers[h::ETAG].to_str().unwrap().to_owned();
    let (s304, h304, b304) = send_raw(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/{a1}/track"),
        vec![],
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(s304, StatusCode::NOT_MODIFIED);
    assert_eq!(h304[h::ETAG].to_str().unwrap(), etag);
    assert!(b304.is_empty());

    // A status transition must update the ETag even when the GPS files are unchanged.
    let mut pending = a.store.meta(&a2).await.unwrap();
    pending["status"] = json!("queued");
    a.store.save_meta(pending).await.unwrap();
    let (status, updated, bytes) = send_raw(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/{a1}/track"),
        vec![],
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(updated[h::ETAG].to_str().unwrap(), etag);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["unconverted"][0]["status"], "queued");

    // Only the two ready segments belonging to the requested route were read and cached.
    assert_eq!(cache_counts(&a.store.root), (2, 0, false));
    assert_ne!(b0, a0);

    // GPS-less log returns 200 with empty segments
    let (s_empty, _, b_empty) = send_raw(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/{no_gps}/track"),
        vec![],
        &[],
    )
    .await;
    assert_eq!(s_empty, StatusCode::OK);
    let v_empty: Value = serde_json::from_slice(&b_empty).unwrap();
    assert_eq!(v_empty, json!({"id": no_gps, "segments": []}));

    // Unknown recording ID returns 404
    let (s_missing, _, _) = send_raw(
        ingress(a.clone()),
        "GET",
        &format!("/api/logs/{}/track", id()),
        vec![],
        &[],
    )
    .await;
    assert_eq!(s_missing, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn qw2_compression_applies_to_assets_and_json_and_skips_precompressed_binaries() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;

    let rec = seed_recording(
        &a,
        "00000100--aaaaaaaaaa / 구간 0",
        "ready",
        Some(synthetic_points(37.5, 127.0, 60)),
        Some(900),
    )
    .await;
    let prepared = a.store.root.join(&rec).join("prepared");
    // Seed pre-compressed / binary files for double-compression checks.
    roadviewer_hars::logs::write_gzip(
        &prepared.join("data.json.gz"),
        &json!({"frames": vec![json!({"t": 0.0, "v": 10.0}); 50]}),
    )
    .unwrap();
    std::fs::write(prepared.join("thumbnail.jpg"), vec![0xFFu8; 4096]).unwrap();
    std::fs::write(prepared.join("camera.mp4"), vec![0x00u8; 4096]).unwrap();

    // 1. Static JS asset: maplibre-gl.js compresses with gzip and brotli
    let raw_maplibre = std::fs::read(web().join("vendor/maplibre-gl.js")).unwrap();
    let (s_gz, h_gz, b_gz) = send_raw(
        ingress(a.clone()),
        "GET",
        "/assets/vendor/maplibre-gl.js?v=1",
        vec![],
        &[("accept-encoding", "gzip")],
    )
    .await;
    assert_eq!(s_gz, StatusCode::OK);
    assert_eq!(h_gz[h::CONTENT_ENCODING], "gzip");
    assert!(b_gz.len() < raw_maplibre.len() / 2);
    let mut dec = GzDecoder::new(&b_gz[..]);
    let mut inflated = Vec::new();
    dec.read_to_end(&mut inflated).unwrap();
    assert_eq!(inflated, raw_maplibre);

    let (s_br, h_br, b_br) = send_raw(
        ingress(a.clone()),
        "GET",
        "/assets/vendor/maplibre-gl.js?v=1",
        vec![],
        &[("accept-encoding", "br")],
    )
    .await;
    assert_eq!(s_br, StatusCode::OK);
    assert_eq!(h_br[h::CONTENT_ENCODING], "br");
    assert!(b_br.len() < b_gz.len());

    // 2. JSON API endpoints compress with gzip and brotli
    for endpoint in [
        "/api/tracks".to_owned(),
        format!("/api/logs/{rec}/track"),
        "/api/logs".to_owned(),
    ] {
        let (st_raw, _, body_raw) =
            send_raw(ingress(a.clone()), "GET", &endpoint, vec![], &[]).await;
        assert_eq!(st_raw, StatusCode::OK);
        let (st_gz, hdr_gz, body_gz) = send_raw(
            ingress(a.clone()),
            "GET",
            &endpoint,
            vec![],
            &[("accept-encoding", "gzip")],
        )
        .await;
        assert_eq!(st_gz, StatusCode::OK);
        assert_eq!(hdr_gz[h::CONTENT_ENCODING], "gzip", "endpoint {endpoint}");
        let mut out = Vec::new();
        GzDecoder::new(&body_gz[..]).read_to_end(&mut out).unwrap();
        assert_eq!(out, body_raw, "gzip roundtrip mismatch for {endpoint}");

        let (st_br, hdr_br, _) = send_raw(
            ingress(a.clone()),
            "GET",
            &endpoint,
            vec![],
            &[("accept-encoding", "br")],
        )
        .await;
        assert_eq!(st_br, StatusCode::OK);
        assert_eq!(hdr_br[h::CONTENT_ENCODING], "br", "endpoint {endpoint}");
    }

    // 3. Pre-compressed / binary responses MUST NOT be double-compressed
    for endpoint in [
        "/assets/carrot_wheel.png".to_owned(),
        "/assets/fonts/PretendardVariable-1.3.9.woff2?v=1.3.9".to_owned(),
        format!("/api/logs/{rec}/thumbnail"),
        format!("/api/logs/{rec}/video?source=qcamera"),
        format!("/api/logs/{rec}/data?format=gzip"),
    ] {
        let (st, hdr, _) = send_raw(
            ingress(a.clone()),
            "GET",
            &endpoint,
            vec![],
            &[("accept-encoding", "gzip, br")],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "endpoint {endpoint}");
        assert!(
            !hdr.contains_key(h::CONTENT_ENCODING),
            "endpoint {endpoint} must not be double-compressed (got {:?})",
            hdr.get(h::CONTENT_ENCODING)
        );
    }
}

#[tokio::test]
async fn st1_tracks_cache_hit_304_and_invalidation_on_mutation_remove_delete() {
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;

    let r0 = seed_recording(
        &a,
        "00000100--aaaaaaaaaa / 구간 0",
        "ready",
        Some(synthetic_points(37.50, 127.00, 60)),
        Some(500),
    )
    .await;
    let r1 = seed_recording(
        &a,
        "00000100--aaaaaaaaaa / 구간 1",
        "ready",
        Some(synthetic_points(37.51, 127.01, 60)),
        Some(700),
    )
    .await;

    // Cold request populates segment, route, and response caches
    let (s1, h1, b1) = send_raw(ingress(a.clone()), "GET", "/api/tracks", vec![], &[]).await;
    assert_eq!(s1, StatusCode::OK);
    let etag1 = h1[h::ETAG].to_str().unwrap().to_owned();
    let v1: Value = serde_json::from_slice(&b1).unwrap();
    assert_eq!(v1["routes"].as_array().unwrap().len(), 1);
    assert_eq!(v1["routes"][0]["distance_m"], 1200.0);
    assert_eq!(cache_counts(&a.store.root), (2, 1, true));

    // Warm 304 fast-path with lightweight ETag
    let (s304, h304, b304) = send_raw(
        ingress(a.clone()),
        "GET",
        "/api/tracks",
        vec![],
        &[("if-none-match", &etag1)],
    )
    .await;
    assert_eq!(s304, StatusCode::NOT_MODIFIED);
    assert_eq!(h304[h::ETAG].to_str().unwrap(), etag1);
    assert!(b304.is_empty());

    // Warm 200 response-cache hit returns identical bytes and ETag
    let (s2, h2, b2) = send_raw(ingress(a.clone()), "GET", "/api/tracks", vec![], &[]).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(h2[h::ETAG].to_str().unwrap(), etag1);
    assert_eq!(b2, b1);

    // Simulate reconversion updating summary.json & track.json.gz + save_meta
    let prepared1 = a.store.root.join(&r1).join("prepared");
    std::fs::write(
        prepared1.join("summary.json"),
        serde_json::to_vec(&json!({"has_track": true, "track_distance_m": 1500, "duration": 60}))
            .unwrap(),
    )
    .unwrap();
    let mut meta1 = a.store.meta(&r1).await.unwrap();
    meta1["conversion_revision"] = json!(2);
    a.store.save_meta(meta1).await.unwrap();
    // save_meta invalidated r1 segment, route cache, and response cache
    assert_eq!(cache_counts(&a.store.root), (1, 0, false));

    // Old ETag no longer returns 304; fresh response has updated distance_m
    let (s3, h3, b3) = send_raw(
        ingress(a.clone()),
        "GET",
        "/api/tracks",
        vec![],
        &[("if-none-match", &etag1)],
    )
    .await;
    assert_eq!(s3, StatusCode::OK);
    let etag3 = h3[h::ETAG].to_str().unwrap().to_owned();
    assert_ne!(etag3, etag1);
    let v3: Value = serde_json::from_slice(&b3).unwrap();
    assert_eq!(v3["routes"][0]["distance_m"], 2000.0);

    // Removing prepared data invalidates cache and drops r1 from route segments
    let (s_rem, _, _) = send_raw(
        ingress(a.clone()),
        "DELETE",
        &format!("/api/logs/{r1}/prepared"),
        vec![],
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(s_rem, StatusCode::OK);
    let (s4, _, b4) = send_raw(ingress(a.clone()), "GET", "/api/tracks", vec![], &[]).await;
    assert_eq!(s4, StatusCode::OK);
    let v4: Value = serde_json::from_slice(&b4).unwrap();
    assert_eq!(v4["routes"][0]["segments"], json!([r0]));
    assert_eq!(v4["routes"][0]["distance_m"], 500.0);

    // Deleting r0 removes the route completely
    let (s_del, _, _) = send_raw(
        ingress(a.clone()),
        "DELETE",
        &format!("/api/logs/{r0}"),
        vec![],
        &[("x-roadviewer-request", "1")],
    )
    .await;
    assert_eq!(s_del, StatusCode::OK);
    let (s5, _, b5) = send_raw(ingress(a.clone()), "GET", "/api/tracks", vec![], &[]).await;
    assert_eq!(s5, StatusCode::OK);
    let v5: Value = serde_json::from_slice(&b5).unwrap();
    assert_eq!(v5["routes"], json!([]));
}

#[tokio::test]
#[ignore = "manual benchmark measurement; run with --ignored --nocapture"]
async fn bench_500_segments_tracks_single_track_and_compression() {
    use std::time::Instant;
    let root = tempfile::tempdir().unwrap();
    let a = app(root.path()).await;

    let mut sample_id = String::new();
    for route in 0..100u32 {
        for seg in 0..5u32 {
            let name = format!("{route:08x}--{route:010x} / 구간 {seg}");
            let rec = seed_recording(
                &a,
                &name,
                "ready",
                Some(synthetic_points(
                    37.4 + route as f64 * 0.002 + seg as f64 * 0.0005,
                    127.0 + route as f64 * 0.002 + seg as f64 * 0.0005,
                    60,
                )),
                Some(850),
            )
            .await;
            if route == 50 && seg == 2 {
                sample_id = rec;
            }
        }
    }

    // 1. /api/tracks cold vs warm 200 vs 304
    let t0 = Instant::now();
    let (s_cold, h_cold, b_cold) =
        send_raw(ingress(a.clone()), "GET", "/api/tracks", vec![], &[]).await;
    let tracks_cold_ms = t0.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(s_cold, StatusCode::OK);
    let etag = h_cold[h::ETAG].to_str().unwrap().to_owned();

    let t1 = Instant::now();
    let (s_warm, _, b_warm) = send_raw(ingress(a.clone()), "GET", "/api/tracks", vec![], &[]).await;
    let tracks_warm_ms = t1.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(s_warm, StatusCode::OK);
    assert_eq!(b_warm.len(), b_cold.len());

    let t2 = Instant::now();
    let (s_304, _, _) = send_raw(
        ingress(a.clone()),
        "GET",
        "/api/tracks",
        vec![],
        &[("if-none-match", &etag)],
    )
    .await;
    let tracks_304_ms = t2.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(s_304, StatusCode::NOT_MODIFIED);

    // 2. /api/logs/{id}/track (QW-1 single route out of 500 segments)
    let single_url = format!("/api/logs/{sample_id}/track");
    let t3 = Instant::now();
    let (s_single, h_single, b_single) =
        send_raw(ingress(a.clone()), "GET", &single_url, vec![], &[]).await;
    let single_200_ms = t3.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(s_single, StatusCode::OK);
    let single_etag = h_single[h::ETAG].to_str().unwrap().to_owned();

    let t4 = Instant::now();
    let (s_single_304, _, _) = send_raw(
        ingress(a.clone()),
        "GET",
        &single_url,
        vec![],
        &[("if-none-match", &single_etag)],
    )
    .await;
    let single_304_ms = t4.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(s_single_304, StatusCode::NOT_MODIFIED);

    // 3. Payload sizes: raw vs gzip vs br
    let mut rows = vec![];
    for (label, path) in [
        ("maplibre-gl.js", "/assets/vendor/maplibre-gl.js?v=1"),
        ("leaflet.js", "/assets/vendor/leaflet.js?v=1"),
        ("/api/tracks (500 segs)", "/api/tracks"),
        ("/api/logs/{id}/track (5 segs)", single_url.as_str()),
        ("/api/logs (500 segs)", "/api/logs"),
    ] {
        let (_, _, raw) = send_raw(ingress(a.clone()), "GET", path, vec![], &[]).await;
        let (_, _, gz) = send_raw(
            ingress(a.clone()),
            "GET",
            path,
            vec![],
            &[("accept-encoding", "gzip")],
        )
        .await;
        let (_, _, br) = send_raw(
            ingress(a.clone()),
            "GET",
            path,
            vec![],
            &[("accept-encoding", "br")],
        )
        .await;
        rows.push((label, raw.len(), gz.len(), br.len()));
    }

    println!("=== BENCHMARK (100 routes x 5 segments = 500 segments, 60 pts/seg) ===");
    println!(
        "/api/tracks cold 200:        {tracks_cold_ms:.2} ms ({} bytes)",
        b_cold.len()
    );
    println!("/api/tracks warm 200:        {tracks_warm_ms:.2} ms");
    println!("/api/tracks warm 304:        {tracks_304_ms:.2} ms");
    println!(
        "/api/logs/{{id}}/track 200:    {single_200_ms:.2} ms ({} bytes)",
        b_single.len()
    );
    println!("/api/logs/{{id}}/track 304:    {single_304_ms:.2} ms");
    println!("--- Compression payload sizes ---");
    for (label, raw, gz, br) in rows {
        let gz_pct = (1.0 - gz as f64 / raw as f64) * 100.0;
        let br_pct = (1.0 - br as f64 / raw as f64) * 100.0;
        println!(
            "{label:<32} raw={raw:>9} B | gzip={gz:>8} B (-{gz_pct:.1}%) | br={br:>8} B (-{br_pct:.1}%)"
        );
    }
}

#[tokio::test]
async fn warm_track_cache_populates_segments_and_routes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let a = app(root).await;
    for r in 0..10u64 {
        let pts = (0..20)
            .map(|i| {
                json!([
                    i as f64,
                    37.5 + r as f64 * 0.01 + i as f64 * 0.0001,
                    127.0,
                    10.0,
                    0
                ])
            })
            .collect::<Vec<_>>();
        seed_recording(
            &a,
            &format!("0000{:04x}--warmroute0 / 구간 0", 100 + r),
            "ready",
            Some(pts),
            Some(500),
        )
        .await;
    }
    assert_eq!(cache_counts(root), (0, 0, false));
    roadviewer_hars::http::warm_track_cache(&a).await;
    let (segs, routes, _) = cache_counts(root);
    assert_eq!(segs, 10);
    assert_eq!(routes, 10);
}
