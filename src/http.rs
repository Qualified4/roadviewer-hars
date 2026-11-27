use crate::{
    auth::{Auth, header},
    error::{ApiError, Result, bad, missing},
    http_archive, http_storage,
    media_read::Source,
    prepared::{self as prepared_storage, NasLocation},
    store::{Store, atomic_json, id, read_json, sha},
    upload::Uploads,
};
use axum::{
    Json, Router,
    body::{Body, Bytes, HttpBody},
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode, Uri, header as h},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tower::ServiceExt;
use tower_http::{
    compression::{CompressionLayer, Predicate},
    services::ServeFile,
};

#[derive(Clone, Copy)]
struct Compressible;

impl Predicate for Compressible {
    fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
    where
        B: axum::body::HttpBody,
    {
        if response.headers().contains_key(h::CONTENT_RANGE) {
            return false;
        }
        if response
            .body()
            .size_hint()
            .upper()
            .is_some_and(|upper| upper < 256)
        {
            return false;
        }
        let Some(ct) = response
            .headers()
            .get(h::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        let mime = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        matches!(
            mime.as_str(),
            "application/json"
                | "application/javascript"
                | "text/javascript"
                | "text/html"
                | "text/css"
                | "text/plain"
                | "image/svg+xml"
        )
    }
}

const BODY_LIMIT: usize = 8 * 1024 * 1024 + 1;

#[derive(Clone)]
pub struct App {
    pub store: Arc<Store>,
    pub auth: Arc<Auth>,
    pub uploads: Arc<Uploads>,
    pub web: PathBuf,
    pub ingress_only: bool,
    pub used: Arc<AtomicU64>,
    pub device_https_enabled: bool,
    pub jobs: Arc<crate::jobs::Jobs>,
    pub archive: Arc<crate::archive::Archive>,
    pub network: Arc<crate::network::Network>,
    pub legacy: Arc<crate::legacy_archive::Legacy>,
    transfer_slots: Arc<tokio::sync::Semaphore>,
    pub slot_wait: std::time::Duration,
    pub body_idle: std::time::Duration,
    pub body_total: std::time::Duration,
}
impl App {
    pub async fn open(
        root: PathBuf,
        web: PathBuf,
        ingress_only: bool,
        max_upload_mb: u64,
    ) -> anyhow::Result<Self> {
        let store = Store::open(root).await?;
        crate::storage::open(&store).await?;
        crate::transfer::cleanup_staging(&store.root)?;
        crate::media_read::cleanup_staging(&store.root)?;
        crate::legacy_archive::cleanup_staging(&store.root)?;
        let legacy = Arc::new(crate::legacy_archive::Legacy::open(&store.root).await?);
        let auth = Arc::new(Auth::open(&store.root).await?);
        let uploads =
            Uploads::open(store.clone(), auth.clone(), max_upload_mb * 1024 * 1024).await?;
        uploads
            .recover_commits()
            .await
            .map_err(|_| anyhow::anyhow!("upload commit recovery failed; data preserved"))?;
        let used = Arc::new(AtomicU64::new(store.used().await?));
        let jobs = crate::jobs::Jobs::open(store.clone(), uploads.clone()).await?;
        let archive = crate::archive::Archive::open(
            crate::archive::Host {
                store: store.clone(),
                uploads: uploads.clone(),
                jobs: jobs.clone(),
            },
            crate::archive::settings::options(),
            crate::archive::settings::allow_any_path(),
            None,
        );
        Ok(Self {
            store,
            auth,
            uploads,
            web,
            ingress_only,
            used,
            device_https_enabled: false,
            jobs,
            archive,
            network: Arc::new(crate::network::Network::from_env()),
            legacy,
            transfer_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            slot_wait: std::time::Duration::from_secs(30),
            body_idle: std::time::Duration::from_secs(30),
            body_total: std::time::Duration::from_secs(120),
        })
    }
    pub fn transfer_slots_available(&self) -> usize {
        self.transfer_slots.available_permits()
    }
    pub fn background(&self) -> tokio::task::JoinHandle<()> {
        let app = self.clone();
        app.archive.start();
        let warm_app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            warm_track_cache(&warm_app).await;
        });
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            let mut tick = 0u64;
            loop {
                interval.tick().await;
                if app.jobs.tick().await.is_err() {
                    tracing::warn!("conversion_scheduler_failed");
                }
                tick += 1;
                if tick % 10 != 1 {
                    continue;
                }
                if let Ok(n) = app.store.used().await {
                    app.used.store(n, Ordering::Relaxed);
                }
                if app.uploads.cleanup().await.is_err() {
                    tracing::warn!("upload_cleanup_failed");
                }
                if app.uploads.recover_commits().await.is_err() {
                    tracing::warn!("upload_commit_recovery_failed");
                }
                if crate::storage::pending(&app.store.root)
                    .await
                    .unwrap_or(true)
                {
                    let registration = app.store.registration.lock().await;
                    if crate::storage::recover(&app.store, &registration)
                        .await
                        .is_err()
                    {
                        tracing::warn!("delete_recovery_failed");
                    }
                }
            }
        })
    }
}

