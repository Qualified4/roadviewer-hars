//! HTTP handlers of the video archive (Python `server.py`: `/api/settings/archive`, `/run`, `/gdrive/connect`,
//! `/gdrive/disconnect`, `/api/logs/<id>/archive/retry`, and the storage panel's `archive` block).
use crate::{
    archive::{
        Archive, ArchiveRead, ErrorCode, Probe, State, WriteBackend, index,
        pending::PendingDeletes, settings,
    },
    error::{Result, missing},
    http::App,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State as AxState},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value, json};
use std::sync::Arc;

const BODY_LIMIT: usize = 65536;

fn reply(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}
fn error(status: StatusCode, code: &str) -> Response {
    reply(status, json!({"error": code}))
}

/// Python `archive_preferences`: the editable preferences, never the OAuth client.
fn preferences(archive: &Archive) -> Value {
    let s = archive.settings();
    let mut out = settings::values(&s);
    out.insert("effective_mode".into(), json!(s.effective_mode().as_str()));
    out.insert(
        "client_configured".into(),
        json!(!s.gdrive_client_id.is_empty()),
    );
    if let Some(e) = archive.settings_error() {
        out.insert("settings_error".into(), json!(e));
    }
    Value::Object(out)
}

pub async fn preferences_api(AxState(app): AxState<App>, method: Method, body: Bytes) -> Response {
    let archive = &app.archive;
    if method == Method::GET {
        return reply(StatusCode::OK, preferences(archive));
    }
    if body.len() > BODY_LIMIT {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large");
    }
    let body: Option<Value> = serde_json::from_slice(&body).ok();
    // The cycle lock spans validation, persistence and swap: even a newly starting job sees one configuration.
    let Ok(_cycle) = archive.cycle.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "archive_running");
    };
    let Some(Value::Object(body)) = body else {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid_archive_settings", "fields": {"_body": "expected_object"}}),
        );
    };
    let mut data = settings::values(&archive.settings());
    data.extend(body);
    let errors = settings::validate(&Value::Object(data.clone()), archive.allow_any_path);
    if !errors.is_empty() {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid_archive_settings", "fields": errors}),
        );
    }
    let updated = settings::parse(&data, &archive.options, archive.allow_any_path);
    let doc = settings::document(&updated, &archive.migrated_from());
    let path = archive.root.join(settings::FILENAME);
    if crate::store::atomic_json(&path, &doc).await.is_err() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "settings_write_failed");
    }
    archive.swap_settings(updated);
    reply(StatusCode::OK, preferences(archive))
}

/// GET: what 'archive everything eligible' would process now and the latest run. POST: starts that run.
pub async fn run(AxState(app): AxState<App>, method: Method) -> Response {
    let archive = &app.archive;
    if !archive.settings().enabled {
        return error(StatusCode::CONFLICT, "archive_disabled");
    }
    if method == Method::GET {
        let status = archive.status();
        return reply(
            StatusCode::OK,
            json!({"plan": archive.manual_plan().await, "running": status["running"], "manual": status["manual"],
                   "manual_active": status["manual_active"]}),
        );
    }
    match archive.start_manual().await {
        Some(job) => reply(
            StatusCode::ACCEPTED,
            json!({"started": true, "manual": job}),
        ),
        None => error(StatusCode::CONFLICT, "archive_running"),
    }
}

/// A remote backend's (state, reason) from its cache; a stale one is refreshed in the background.
fn remote_result(archive: &Archive) -> (State, Option<String>) {
    let backend = archive.backend();
    let WriteBackend::Drive(drive) = &*backend else {
        return (State::Unavailable, None);
    };
    match drive.last_result() {
        Some(p) => (p.state, p.reason),
        None => {
            let b = backend.clone();
            tokio::spawn(async move {
                b.probe(false).await;
            });
            match drive.known_result() {
                Some(p) => (p.state, p.reason),
                None => (State::Checking, Some("probe_pending".into())),
            }
        }
    }
}

const FOLDER_REASONS: [&str; 3] = ["folder_missing", "folder_trashed", "storage_id_mismatch"];

