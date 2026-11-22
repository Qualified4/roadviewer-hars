//! The running app renders its own page version and serves the web assets byte-for-byte.
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use roadviewer_hars::{
    http::{App, ingress},
    store::id,
};
use serde_json::json;
use std::path::PathBuf;
use tower::ServiceExt;

const HARS: &str = env!("CARGO_PKG_VERSION");
const FONT: &str = "fonts/PretendardVariable-1.3.9.woff2";

fn web() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web")
}
async fn get(app: &App, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let response = ingress(app.clone())
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let cache = response
        .headers()
        .get("cache-control")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (status, cache, body.to_vec())
}
/// App asset versions. The Pretendard preload uses its own release number.
fn asset_versions(html: &str) -> Vec<&str> {
    html.match_indices("?v=")
        .filter(|(at, _)| !html[..*at].ends_with(FONT))
        .map(|(at, _)| {
            let rest = &html[at + 3..];
            &rest[..rest.find(['"', '&', '\'']).unwrap()]
        })
        .collect()
}

#[tokio::test]
async fn pages_name_the_hars_version_only() {
    let root = tempfile::tempdir().unwrap();
    let app = App::open(root.path().to_path_buf(), web(), false, 512)
        .await
        .unwrap();
    let record = id();
    std::fs::create_dir_all(app.store.root.join(&record)).unwrap();
    app.store
        .save_meta(json!({"id":record,"name":"00000001--0123456789 / 구간 0","uploaded":1700000000.0,"status":"unconverted","files":{},"error":null}))
        .await
        .unwrap();
    for (uri, assets, fragment) in [
        ("/".to_string(), "assets/", false),
        ("/?fragment=1".to_string(), "assets/", true),
        (format!("/view/{record}/"), "../../assets/", false),
        (format!("/view/{record}/?fragment=1"), "../../assets/", true),
    ] {
        let (status, cache, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(cache, "no-store", "{uri}");
        let html = String::from_utf8(body).unwrap();
        assert!(
            !html.contains("__VERSION__") && !html.contains("__ASSETS__"),
            "{uri}"
        );
        let versions = asset_versions(&html);
        assert!(versions.len() >= 5, "{uri}: {versions:?}");
        assert!(versions.iter().all(|v| *v == HARS), "{uri}: {versions:?}");
        assert!(
            html.contains(&format!("\"{assets}style.css?v={HARS}\"")),
            "{uri}"
        );
        // The pages name the product "Road Viewer", with no HARS suffix anywhere.
        assert!(!html.contains("HARS"), "{uri}: no page names HARS");
        assert!(
            html.contains("<title>Road Viewer · Home Assistant</title>"),
            "{uri}: the product title"
        );
        if uri == "/?fragment=1" {
            assert!(
                html.contains("<h1>Road Viewer</h1>"),
                "{uri}: the product wordmark"
            );
        }
        if fragment {
            assert!(
                html.contains(&format!(
                    ">Road Viewer <span class=\"app-version\">v{HARS}</span>"
                )),
                "{uri}: the version label uses the product name"
            );
        } else {
            assert!(
                html.contains(&format!("{assets}{FONT}?v=1.3.9\"")),
                "{uri}: the font preload keeps the font release"
            );
        }
    }
}

#[tokio::test]
async fn assets_are_served_unmodified_and_versioned_ones_are_cached() {
    let root = tempfile::tempdir().unwrap();
    let app = App::open(root.path().to_path_buf(), web(), false, 512)
        .await
        .unwrap();
    for name in ["maps.js", "style.css", "app.js"] {
        let file = std::fs::read(web().join(name)).unwrap();
        let (status, cache, body) = get(&app, &format!("/assets/{name}?v={HARS}")).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert_eq!(cache, "public, max-age=31536000", "{name}");
        assert_eq!(body, file, "{name} is served byte-for-byte");
        let (status, cache, _) = get(&app, &format!("/assets/{name}")).await;
        assert_eq!(
            (status, cache.as_str()),
            (StatusCode::OK, "no-cache"),
            "{name}"
        );
    }
    let (status, cache, body) = get(&app, &format!("/assets/page-replay.js?v={HARS}")).await;
    assert_eq!(
        (status, cache.as_str()),
        (StatusCode::OK, "public, max-age=31536000")
    );
    let script = String::from_utf8(body).unwrap();
    let maps = std::fs::read_to_string(web().join("maps.js")).unwrap();
    assert!(
        script.contains(&maps),
        "the replay module carries maps.js byte-for-byte"
    );
}
