use crate::{
    auth::{Auth, header},
    error::{ApiError, Result, bad, missing},
    store::{Store, atomic_json, id, read_json, sha},
    upload::{KINDS, Uploads},
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
use unicode_normalization::UnicodeNormalization;

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
        let auth = Arc::new(Auth::open(&store.root).await?);
        let uploads =
            Uploads::open(store.clone(), auth.clone(), max_upload_mb * 1024 * 1024).await?;
        uploads
            .recover_commits()
            .await
            .map_err(|_| anyhow::anyhow!("upload commit recovery failed; data preserved"))?;
        let used = Arc::new(AtomicU64::new(store.used().await?));
        let jobs = crate::jobs::Jobs::open(store.clone(), uploads.clone()).await?;
        Ok(Self {
            store,
            auth,
            uploads,
            web,
            ingress_only,
            used,
            device_https_enabled: false,
            jobs,
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
        .route("/api/tracks", get(tracks))
        .route("/api/progress", get(progress))
        .route("/api/settings/processing", get(processing).post(processing))
        .route("/api/settings/storage", get(storage).post(storage))
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
        .route("/api/logs/{id}/bookmarks", get(bookmarks).post(bookmarks))
        .route(
            "/api/logs/{id}/bookmarks/{bookmark}",
            put(bookmark_item).delete(bookmark_item),
        )
        .route("/api/logs/{id}/tags", post(tags))
        .route("/api/logs/{id}/pin", post(pin))
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
        .fallback(unsupported)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
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
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
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
        let limits = crate::transfer::Limits {
            idle: app.body_idle,
            total: app.body_total,
            max: BODY_LIMIT,
        };
        return match crate::transfer::receive(body, &limits).await {
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
async fn unsupported() -> ApiError {
    ApiError(StatusCode::NOT_IMPLEMENTED, "feature_not_ported")
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
    let pin_path = app.store.root.join(".storage-settings.json");
    let pins: Value = if pin_path.exists() {
        read_json(&pin_path).await?
    } else {
        json!({"pinned_logs":[]})
    };
    let settings = app.store.processing.read().await.clone();
    let progress = app.jobs.progress.read().await.clone();
    let mut logs = app
        .store
        .records
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for m in &mut logs {
        let o = m.as_object_mut().unwrap();
        o.remove("content_hashes");
        o.remove("storage");
        o.remove("bookmarks");
        let pinned = pins["pinned_logs"]
            .as_array()
            .is_some_and(|v| v.contains(&o["id"]));
        o.insert("pinned".into(), json!(pinned));
        let current = progress
            .get(o["id"].as_str().unwrap_or(""))
            .cloned()
            .unwrap_or(Value::Null);
        o.insert("progress".into(), current);
    }
    logs.sort_by(|a, b| {
        b["name"].as_str().cmp(&a["name"].as_str()).then_with(|| {
            b["uploaded"]
                .as_f64()
                .partial_cmp(&a["uploaded"].as_f64())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
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
async fn begin_browser(
    State(app): State<App>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>)> {
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
) -> Result<(StatusCode, Json<Value>)> {
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
    serve(app.web.join("device-test.html"), request).await
}
async fn processing(State(app): State<App>, method: Method, body: Bytes) -> Result<Json<Value>> {
    if method == Method::POST {
        let body: Value = serde_json::from_slice(&body).map_err(|_| bad("invalid_settings"))?;
        let Some(fields) = body.as_object() else {
            return Err(bad("invalid_settings"));
        };
        if fields.is_empty()
            || fields.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "concurrency" | "auto_convert" | "keep_original_video"
                )
            })
        {
            return Err(bad("invalid_settings"));
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
            return Err(bad("invalid_settings"));
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
    if q.get("skip_pinned").is_some_and(|v| v == "1") {
        let settings: Value = read_json(&app.store.root.join(".storage-settings.json"))
            .await
            .unwrap_or(json!({}));
        if settings["pinned_logs"]
            .as_array()
            .is_some_and(|v| v.contains(&json!(id)))
        {
            return Ok(Json(json!({"skipped":"pinned"})));
        }
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
async fn storage(State(app): State<App>, method: Method, body: Bytes) -> Result<Json<Value>> {
    let _registration = app.store.registration.lock().await;
    let path = app.store.root.join(".storage-settings.json");
    let mut settings: Value = if path.exists() {
        read_json(&path).await?
    } else {
        json!({"max_bytes":0,"policy":"reject_new","pinned_logs":[]})
    };
    if method == Method::POST {
        let body: Value =
            serde_json::from_slice(&body).map_err(|_| bad("invalid_storage_settings"))?;
        if body.as_object().is_none_or(|o| o.len() != 2)
            || !body["max_bytes"].as_u64().is_some_and(|v| v < (1u64 << 53))
            || body["policy"] != "reject_new"
        {
            return Err(bad("invalid_storage_settings"));
        }
        settings["max_bytes"] = body["max_bytes"].clone();
        settings["policy"] = body["policy"].clone();
        atomic_json(&path, &settings).await?;
    }
    let (reserved, _) = app.uploads.snapshot().await;
    let used = app.used.load(Ordering::Relaxed);
    let free = fs2::available_space(&app.store.root)?;
    Ok(Json(
        json!({"max_bytes":settings["max_bytes"],"policy":settings["policy"],"used_bytes":used,"free_bytes":free,"reserved_bytes":reserved,"margin_bytes":100*1024*1024,"pinned_logs":settings["pinned_logs"],"archive":{"enabled":false,"state":"disabled","reason":"not_ported"}}),
    ))
}
fn clean_text(s: &str, max: usize, multiline: bool) -> Result<String> {
    let n = s.nfc().collect::<String>();
    let text = n.trim();
    if text.chars().count() > max
        || text
            .chars()
            .any(|c| c.is_control() && !(multiline && (c == '\n' || c == '\t')))
    {
        return Err(bad("invalid_text"));
    }
    Ok(text.to_owned())
}
async fn bookmarks(
    State(app): State<App>,
    Path(id): Path<String>,
    method: Method,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>)> {
    bookmark_update(&app, &id, None, method, &body).await
}
async fn bookmark_item(
    State(app): State<App>,
    Path((id, bookmark)): Path<(String, String)>,
    method: Method,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>)> {
    bookmark_update(&app, &id, Some(&bookmark), method, &body).await
}
async fn bookmark_update(
    app: &App,
    name: &str,
    bookmark: Option<&str>,
    method: Method,
    body: &[u8],
) -> Result<(StatusCode, Json<Value>)> {
    let lock = app.store.record_lock(name).await;
    let _lock = lock.lock().await;
    let mut meta = app.store.meta(name).await?;
    let mut items = meta["bookmarks"].as_array().cloned().unwrap_or_default();
    if bookmark.is_some_and(|id| !items.iter().any(|b| b["id"] == id)) {
        return Err(missing("not_found"));
    }
    if method != Method::GET {
        if method == Method::DELETE {
            items.retain(|b| b["id"] != bookmark.unwrap());
        } else {
            if body.len() > 8192 {
                return Err(bad("invalid_bookmark"));
            }
            let body: Value = serde_json::from_slice(body).map_err(|_| bad("invalid_bookmark"))?;
            if body.as_object().is_none_or(|v| v.len() != 3) {
                return Err(bad("invalid_bookmark"));
            }
            let time = body["time"]
                .as_f64()
                .filter(|v| {
                    v.is_finite()
                        && (0.0..=86400.).contains(v)
                        && meta["duration"].as_f64().is_none_or(|d| *v <= d)
                })
                .ok_or_else(|| bad("invalid_bookmark"))?;
            let title = clean_text(
                body["title"]
                    .as_str()
                    .ok_or_else(|| bad("invalid_bookmark"))?,
                80,
                true,
            )?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
            let note = clean_text(
                body["note"]
                    .as_str()
                    .ok_or_else(|| bad("invalid_bookmark"))?,
                1000,
                true,
            )?;
            if title.is_empty() || (method == Method::POST && items.len() >= 100) {
                return Err(bad("invalid_bookmark"));
            }
            items.retain(|b| bookmark.is_none_or(|id| b["id"] != id));
            items.push(json!({"id":bookmark.map(String::from).unwrap_or_else(id),"time":time,"title":title,"note":note}));
        }
        items.sort_by(|a, b| {
            a["time"]
                .as_f64()
                .partial_cmp(&b["time"].as_f64())
                .unwrap()
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });
        meta["bookmarks"] = json!(items);
        app.store.save_meta(meta).await?;
    }
    Ok((
        if method == Method::POST {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(json!({"bookmarks":items})),
    ))
}
async fn tags(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    if body
        .as_object()
        .is_none_or(|o| o.keys().any(|k| k != "action" && k != "tags"))
    {
        return Err(bad("invalid_tags"));
    }
    let action = body.get("action").and_then(Value::as_str).unwrap_or("set");
    if !["set", "add", "remove"].contains(&action) {
        return Err(bad("invalid_tags"));
    }
    let values = body["tags"]
        .as_array()
        .filter(|v| v.len() <= 100)
        .ok_or_else(|| bad("invalid_tags"))?;
    let mut tags = BTreeMap::new();
    for tag in values {
        if tag
            .as_object()
            .is_none_or(|o| o.keys().any(|k| k != "name" && k != "color"))
        {
            return Err(bad("invalid_tags"));
        }
        let name = clean_text(
            tag["name"].as_str().ok_or_else(|| bad("invalid_tags"))?,
            32,
            false,
        )?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
        let color = tag.get("color").and_then(Value::as_str).unwrap_or("mint");
        if name.is_empty()
            || ![
                "mint", "blue", "violet", "amber", "rose", "slate", "red", "coral", "lime",
                "yellow", "cyan", "indigo",
            ]
            .contains(&color)
        {
            return Err(bad("invalid_tags"));
        }
        tags.insert(name.to_lowercase(), json!({"name":name,"color":color}));
    }
    let lock = app.store.record_lock(&id).await;
    let _lock = lock.lock().await;
    let mut meta = app.store.meta(&id).await?;
    let mut current = BTreeMap::new();
    if action != "set" {
        for tag in meta["tags"].as_array().cloned().unwrap_or_default() {
            current.insert(tag["name"].as_str().unwrap().to_lowercase(), tag);
        }
    }
    for (key, value) in tags {
        if action == "remove" {
            current.remove(&key);
        } else {
            current.entry(key).or_insert(value);
        }
    }
    if current.len() > 100 {
        return Err(bad("invalid_tags"));
    }
    meta["tags"] = json!(current.into_values().collect::<Vec<_>>());
    let response = json!({"tags":meta["tags"]});
    app.store.save_meta(meta).await?;
    Ok(Json(response))
}
async fn pin(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    let pinned = body["pinned"].as_bool().ok_or_else(|| bad("invalid_pin"))?;
    app.store.folder(&id).await?;
    let _registration = app.store.registration.lock().await;
    let path = app.store.root.join(".storage-settings.json");
    let mut settings: Value = if path.exists() {
        read_json(&path).await?
    } else {
        json!({"max_bytes":0,"policy":"reject_new","pinned_logs":[]})
    };
    let mut pins = settings["pinned_logs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    pins.retain(|v| *v != id);
    if pinned {
        pins.push(json!(id));
    }
    pins.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    settings["pinned_logs"] = json!(pins);
    atomic_json(&path, &settings).await?;
    Ok(Json(json!({"pinned":pinned})))
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
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"status":meta["status"],"error":meta["error"]})),
        )
            .into_response());
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
async fn track(State(app): State<App>, Path(id): Path<String>) -> Result<Response> {
    let target = app.store.meta(&id).await?;
    let key = route_order(&target).0;
    let rows = map_rows(&app).await?;
    let segments=rows.into_iter().filter(|r|r.0==key).map(|(_,segment,m,summary,track)|json!({"id":m["id"],"segment":segment,"duration":summary["duration"],"distance_m":summary["track_distance_m"],"points":track["points"]})).collect::<Vec<_>>();
    Ok(Json(json!({"id":id,"segments":segments})).into_response())
}
fn route_order(meta: &Value) -> (String, Option<u64>) {
    if let Some((route, number)) = meta["name"].as_str().unwrap_or("").rsplit_once(" / 구간 ")
        && let Ok(number) = number.parse()
        && let Some((dongle, route)) = route.split_once("--")
        && let Ok(dongle) = u64::from_str_radix(dongle, 16)
    {
        return (format!("{dongle:x}--{route}"), Some(number));
    }
    (meta["id"].as_str().unwrap_or("").into(), None)
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
) -> Result<Json<Value>> {
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
    let wanted = q
        .get("ids")
        .map(|v| v.split(',').collect::<std::collections::BTreeSet<_>>());
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
    Ok(Json(json!({"routes":routes})))
}
async fn video(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    request: Request,
) -> Result<Response> {
    let source = q.get("source").map(String::as_str).unwrap_or("qcamera");
    let name = match source {
        "qcamera" => "camera.mp4",
        "front" => "fcamera.mp4",
        "wide" => "ecamera.mp4",
        _ => return Err(bad("invalid_video_source")),
    };
    let folder = app.store.folder(&id).await?;
    let prepared = folder.join("prepared").join(name);
    let path = if prepared.exists() {
        prepared
    } else {
        folder.join(name)
    };
    safe_file(&folder, &path).await?;
    serve(path, request).await
}
async fn thumbnail(
    State(app): State<App>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response> {
    let folder = app.store.folder(&id).await?;
    let path = folder.join("prepared/thumbnail.jpg");
    safe_file(&folder, &path).await?;
    serve(path, request).await
}
async fn original(
    State(app): State<App>,
    Path((id, kind)): Path<(String, String)>,
    request: Request,
) -> Result<Response> {
    if !KINDS.contains(&kind.as_str()) {
        return Err(missing("not_found"));
    }
    let folder = app.store.folder(&id).await?;
    let mut path = folder.join(&kind);
    let meta = app.store.meta(&id).await?;
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
        let prepared = folder.join("prepared").join(mp4);
        let retained = if folder.join(mp4).exists() {
            folder.join(mp4)
        } else {
            prepared
        };
        safe_file(&folder, &retained).await?;
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
        crate::media::reverse(&retained, &kind, &output, offset)
            .await
            .map_err(|_| bad("media_export_failed"))?;
        let response = serve(output.clone(), request)
            .await
            .map(|response| attachment(response, &meta, &kind));
        // ServeFile has already opened its file. Linux keeps it readable after unlink.
        let _ = tokio::fs::remove_file(&output).await;
        return response;
    }
    safe_file(&folder, &path).await?;
    Ok(attachment(serve(path, request).await?, &meta, &kind))
}
fn attachment(mut response: Response, meta: &Value, kind: &str) -> Response {
    let raw = match kind {
        "camera.mp4" => "qcamera.ts",
        "fcamera.mp4" => "fcamera.hevc",
        "ecamera.mp4" => "ecamera.hevc",
        _ => kind,
    };
    let fallback = meta["files"]["rlog.zst"]
        .as_str()
        .and_then(|v| v.strip_suffix("rlog.zst"))
        .map(|prefix| format!("{prefix}{raw}"))
        .unwrap_or_else(|| kind.into());
    let source = meta["files"][raw]
        .as_str()
        .or_else(|| meta["files"][kind].as_str())
        .unwrap_or(&fallback)
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(kind);
    let name = if kind.ends_with(".mp4") {
        format!(
            "{}.mp4",
            source.rsplit_once('.').map(|v| v.0).unwrap_or(source)
        )
    } else {
        source.into()
    };
    let encoded = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect::<String>();
    if let Ok(value) = format!("attachment; filename*=UTF-8''{encoded}").parse() {
        response.headers_mut().insert(h::CONTENT_DISPOSITION, value);
    }
    response
}
async fn download_video(
    State(app): State<App>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response> {
    let meta = app.store.meta(&id).await?;
    let kind = meta["video_downloads"]
        .as_array()
        .and_then(|v| {
            v.iter()
                .find(|v| v["source"] == "front")
                .or_else(|| v.first())
        })
        .and_then(|v| v["kind"].as_str())
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
    let source = q.get("source").map(String::as_str).unwrap_or("qcamera");
    let (mp4, raw) = match source {
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
    original(State(app), Path((id, kind.into())), request).await
}