fn gdrive_auth_error(archive: &Archive, reason: Option<&str>) -> Option<String> {
    let conn = archive.connection();
    conn.and_then(|c| c.last_error())
        .or_else(|| archive.last_auth_error())
        .or_else(|| (reason == Some("auth_expired")).then(|| "auth_expired".to_owned()))
}

fn gdrive_status(archive: &Archive, reason: Option<&str>) -> Value {
    let backend = archive.backend();
    let WriteBackend::Drive(drive) = &*backend else {
        return Value::Null;
    };
    let pending = archive.connection().and_then(|c| c.pending());
    json!({
        "configured": !archive.settings().gdrive_client_id.is_empty(),
        "connected": drive.auth.connected(),
        "folder_ok": drive.auth.folder_id().is_some() && !reason.is_some_and(|r| FOLDER_REASONS.contains(&r)),
        "pending": pending.map(|p| json!({"user_code": p.user_code, "verification_url": p.verification_url, "expires_at": p.expires_at})),
        "last_auth_error": gdrive_auth_error(archive, reason),
    })
}

/// Python `StoragePolicy.archive()`: the storage panel's archive block (never a token or secret).
pub async fn archive_block(archive: &Arc<Archive>) -> Value {
    let ctx = archive.ctx();
    let s = &ctx.settings;
    let remote = s.remote();
    let (state, reason, used, free, usage, measuring) = match &*ctx.backend {
        WriteBackend::Drive(drive) => {
            let measuring = s.enabled && drive.refresh_usage();
            let (state, reason) = remote_result(archive);
            let usage = drive.last_usage();
            let (used, free) = usage
                .as_ref()
                .map_or((0, 0), |u| (u.used, u.free.unwrap_or(0)));
            (state, reason, used, free, usage, measuring)
        }
        WriteBackend::Nas(nas) => {
            let Probe { state, reason } = nas.probe(false).await;
            let (used, free) = if state == State::Connected {
                nas.usage()
                    .await
                    .map_or((0, 0), |u| (u.used, u.free.unwrap_or(0)))
            } else {
                (0, 0)
            };
            (state, reason, used, free, None, false)
        }
    };
    let totals = if s.enabled {
        archive.index_totals().await
    } else {
        [0; 5]
    };
    let pending = if s.enabled {
        PendingDeletes::new(&archive.root)
            .count()
            .await
            .unwrap_or(0)
    } else {
        0
    };
    let status = archive.status();
    let mut block = json!({
        "enabled": s.enabled, "state": state.as_str(), "reason": reason, "path_configured": s.path.is_some(),
        "used_bytes": used, "free_bytes": free, "max_bytes": s.nas_max_bytes, "margin_bytes": s.nas_margin_bytes,
        "last_cycle_at": status["last_cycle_at"], "last_error": status["last_error"],
        "consecutive_failures": status["consecutive_failures"], "running": status["running"],
        "pending_deletes": pending, "archived_files": totals[0], "archived_bytes": totals[1],
        "missing_archived": status["missing_archived"], "manual": status["manual"], "manual_active": status["manual_active"],
    });
    if let Some(e) = archive.settings_error() {
        block["settings_error"] = json!(e);
    }
    if !remote {
        return block;
    }
    let extra = json!({
        "backend": s.backend.as_str(), "mode": s.mode.as_str(), "effective_mode": s.effective_mode().as_str(),
        "local_delete_allowed": s.allow_local_delete, "usage_reliable": usage.as_ref().is_some_and(|u| u.reliable),
        "usage_known": usage.is_some(), "usage_measuring": measuring,
        "remote_copies_files": totals[2], "remote_copies_bytes": totals[3],
        "archive_errors": {"remote_mismatch": totals[4]},
        "gdrive": gdrive_status(archive, reason.as_deref()),
    });
    if let (Value::Object(b), Value::Object(e)) = (&mut block, extra) {
        b.extend(e);
    }
    block
}