pub fn ingress(app: App) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/view/{id}/", get(replay_page))
        .route("/assets/{*name}", get(asset))
        .route("/api/logs", get(list))
        .route("/api/logs/download.zip", get(crate::transfer::download_zip))
        .route("/api/logs/export.tar", get(crate::transfer::export))
        .route("/api/logs/import", post(crate::transfer::begin))
        .route(
            "/api/logs/import/{id}/finish",
            post(crate::transfer::finish),
        )
        .route(
            "/api/upload",
            post(crate::transfer::legacy_upload)
                .layer(DefaultBodyLimit::max(app.uploads.max_log as usize)),
        )
        .route("/api/uploads/{id}/failure", post(crate::transfer::failure))
        .route(
            "/api/settings/device-network",
            get(crate::network::settings).post(crate::network::settings),
        )
        .route(
            "/api/settings/device-network/restart",
            post(crate::network::restart),
        )
        .route("/api/tracks", get(tracks))
        .route("/api/progress", get(progress))
        .route("/api/settings/processing", get(processing).post(processing))
        .route(
            "/api/settings/storage",
            get(http_storage::settings).post(http_storage::settings),
        )
        .route("/api/storage/cleanup", post(http_storage::cleanup))
        .route(
            "/api/settings/prepared",
            get(http_storage::prepared_settings).post(http_storage::prepared_settings),
        )
        .route(
            "/api/settings/prepared/move",
            get(http_storage::prepared_move).post(http_storage::prepared_move),
        )
        .route(
            "/api/settings/archive",
            get(http_archive::preferences_api).post(http_archive::preferences_api),
        )
        .route(
            "/api/settings/archive/run",
            get(http_archive::run).post(http_archive::run),
        )
        .route(
            "/api/settings/archive/gdrive/connect",
            post(http_archive::gdrive_connect),
        )
        .route(
            "/api/settings/archive/gdrive/disconnect",
            post(http_archive::gdrive_disconnect),
        )
        .route("/api/logs/{id}/archive/retry", post(http_archive::retry))
        .route(
            "/api/settings/archive/legacy/scan",
            post(crate::legacy_archive::scan),
        )
        .route(
            "/api/settings/archive/legacy/import",
            post(crate::legacy_archive::import),
        )
        .route(
            "/api/settings/archive/legacy/status",
            get(crate::legacy_archive::status),
        )
        .route(
            "/api/settings/archive/legacy/cancel",
            post(crate::legacy_archive::cancel),
        )
        .route("/api/settings/devices", get(devices))
        .route(
            "/api/settings/devices/pairing",
            get(pairing).post(pairing).delete(pairing),
        )
        .route("/api/settings/devices/{id}/revoke", post(revoke))
        .route("/api/settings/devices/{id}", delete(remove_device))
        .route("/api/uploads", post(begin_browser))
        .route("/api/uploads/{id}/files/{index}", put(browser_chunk))
        .route("/api/uploads/{id}/finish", post(browser_finish))
        .route("/api/uploads/{id}", delete(browser_cancel))
        .route(
            "/api/logs/{id}/bookmarks",
            get(crate::annotations::bookmarks).post(crate::annotations::bookmarks),
        )
        .route(
            "/api/logs/{id}/bookmarks/{bookmark}",
            put(crate::annotations::bookmark_item).delete(crate::annotations::bookmark_item),
        )
        .route("/api/logs/{id}/tags", post(crate::annotations::tags))
        .route("/api/logs/{id}", delete(http_storage::delete_log))
        .route("/api/logs/{id}/pin", post(http_storage::pin))
        .route("/api/logs/{id}/data", get(data))
        .route("/api/logs/{id}/convert", post(convert))
        .route("/api/logs/{id}/prepared", delete(remove_prepared))
        .route("/api/logs/{id}/cancel", post(cancel_conversion))
        .route("/api/logs/{id}/telemetry", get(telemetry))
        .route("/api/logs/{id}/track", get(track))
        .route("/api/logs/{id}/video", get(video))
        .route("/api/logs/{id}/playback", post(playback))
        .route("/api/logs/{id}/thumbnail", get(thumbnail))
        .route("/api/logs/{id}/original/{kind}", get(original))
        .route("/api/logs/{id}/download/video", get(download_video))
        .route("/api/logs/{id}/download", get(download_camera))
        .route("/health", get(health))
        .fallback(crate::parity::fallback)
        .method_not_allowed_fallback(crate::parity::method_not_allowed)
        .layer(CompressionLayer::new().compress_when(Compressible))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::map_response(|r: Response| async move {
            crate::parity::json_too_large(false, r)
        }))
        .layer(middleware::from_fn_with_state(
            app.clone(),
            transfer_backpressure,
        ))
        .layer(middleware::from_fn_with_state(app.clone(), ingress_access))
        .layer(middleware::from_fn(diagnostics))
        .with_state(app)
}
pub fn device(app: App) -> Router {
    Router::new()
        .route("/api/device/test", get(device_test))
        .route("/api/device/pair", post(pair))
        .route("/api/device/uploads", post(begin_device))
        .route(
            "/api/device/uploads/{id}",
            get(device_status).delete(device_cancel),
        )
        .route("/api/device/uploads/{id}/files/{index}", put(device_chunk))
        .route("/api/device/uploads/{id}/finish", post(device_finish))
        .fallback(|| async { missing("not_found") })
        .method_not_allowed_fallback(crate::parity::method_not_allowed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::map_response(|r: Response| async move {
            crate::parity::json_too_large(true, r)
        }))
        .layer(middleware::from_fn_with_state(
            app.clone(),
            transfer_backpressure,
        ))
        .layer(middleware::from_fn(device_access))
        .layer(middleware::from_fn(diagnostics))
        .with_state(app)
}
async fn transfer_backpressure(State(app): State<App>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if request.method() == Method::PUT && path.contains("/uploads/") && path.contains("/files/") {
        if path.starts_with("/api/device/") {
            let name = path.split('/').nth(4).unwrap_or("");
            match app.uploads.authorize(name, request.headers()).await {
                Ok(meta) if meta.get("result").is_some() => {
                    return crate::error::conflict("session_completed").into_response();
                }
                Err(e) => return e.into_response(),
                _ => {}
            }
        }
        let Ok(Ok(_permit)) =
            tokio::time::timeout(app.slot_wait, app.transfer_slots.acquire()).await
        else {
            return ApiError(StatusCode::SERVICE_UNAVAILABLE, "upload_busy").into_response();
        };
        let (parts, body) = request.into_parts();
        let limits = crate::body_limit::Limits {
            idle: app.body_idle,
            total: app.body_total,
            max: BODY_LIMIT,
        };
        return match crate::body_limit::receive(body, &limits).await {
            Ok(bytes) => {
                next.run(Request::from_parts(parts, Body::from(bytes)))
                    .await
            }
            Err(e) => {
                let mut response = e.into_response();
                response
                    .headers_mut()
                    .insert(h::CONNECTION, "close".parse().unwrap());
                response
            }
        };
    }
    next.run(request).await
}
async fn ingress_access(State(app): State<App>, request: Request, next: Next) -> Response {
    if request.uri().path().starts_with("/api/device/") {
        return missing("not_found").into_response();
    }
    if app.ingress_only
        && request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_none_or(|p| p.0.ip() != std::net::Ipv4Addr::new(172, 30, 32, 2))
    {
        return ApiError(StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    if matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::DELETE
    ) && header(request.headers(), "x-roadviewer-request") != "1"
    {
        return ApiError(StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    next.run(request).await
}
async fn device_access(request: Request, next: Next) -> Response {
    if request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|p| !p.0.ip().is_loopback())
    {
        return missing("not_found").into_response();
    }
    next.run(request).await
}
async fn diagnostics(mut request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| crate::store::valid_id(v))
        .map(str::to_owned)
        .unwrap_or_else(id);
    let time = std::time::Instant::now();
    let path = request.uri().path().to_owned();
    tracing::info!(request_id=%request_id,method=%request.method(),path=%request.uri().path(),"request_enter");
    let span = tracing::info_span!("http_request",request_id=%request_id,method=%request.method(),path=%request.uri().path());
    request.extensions_mut().insert(request_id.clone());
    use tracing::Instrument;
    let mut response = next.run(request).instrument(span).await;
    response
        .headers_mut()
        .insert("x-request-id", request_id.parse().unwrap());
    if path == "/"
        || path.starts_with("/view/")
        || path.starts_with("/api/settings/")
        || path.starts_with("/api/device/")
        || matches!(path.as_str(), "/api/logs" | "/api/progress" | "/health")
        || response.status().is_client_error()
        || response.status().is_server_error()
    {
        response
            .headers_mut()
            .insert(h::CACHE_CONTROL, "no-store".parse().unwrap());
    } else if path == "/api/tracks" || (path.starts_with("/api/logs/") && path.ends_with("/track"))
    {
        response
            .headers_mut()
            .insert(h::CACHE_CONTROL, "private, no-cache".parse().unwrap());
    } else if path.starts_with("/api/logs/")
        && (path.ends_with("/data") || path.ends_with("/telemetry"))
    {
        response
            .headers_mut()
            .insert(h::CACHE_CONTROL, "no-cache".parse().unwrap());
    }
    tracing::info!(request_id=%request_id,status=response.status().as_u16(),response_headers_us=time.elapsed().as_micros() as u64,"response_headers_ready");
    response
}
async fn health() -> Json<Value> {
    Json(json!({"product":"Road Viewer","version":env!("CARGO_PKG_VERSION"),"release_ready":true}))
}
async fn page(
    State(app): State<App>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Result<Response> {
    page_body(&app, false, q.get("fragment").is_some_and(|v| v == "1")).await
}
async fn replay_page(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Result<Response> {
    app.store.folder(&id).await?;
    page_body(&app, true, q.get("fragment").is_some_and(|v| v == "1")).await
}
async fn page_body(app: &App, replay: bool, fragment: bool) -> Result<Response> {
    let name = if fragment {
        if replay { "index.html" } else { "library.html" }
    } else {
        "shell.html"
    };
    let template = tokio::fs::read_to_string(app.web.join(name)).await?;
    let library = if name == "library.html" {
        template.clone()
    } else {
        tokio::fs::read_to_string(app.web.join("library.html")).await?
    };
    let upstream = upstream_version(&library);
    if upstream.is_none() {
        tracing::warn!(
            "library.html names no assets/style.css?v= release; Python version strings are served as is"
        );
    }
    let html = page_html(&template, upstream, replay);
    Ok(([(h::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response())
}
/// Python's own release, read where Python reads it (`server.page_response`): the `?v=` of
/// `assets/style.css` in `library.html`.
fn upstream_version(library: &str) -> Option<&str> {
    const MARK: &str = "assets/style.css?v=";
    let rest = &library[library.find(MARK)? + MARK.len()..];
    Some(&rest[..rest.find(['"', '&'])?]).filter(|v| !v.is_empty())
}
/// The preserved pages stamp Python's release into asset URLs (`?v=`) and the version label
/// (`>v…<`). HARS serves them under its own version so a HARS release, not a Python one,
/// renews the year-long asset cache. Other version-like text (the font file name) is kept.
/// The product name on the pages stays the preserved "Road Viewer": this app is the Road Viewer
/// that replaces the Python one, and the wordmark is sized for those letters.
fn page_html(template: &str, upstream: Option<&str>, replay: bool) -> String {
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    let mut html = template
        .replace(
            "__ASSETS__",
            if replay { "../../assets/" } else { "assets/" },
        )
        .replace("__VERSION__", VERSION);
    if let Some(upstream) = upstream {
        html = html
            .replace(&format!("?v={upstream}"), &format!("?v={VERSION}"))
            .replace(&format!(">v{upstream}<"), &format!(">v{VERSION}<"));
    }
    html
}
async fn asset(
    State(app): State<App>,
    Path(name): Path<String>,
    request: Request,
) -> Result<Response> {
    let cache = if request
        .uri()
        .query()
        .is_some_and(|q| q.split('&').any(|p| p.starts_with("v=")))
    {
        "public, max-age=31536000"
    } else {
        "no-cache"
    };
    if name == "page-library.js" || name == "page-replay.js" {
        let library = name == "page-library.js";
        let scripts: &[&str] = if library {
            &[
                "disclosure",
                "recording-name",
                "download-dialog",
                "tags",
                "library",
                "choices",
                "bulk",
                "storage-devices",
                "log-transfer",
                "maps",
            ]
        } else {
            &[
                "recording-name",
                "download-dialog",
                "tags",
                "app",
                "choices",
                "video-overlay",
                "camera-info",
                "telemetry",
                "bookmarks",
                "maps",
            ]
        };
        let mut source = String::from(
            "export function mount(env) {\nconst {window,document,history,location,fetch,setTimeout,clearTimeout,setInterval,clearInterval,requestAnimationFrame,cancelAnimationFrame,requestIdleCallback,ResizeObserver,MutationObserver,XMLHttpRequest,matchMedia}=env;\nconst addEventListener=env.addEventListener||globalThis.addEventListener.bind(globalThis),removeEventListener=env.removeEventListener||globalThis.removeEventListener.bind(globalThis),IntersectionObserver=env.IntersectionObserver||globalThis.IntersectionObserver,onPageDispose=env.onPageDispose||(()=>{}),onPageSuspend=env.onPageSuspend||(()=>{}),onPageResume=env.onPageResume||(()=>{});\n",
        );
        for script in scripts {
            source
                .push_str(&tokio::fs::read_to_string(app.web.join(format!("{script}.js"))).await?);
            source.push_str("\n;\n");
        }
        source.push_str(if library {
            "return {ready:pageReady,canLeave:()=>!busy&&!cleaning&&!networkRestarting};\n}"
        } else {
            "return {ready:pageReady,canLeave:()=>true};\n}"
        });
        return Ok((
            [
                (h::CONTENT_TYPE, "application/javascript"),
                (h::CACHE_CONTROL, cache),
            ],
            source,
        )
            .into_response());
    }
    let path = app.web.join(&name);
    safe_file(&app.web, &path).await?;
    let mut response = serve(path, request).await?;
    response
        .headers_mut()
        .insert(h::CACHE_CONTROL, cache.parse().unwrap());
    Ok(response)
}
async fn safe_file(root: &std::path::Path, path: &std::path::Path) -> Result<()> {
    let real = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| missing("not_found"))?;
    let root = tokio::fs::canonicalize(root).await?;
    if !real.starts_with(root) || !tokio::fs::metadata(real).await?.is_file() {
        return Err(missing("not_found"));
    }
    Ok(())
}
async fn serve(path: PathBuf, request: Request) -> Result<Response> {
    let response = ServeFile::new(path).oneshot(request).await.unwrap();
    Ok(response.map(Body::new))
}
async fn list(State(app): State<App>, headers: HeaderMap) -> Result<Response> {
    let (_, uploads) = app.uploads.snapshot().await;
    let pins = crate::storage::pins(&crate::storage::load_settings(&app.store.root).await?);
    let settings = app.store.processing.read().await.clone();
    let progress = app.jobs.progress.read().await.clone();
    let metas = app
        .store
        .records
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let share = if metas.iter().any(http_archive::touched) {
        Some(http_archive::list_state(&app.archive).await)
    } else {
        None
    };
    let root = app.store.root.clone();
    let archive = app.archive.clone();
    let mut logs = tokio::task::spawn_blocking(move || {
        let ids = metas
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        crate::listing::forget_missing(&root, &ids);
        metas
            .iter()
            .map(|m| {
                let mut row = crate::listing::row(&root.join(m["id"].as_str().unwrap_or("")), m);
                let touched = share.filter(|_| http_archive::touched(m));
                row.extend(http_archive::list_fields(&archive, m, touched));
                row
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|_| bad("invalid_list"))?;
    for o in &mut logs {
        let pinned = o["id"].as_str().is_some_and(|id| pins.contains(id));
        o.insert("pinned".into(), json!(pinned));
        let current = progress
            .get(o["id"].as_str().unwrap_or(""))
            .cloned()
            .unwrap_or(Value::Null);
        o.insert("progress".into(), current);
    }
    crate::listing::route_starts(&mut logs, crate::store::now());
    crate::listing::sort(&mut logs);
    let value = json!({"logs":logs,"uploads":uploads,"max_upload_mb":app.uploads.max_log/1024/1024,"storage_used_bytes":app.used.load(Ordering::Relaxed),"concurrency":settings["concurrency"],"auto_convert":settings["auto_convert"],"keep_original_video":settings["keep_original_video"]});
    let bytes = serde_json::to_vec(&value).unwrap();
    let etag = format!("\"{}\"", &sha(&bytes)[..32]);
    if header(&headers, "if-none-match") == etag {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    Ok((
        [
            (h::ETAG, etag),
            (h::CONTENT_TYPE, "application/json".to_owned()),
        ],
        bytes,
    )
        .into_response())
}
async fn progress(State(app): State<App>) -> Json<Value> {
    Json(json!({"progress":app.jobs.progress.read().await.clone()}))
}
async fn devices(State(app): State<App>) -> Json<Value> {
    let mut value = app.auth.manage().await;
    value["enabled"] = json!(app.device_https_enabled);
    value["host_port"] = json!(app.network.active_port);
    value["configuration_error"] = json!(app.network.port_error);
    Json(value)
}
async fn pairing(State(app): State<App>, method: Method) -> Json<Value> {
    Json(app.auth.pairing(method.as_str()).await)
}
async fn pair(State(app): State<App>, body: Bytes) -> Result<(StatusCode, Json<Value>)> {
    if body.len() > 4096 {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"));
    }
    let body = serde_json::from_slice(&body).map_err(|_| bad("invalid_pairing_request"))?;
    Ok((StatusCode::CREATED, Json(app.auth.pair(body).await?)))
}
async fn revoke(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>> {
    let result = app.auth.device_action(&id, false).await?;
    // Python drops the device's idle sessions at revocation, releasing their reservations.
    if app.uploads.cleanup().await.is_err() {
        tracing::warn!("upload_cleanup_failed");
    }
    Ok(Json(result))
}
async fn remove_device(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(app.auth.device_action(&id, true).await?))
}
async fn begin_device(
    State(app): State<App>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>)> {
    let (s, v) = app
        .uploads
        .begin_device(&headers, &body, uri.query().is_some())
        .await?;
    Ok((s, Json(v)))
}
async fn begin_browser(State(app): State<App>, body: Bytes) -> Result<(StatusCode, Json<Value>)> {
    let body = serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| bad("invalid_upload"))?;
    Ok((
        StatusCode::CREATED,
        Json(app.uploads.begin_browser(body).await?),
    ))
}
async fn device_status(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(app.uploads.status(&id, &headers).await?))
}
async fn device_chunk(
    State(app): State<App>,
    Path((id, index)): Path<(String, usize)>,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    Ok(Json(
        app.uploads
            .chunk(
                &id,
                index,
                q.get("offset").and_then(|v| v.parse().ok()),
                &body,
                true,
                &headers,
            )
            .await?,
    ))
}
async fn browser_chunk(
    State(app): State<App>,
    Path((id, index)): Path<(String, usize)>,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    Ok(Json(
        app.uploads
            .chunk(
                &id,
                index,
                q.get("offset").and_then(|v| v.parse().ok()),
                &body,
                false,
                &headers,
            )
            .await?,
    ))
}
async fn device_finish(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>)> {
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).map_err(|_| bad("invalid_file"))?
    };
    let (s, v) = app.uploads.finish(&id, true, &headers, body).await?;
    Ok((s, Json(v)))
}
async fn browser_finish(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> crate::parity::Answer<(StatusCode, Json<Value>)> {
    if crate::transfer::is_import(&app, &id).await {
        return Err(crate::parity::Fail::new(
            StatusCode::CONFLICT,
            "invalid_upload",
            "불러오기 업로드는 불러오기로만 등록합니다.",
        ));
    }
    let (s, v) = app
        .uploads
        .finish(&id, false, &headers, Value::Null)
        .await?;
    Ok((s, Json(v)))
}
async fn device_cancel(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(app.uploads.cancel(&id, true, &headers).await?))
}
async fn browser_cancel(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(app.uploads.cancel(&id, false, &headers).await?))
}
async fn device_test(State(app): State<App>, request: Request) -> Result<Response> {
    let mut response = serve(app.web.join("device-test.html"), request).await?;
    for (name, value) in [
        (
            "content-security-policy",
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
    ] {
        response.headers_mut().insert(name, value.parse().unwrap());
    }
    Ok(response)
}
async fn processing(
    State(app): State<App>,
    method: Method,
    body: Bytes,
) -> crate::parity::Answer<Json<Value>> {
    if method == Method::POST {
        let invalid = |m: &str| crate::parity::Fail::bad("invalid_settings", m);
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let Some(fields) = body.as_object() else {
            return Err(invalid("처리 설정이 올바르지 않습니다."));
        };
        if fields.is_empty()
            || fields.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "concurrency" | "auto_convert" | "keep_original_video"
                )
            })
        {
            return Err(invalid("처리 설정이 올바르지 않습니다."));
        }
        if fields
            .get("concurrency")
            .is_some_and(|v| !v.as_u64().is_some_and(|v| (1..=16).contains(&v)))
        {
            return Err(invalid("동시 처리 개수는 1~16이어야 합니다."));
        }
        if fields.get("auto_convert").is_some_and(|v| !v.is_boolean()) {
            return Err(invalid("자동 변환 설정은 켜짐 또는 꺼짐이어야 합니다."));
        }
        if fields
            .get("keep_original_video")
            .is_some_and(|v| !v.is_boolean())
        {
            return Err(invalid(
                "원본 영상 보관 설정은 켜짐 또는 꺼짐이어야 합니다.",
            ));
        }
        let _guard = app.store.registration.lock().await;
        let mut settings = app.store.processing.read().await.clone();
        for (key, value) in fields {
            settings[key] = value.clone();
        }
        if !settings["concurrency"]
            .as_u64()
            .is_some_and(|v| (1..=16).contains(&v))
            || !settings["auto_convert"].is_boolean()
            || !settings["keep_original_video"].is_boolean()
        {
            return Err(invalid("처리 설정이 올바르지 않습니다."));
        }
        atomic_json(&app.store.root.join(".processing-settings.json"), &settings).await?;
        *app.store.processing.write().await = settings;
    }
    Ok(Json(app.store.processing.read().await.clone()))
}
async fn convert(State(app): State<App>, Path(id): Path<String>) -> Result<Response> {
    // Outside the lock: archived files may wait on the share.
    let states =
        crate::recording_update::archived_states(&app.archive, &app.store.meta(&id).await?).await;
    let lock = app.store.record_lock(&id).await;
    let _guard = lock.lock().await;
    let folder = app.store.folder(&id).await?;
    let mut meta = app.store.meta(&id).await?;
    if matches!(
        meta["status"].as_str(),
        Some("ready" | "queued" | "processing")
    ) {
        return Ok(Json(json!({"status":meta["status"]})).into_response());
    }
    // Python `convert`: local files are checked here; an archived one not resolved beforehand counts as
    // unreachable. A conversion needs every archived input, videos included.
    let state = |kind: &str| -> &str {
        match crate::listing::location(&meta, kind) {
            "nas" | "gdrive" => states
                .get(kind)
                .map_or("storage_unavailable", String::as_str),
            "removed" => "missing",
            _ if folder.join(kind).is_file() => {
                let error = &meta["storage"][kind]["error"];
                if error.is_null() || error == "remote_mismatch" {
                    "available"
                } else {
                    "corrupt"
                }
            }
            _ if meta["storage"][kind]["remote"].is_object() => "storage_unavailable",
            _ => "missing",
        }
    };
    let kinds = crate::parity::STORED_KINDS;
    if kinds
        .iter()
        .any(|k| matches!(state(k), "storage_unavailable" | "checking"))
    {
        return Ok((
            StatusCode::CONFLICT,
            Json(
                json!({"error":"storage_unavailable","message":"저장소 연결 후 다시 시도하세요."}),
            ),
        )
            .into_response());
    }
    // An unreadable archived camera could not be converted again: its MP4 in prepared/ is the only playable copy.
    if kinds.iter().any(|k| state(k) == "corrupt") {
        return Ok(crate::media_read::MediaError::new("media_corrupt", false).into_response());
    }
    let log = crate::listing::location(&meta, "rlog.zst");
    let reconvertible =
        log == "local" || matches!(log, "nas" | "gdrive") && state("rlog.zst") == "available";
    if state("rlog.zst") == "missing" || !reconvertible {
        return Err(crate::error::conflict("원본 로그가 없습니다."));
    }
    let revision = meta["conversion_revision"].as_u64().unwrap_or(0) + 1;
    let archived = crate::recording_update::available_originals(Some(&states));
    crate::recording_update::clear_prepared(&folder, &mut meta, revision, &archived).await?;
    if let Some(m) = meta.as_object_mut() {
        m.remove("deferred");
    }
    meta["status"] = json!("queued");
    meta["manual_conversion"] = json!(true);
    meta["auto_excluded"] = json!(false);
    meta["queued_at"] = json!(crate::store::now());
    meta["error"] = Value::Null;
    app.store.save_meta(meta).await?;
    Ok(Json(json!({"status":"queued"})).into_response())
}
async fn remove_prepared(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Result<Json<Value>> {
    let lock = app.store.record_lock(&id).await;
    let _guard = lock.lock().await;
    let folder = app.store.folder(&id).await?;
    let mut meta = app.store.meta(&id).await?;
    if q.get("skip_pinned").is_some_and(|v| v == "1")
        && crate::storage::pins(&crate::storage::load_settings(&app.store.root).await?)
            .contains(&id)
    {
        return Ok(Json(json!({"skipped":"pinned"})));
    }
    if meta["status"] == "processing" || app.jobs.busy(&id).await {
        return Err(crate::error::conflict(
            "처리 중에는 변환 데이터를 제거할 수 없습니다.",
        ));
    }
    app.store
        .prepared
        .finish_pending(&app.store, &folder, &mut meta)
        .await
        .map_err(prepared_storage::api_error)?;
    let remote = prepared_storage::location(&meta)?;
    if let Some(target) = &remote {
        app.store
            .prepared
            .remove_nas(&id, target)
            .await
            .map_err(prepared_storage::api_error)?;
        prepared_storage::set_location(&mut meta, None, Default::default());
    }
    let prepared = folder.join("prepared");
    if prepared.exists() {
        let real = tokio::fs::canonicalize(&prepared).await?;
        let root = tokio::fs::canonicalize(&folder).await?;
        if !real.starts_with(&root) || real == root {
            return Err(bad("invalid_prepared_path"));
        }
        tokio::fs::remove_dir_all(prepared).await?;
    }
    meta["status"] = json!("unconverted");
    meta["manual_conversion"] = json!(false);
    meta["auto_excluded"] = json!(true);
    meta.as_object_mut().unwrap().remove("deferred");
    app.store.save_meta(meta).await?;
    app.jobs.progress.write().await.remove(&id);
    Ok(Json(json!({"status":"unconverted"})))
}
async fn cancel_conversion(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>> {
    let lock = app.store.record_lock(&id).await;
    let _guard = lock.lock().await;
    let mut meta = app.store.meta(&id).await?;
    if meta["status"] == "processing" && app.jobs.busy(&id).await {
        meta["cancel_requested"] = meta["conversion_id"].clone();
        app.store.save_meta(meta.clone()).await?;
        app.jobs.cancel(&id).await;
        return Ok(Json(json!({"status":"cancelling"})));
    }
    if meta["status"] == "queued" {
        meta["status"] = json!("unconverted");
        meta["manual_conversion"] = json!(false);
        meta["auto_excluded"] = json!(true);
        app.store.save_meta(meta).await?;
    }
    Ok(Json(json!({"status":app.store.meta(&id).await?["status"]})))
}
// The record lock closes the race between a new replay request and the migration's skip check.
async fn playback_meta(app: &App, id: &str) -> Result<(Value, Arc<()>)> {
    let lock = app.store.record_lock(id).await;
    let _guard = tokio::time::timeout(prepared_storage::PROBE, lock.lock())
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "prepared_migration_running",
            )
        })?;
    let meta = app.store.meta(id).await?;
    Ok((meta, app.store.prepared.touch_playback(id).await))
}
async fn playback(State(app): State<App>, Path(id): Path<String>) -> Result<StatusCode> {
    playback_meta(&app, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
fn hold_playback(response: Response, lease: Arc<()>) -> Response {
    use futures_util::StreamExt;
    if response.body().size_hint().exact() == Some(0) {
        return response;
    }
    response.map(|body| {
        Body::from_stream(body.into_data_stream().map(move |chunk| {
            let _lease = &lease;
            chunk
        }))
    })
}

// ServeFile retains the established Range and HEAD semantics. The descriptor keeps the
// already-validated file open across renames and avoids resolving the share path again.
async fn serve_nas_file(
    app: &App,
    file: std::fs::File,
    request: Request,
    mime: &'static str,
) -> Result<Response> {
    use futures_util::StreamExt;
    use http_body_util::BodyExt;
    use std::os::fd::AsRawFd;
    let slot = app
        .store
        .prepared
        .read_slot()
        .map_err(prepared_storage::api_error)?;
    let open = tokio::spawn(async move {
        let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let response = serve(path, request).await?;
        drop(file);
        Ok::<_, ApiError>((slot, response))
    });
    let (slot, mut response) = tokio::time::timeout(prepared_storage::PROBE, open)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "prepared_storage_unavailable",
            )
        })?
        .map_err(|_| bad("invalid_prepared_data"))??;
    if response.status().is_success() {
        response
            .headers_mut()
            .insert(h::CONTENT_TYPE, axum::http::HeaderValue::from_static(mime));
    }
    if response.body().is_end_stream() {
        return Ok(response);
    }
    // The in-flight read owns its slot, even when the HTTP client closes the stream.
    Ok(response.map(|body| {
        Body::from_stream(
            futures_util::stream::unfold(Some((slot, body)), |state| async {
                let (slot, mut body) = state?;
                let read = tokio::spawn(async move {
                    let frame = body.frame().await?;
                    Some((frame, (slot, body)))
                });
                match tokio::time::timeout(std::time::Duration::from_secs(20), read).await {
                    Ok(Ok(Some((Ok(frame), state)))) => match frame.into_data() {
                        Ok(bytes) => Some((Ok(bytes), Some(state))),
                        Err(_) => None,
                    },
                    Ok(Ok(None)) => None,
                    _ => Some((
                        Err(std::io::Error::other("prepared_storage_unavailable")),
                        None,
                    )),
                }
            })
            .fuse(),
        )
    }))
}

