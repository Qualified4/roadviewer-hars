use crate::{
    auth::{Auth, header},
    error::{ApiError, Result, bad, missing},
    http_archive, http_storage,
    media_read::Source,
    store::{Store, atomic_json, id, read_json, sha},
    upload::Uploads,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
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
use tower_http::services::ServeFile;

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
        .route("/api/logs/{id}/thumbnail", get(thumbnail))
        .route("/api/logs/{id}/original/{kind}", get(original))
        .route("/api/logs/{id}/download/video", get(download_video))
        .route("/api/logs/{id}/download", get(download_camera))
        .route("/health", get(health))
        .fallback(crate::parity::fallback)
        .method_not_allowed_fallback(crate::parity::method_not_allowed)
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
    Json(
        json!({"product":"Road Viewer HARS","version":env!("CARGO_PKG_VERSION"),"release_ready":false}),
    )
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
    let html = tokio::fs::read_to_string(app.web.join(name))
        .await?
        .replace(
            "__ASSETS__",
            if replay { "../../assets/" } else { "assets/" },
        )
        .replace("__VERSION__", env!("CARGO_PKG_VERSION"))
        .replace("1.3.16", env!("CARGO_PKG_VERSION"))
        .replace("Road Viewer", "Road Viewer HARS");
    Ok(([(h::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response())
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
            "export function mount(env) {\nconst {window,document,location,fetch,setTimeout,clearTimeout,setInterval,clearInterval,requestAnimationFrame,cancelAnimationFrame,ResizeObserver,MutationObserver,XMLHttpRequest,matchMedia}=env;\nconst addEventListener=env.addEventListener||globalThis.addEventListener.bind(globalThis),removeEventListener=env.removeEventListener||globalThis.removeEventListener.bind(globalThis),IntersectionObserver=env.IntersectionObserver||globalThis.IntersectionObserver,onPageDispose=env.onPageDispose||(()=>{});\n",
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
            .is_some_and(|v| !v.as_u64().is_some_and(|v| (1..=4).contains(&v)))
        {
            return Err(invalid("동시 처리 개수는 1~4여야 합니다."));
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
            .is_some_and(|v| (1..=4).contains(&v))
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
async fn convert(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>> {
    let lock = app.store.record_lock(&id).await;
    let _guard = lock.lock().await;
    let folder = app.store.folder(&id).await?;
    let mut meta = app.store.meta(&id).await?;
    if matches!(
        meta["status"].as_str(),
        Some("ready" | "queued" | "processing")
    ) {
        return Ok(Json(json!({"status":meta["status"]})));
    }
    if !folder.join("rlog.zst").is_file() {
        return Err(crate::error::conflict("원본 로그가 없습니다."));
    }
    meta["status"] = json!("queued");
    meta["manual_conversion"] = json!(true);
    meta["auto_excluded"] = json!(false);
    meta["queued_at"] = json!(crate::store::now());
    meta["error"] = Value::Null;
    app.store.save_meta(meta).await?;
    Ok(Json(json!({"status":"queued"})))
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
async fn prepared(
    app: &App,
    id: &str,
    name: &str,
    query: &BTreeMap<String, String>,
    request: Request,
) -> Result<Response> {
    let folder = app.store.folder(id).await?;
    let meta = app.store.meta(id).await?;
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
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error":"차량 정보가 없는 변환 데이터입니다. 로그 목록에서 제거 후 변환해 주세요."})),
        )
            .into_response());
    }
    safe_file(&folder, &path).await?;
    serve(path, request).await
}
async fn data(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    request: Request,
) -> Result<Response> {
    prepared(&app, &id, "data", &q, request).await
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
    let key = route_order(&target).0;
    let rows = map_rows(&app).await?;
    let segments=rows.into_iter().filter(|r|r.0==key).map(|(_,segment,m,summary,track)|json!({"id":m["id"],"segment":segment,"duration":summary["duration"],"distance_m":summary["track_distance_m"],"points":track["points"]})).collect::<Vec<_>>();
    Ok(tagged(&headers, json!({"id":id,"segments":segments})))
}
fn route_order(meta: &Value) -> (String, Option<u64>) {
    let order = crate::parity::RecordingOrder::of(meta);
    match order.route_key() {
        Some(key) => (key, order.segment()),
        None => (meta["id"].as_str().unwrap_or("").into(), None),
    }
}
fn tagged(request: &HeaderMap, value: Value) -> Response {
    let bytes = serde_json::to_vec(&value).unwrap();
    let etag = format!("\"{}\"", &sha(&bytes)[..32]);
    if header(request, "if-none-match") == etag {
        return ([(h::ETAG, etag)], StatusCode::NOT_MODIFIED).into_response();
    }
    (
        [
            (h::ETAG, etag),
            (h::CONTENT_TYPE, "application/json".to_owned()),
        ],
        bytes,
    )
        .into_response()
}
type MapRow = (String, Option<u64>, Value, Value, Value);
async fn map_rows(app: &App) -> Result<Vec<MapRow>> {
    let metas = app
        .store
        .records
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let root = app.store.root.clone();
    tokio::task::spawn_blocking(move || {
        let mut rows = vec![];
        for m in metas {
            if m["status"] != "ready" {
                continue;
            }
            let folder = root.join(m["id"].as_str().unwrap()).join("prepared");
            let Ok(summary) = std::fs::read(folder.join("summary.json"))
                .and_then(|v| serde_json::from_slice::<Value>(&v).map_err(std::io::Error::other))
            else {
                continue;
            };
            if summary["has_track"] != true {
                continue;
            }
            let Ok(track) = crate::logs::read_gzip(&folder.join("track.json.gz")) else {
                continue;
            };
            if track["points"].as_array().is_none_or(|p| p.is_empty()) {
                continue;
            }
            let (key, segment) = route_order(&m);
            rows.push((key, segment, m, summary, track));
        }
        rows.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.cmp(&b.1))
                .then(a.2["id"].as_str().cmp(&b.2["id"].as_str()))
        });
        rows
    })
    .await
    .map_err(|_| bad("invalid_track_data"))
}
async fn tracks(
    State(app): State<App>,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    let rows = map_rows(&app).await?;
    let metas = app
        .store
        .records
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let mut groups: BTreeMap<String, Value> = BTreeMap::new();
    for m in metas {
        let key = route_order(&m).0;
        groups.entry(key.clone()).or_insert_with(
            || json!({"key":key,"ids":[],"segments":[],"points":[],"distance_m":0.}),
        )["ids"]
            .as_array_mut()
            .unwrap()
            .push(m["id"].clone());
    }
    for (key, _, m, s, t) in rows {
        let group = groups.get_mut(&key).unwrap();
        group["segments"]
            .as_array_mut()
            .unwrap()
            .push(m["id"].clone());
        let coords = t["points"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| Some([p[1].as_f64()?, p[2].as_f64()?]))
            .collect::<Vec<_>>();
        group["points"].as_array_mut().unwrap().extend(
            crate::track::simplify(&coords, 150)
                .into_iter()
                .map(|p| json!(p)),
        );
        group["distance_m"] = json!(
            group["distance_m"].as_f64().unwrap_or(0.)
                + s["track_distance_m"].as_f64().unwrap_or(0.)
        );
    }
    let wanted = q.get("ids").filter(|v| !v.is_empty()).map(|v| {
        v.split(',')
            .filter(|i| crate::store::valid_id(i))
            .collect::<std::collections::BTreeSet<_>>()
    });
    let mut routes = vec![];
    for mut route in groups.into_values() {
        if route["segments"].as_array().unwrap().is_empty()
            || wanted.as_ref().is_some_and(|wanted| {
                !route["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|id| wanted.contains(id.as_str().unwrap_or("")))
            })
        {
            continue;
        }
        let coords = route["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| [p[0].as_f64().unwrap(), p[1].as_f64().unwrap()])
            .collect::<Vec<_>>();
        route["points"] = json!(crate::track::simplify(&coords, 300));
        routes.push(route);
    }
    Ok(tagged(&headers, json!({"routes":routes})))
}
async fn video(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    request: Request,
) -> Result<Response> {
    let folder = app.store.folder(&id).await?;
    let meta = app.store.meta(&id).await?;
    if meta["status"] != "ready" {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"error":"로그를 준비 중이거나 변환에 실패했습니다."})),
        )
            .into_response());
    }
    // Python `video`: the front camera by default when the conversion produced it.
    let summary: Value = read_json(&folder.join("prepared/summary.json"))
        .await
        .unwrap_or(Value::Null);
    let preferred = ["front", "qcamera", "wide"]
        .into_iter()
        .find(|k| {
            summary["videos"][k]
                .as_object()
                .is_some_and(|v| !v.is_empty())
        })
        .unwrap_or("qcamera");
    let source = q.get("source").map(String::as_str).unwrap_or(preferred);
    let name = match source {
        "qcamera" => "camera.mp4",
        "front" => "fcamera.mp4",
        "wide" => "ecamera.mp4",
        _ => return Err(missing("not_found")),
    };
    // Python `video`: the stored MP4 wherever the index has it, else the converted one in prepared/.
    let path = match crate::media_read::media_file(&app.archive, &id, &meta, name).await {
        Err(e) => return Ok(e.into_response()),
        Ok(Some(Source::Remote(file))) => {
            return Ok(crate::media_read::remote_response(
                &app.archive,
                &file,
                "video/mp4",
                None,
                request.headers(),
            )
            .await);
        }
        Ok(Some(Source::Path(path))) => path,
        Ok(None) => folder.join("prepared").join(name),
    };
    media_file_check(&folder, &path).await?;
    let mut response = serve(path, request).await?;
    response
        .headers_mut()
        .insert(h::CACHE_CONTROL, "no-cache".parse().unwrap());
    Ok(response)
}
async fn thumbnail(
    State(app): State<App>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response> {
    let folder = app.store.folder(&id).await?;
    let path = folder.join("prepared/thumbnail.jpg");
    safe_file(&folder, &path).await?;
    let mut response = serve(path, request).await?;
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
    if kind.ends_with(".mp4") && !path.is_file() {
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
                match crate::media_read::stage_remote(&app.archive, &folder, &file).await {
                    Ok(path) => {
                        staged = Some(path.clone());
                        path
                    }
                    Err(e) => return Ok(e.into_response()),
                }
            }
            Ok(None) => folder.join("prepared").join(mp4),
        };
        let exported = async {
            media_file_check(&folder, &retained).await?;
            let exports = app.store.root.join(".exports");
            tokio::fs::create_dir_all(&exports).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&exports, std::fs::Permissions::from_mode(0o700))
                    .await?;
            }
            let output = exports.join(format!(
                "{}.{}",
                crate::store::id(),
                if kind == "qcamera.ts" { "ts" } else { "hevc" }
            ));
            let meta = app.store.meta(&id).await?;
            let offset = meta["video_origins"][source].as_f64().unwrap_or(0.);
            crate::media::reverse(&retained, &kind, &output, offset)
                .await
                .map_err(|_| bad("media_export_failed"))?;
            let response = serve(output.clone(), request)
                .await
                .map(|response| attachment(response, &meta, &kind));
            // ServeFile has already opened its file. Linux keeps it readable after unlink.
            let _ = tokio::fs::remove_file(&output).await;
            response
        }
        .await;
        if let Some(path) = &staged {
            crate::media_read::unstage(path).await;
        }
        return exported;
    }
    media_file_check(&folder, &path).await?;
    Ok(attachment(serve(path, request).await?, &meta, &kind))
}
/// A file a media route serves: inside the recording folder it must stay there (no link out); elsewhere it is a
/// path the archive index resolved on the share.
async fn media_file_check(folder: &std::path::Path, path: &std::path::Path) -> Result<()> {
    if path.starts_with(folder) {
        return safe_file(folder, path).await;
    }
    match tokio::fs::metadata(path).await {
        Ok(m) if m.is_file() => Ok(()),
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
