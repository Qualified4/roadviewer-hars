//! HTTP handlers of the storage policy: settings, pins, deletion and upload cleanup.
use crate::{
    error::{Result, bad, conflict},
    http::App,
    storage,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::Method,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::atomic::Ordering, time::Duration};

/// A conversion is stopped before its recording is deleted (Python terminates the decoder).
const CONVERSION_STOP: Duration = Duration::from_secs(30);

pub async fn settings(State(app): State<App>, method: Method, body: Bytes) -> Result<Json<Value>> {
    let _registration = app.store.registration.lock().await;
    let mut settings = storage::load_settings(&app.store.root).await?;
    if method == Method::POST {
        let (max, policy) =
            storage::parse_settings_update(&body).ok_or_else(|| bad("invalid_storage_settings"))?;
        settings["max_bytes"] = json!(max);
        settings["policy"] = json!(policy);
        storage::save_settings(&app.store.root, &settings).await?;
    }
    let used = app.store.used().await?;
    app.used.store(used, Ordering::Relaxed);
    Ok(Json(json!({
        "max_bytes": settings["max_bytes"],
        "policy": settings["policy"],
        "used_bytes": used,
        "free_bytes": fs2::available_space(&app.store.root)?,
        "reserved_bytes": storage::reserved(&app.store.root).await?,
        "archive": {"enabled": false, "state": "disabled", "reason": "not_ported"},
    })))
}

pub async fn pin(
    State(app): State<App>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Value>> {
    let pinned = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("pinned").and_then(Value::as_bool))
        .ok_or_else(|| bad("invalid_pin"))?;
    let _registration = app.store.registration.lock().await;
    app.store.folder(&id).await?;
    let mut settings = storage::load_settings(&app.store.root).await?;
    let mut pins = storage::pins(&settings);
    if pinned {
        pins.insert(id);
    } else {
        pins.remove(&id);
    }
    settings["pinned_logs"] = json!(pins);
    storage::save_settings(&app.store.root, &settings).await?;
    Ok(Json(json!({"pinned":pinned})))
}

pub async fn delete_log(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Result<Json<Value>> {
    let skip_pinned = q.get("skip_pinned").is_some_and(|v| v == "1");
    if let Some(skipped) = refusal(&app, &id, skip_pinned).await? {
        return Ok(skipped);
    }
    // Every upload finish waits on the registration lock, so the conversion is stopped outside it
    // and the recording is judged again once it is held.
    stop_conversion(&app, &id).await?;
    let registration = app.store.registration.lock().await;
    if let Some(skipped) = refusal(&app, &id, skip_pinned).await? {
        return Ok(skipped);
    }
    let deleted = storage::delete_records(
        &app.store,
        &registration,
        std::slice::from_ref(&id),
        &[],
        "manual",
        |m| m["status"] == "processing",
    )
    .await?;
    drop(registration);
    if deleted.is_empty() {
        // Started converting again between the stop and the record lock.
        return Err(conflict("conversion_busy"));
    }
    app.jobs.progress.write().await.remove(&id);
    if let Ok(used) = app.store.used().await {
        app.used.store(used, Ordering::Relaxed);
    }
    Ok(Json(json!({"deleted":id})))
}

async fn refusal(app: &App, id: &str, skip_pinned: bool) -> Result<Option<Json<Value>>> {
    app.store.folder(id).await?;
    if skip_pinned && storage::pins(&storage::load_settings(&app.store.root).await?).contains(id) {
        return Ok(Some(Json(json!({"skipped":"pinned"}))));
    }
    let guards = storage::guards(&app.store.root, &[], &std::collections::BTreeSet::new()).await?;
    if storage::manual_protection(id, &guards).is_some() {
        // Its registration journal still moves files into this folder.
        return Err(conflict("upload_busy"));
    }
    Ok(None)
}

async fn stop_conversion(app: &App, id: &str) -> Result<()> {
    if !app.jobs.busy(id).await {
        return Ok(());
    }
    {
        let lock = app.store.record_lock(id).await;
        let _guard = lock.lock().await;
        let mut meta = app.store.meta(id).await?;
        if meta["status"] == "processing" {
            // A conversion already past its child process publishes, sees this and stops.
            meta["cancel_requested"] = meta["conversion_id"].clone();
            app.store.save_meta(meta).await?;
        }
    }
    app.jobs.cancel(id).await;
    let deadline = tokio::time::Instant::now() + CONVERSION_STOP;
    while app.jobs.busy(id).await {
        if tokio::time::Instant::now() >= deadline {
            return Err(conflict("conversion_busy"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

pub async fn cleanup(State(app): State<App>) -> Result<Json<Value>> {
    let removed = app.uploads.discard_incomplete().await?;
    if let Ok(used) = app.store.used().await {
        app.used.store(used, Ordering::Relaxed);
    }
    Ok(Json(json!({"removed_bytes":removed})))
}
