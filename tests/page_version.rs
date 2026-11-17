//! The preserved Python pages are served under the HARS version, whatever Python release they
//! came from, and the web assets are served unmodified.
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

fn reference() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("reference/python/roadviewer")
}
fn web() -> PathBuf {
    reference().join("app/web")
}
/// The preserved Python release, from its app config (not from the pages under test).
fn python_version() -> String {
    let config = std::fs::read_to_string(reference().join("config.yaml.reference")).unwrap();
    let line = config
        .lines()
        .find_map(|l| l.strip_prefix("version:"))
        .expect("config.yaml.reference names a version");
    line.trim().trim_matches('"').to_string()
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
/// Every `?v=` value in an HTML page.
fn asset_versions(html: &str) -> Vec<&str> {
    html.match_indices("?v=")
        .map(|(at, _)| {
            let rest = &html[at + 3..];
            &rest[..rest.find(['"', '&', '\'']).unwrap()]
        })
        .collect()
}

#[test]
fn preserved_pages_carry_the_python_release_that_is_replaced() {
    let python = python_version();
    assert_ne!(python, HARS);
    let library = std::fs::read_to_string(web().join("library.html")).unwrap();
    assert!(
        library.contains(&format!("assets/style.css?v={python}\"")),
        "library.html no longer names the Python release where HARS reads it"
    );
    for page in ["library.html", "index.html", "shell.html"] {
        let html = std::fs::read_to_string(web().join(page)).unwrap();
        assert!(html.contains(&format!("?v={python}")), "{page}");
    }
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
    let python = python_version();
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
        assert!(!html.contains(&python), "{uri} still names Python {python}");
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
        // The pages name the product as preserved, "Road Viewer", with no HARS suffix anywhere.
        assert!(!html.contains("HARS"), "{uri}: no page names HARS");
        assert!(
            html.contains("<title>Road Viewer · Home Assistant</title>"),
            "{uri}: the title is the preserved one"
        );
        if uri == "/?fragment=1" {
            assert!(
                html.contains("<h1>Road Viewer</h1>"),
                "{uri}: the wordmark keeps the preserved letters"
            );
        }
        if fragment {
            assert!(
                html.contains(&format!(
                    ">Road Viewer <span class=\"app-version\">v{HARS}</span>"
                )),
                "{uri}: the version label keeps the preserved name"
            );
        }
        if fragment {
            assert!(
                html.contains(&format!("<span class=\"app-version\">v{HARS}</span>")),
                "{uri}"
            );
        } else {
            assert!(
                html.contains(&format!(
                    "{assets}fonts/PretendardVariable-1.3.9.woff2?v={HARS}\""
                )),
                "{uri}: the font file name is kept"
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
        assert_eq!(body, file, "{name} is served as preserved");
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
        "the replay module carries maps.js as preserved"
    );
}
