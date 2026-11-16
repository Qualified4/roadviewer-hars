//! Reading the Python Road Viewer's archive (its Drive folder or NAS share) and copying chosen recordings into HARS
//! (`/api/settings/archive/legacy/*`). The remote is reached only through [`LegacyBackend`], which has no upload,
//! delete, rename or activate; nothing in this module takes a write backend. A copied recording is a new HARS
//! recording registered by the ordinary upload path (admission, rlog duplicate check, camera addition, journal); its
//! storage index never names the Python remote, so no HARS deletion or cleanup can reach the Python files.
use crate::{
    archive::{
        ArchiveRead, BackendError, DriveLegacy, ErrorCode, LegacyBackend, LegacyFile, NasLegacy,
        OWNER_VALUE, PYTHON_DEFAULT_FOLDER, PYTHON_NAS_OWNER, State, drive::LegacyFolder, index,
        monotonic_clock, nas::valid_path,
    },
    http::App,
    store::{atomic_json, now, read_json},
    upload::{Session, UploadFile, file_hash, remove_session},
};
use axum::{
    Json,
    body::Bytes,
    extract::State as AxState,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use md5::Md5;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncWriteExt;

/// The job's last state, kept in the HARS data root only (never in the archive settings).
pub const STATUS_FILE: &str = ".legacy-import.json";
/// Marks an upload session this module feeds; startup removes every such session that holds no journal.
pub const MARKER: &str = "legacy-import.json";
const BODY_LIMIT: usize = 65536;
const MAX_IDS: usize = 1000;
const ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Python's camera files: the original and the MP4 a conversion keeps. An upload takes one file per camera.
const CAMERAS: [(&str, &str); 3] = [
    ("qcamera.ts", "camera.mp4"),
    ("fcamera.hevc", "fcamera.mp4"),
    ("ecamera.hevc", "ecamera.mp4"),
];
const NOTE: &str =
    "Python의 태그·북마크·고정·이름 같은 메타데이터는 원격에 없으므로 가져오지 않습니다.";
static ITEM_ID: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[A-Za-z0-9_-]{1,64}$").unwrap());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
/// A JSON answer, small enough to travel in a .
pub struct Answer(StatusCode, Value);
impl IntoResponse for Answer {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}
fn reply(status: StatusCode, body: Value) -> Answer {
    Answer(status, body)
}
fn error(status: StatusCode, code: &str) -> Answer {
    reply(status, json!({"error": code}))
}
fn error_reason(status: StatusCode, code: &str, reason: &str) -> Answer {
    reply(status, json!({"error": code, "reason": reason}))
}
fn invalid(field: &str, why: &str) -> Answer {
    reply(
        StatusCode::BAD_REQUEST,
        json!({"error": "invalid_legacy_request", "fields": {field: why}}),
    )
}

/// Startup, before upload sessions are loaded: sessions of an interrupted copy (and their partial files) go, so
/// their reservations do not outlive the process. A session whose journal was written is registered as usual.
pub fn cleanup_staging(root: &Path) -> std::io::Result<()> {
    let uploads = root.join(".uploads");
    let entries = match std::fs::read_dir(&uploads) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.join(MARKER).is_file() && !path.join("commit.json").exists() {
            std::fs::remove_dir_all(&path)?;
        }
    }
    Ok(())
}

struct Cancel {
    set: AtomicBool,
    notify: tokio::sync::Notify,
}
impl Cancel {
    fn is_set(&self) -> bool {
        self.set.load(Ordering::SeqCst)
    }
    fn cancel(&self) {
        self.set.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }
}

struct Running {
    cancel: Arc<Cancel>,
    task: tokio::task::JoinHandle<()>,
}

/// One copy job at a time per data root.
pub struct Legacy {
    root: PathBuf,
    slot: tokio::sync::Mutex<Option<Running>>,
    status: Mutex<Option<Value>>,
}