pub async fn gdrive_connect(AxState(app): AxState<App>) -> Response {
    let archive = &app.archive;
    let Some(conn) = archive.connection() else {
        return error(StatusCode::CONFLICT, "gdrive_not_selected");
    };
    if archive.settings().gdrive_client_id.is_empty() {
        return error(StatusCode::CONFLICT, "gdrive_not_configured");
    }
    // Signed in and working: disconnect first. A grant Google no longer accepts can be renewed in place.
    let reason = conn.backend.last_result().and_then(|p| p.reason);
    if conn.backend.auth.connected() && gdrive_auth_error(archive, reason.as_deref()).is_none() {
        return error(StatusCode::CONFLICT, "gdrive_connected");
    }
    match conn.start().await {
        Ok(flow) => {
            let left = (flow.expires_at - (conn.backend.auth.clock)()).max(0.0) as u64;
            reply(
                StatusCode::ACCEPTED,
                json!({"user_code": flow.user_code, "verification_url": flow.verification_url, "expires_in": left}),
            )
        }
        Err(e) if matches!(e.code, ErrorCode::Unavailable | ErrorCode::RateLimited) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "gdrive_unavailable")
        }
        Err(e) => reply(
            StatusCode::BAD_GATEWAY,
            json!({"error": "gdrive_auth_failed", "reason": e.reason().unwrap_or(e.code.as_str())}),
        ),
    }
}

/// Forgets the grant; the index is untouched: Drive files stay recorded and answer storage_unavailable.
pub async fn gdrive_disconnect(AxState(app): AxState<App>) -> Response {
    let Some(conn) = app.archive.connection() else {
        return error(StatusCode::CONFLICT, "gdrive_not_selected");
    };
    conn.disconnect();
    reply(StatusCode::OK, json!({"connected": false}))
}