async fn open_prepared_nas(
    app: &App,
    target: &NasLocation,
    id: &str,
    file: String,
) -> Result<std::fs::File> {
    app.store
        .prepared
        .open_file(target, id, file)
        .await
        .map_err(prepared_storage::api_error)
}

async fn prepared(
    app: &App,
    id: &str,
    name: &str,
    query: &BTreeMap<String, String>,
    request: Request,
) -> Result<Response> {
    let (meta, _lease) = playback_meta(app, id).await?;
    let folder = app.store.folder(id).await?;
    if meta["status"] != "ready" {
        // Python `data`/`telemetry`: a sentence when the record holds no error of its own.
        let body = if name == "telemetry" {
            json!({"error":"로그를 준비 중입니다."})
        } else {
            let error = match &meta["error"] {
                Value::String(e) if !e.is_empty() => json!(e),
                _ if meta["status"] == "unconverted" => {
                    json!("변환 데이터가 없습니다. 로그 목록에서 변환해 주세요.")
                }
                _ => json!("로그를 준비 중입니다."),
            };
            json!({"status":meta["status"],"error":error})
        };
        return Ok((StatusCode::CONFLICT, Json(body)).into_response());
    }
    if let Some(target) = prepared_storage::location(&meta)? {
        match open_prepared_nas(app, &target, id, format!("{name}.json.gz")).await {
            Ok(file) if query.get("format").is_some_and(|v| v == "gzip") => {
                return Ok(hold_playback(
                    serve_nas_file(app, file, request, "application/gzip").await?,
                    _lease,
                ));
            }
            Ok(file) => {
                let result = app
                    .store
                    .prepared
                    .quick(move || crate::logs::read_gzip_file(file).map_err(std::io::Error::other))
                    .await
                    .map_err(prepared_storage::api_error)?;
                return Ok(Json(result).into_response());
            }
            Err(e) if e.0 == StatusCode::NOT_FOUND => {}
            Err(e) => return Err(e),
        }
        match open_prepared_nas(app, &target, id, format!("{name}.json")).await {
            Ok(file) => {
                return Ok(hold_playback(
                    serve_nas_file(app, file, request, "application/json").await?,
                    _lease,
                ));
            }
            Err(e) if name == "telemetry" && e.0 == StatusCode::NOT_FOUND => {
                return Ok(no_telemetry());
            }
            Err(e) => return Err(e),
        }
    }
    let path = folder.join("prepared").join(format!("{name}.json.gz"));
    if path.exists() {
        safe_file(&folder, &path).await?;
        if query.get("format").is_some_and(|v| v == "gzip") {
            let mut res = serve(path, request).await?;
            res.headers_mut()
                .insert(h::CONTENT_TYPE, "application/gzip".parse().unwrap());
            return Ok(res);
        }
        let result = tokio::task::spawn_blocking(move || crate::logs::read_gzip(&path))
            .await
            .map_err(|_| bad("invalid_prepared_data"))?
            .map_err(|_| bad("invalid_prepared_data"))?;
        return Ok(Json(result).into_response());
    }
    let path = folder.join("prepared").join(format!("{name}.json"));
    if name == "telemetry" && !path.is_file() {
        return Ok(no_telemetry());
    }
    safe_file(&folder, &path).await?;
    serve(path, request).await
}
fn no_telemetry() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error":"차량 정보가 없는 변환 데이터입니다. 로그 목록에서 제거 후 변환해 주세요."}))).into_response()
}
async fn data(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    request: Request,
) -> Result<Response> {
    let mut response = prepared(&app, &id, "data", &q, request).await?;
    if response.status().is_success() {
        response.headers_mut().insert(
            "x-roadviewer-playback",
            axum::http::HeaderValue::from_static("1"),
        );
    }
    Ok(response)
}
async fn telemetry(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    request: Request,
) -> Result<Response> {
    prepared(&app, &id, "telemetry", &q, request).await
}
async fn track(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let target = app.store.meta(&id).await?;
    let key = crate::track::route_order(&target).0;
    let (ready, unconverted): (Vec<_>, Vec<_>) = app
        .store
        .records
        .read()
        .await
        .values()
        .filter(|m| crate::track::route_order(m).0 == key)
        .cloned()
        .partition(|m| m["status"] == "ready");
    let mut unconverted: Vec<_> = unconverted
        .iter()
        .filter_map(crate::track::UnconvertedSegment::from_meta)
        .collect();
    unconverted.sort_unstable();
    let shares = app.store.prepared.track_shares(&ready).await?;
    let mut ready_segments = ready
        .iter()
        .filter_map(|m| Some((crate::track::route_order(m).1, m["id"].as_str()?.to_owned())))
        .collect::<Vec<_>>();
    ready_segments.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let root = app.store.root.clone();
    let if_none_match = header(&headers, "if-none-match").to_owned();
    let reads = prepared_storage::TrackReads::new(&root, shares, true);
    let has_nas = reads.needs_nas();
    let operation = move || {
        let segments = reads.load(&root, Some(&id))?;
        Ok(crate::track::build_single_track(
            &root,
            &id,
            &key,
            &ready_segments,
            &unconverted,
            &segments,
            &if_none_match,
        ))
    };
    let answer = run_tracks(&app, has_nas, operation).await?;
    Ok(tracks_answer_response(answer))
}
fn tracks_answer_response(answer: crate::track::TracksAnswer) -> Response {
    match answer {
        crate::track::TracksAnswer::NotModified { etag } => {
            ([(h::ETAG, etag)], StatusCode::NOT_MODIFIED).into_response()
        }
        crate::track::TracksAnswer::Ok { etag, bytes } => (
            [
                (h::ETAG, etag),
                (h::CONTENT_TYPE, "application/json".to_owned()),
            ],
            bytes,
        )
            .into_response(),
    }
}
async fn tracks(
    State(app): State<App>,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    let mut metas = app
        .store
        .records
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let wanted = q.get("ids").filter(|v| !v.is_empty()).map(|v| {
        v.split(',')
            .filter(|i| crate::store::valid_id(i))
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
    });
    if let Some(wanted) = &wanted {
        let routes = metas
            .iter()
            .filter(|m| m["id"].as_str().is_some_and(|id| wanted.contains(id)))
            .map(|m| crate::track::route_order(m).0)
            .collect::<std::collections::BTreeSet<_>>();
        metas.retain(|m| routes.contains(&crate::track::route_order(m).0));
    }
    let shares = app.store.prepared.track_shares(&metas).await?;
    let root = app.store.root.clone();
    let if_none_match = header(&headers, "if-none-match").to_owned();
    let reads = prepared_storage::TrackReads::new(&root, shares, false);
    let has_nas = reads.needs_nas();
    let operation = move || {
        let segments = reads.load(&root, None)?;
        Ok(crate::track::build_tracks(
            &root,
            &metas,
            wanted.as_ref(),
            &segments,
            &if_none_match,
        ))
    };
    Ok(tracks_answer_response(
        run_tracks(&app, has_nas, operation).await?,
    ))
}
async fn run_tracks(
    app: &App,
    has_nas: bool,
    op: impl FnOnce() -> std::io::Result<crate::track::TracksAnswer> + Send + 'static,
) -> Result<crate::track::TracksAnswer> {
    if has_nas {
        app.store
            .prepared
            .run_track_query(op)
            .await
            .map_err(prepared_storage::api_error)
    } else {
        tokio::task::spawn_blocking(op)
            .await
            .map_err(|_| bad("invalid_track_data"))?
            .map_err(prepared_storage::api_error)
    }
}