impl Legacy {
    /// A job that was running when the process stopped is reported as interrupted; asking again converges.
    pub async fn open(root: &Path) -> std::io::Result<Self> {
        let path = root.join(STATUS_FILE);
        let mut status = match read_json::<Value>(&path).await {
            Ok(v) if v.is_object() => Some(v),
            Ok(_) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => None,
        };
        if let Some(s) = status.as_mut().filter(|s| s["state"] == "running") {
            s["state"] = json!("interrupted");
            s["finished_at"] = json!(now());
            for r in s["records"].as_array_mut().into_iter().flatten() {
                if matches!(r["state"].as_str(), Some("pending" | "running")) {
                    r["state"] = json!("failed");
                    r["reason"] = json!("interrupted");
                }
            }
            count(s);
            atomic_json(&path, s).await?;
        }
        Ok(Self {
            root: root.to_path_buf(),
            slot: tokio::sync::Mutex::new(None),
            status: Mutex::new(status),
        })
    }
    fn snapshot(&self) -> Option<Value> {
        lock(&self.status).clone()
    }
    async fn update(&self, f: impl FnOnce(&mut Value)) {
        let value = {
            let mut status = lock(&self.status);
            let Some(s) = status.as_mut() else {
                return;
            };
            f(s);
            count(s);
            s.clone()
        };
        if atomic_json(&self.root.join(STATUS_FILE), &value)
            .await
            .is_err()
        {
            tracing::warn!("legacy_import_status_write_failed");
        }
    }
    async fn running(&self) -> bool {
        self.slot
            .lock()
            .await
            .as_ref()
            .is_some_and(|r| !r.task.is_finished())
    }
}

fn count(s: &mut Value) {
    let mut counts = BTreeMap::<String, u64>::new();
    let mut done = 0;
    for r in s["records"].as_array().into_iter().flatten() {
        let state = r["state"].as_str().unwrap_or_default();
        if matches!(state, "imported" | "duplicate" | "failed") {
            done += 1;
            *counts.entry(state.to_owned()).or_default() += 1;
        }
    }
    s["done"] = json!(done);
    s["counts"] = json!({
        "imported": counts.get("imported").copied().unwrap_or(0),
        "duplicate": counts.get("duplicate").copied().unwrap_or(0),
        "failed": counts.get("failed").copied().unwrap_or(0),
    });
}

// ---------------------------------------------------------------- where the Python archive is

#[derive(Clone, Debug)]
enum Location {
    Drive {
        folder_name: String,
        storage_id: Option<String>,
    },
    Nas {
        path: PathBuf,
        storage_id: Option<String>,
    },
}

fn parse_body(body: &Bytes) -> Result<Map<String, Value>, Answer> {
    if body.len() > BODY_LIMIT {
        return Err(error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(o)) => Ok(o),
        _ => Err(invalid("_body", "expected_object")),
    }
}

fn location(app: &App, body: &Map<String, Value>, extra: &[&str]) -> Result<Location, Answer> {
    let allowed = ["backend", "folder_name", "storage_id", "path"];
    if let Some(k) = body
        .keys()
        .find(|k| !allowed.contains(&k.as_str()) && !extra.contains(&k.as_str()))
    {
        return Err(invalid(k, "unknown_field"));
    }
    let storage_id = match body.get("storage_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if ITEM_ID.is_match(s) => Some(s.clone()),
        Some(_) => return Err(invalid("storage_id", "invalid")),
    };
    match body.get("backend").and_then(Value::as_str) {
        Some("gdrive") => {
            if body.contains_key("path") {
                return Err(invalid("path", "unknown_field"));
            }
            let folder_name = match body.get("folder_name") {
                None | Some(Value::Null) => PYTHON_DEFAULT_FOLDER.to_owned(),
                Some(Value::String(s))
                    if !s.trim().is_empty()
                        && s.chars().count() <= 200
                        && !s.chars().any(char::is_control) =>
                {
                    s.clone()
                }
                Some(_) => return Err(invalid("folder_name", "invalid")),
            };
            Ok(Location::Drive {
                folder_name,
                storage_id,
            })
        }
        Some("smb") => {
            if body.contains_key("folder_name") {
                return Err(invalid("folder_name", "unknown_field"));
            }
            let Some(path) = body
                .get("path")
                .and_then(Value::as_str)
                .filter(|p| valid_path(p, app.archive.allow_any_path))
            else {
                return Err(invalid("path", "invalid"));
            };
            let path = PathBuf::from(path);
            let root = &app.store.root;
            if path.starts_with(root) || root.starts_with(&path) {
                return Err(invalid("path", "data_folder"));
            }
            Ok(Location::Nas { path, storage_id })
        }
        _ => Err(invalid("backend", "invalid")),
    }
}

struct Source {
    backend: LegacyBackend,
    info: Value,
}