/// A file whose remote copy did not verify is skipped by the archiver; this clears the error so the next cycle
/// uploads it again. Local and remote files are untouched (the bad copy stays on Drive).
pub async fn retry(
    AxState(app): AxState<App>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response> {
    if body.len() > BODY_LIMIT {
        return Ok(error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"));
    }
    let parsed: Option<Value> = if body.is_empty() {
        Some(json!({}))
    } else {
        serde_json::from_slice(&body).ok()
    };
    let kinds: Option<Vec<String>> = match parsed {
        Some(Value::Object(o)) if o.keys().all(|k| k == "kinds") => match o.get("kinds") {
            None => Some(
                crate::archive::MEDIA_KINDS
                    .iter()
                    .map(|k| (*k).to_owned())
                    .collect(),
            ),
            Some(Value::Array(a)) => a.iter().map(|k| k.as_str().map(str::to_owned)).collect(),
            Some(_) => None,
        },
        _ => None,
    };
    let Some(kinds) = kinds.filter(|k| !k.is_empty() && k.iter().all(|k| index::known_kind(k)))
    else {
        return Ok(error(StatusCode::BAD_REQUEST, "invalid_archive_retry"));
    };
    app.store.folder(&id).await?;
    let lock = app.store.record_lock(&id).await;
    let guard = lock.lock().await;
    let mut meta = app
        .store
        .meta(&id)
        .await
        .map_err(|_| missing("not_found"))?;
    let failed: Vec<&str> = crate::archive::MEDIA_KINDS
        .iter()
        .copied()
        .filter(|k| {
            kinds.iter().any(|x| x == k) && index::entry(&meta, k)["error"] == "remote_mismatch"
        })
        .collect();
    if failed.is_empty() {
        return Ok(error(StatusCode::CONFLICT, "no_archive_error"));
    }
    if let Some(storage) = meta["storage"].as_object_mut() {
        for kind in &failed {
            if let Some(e) = storage.get_mut(*kind).and_then(Value::as_object_mut) {
                e.remove("error");
                if e.len() == 1 && e.get("location") == Some(&json!("local")) {
                    storage.remove(*kind);
                }
            }
        }
    }
    app.store.save_meta(meta).await?;
    drop(guard);
    app.archive.forget_index();
    Ok(reply(StatusCode::OK, json!({"retried": failed})))
}

/// The storage index as API responses show it (Python `public_meta`).
pub fn public_meta(meta: &Value) -> Value {
    let mut m = meta.clone();
    if let Some(storage) = meta.get("storage") {
        m["storage"] = index::public_storage(storage);
    }
    m
}

/// Python `SOURCES`' (original, kept MP4) per camera.
const SOURCES: [(&str, &str, &str); 3] = [
    ("qcamera", "qcamera.ts", "camera.mp4"),
    ("front", "fcamera.hevc", "fcamera.mp4"),
    ("wide", "ecamera.hevc", "ecamera.mp4"),
];
const STATE_RANK: [&str; 5] = [
    "available",
    "corrupt",
    "checking",
    "storage_unavailable",
    "missing",
];

fn parts() -> [(&'static str, Vec<&'static str>); 4] {
    [
        ("rlog", vec!["rlog.zst"]),
        (SOURCES[0].0, vec![SOURCES[0].1, SOURCES[0].2]),
        (SOURCES[1].0, vec![SOURCES[1].1, SOURCES[1].2]),
        (SOURCES[2].0, vec![SOURCES[2].1, SOURCES[2].2]),
    ]
}

/// The archive's state for the library list, checked once per request (Python `list_archive_state`).
pub async fn list_state(archive: &Archive) -> State {
    match &*archive.backend() {
        WriteBackend::Nas(nas) => nas.probe(false).await.state,
        WriteBackend::Drive(drive) => match remote_result(archive).0 {
            State::Checking => drive.known_result().map_or(State::Checking, |p| p.state),
            s => s,
        },
    }
}

/// Python `build_list_row`'s archive fields: `video_state` and `storage_locations` on rows touched by the archive,
/// `remote_copies` and `archive_error` only on rows that have them. The index itself never reaches the list.
pub fn list_fields(archive: &Archive, meta: &Value, share: Option<State>) -> Map<String, Value> {
    let id = meta["id"].as_str().unwrap_or_default();
    let host = &archive.host;
    let registered = |k: &str| host.stored(id, meta, k) || index::location(meta, k) == "removed";
    let mut out = Map::new();
    if let Some(share) = share {
        let mut states = Map::new();
        for (source, original, mp4) in SOURCES {
            let found = [original, mp4]
                .into_iter()
                .filter(|k| registered(k))
                .map(|k| archive.indexed_state(id, meta, k, share))
                .min_by_key(|s| STATE_RANK.iter().position(|r| r == s).unwrap_or(0));
            if let Some(state) = found {
                states.insert(source.into(), json!(state));
            }
        }
        let mut locations = Map::new();
        for (part, kinds) in parts() {
            let where_: Vec<String> = kinds
                .iter()
                .filter(|k| registered(k))
                .map(|k| index::location(meta, k))
                .collect();
            if let Some(w) = ["nas", "gdrive", "local", "removed"]
                .into_iter()
                .find(|w| where_.iter().any(|x| x == w))
            {
                locations.insert(part.into(), json!(w));
            }
        }
        out.insert("video_state".into(), Value::Object(states));
        out.insert("storage_locations".into(), Value::Object(locations));
    }
    let mut copies = Map::new();
    for (part, kinds) in parts() {
        for k in kinds {
            if let Some(block) = index::remote(meta, k)
                && index::location(meta, k) == "local"
                && let Some(b) = block["backend"].as_str()
            {
                copies.entry(part).or_insert(json!(b));
            }
        }
    }
    if !copies.is_empty() {
        out.insert("remote_copies".into(), Value::Object(copies));
    }
    let errors: Map<String, Value> = crate::archive::MEDIA_KINDS
        .iter()
        .filter(|k| index::entry(meta, k)["error"] == "remote_mismatch")
        .map(|k| ((*k).to_owned(), json!("remote_mismatch")))
        .collect();
    if !errors.is_empty() {
        out.insert("archive_error".into(), Value::Object(errors));
    }
    out
}

/// Whether a row carries `video_state`/`storage_locations` (Python: any kind not local).
pub fn touched(meta: &Value) -> bool {
    crate::archive::MEDIA_KINDS
        .iter()
        .any(|k| index::location(meta, k) != "local")
}

/// After an explicit deletion (Python `process_pending`): the NAS files go now, outside the locks; a remote
/// backend's files are only queued and removed by the archiver cycle, so Drive never holds a request.
pub async fn process_pending(archive: &Archive) {
    if let WriteBackend::Nas(nas) = &*archive.backend()
        && PendingDeletes::new(&archive.root).path.exists()
    {
        let _ = PendingDeletes::new(&archive.root).process_nas(nas).await;
    }
}