/// Prewarms the in-memory track cache in small newest-first batches after startup.
/// Yields whenever a live user track query is active and stops gracefully on NAS errors.
pub async fn warm_track_cache(app: &App) {
    const WARM_ROUTES_PER_BATCH: usize = 8;
    const BATCH_PAUSE: std::time::Duration = std::time::Duration::from_millis(150);
    const BUSY_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

    let ready_metas = app
        .store
        .records
        .read()
        .await
        .values()
        .filter(|m| m["status"] == "ready")
        .cloned()
        .collect::<Vec<_>>();
    if ready_metas.is_empty() {
        return;
    }

    let mut by_route: BTreeMap<String, (f64, Vec<Value>)> = BTreeMap::new();
    for meta in ready_metas {
        let key = crate::track::route_order(&meta).0;
        let uploaded = meta["uploaded"].as_f64().unwrap_or(0.0);
        let entry = by_route
            .entry(key)
            .or_insert_with(|| (uploaded, Vec::new()));
        if uploaded > entry.0 {
            entry.0 = uploaded;
        }
        entry.1.push(meta);
    }
    let mut routes = by_route.into_values().collect::<Vec<_>>();
    routes.sort_by(|a, b| b.0.total_cmp(&a.0));

    let root = app.store.root.clone();
    for chunk in routes.chunks(WARM_ROUTES_PER_BATCH) {
        let batch_metas = chunk
            .iter()
            .flat_map(|(_, metas)| metas.iter().cloned())
            .collect::<Vec<_>>();
        let batch_ids = batch_metas
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect::<std::collections::BTreeSet<_>>();
        if batch_ids.is_empty() {
            continue;
        }
        let Ok(shares) = app.store.prepared.track_shares(&batch_metas).await else {
            return;
        };
        let reads = prepared_storage::TrackReads::new(&root, shares, false);
        let has_nas = reads.needs_nas();
        if has_nas {
            let mut warmed = false;
            let mut reads = Some(reads);
            for _ in 0..3 {
                let current_reads = reads.take().unwrap();
                let op_root = root.clone();
                let op_metas = batch_metas.clone();
                let op_ids = batch_ids.clone();
                let op = move || {
                    let segments = current_reads.load(&op_root, None)?;
                    let _ = crate::track::build_tracks(
                        &op_root,
                        &op_metas,
                        Some(&op_ids),
                        &segments,
                        "",
                    );
                    Ok(())
                };
                match app.store.prepared.run_track_warm(op).await {
                    Ok(Some(())) => {
                        warmed = true;
                        break;
                    }
                    Ok(None) => {
                        tokio::time::sleep(BUSY_PAUSE).await;
                        let Ok(retry_shares) = app.store.prepared.track_shares(&batch_metas).await
                        else {
                            return;
                        };
                        let retry_reads =
                            prepared_storage::TrackReads::new(&root, retry_shares, false);
                        if !retry_reads.needs_nas() {
                            warmed = true;
                            break;
                        }
                        reads = Some(retry_reads);
                    }
                    Err(_) => return,
                }
            }
            if !warmed {
                continue;
            }
        } else {
            let root = root.clone();
            let op = move || {
                let segments = reads.load(&root, None)?;
                let _ = crate::track::build_tracks(
                    &root,
                    &batch_metas,
                    Some(&batch_ids),
                    &segments,
                    "",
                );
                Ok::<(), std::io::Error>(())
            };
            let _ = tokio::task::spawn_blocking(op).await;
        }
        tokio::time::sleep(BATCH_PAUSE).await;
    }
}
async fn video(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    request: Request,
) -> Result<Response> {
    let folder = app.store.folder(&id).await?;
    let (meta, lease) = playback_meta(&app, &id).await?;
    if meta["status"] != "ready" {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"error":"로그를 준비 중이거나 변환에 실패했습니다."})),
        )
            .into_response());
    }
    // Python `video`: the front camera by default when the conversion produced it.
    let target = prepared_storage::location(&meta)?;
    let videos = if target.is_some() {
        meta["videos"].clone()
    } else {
        read_json::<Value>(&folder.join("prepared/summary.json"))
            .await
            .unwrap_or(Value::Null)["videos"]
            .clone()
    };
    let preferred = ["front", "qcamera", "wide"]
        .into_iter()
        .find(|k| videos[k].as_object().is_some_and(|v| !v.is_empty()))
        .unwrap_or("qcamera");
    let source = q.get("source").map(String::as_str).unwrap_or(preferred);
    let name = match source {
        "qcamera" => "camera.mp4",
        "front" => "fcamera.mp4",
        "wide" => "ecamera.mp4",
        _ => return Err(missing("not_found")),
    };
    // Python `video`: the stored MP4 wherever the index has it, else the converted one in prepared/.
    let mut response = match crate::media_read::media_file(&app.archive, &id, &meta, name).await {
        Err(e) => return Ok(e.into_response()),
        Ok(Some(Source::Remote(file))) => {
            return Ok(hold_playback(
                crate::media_read::remote_response(
                    &app.archive,
                    &file,
                    "video/mp4",
                    None,
                    request.headers(),
                )
                .await,
                lease,
            ));
        }
        Ok(None) if target.is_some() => {
            let file = open_prepared_nas(&app, &target.unwrap(), &id, name.to_owned()).await?;
            serve_nas_file(&app, file, request, "video/mp4").await?
        }
        found => {
            let path = match found {
                Ok(Some(Source::Path(path))) => path,
                _ => folder.join("prepared").join(name),
            };
            media_file_check(&folder, &path).await?;
            serve(path, request).await?
        }
    };
    response
        .headers_mut()
        .insert(h::CACHE_CONTROL, "no-cache".parse().unwrap());
    Ok(hold_playback(response, lease))
}
async fn thumbnail(
    State(app): State<App>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response> {
    let folder = app.store.folder(&id).await?;
    let meta = app.store.meta(&id).await?;
    let mut response = if let Some(target) = prepared_storage::location(&meta)? {
        if meta["prepared_stale"] == true {
            return Err(missing("not_found"));
        }
        let file = open_prepared_nas(&app, &target, &id, "thumbnail.jpg".into()).await?;
        serve_nas_file(&app, file, request, "image/jpeg").await?
    } else {
        let path = folder.join("prepared/thumbnail.jpg");
        safe_file(&folder, &path).await?;
        serve(path, request).await?
    };
    response
        .headers_mut()
        .insert(h::CACHE_CONTROL, "private, max-age=86400".parse().unwrap());
    Ok(response)
}
async fn original(
    State(app): State<App>,
    Path((id, kind)): Path<(String, String)>,
    request: Request,
) -> Result<Response> {
    if !crate::parity::STORED_KINDS.contains(&kind.as_str()) {
        return Err(missing("not_found"));
    }
    let folder = app.store.folder(&id).await?;
    let meta = app.store.meta(&id).await?;
    let mut path = match crate::media_read::media_file(&app.archive, &id, &meta, &kind).await {
        Err(e) => return Ok(e.into_response()),
        Ok(Some(Source::Remote(file))) => {
            let name = crate::parity::download_name(&meta, &kind);
            return Ok(crate::media_read::remote_response(
                &app.archive,
                &file,
                media_mime(&kind),
                Some(&name),
                request.headers(),
            )
            .await);
        }
        Ok(Some(Source::Path(path))) => path,
        Ok(None) => folder.join(&kind),
    };
    if kind.ends_with(".mp4") && !tokio::fs::try_exists(&path).await? {
        if let Some(target) = prepared_storage::location(&meta)? {
            let file = open_prepared_nas(&app, &target, &id, kind.clone()).await?;
            return Ok(attachment(
                serve_nas_file(&app, file, request, "video/mp4").await?,
                &meta,
                &kind,
            ));
        }
        path = folder.join("prepared").join(&kind);
    }
    if !path.exists()
        && matches!(
            kind.as_str(),
            "qcamera.ts" | "fcamera.hevc" | "ecamera.hevc"
        )
    {
        let (mp4, source) = match kind.as_str() {
            "qcamera.ts" => ("camera.mp4", "qcamera"),
            "fcamera.hevc" => ("fcamera.mp4", "front"),
            _ => ("ecamera.mp4", "wide"),
        };
        // Python `planned_video`: the kept MP4 wherever it is (a Drive-only one is staged, verified), else the
        // converted one in prepared/.
        let mut staged = None;
        let retained = match crate::media_read::media_file(&app.archive, &id, &meta, mp4).await {
            Err(e) => return Ok(e.into_response()),
            Ok(Some(Source::Path(path))) => path,
            Ok(Some(Source::Remote(file))) => {
                match crate::media_read::stage(&app.archive, &folder, &file).await {
                    Ok(copy) => staged.insert(copy).path().to_owned(),
                    Err(e) => return Ok(e.into_response()),
                }
            }
            Ok(None) => prepared_download_path(&app, &folder, &meta, mp4)
                .await?
                .ok_or_else(|| missing("not_found"))?,
        };
        media_file_check(&folder, &retained).await?;
        let exports = app.store.root.join(".exports");
        tokio::fs::create_dir_all(&exports).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&exports, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let output = exports.join(format!(
            "{}.{}",
            crate::store::id(),
            if kind == "qcamera.ts" { "ts" } else { "hevc" }
        ));
        let meta = app.store.meta(&id).await?;
        let offset = meta["video_origins"][source].as_f64().unwrap_or(0.);
        let output =
            crate::media_read::export_original(retained, staged, kind.clone(), output, offset)
                .await
                .map_err(|_| bad("media_export_failed"))?;
        let response = serve(output.path().to_owned(), request)
            .await
            .map(|response| attachment(response, &meta, &kind));
        // ServeFile has already opened its file. Linux keeps it readable after unlink.
        output.remove().await;
        return response;
    }
    media_file_check(&folder, &path).await?;
    Ok(attachment(serve(path, request).await?, &meta, &kind))
}
pub(crate) async fn prepared_download_path(
    app: &App,
    folder: &std::path::Path,
    meta: &Value,
    mp4: &str,
) -> Result<Option<PathBuf>> {
    if meta["prepared_stale"] == true {
        return Ok(None);
    }
    let prepared = app
        .store
        .prepared
        .read_dir(folder, meta)
        .await
        .map_err(prepared_storage::api_error)?;
    let path = prepared.join(mp4);
    if prepared_storage::location(meta)?.is_some() {
        let check = path.clone();
        let found = app
            .store
            .prepared
            .quick(move || match crate::archive::nas::share_meta(&check) {
                Ok(m) => Ok(m.is_file()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            })
            .await
            .map_err(prepared_storage::api_error)?;
        Ok(found.then_some(path))
    } else {
        Ok(path.is_file().then_some(path))
    }
}

/// A file a media route serves: inside the recording folder it must stay there (no link out); elsewhere it is a
/// path the archive index resolved on the share, which must be a regular file there and not a link.
async fn media_file_check(folder: &std::path::Path, path: &std::path::Path) -> Result<()> {
    if path.starts_with(folder) {
        return safe_file(folder, path).await;
    }
    let share_path = path.to_owned();
    match tokio::task::spawn_blocking(move || crate::archive::nas::share_meta(&share_path)).await {
        Ok(Ok(m)) if m.is_file() => Ok(()),
        _ => Err(missing("not_found")),
    }
}
fn media_mime(kind: &str) -> &'static str {
    if kind.ends_with(".mp4") {
        "video/mp4"
    } else if kind.ends_with(".hevc") {
        "video/hevc"
    } else if kind == "qcamera.ts" {
        "video/mp2t"
    } else {
        "application/zstd"
    }
}
/// Python `original` answers through Werkzeug `send_file`: its filename form and `no-cache`.
fn attachment(mut response: Response, meta: &Value, kind: &str) -> Response {
    let name = crate::parity::download_name(meta, kind);
    if let Ok(value) = crate::parity::send_file_disposition(&name).parse() {
        response.headers_mut().insert(h::CONTENT_DISPOSITION, value);
    }
    if response.status().is_success() {
        response
            .headers_mut()
            .insert(h::CONTENT_TYPE, media_mime(kind).parse().unwrap());
        response
            .headers_mut()
            .insert(h::CACHE_CONTROL, "no-cache".parse().unwrap());
    }
    response
}
async fn download_video(
    State(app): State<App>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response> {
    let folder = app.store.folder(&id).await?;
    let meta = app.store.meta(&id).await?;
    // The conversion's own list (it also offers an MP4 left in prepared/), else Python `video_download_kind`.
    let kind = meta["video_downloads"]
        .as_array()
        .and_then(|v| {
            v.iter()
                .find(|v| v["source"] == "front")
                .or_else(|| v.first())
        })
        .and_then(|v| v["kind"].as_str())
        .or_else(|| crate::listing::video_download_kind(&folder, &meta))
        .ok_or_else(|| missing("not_found"))?
        .to_owned();
    original(State(app), Path((id, kind)), request).await
}
async fn download_camera(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    request: Request,
) -> Result<Response> {
    // Python `download_one`: the log by default, then the camera in the asked format.
    let source = q.get("source").map(String::as_str).unwrap_or("rlog");
    let (mp4, raw) = match source {
        "rlog" => ("rlog.zst", "rlog.zst"),
        "qcamera" => ("camera.mp4", "qcamera.ts"),
        "front" => ("fcamera.mp4", "fcamera.hevc"),
        "wide" => ("ecamera.mp4", "ecamera.hevc"),
        _ => return Err(bad("invalid_video_source")),
    };
    let kind = match q.get("format").map(String::as_str).unwrap_or("mp4") {
        "mp4" => mp4,
        "original" => raw,
        _ => return Err(bad("invalid_download_format")),
    };
    let meta = app.store.meta(&id).await?;
    let mut response = original(State(app), Path((id, kind.into())), request).await?;
    if response.status().is_success() {
        let name = crate::parity::download_name(&meta, kind);
        if let Ok(value) = crate::parity::message_disposition(&name).parse() {
            response.headers_mut().insert(h::CONTENT_DISPOSITION, value);
        }
        response
            .headers_mut()
            .insert(h::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    Ok(response)
}