fn drive_error(e: &BackendError) -> Answer {
    match e.code {
        ErrorCode::AuthExpired => error_reason(
            StatusCode::BAD_GATEWAY,
            "gdrive_auth_failed",
            e.reason().unwrap_or(e.code.as_str()),
        ),
        ErrorCode::Unavailable | ErrorCode::RateLimited => {
            error(StatusCode::SERVICE_UNAVAILABLE, "gdrive_unavailable")
        }
        _ => error_reason(
            StatusCode::BAD_GATEWAY,
            "legacy_remote_error",
            e.reason().unwrap_or(e.code.as_str()),
        ),
    }
}

fn probe_error(backend: &str, reason: Option<&str>) -> Answer {
    let reason = reason.unwrap_or("unavailable");
    match (backend, reason) {
        ("gdrive", "not_connected") => error(StatusCode::CONFLICT, "gdrive_not_connected"),
        ("gdrive", "auth_expired") => {
            error_reason(StatusCode::BAD_GATEWAY, "gdrive_auth_failed", reason)
        }
        ("gdrive", "network" | "rate_limited" | "unavailable" | "probe_pending") => {
            error(StatusCode::SERVICE_UNAVAILABLE, "gdrive_unavailable")
        }
        (
            _,
            "folder_missing"
            | "folder_trashed"
            | "storage_id_mismatch"
            | "path_missing"
            | "sentinel_missing",
        ) => error_reason(StatusCode::NOT_FOUND, "legacy_not_found", reason),
        _ => error_reason(
            StatusCode::SERVICE_UNAVAILABLE,
            "legacy_unavailable",
            reason,
        ),
    }
}

async fn open_source(app: &App, loc: &Location) -> Result<Source, Answer> {
    match loc {
        Location::Drive {
            folder_name,
            storage_id,
        } => {
            if app.archive.settings().gdrive_client_id.is_empty() {
                return Err(error(StatusCode::CONFLICT, "gdrive_not_configured"));
            }
            let Some(client) = app.archive.drive_reader() else {
                return Err(error(StatusCode::CONFLICT, "gdrive_not_selected"));
            };
            if !client.auth.connected() {
                return Err(error(StatusCode::CONFLICT, "gdrive_not_connected"));
            }
            let (folder_id, sid) = match storage_id {
                Some(sid) => {
                    let finder = DriveLegacy::new(client.clone(), sid, None, monotonic_clock());
                    match finder.locate().await {
                        Ok(Some(f)) => (f, sid.clone()),
                        Ok(None) => {
                            return Err(error_reason(
                                StatusCode::NOT_FOUND,
                                "legacy_not_found",
                                "folder_missing",
                            ));
                        }
                        Err(e) => return Err(drive_error(&e)),
                    }
                }
                None => {
                    let folders = DriveLegacy::python_folders(&client, folder_name)
                        .await
                        .map_err(|e| drive_error(&e))?;
                    let ids: BTreeSet<&str> =
                        folders.iter().map(|f| f.storage_id.as_str()).collect();
                    match ids.len() {
                        0 => {
                            return Err(error_reason(
                                StatusCode::NOT_FOUND,
                                "legacy_not_found",
                                "folder_missing",
                            ));
                        }
                        1 => {
                            let LegacyFolder {
                                folder_id,
                                storage_id,
                                ..
                            } = folders[0].clone();
                            (folder_id, storage_id)
                        }
                        _ => {
                            let candidates: Vec<Value> = folders
                                .iter()
                                .map(|f| json!({"storage_id": f.storage_id, "created": f.created}))
                                .collect();
                            return Err(reply(
                                StatusCode::CONFLICT,
                                json!({"error": "legacy_ambiguous", "candidates": candidates}),
                            ));
                        }
                    }
                }
            };
            let legacy = DriveLegacy::new(client, &sid, Some(&folder_id), monotonic_clock());
            let probe = legacy.probe(true).await;
            if probe.state != State::Connected {
                return Err(probe_error("gdrive", probe.reason.as_deref()));
            }
            Ok(Source {
                backend: LegacyBackend::Drive(Arc::new(legacy)),
                info: json!({"backend": "gdrive", "storage_id": sid, "folder_id": folder_id,
                             "folder_name": folder_name}),
            })
        }
        Location::Nas { path, storage_id } => {
            let at = path.clone();
            let sentinel = tokio::task::spawn_blocking(move || NasLegacy::sentinel(&at))
                .await
                .unwrap_or(Err("os_error"))
                .map_err(|reason| probe_error("smb", Some(reason)))?;
            let owner = sentinel.get("owner").and_then(Value::as_str);
            if owner == Some(OWNER_VALUE) {
                return Err(error_reason(
                    StatusCode::CONFLICT,
                    "legacy_not_python",
                    "hars_owned",
                ));
            }
            if owner != Some(PYTHON_NAS_OWNER) {
                return Err(error_reason(
                    StatusCode::CONFLICT,
                    "legacy_not_python",
                    "owner_mismatch",
                ));
            }
            let Some(sid) = sentinel
                .get("storage_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
            else {
                return Err(error_reason(
                    StatusCode::CONFLICT,
                    "legacy_not_python",
                    "storage_id_missing",
                ));
            };
            if storage_id.as_ref().is_some_and(|s| *s != sid) {
                return Err(error(StatusCode::CONFLICT, "legacy_storage_id_mismatch"));
            }
            let legacy = NasLegacy::new(path, &sid, monotonic_clock());
            let probe = legacy.probe(true).await;
            if probe.state != State::Connected {
                return Err(probe_error("smb", probe.reason.as_deref()));
            }
            Ok(Source {
                backend: LegacyBackend::Nas(Arc::new(legacy)),
                info: json!({"backend": "smb", "storage_id": sid, "path": path.to_string_lossy()}),
            })
        }
    }
}

// ---------------------------------------------------------------- what a Python recording holds

type Listing = BTreeMap<String, BTreeMap<String, Vec<LegacyFile>>>;

fn group(files: Vec<LegacyFile>) -> Listing {
    let mut out = Listing::new();
    for f in files {
        out.entry(f.id.clone())
            .or_default()
            .entry(f.kind.clone())
            .or_default()
            .push(f);
    }
    out
}

/// The files an upload takes: the rlog and one file per camera, the original before the MP4 Python kept.
fn choose(kinds: &BTreeSet<&str>) -> (Vec<&'static str>, Vec<&'static str>) {
    let (mut take, mut skip) = (Vec::new(), Vec::new());
    if kinds.contains("rlog.zst") {
        take.push("rlog.zst");
    }
    for (original, mp4) in CAMERAS {
        match (kinds.contains(original), kinds.contains(mp4)) {
            (true, has_mp4) => {
                take.push(original);
                if has_mp4 {
                    skip.push(mp4);
                }
            }
            (false, true) => take.push(mp4),
            (false, false) => {}
        }
    }
    (take, skip)
}

/// Several Python files for one kind (an upload retried by hand): the one whose content matches its mark, else
/// the lowest file id, so every run picks the same.
fn pick(copies: &[LegacyFile]) -> &LegacyFile {
    copies
        .iter()
        .min_by_key(|f| {
            let matching = f.marked_sha256.is_some() && f.marked_sha256 == f.remote.sha256;
            (!matching, f.remote.file_id.clone())
        })
        .expect("a listed kind has a file")
}

fn known_sha(f: &LegacyFile) -> Option<&str> {
    f.marked_sha256.as_deref().or(f.remote.sha256.as_deref())
}

fn rlog_sha(meta: &Value) -> Option<String> {
    meta["content_hashes"]["rlog.zst"]["sha256"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            index::entry(meta, "rlog.zst")["sha256"]
                .as_str()
                .map(str::to_owned)
        })
}

/// The HARS recording this Python one already is: the same id (an earlier export/import), one copied from it, or
/// the same rlog content.
fn in_hars(metas: &[Value], id: &str, storage_id: &str, rlog: Option<&str>) -> Option<Value> {
    let found = |m: &Value, how: &str| Some(json!({"id": m["id"], "match": how}));
    for m in metas {
        if m["id"] == id {
            return found(m, "id");
        }
        let from = &m["imported_from"];
        if from["id"] == id && from["storage_id"] == storage_id {
            return found(m, "imported_from");
        }
    }
    let rlog = rlog?;
    metas
        .iter()
        .find(|m| rlog_sha(m).as_deref() == Some(rlog))
        .and_then(|m| found(m, "sha256"))
}

async fn metas(app: &App) -> Vec<Value> {
    app.store.records.read().await.values().cloned().collect()
}

pub async fn scan(AxState(app): AxState<App>, body: Bytes) -> Answer {
    let body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let loc = match location(&app, &body, &[]) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let source = match open_source(&app, &loc).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let listing = match source.backend.list_media().await {
        Ok(files) => group(files),
        Err(e) if source.info["backend"] == "gdrive" => return drive_error(&e),
        Err(e) => {
            return error_reason(
                StatusCode::SERVICE_UNAVAILABLE,
                "legacy_unavailable",
                e.reason().unwrap_or(e.code.as_str()),
            );
        }
    };
    let storage_id = source.info["storage_id"].as_str().unwrap_or_default();
    let metas = metas(&app).await;
    let mut records = Vec::new();
    let mut importable = 0;
    for (id, kinds) in &listing {
        let present: BTreeSet<&str> = kinds.keys().map(String::as_str).collect();
        let (take, skip) = choose(&present);
        let files: Vec<Value> = kinds
            .iter()
            .map(|(kind, copies)| {
                let f = pick(copies);
                json!({"kind": kind, "bytes": f.remote.bytes, "sha256": known_sha(f),
                       "sha256_marked": f.marked_sha256.is_some(), "copies": copies.len()})
            })
            .collect();
        let ok = take.contains(&"rlog.zst") && ITEM_ID.is_match(id);
        importable += usize::from(ok);
        let reason = if !ITEM_ID.is_match(id) {
            Some("invalid_id")
        } else if !ok {
            Some("rlog_required")
        } else {
            None
        };
        let rlog = kinds.get("rlog.zst").and_then(|c| known_sha(pick(c)));
        records.push(json!({
            "id": id, "files": files, "import_kinds": take, "skipped_kinds": skip,
            "bytes": take.iter().map(|k| pick(&kinds[*k]).remote.bytes).sum::<u64>(),
            "importable": ok, "reason": reason,
            "in_hars": in_hars(&metas, id, storage_id, rlog),
        }));
    }
    reply(
        StatusCode::OK,
        json!({"source": source.info, "records": records, "total": listing.len(), "importable": importable,
               "metadata": "not_imported", "note": NOTE}),
    )
}

// ---------------------------------------------------------------- the copy job

pub async fn status(AxState(app): AxState<App>) -> Answer {
    let running = app.legacy.running().await;
    reply(
        StatusCode::OK,
        json!({"running": running, "job": app.legacy.snapshot()}),
    )
}

pub async fn cancel(AxState(app): AxState<App>) -> Answer {
    let slot = app.legacy.slot.lock().await;
    match slot.as_ref().filter(|r| !r.task.is_finished()) {
        Some(r) => {
            r.cancel.cancel();
            reply(StatusCode::ACCEPTED, json!({"cancelling": true}))
        }
        None => error(StatusCode::CONFLICT, "legacy_import_not_running"),
    }
}

fn parse_ids(body: &Map<String, Value>) -> Result<Vec<String>, Answer> {
    let Some(list) = body.get("ids").and_then(Value::as_array) else {
        return Err(invalid("ids", "expected_list"));
    };
    if list.is_empty() || list.len() > MAX_IDS {
        return Err(invalid("ids", "count"));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for v in list {
        let Some(id) = v.as_str().filter(|s| ITEM_ID.is_match(s)) else {
            return Err(invalid("ids", "invalid_id"));
        };
        if seen.insert(id) {
            out.push(id.to_owned());
        }
    }
    Ok(out)
}

pub async fn import(AxState(app): AxState<App>, body: Bytes) -> Answer {
    let body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let (loc, ids) = match location(&app, &body, &["ids"]).and_then(|l| Ok((l, parse_ids(&body)?)))
    {
        Ok(v) => v,
        Err(r) => return r,
    };
    // Held while the source is checked, so a second request answers 409 instead of racing.
    let Ok(mut slot) = app.legacy.slot.try_lock() else {
        return error(StatusCode::CONFLICT, "legacy_import_running");
    };
    if slot.as_ref().is_some_and(|r| !r.task.is_finished()) {
        return error(StatusCode::CONFLICT, "legacy_import_running");
    }
    let source = match open_source(&app, &loc).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let job = json!({
        "id": crate::store::id(), "state": "running", "source": source.info,
        "started_at": now(), "finished_at": null, "total": ids.len(),
        "records": ids.iter().map(|id| json!({"id": id, "state": "pending"})).collect::<Vec<_>>(),
        "metadata": "not_imported", "note": NOTE, "error": null,
    });
    *lock(&app.legacy.status) = Some(job);
    app.legacy.update(|_| {}).await;
    let cancel = Arc::new(Cancel {
        set: AtomicBool::new(false),
        notify: tokio::sync::Notify::new(),
    });
    let task = tokio::spawn(run(app.clone(), source, ids, cancel.clone()));
    *slot = Some(Running { cancel, task });
    drop(slot);
    reply(
        StatusCode::ACCEPTED,
        json!({"started": true, "job": app.legacy.snapshot()}),
    )
}

/// What happened to one Python recording.
struct Outcome {
    state: &'static str,
    reason: Option<String>,
    status: Option<u16>,
    hars_id: Option<String>,
    mode: Option<&'static str>,
    files: Vec<Value>,
    skipped: Vec<&'static str>,
}
impl Outcome {
    fn failed(reason: impl Into<String>) -> Self {
        Self {
            state: "failed",
            reason: Some(reason.into()),
            status: None,
            hars_id: None,
            mode: None,
            files: Vec::new(),
            skipped: Vec::new(),
        }
    }
    fn done(state: &'static str, hars_id: Option<String>, mode: Option<&'static str>) -> Self {
        Self {
            state,
            reason: None,
            status: None,
            hars_id,
            mode,
            files: Vec::new(),
            skipped: Vec::new(),
        }
    }
    fn apply(self, r: &mut Value) {
        r["state"] = json!(self.state);
        r["reason"] = json!(self.reason);
        r["message"] = json!(self.reason.as_deref().and_then(crate::storage::message));
        r["status"] = json!(self.status);
        r["hars_id"] = json!(self.hars_id);
        r["mode"] = json!(self.mode);
        r["files"] = json!(self.files);
        r["skipped_kinds"] = json!(self.skipped);
    }
}

async fn run(app: App, source: Source, ids: Vec<String>, cancel: Arc<Cancel>) {
    let listing = match source.backend.list_media().await {
        Ok(files) => group(files),
        Err(e) => {
            let reason = e.reason().unwrap_or(e.code.as_str()).to_owned();
            app.legacy
                .update(|s| {
                    for r in s["records"].as_array_mut().into_iter().flatten() {
                        Outcome::failed("listing_failed").apply(r);
                    }
                    s["state"] = json!("failed");
                    s["error"] = json!(reason);
                    s["finished_at"] = json!(now());
                })
                .await;
            return;
        }
    };
    for (i, id) in ids.iter().enumerate() {
        if cancel.is_set() {
            break;
        }
        app.legacy
            .update(|s| s["records"][i]["state"] = json!("running"))
            .await;
        let outcome = import_one(&app, &source, id, listing.get(id), &cancel).await;
        app.legacy
            .update(|s| outcome.apply(&mut s["records"][i]))
            .await;
    }
    let cancelled = cancel.is_set();
    app.legacy
        .update(|s| {
            for r in s["records"].as_array_mut().into_iter().flatten() {
                if matches!(r["state"].as_str(), Some("pending" | "running")) {
                    Outcome::failed("cancelled").apply(r);
                }
            }
            s["state"] = json!(if cancelled { "cancelled" } else { "done" });
            s["finished_at"] = json!(now());
        })
        .await;
}

/// An imported recording's origin, for the scan's `in_hars` and for people reading meta.json. Not a storage
/// location: HARS never treats the Python copy as one of its archived files.
async fn mark_origin(app: &App, hars_id: &str, source: &Value, python_id: &str) {
    let lock = app.store.record_lock(hars_id).await;
    let _guard = lock.lock().await;
    let Ok(mut meta) = app.store.meta(hars_id).await else {
        return;
    };
    meta["imported_from"] = json!({"backend": source["backend"], "storage_id": source["storage_id"],
                                   "id": python_id, "imported_at": now()});
    if app.store.save_meta(meta).await.is_err() {
        tracing::warn!("legacy_import_origin_write_failed");
    }
}

async fn discard(app: &App, session: &Session) {
    let Ok(_life) = session.lifecycle.try_write() else {
        return;
    };
    if session.path.join("commit.json").exists() || !session.path.exists() {
        return;
    }
    if remove_session(&session.path).await.is_ok() {
        app.uploads
            .forget(
                &session
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
            )
            .await;
    }
}

async fn import_one(
    app: &App,
    source: &Source,
    id: &str,
    kinds: Option<&BTreeMap<String, Vec<LegacyFile>>>,
    cancel: &Cancel,
) -> Outcome {
    let Some(kinds) = kinds else {
        return Outcome::failed("not_found");
    };
    let present: BTreeSet<&str> = kinds.keys().map(String::as_str).collect();
    let (take, skipped) = choose(&present);
    let with_skipped = |mut o: Outcome| {
        o.skipped.clone_from(&skipped);
        o
    };
    if !take.contains(&"rlog.zst") {
        return with_skipped(Outcome::failed("rlog_required"));
    }
    let picks: Vec<&LegacyFile> = take.iter().map(|k| pick(&kinds[*k])).collect();
    if picks.iter().any(|f| f.remote.bytes == 0) {
        return with_skipped(Outcome::failed("empty_file"));
    }
    // Every file's content is already a HARS recording's: nothing to copy.
    let metas = metas(app).await;
    if let Some(m) = metas.iter().find(|m| {
        picks
            .iter()
            .all(|f| known_sha(f).is_some_and(|sha| m["content_hashes"][&f.kind]["sha256"] == sha))
    }) {
        return with_skipped(Outcome::done(
            "duplicate",
            m["id"].as_str().map(str::to_owned),
            None,
        ));
    }
    let files: Vec<UploadFile> = picks
        .iter()
        .map(|f| UploadFile {
            name: format!("{id}/{}", f.kind),
            size: f.remote.bytes,
            sha256: None,
        })
        .collect();
    let marker = json!({"source": source.info, "id": id, "created_at": now()});
    let session = match app.uploads.create_marked(files, (MARKER, &marker)).await {
        Ok(s) => s,
        Err(e) => {
            let mut o = Outcome::failed(e.1);
            o.status = Some(e.0.as_u16());
            return with_skipped(o);
        }
    };
    let mut reports = Vec::new();
    let staged = async {
        let _life = session.lifecycle.read().await;
        for (i, f) in picks.iter().enumerate() {
            reports.push(
                fetch(
                    source,
                    f,
                    &session.path.join(i.to_string()),
                    &session,
                    cancel,
                )
                .await?,
            );
        }
        Ok::<_, String>(())
    }
    .await;
    if let Err(reason) = staged {
        discard(app, &session).await;
        return with_skipped(Outcome::failed(reason));
    }
    crate::fault::point("legacy_import_before_finish");
    let name = session
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let result = app
        .uploads
        .finish(&name, false, &HeaderMap::new(), Value::Null)
        .await;
    let mut outcome = match result {
        Ok((_, result)) => {
            let first = |k: &str| result[k][0]["id"].as_str().map(str::to_owned);
            if let Some(new) = first("logs") {
                mark_origin(app, &new, &source.info, id).await;
                Outcome::done("imported", Some(new), Some("new"))
            } else if let Some(updated) = first("updated") {
                Outcome::done("imported", Some(updated), Some("cameras_added"))
            } else if let Some(dup) = first("duplicates") {
                Outcome::done("duplicate", Some(dup), None)
            } else {
                Outcome::failed("not_registered")
            }
        }
        Err(e) => {
            discard(app, &session).await;
            let mut o = Outcome::failed(e.1);
            o.status = Some(e.0.as_u16());
            o
        }
    };
    if outcome.state != "failed" {
        outcome.files = reports;
    }
    with_skipped(outcome)
}

enum Fetch {
    Retry(String),
    Fail(String),
}
fn reason_of(e: &BackendError) -> String {
    e.reason().unwrap_or(e.code.as_str()).to_owned()
}
fn retryable(e: &BackendError) -> bool {
    e.retryable || matches!(e.code, ErrorCode::Unavailable | ErrorCode::RateLimited)
}

/// One Python file into the session's slot `dest`, verified; a failed attempt leaves nothing behind.
async fn fetch(
    source: &Source,
    f: &LegacyFile,
    dest: &Path,
    session: &Session,
    cancel: &Cancel,
) -> Result<Value, String> {
    for attempt in 1..=ATTEMPTS {
        let outcome = fetch_once(source, f, dest, session, cancel).await;
        if outcome.is_err() {
            let _ = tokio::fs::remove_file(dest).await;
        }
        match outcome {
            Ok(v) => return Ok(v),
            Err(Fetch::Retry(_)) if attempt < ATTEMPTS && !cancel.is_set() => {
                tokio::select! {
                    _ = tokio::time::sleep(RETRY_DELAY * attempt) => {}
                    _ = cancel.wait() => return Err("cancelled".into()),
                }
            }
            Err(Fetch::Retry(r) | Fetch::Fail(r)) => return Err(r),
        }
    }
    unreachable!("the last attempt returns")
}

async fn nas_stamp(
    source: &Source,
    f: &LegacyFile,
) -> Option<(u64, Option<std::time::SystemTime>)> {
    let LegacyBackend::Nas(nas) = &source.backend else {
        return None;
    };
    let path = nas.path_for(&f.id, &f.kind).ok()?;
    tokio::task::spawn_blocking(move || {
        let m = std::fs::metadata(path).ok()?;
        Some((m.len(), m.modified().ok()))
    })
    .await
    .ok()?
}

async fn fetch_once(
    source: &Source,
    f: &LegacyFile,
    dest: &Path,
    session: &Session,
    cancel: &Cancel,
) -> Result<Value, Fetch> {
    let nas = matches!(source.backend, LegacyBackend::Nas(_));
    let before = nas_stamp(source, f).await;
    let mut reader = tokio::select! {
        r = source.backend.open(&f.remote, 0, None) => r.map_err(|e| {
            if retryable(&e) { Fetch::Retry(reason_of(&e)) } else { Fetch::Fail(reason_of(&e)) }
        })?,
        _ = cancel.wait() => return Err(Fetch::Fail("cancelled".into())),
    };
    let part = {
        let mut name = dest.file_name().unwrap_or_default().to_os_string();
        name.push(".part");
        dest.with_file_name(name)
    };
    let outcome = async {
        let io = |e: std::io::Error| Fetch::Fail(crate::error::ApiError::from(e).1.to_owned());
        let mut file = tokio::fs::File::create(&part).await.map_err(io)?;
        let (mut sha, mut md5, mut count) = (Sha256::new(), Md5::new(), 0u64);
        loop {
            let chunk = tokio::select! {
                c = reader.next_chunk() => c.map_err(|e| Fetch::Retry(reason_of(&e)))?,
                _ = cancel.wait() => return Err(Fetch::Fail("cancelled".into())),
            };
            let Some(chunk) = chunk else {
                break;
            };
            count += chunk.len() as u64;
            // The listed size is what was reserved: a file that grew since, or a server sending
            // more, stops here instead of filling the disk before the size check.
            if count > f.remote.bytes {
                return Err(Fetch::Retry("size_mismatch".into()));
            }
            sha.update(&chunk);
            md5.update(&chunk);
            file.write_all(&chunk).await.map_err(io)?;
            session.touch();
            crate::fault::point("legacy_import_mid_file");
        }
        if count != f.remote.bytes {
            return Err(Fetch::Retry("size_mismatch".into()));
        }
        let sha = hex::encode(sha.finalize());
        let md5 = hex::encode(md5.finalize());
        let mut verified = vec!["size"];
        for (name, expected, actual) in [
            ("rv_sha256", f.marked_sha256.as_deref(), &sha),
            ("sha256Checksum", f.remote.sha256.as_deref(), &sha),
            ("md5Checksum", f.remote.md5.as_deref(), &md5),
        ] {
            if let Some(expected) = expected {
                if expected != actual {
                    return Err(Fetch::Fail("checksum_mismatch".into()));
                }
                verified.push(name);
            }
        }
        file.sync_all().await.map_err(io)?;
        drop(file);
        if nas && nas_stamp(source, f).await != before {
            return Err(Fetch::Retry("source_changed".into()));
        }
        tokio::fs::rename(&part, dest).await.map_err(io)?;
        if nas {
            // Nothing on a share vouches for the bytes: read the copy back.
            if file_hash(dest).await.map_err(io)? != sha {
                return Err(Fetch::Fail("readback_mismatch".into()));
            }
            verified.push("readback");
        }
        Ok(json!({"kind": f.kind, "bytes": count, "sha256": sha,
                  "sha256_marked": f.marked_sha256.is_some(), "verified_by": verified}))
    }
    .await;
    if outcome.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    outcome
}

// ---------------------------------------------------------------- without a server

const USAGE: &str = "usage: roadviewer-hars legacy-archive scan|import JSON (the body of \
                     /api/settings/archive/legacy/scan or /import)";

/// `roadviewer-hars legacy-archive scan|import JSON`: the routes' work on a data folder no server holds (opening
/// it takes the same instance lock). Prints the scan or the finished job; copies wait for the server to convert.
pub async fn cli(app: App, args: &[String]) -> anyhow::Result<i32> {
    anyhow::ensure!(args.len() == 2, USAGE);
    let body = Bytes::from(args[1].clone());
    let answer = match args[0].as_str() {
        "scan" => scan(AxState(app.clone()), body).await,
        "import" => {
            let started = import(AxState(app.clone()), body).await;
            if started.0 == StatusCode::ACCEPTED {
                while app.legacy.running().await {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                reply(StatusCode::OK, json!({"job": app.legacy.snapshot()}))
            } else {
                started
            }
        }
        _ => anyhow::bail!(USAGE),
    };
    println!("{}", answer.1);
    Ok(if answer.0.is_success() { 0 } else { 1 })
}
