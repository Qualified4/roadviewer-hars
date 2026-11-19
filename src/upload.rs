use crate::{
    auth::{Auth, equal, header, random_token, valid_hash, valid_nonce},
    error::{ApiError, Result, bad, conflict, missing, unauthorized},
    store::{Store, atomic_json, id, now, read_json, sha, valid_id},
};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{Mutex, RwLock},
};

pub const KINDS: &[&str] = &[
    "rlog.zst",
    "qcamera.ts",
    "fcamera.hevc",
    "ecamera.hevc",
    "camera.mp4",
    "fcamera.mp4",
    "ecamera.mp4",
    "qcamera.mp4",
];
const MARGIN: u64 = 100 * 1024 * 1024;
const MAX_BATCH: u64 = 8 * 1024 * 1024 * 1024;
const IDLE: f64 = 900.;
static ROUTE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^(?:[0-9a-fA-F]{8}--[0-9a-fA-F]{10}|\d{4}-\d{2}-\d{2}--\d{2}-\d{2}-\d{2})$")
        .unwrap()
});

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadFile {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub sha256: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    #[serde(rename = "batch_id")]
    _batch_id: String,
    segments: Vec<Segment>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Segment {
    route: String,
    segment: u32,
    files: Vec<BatchFile>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchFile {
    kind: String,
    size: u64,
    sha256: Option<String>,
}

pub fn camera(kind: &str) -> Option<&'static str> {
    match kind {
        "qcamera.ts" | "camera.mp4" | "qcamera.mp4" => Some("qcamera"),
        "fcamera.hevc" | "fcamera.mp4" => Some("front"),
        "ecamera.hevc" | "ecamera.mp4" => Some("wide"),
        _ => None,
    }
}
pub fn flatten_batch(body: &Value, streaming: bool, max_log: u64) -> Result<Vec<UploadFile>> {
    if !body.is_object()
        || body.as_object().unwrap().len() != 2
        || body
            .as_object()
            .unwrap()
            .keys()
            .any(|k| k != "batch_id" && k != "segments")
        || !body["batch_id"].as_str().is_some_and(valid_nonce)
    {
        return Err(bad("invalid_batch"));
    }
    if !body["segments"]
        .as_array()
        .is_some_and(|v| (1..=50).contains(&v.len()))
    {
        return Err(bad("invalid_segments"));
    }
    for segment in body["segments"].as_array().unwrap() {
        if segment.as_object().is_none_or(|o| {
            o.len() != 3
                || o.keys()
                    .any(|k| !matches!(k.as_str(), "route" | "segment" | "files"))
        }) || !segment["route"].as_str().is_some_and(|v| ROUTE.is_match(v))
            || !segment["segment"].as_u64().is_some_and(|v| v <= 999999)
            || !segment["files"]
                .as_array()
                .is_some_and(|v| (1..=4).contains(&v.len()))
        {
            return Err(bad("invalid_segment"));
        }
        for file in segment["files"].as_array().unwrap() {
            if file.as_object().is_none_or(|o| {
                o.len() != 3
                    || o.keys()
                        .any(|k| !matches!(k.as_str(), "kind" | "size" | "sha256"))
            }) || !file["kind"].as_str().is_some_and(|v| KINDS.contains(&v))
                || !file["size"].as_u64().is_some_and(|v| v > 0)
                || !(file["sha256"].as_str().is_some_and(valid_hash)
                    || (file["sha256"].is_null() && streaming))
            {
                return Err(bad("invalid_file"));
            }
        }
    }
    let batch: Batch = serde_json::from_value(body.clone()).map_err(|_| bad("invalid_segment"))?;
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for seg in batch.segments {
        if !ROUTE.is_match(&seg.route)
            || seg.segment > 999999
            || !(1..=4).contains(&seg.files.len())
        {
            return Err(bad("invalid_segment"));
        }
        if !seen.insert((seg.route.clone(), seg.segment)) {
            return Err(bad("duplicate_segment"));
        }
        let mut kinds = BTreeSet::new();
        let mut cameras = BTreeSet::new();
        for f in seg.files {
            if !KINDS.contains(&f.kind.as_str())
                || f.size == 0
                || f.size
                    > if f.kind == "rlog.zst" {
                        max_log
                    } else {
                        2 * 1024 * 1024 * 1024
                    }
                || f.sha256.as_ref().is_some_and(|s| !valid_hash(s))
                || (f.sha256.is_none() && !streaming)
            {
                return Err(bad("invalid_file"));
            }
            if !kinds.insert(f.kind.clone()) || camera(&f.kind).is_some_and(|c| !cameras.insert(c))
            {
                return Err(bad("duplicate_file"));
            }
            files.push(UploadFile {
                name: format!("{}--{}--{}", seg.route, seg.segment, f.kind),
                size: f.size,
                sha256: f.sha256,
            });
        }
        if !kinds.contains("rlog.zst") {
            return Err(bad("rlog_required"));
        }
    }
    Ok(files)
}
pub(crate) fn split_name(name: &str) -> Result<(String, String, String)> {
    let clean = name.replace('\\', "/");
    if clean.starts_with('/')
        || clean.contains(':')
        || clean.split('/').any(|p| p == ".." || p == ".")
    {
        return Err(bad("invalid_file_path"));
    }
    let (parent, base) = clean.rsplit_once('/').unwrap_or(("", &clean));
    let mut parts = base.rsplitn(3, "--");
    let kind = parts.next().unwrap_or("");
    if let (Some(number), Some(route)) = (parts.next(), parts.next())
        && ROUTE.is_match(route)
        && number.parse::<u32>().is_ok_and(|n| n <= 999999)
        && KINDS.contains(&kind)
    {
        return Ok((
            format!("{parent}/{route}--{number}"),
            if kind == "qcamera.mp4" {
                "camera.mp4"
            } else {
                kind
            }
            .to_owned(),
            format!("{route} / 구간 {}", number.parse::<u32>().unwrap()),
        ));
    }
    if KINDS.contains(&base) {
        return Ok((
            parent.to_owned(),
            if base == "qcamera.mp4" {
                "camera.mp4"
            } else {
                base
            }
            .to_owned(),
            parent
                .rsplit('/')
                .next()
                .filter(|v| !v.is_empty())
                .unwrap_or("업로드한 로그")
                .to_owned(),
        ));
    }
    Err(bad("invalid_file_path"))
}

pub struct Session {
    pub path: PathBuf,
    pub files: Vec<UploadFile>,
    pub meta: RwLock<Value>,
    pub lifecycle: RwLock<()>,
    file_locks: Vec<Mutex<()>>,
    received: Vec<AtomicU64>,
    progressed_ms: AtomicU64,
}
impl Session {
    async fn load(path: PathBuf, files: Vec<UploadFile>, meta: Value) -> std::io::Result<Self> {
        let mut progress = tokio::fs::metadata(path.join("reservation.json"))
            .await?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut received = Vec::new();
        for i in 0..files.len() {
            match tokio::fs::metadata(path.join(i.to_string())).await {
                Ok(m) => {
                    received.push(AtomicU64::new(m.len()));
                    progress = progress.max(
                        m.modified()?
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    received.push(AtomicU64::new(0))
                }
                Err(e) => return Err(e),
            }
        }
        let file_locks = files.iter().map(|_| Mutex::new(())).collect();
        Ok(Self {
            path,
            files,
            meta: RwLock::new(meta),
            lifecycle: RwLock::new(()),
            file_locks,
            received,
            progressed_ms: AtomicU64::new(progress),
        })
    }
    async fn offsets(&self) -> Result<Value> {
        let mut result = Vec::new();
        for (i, f) in self.files.iter().enumerate() {
            let received = self.received[i].load(Ordering::Acquire);
            result.push(json!({"index":i,"name":f.name,"size":f.size,"received":received}));
        }
        Ok(json!(result))
    }
    async fn last_progress(&self) -> Result<f64> {
        Ok(self.progressed_ms.load(Ordering::Acquire) as f64 / 1000.)
    }
    /// Progress made by a writer other than the chunk route (keeps the session from idling out).
    pub(crate) fn touch(&self) {
        self.progressed_ms
            .store((now() * 1000.) as u64, Ordering::Release);
    }
}
/// Archived recordings an upload re-sends, by id, with the states of their archived files.
type ArchivedMatches = BTreeMap<String, BTreeMap<String, String>>;

pub struct Uploads {
    pub store: Arc<Store>,
    pub auth: Arc<Auth>,
    pub sessions: RwLock<BTreeMap<String, Arc<Session>>>,
    admission: Mutex<()>,
    pub max_log: u64,
}
impl Uploads {
    pub async fn open(
        store: Arc<Store>,
        auth: Arc<Auth>,
        max_log: u64,
    ) -> anyhow::Result<Arc<Self>> {
        let mut sessions = BTreeMap::new();
        let mut dirs = tokio::fs::read_dir(store.root.join(".uploads")).await?;
        while let Some(entry) = dirs.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let interrupted = name
                .strip_suffix(".creating")
                .or_else(|| name.strip_suffix(".removing"))
                .is_some_and(valid_id);
            // Before staged creation/removal existed, a crash could leave a session folder without
            // its manifest. Such a folder has no journal, so nothing in it can be registered.
            let incomplete = valid_id(&name)
                && !entry.path().join("commit.json").exists()
                && !(entry.path().join("manifest.json").exists()
                    && entry.path().join("reservation.json").exists());
            if interrupted || incomplete {
                tokio::fs::remove_dir_all(entry.path()).await?;
                continue;
            }
            if !valid_id(&name) {
                continue;
            }
            let files: Vec<UploadFile> = read_json(&entry.path().join("manifest.json")).await?;
            let device = entry.path().join("device.json");
            let meta: Value = if device.exists() {
                read_json(&device).await?
            } else {
                read_json(&entry.path().join("reservation.json")).await?
            };
            sessions.insert(
                name,
                Arc::new(Session::load(entry.path(), files, meta).await?),
            );
        }
        Ok(Arc::new(Self {
            store,
            auth,
            sessions: RwLock::new(sessions),
            admission: Mutex::new(()),
            max_log,
        }))
    }
    pub(crate) async fn get(&self, session: &str) -> Result<Arc<Session>> {
        self.sessions
            .read()
            .await
            .get(session)
            .cloned()
            .ok_or_else(|| missing("session_not_found"))
    }
    pub async fn authorize(&self, session: &str, headers: &HeaderMap) -> Result<Value> {
        if !valid_id(session) {
            return Err(missing("session_not_found"));
        }
        let bearer = header(headers, "authorization");
        if !bearer.starts_with("Bearer ") || !(20..=100).contains(&bearer.len()) {
            return Err(unauthorized("invalid_upload_token"));
        }
        let active = { self.sessions.read().await.get(session).cloned() };
        let (meta, idle) = if let Some(s) = active {
            (s.meta.read().await.clone(), Some(s.last_progress().await?))
        } else if let Some(receipt) = self.auth.receipt(session).await {
            (receipt, None)
        } else {
            return Err(missing("session_not_found"));
        };
        let device = meta["device_id"]
            .as_str()
            .ok_or_else(|| unauthorized("invalid_upload_token"))?;
        self.auth.check_device(device).await?;
        if meta["expires_at"].as_f64().unwrap_or(0.) <= now()
            || (meta.get("result").is_none() && idle.is_some_and(|v| now() - v > IDLE))
        {
            return Err(ApiError(StatusCode::GONE, "session_expired"));
        }
        if !equal(
            meta["token_hash"].as_str().unwrap_or(""),
            &sha(&bearer[7..]),
        ) {
            return Err(unauthorized("invalid_upload_token"));
        }
        Ok(meta)
    }
    pub async fn begin_device(
        &self,
        headers: &HeaderMap,
        body: &[u8],
        query: bool,
    ) -> Result<(StatusCode, Value)> {
        if body.len() > 65536 {
            return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"));
        }
        let device = self
            .auth
            .authenticate(headers, "POST", "/api/device/uploads", query, body)
            .await?;
        let body: Value = serde_json::from_slice(body).map_err(|_| bad("invalid_batch"))?;
        let files = flatten_batch(
            &body,
            header(headers, "x-rv-checksum") == "sha256-stream",
            self.max_log,
        )?;
        // serde_json's default map is sorted; ASCII-only manifest strings make this identical to Python sort_keys.
        let digest = sha(serde_json::to_vec(&body).unwrap());
        let batch = body["batch_id"].as_str().unwrap();
        let _admission = self.admission.lock().await;
        self.auth.check_device(&device).await?;
        if let Some(receipt) = self.auth.batch_receipt(&device, batch).await {
            if receipt["batch_hash"] != digest {
                return Err(conflict("batch_conflict"));
            }
            return Ok((
                StatusCode::OK,
                json!({"state":"completed","result":receipt["result"]}),
            ));
        }
        let sessions = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>();
        for (name, s) in sessions {
            let _life = s.lifecycle.write().await;
            let meta = s.meta.read().await.clone();
            if meta["device_id"] == device && meta["batch_id"] == batch {
                if meta["batch_hash"] != digest {
                    return Err(conflict("batch_conflict"));
                }
                if meta["expires_at"].as_f64().unwrap_or(0.) <= now()
                    || now() - s.last_progress().await? > IDLE
                {
                    if s.path.join("commit.json").exists() {
                        return Err(conflict("upload_busy"));
                    }
                    // Python's begin runs expiry cleanup first, so a retried batch starts a new session.
                    remove_session(&s.path).await?;
                    self.sessions.write().await.remove(&name);
                    break;
                }
                if let Some(result) = meta.get("result") {
                    return Ok((StatusCode::OK, json!({"state":"completed","result":result})));
                }
                return Ok((StatusCode::OK, self.issue_token(&s, meta, headers).await?));
            }
        }
        if let Some(receipt) = self.auth.batch_receipt(&device, batch).await {
            if receipt["batch_hash"] != digest {
                return Err(conflict("batch_conflict"));
            }
            return Ok((
                StatusCode::OK,
                json!({"state":"completed","result":receipt["result"]}),
            ));
        }
        if self.auth.receipt_count().await >= 256 {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "session_limit_reached",
            ));
        }
        let streaming = files.iter().any(|f| f.sha256.is_none());
        let meta = json!({"device_id":device,"batch_id":batch,"batch_hash":digest,"expires_at":now()+7200.,"streaming":streaming});
        let s = self.create(files, meta, MAX_BATCH).await?;
        let meta = s.meta.read().await.clone();
        Ok((
            StatusCode::CREATED,
            self.issue_token(&s, meta, headers).await?,
        ))
    }
    async fn issue_token(
        &self,
        s: &Session,
        mut meta: Value,
        headers: &HeaderMap,
    ) -> Result<Value> {
        let token = random_token();
        meta["token_hash"] = json!(sha(&token));
        let max = header(headers, "x-rv-max-chunk")
            .parse::<u64>()
            .ok()
            .filter(|v| *v >= 1)
            .unwrap_or(256 * 1024)
            .min(8 * 1024 * 1024);
        meta["chunk_size"] = json!(max);
        atomic_json(&s.path.join("device.json"), &meta).await?;
        *s.meta.write().await = meta.clone();
        Ok(
            json!({"id":s.path.file_name().unwrap().to_string_lossy(),"token":token,"expires_at":meta["expires_at"],"idle_timeout_seconds":900,"chunk_size":max,"files":s.offsets().await?,"checksum_mode":if meta["streaming"]==true {"sha256-stream"} else {"sha256-manifest"}}),
        )
    }
    pub async fn begin_browser(&self, body: Value) -> Result<Value> {
        let files: Vec<UploadFile> =
            serde_json::from_value(body["files"].clone()).map_err(|_| bad("invalid_files"))?;
        if files.is_empty() || files.len() > 100 {
            return Err(bad("invalid_files"));
        }
        for f in &files {
            if f.size == 0 {
                return Err(bad("invalid_file"));
            }
            split_name(&f.name)?;
        }
        let _admission = self.admission.lock().await;
        let s = self
            .create(files, json!({"expires_at":now()+7200.}), self.max_log)
            .await?;
        Ok(json!({"id":s.path.file_name().unwrap().to_string_lossy(),"chunk_size":2*1024*1024}))
    }
    /// A browser session for files whose names the caller has validated (imports, multipart).
    pub(crate) async fn create_browser(&self, files: Vec<UploadFile>) -> Result<Arc<Session>> {
        let _admission = self.admission.lock().await;
        self.create(files, json!({"expires_at":now()+7200.}), self.max_log)
            .await
    }
    /// A browser session that carries `marker` from the moment it exists (an import fed by the server itself,
    /// whose leftovers startup removes by that file).
    pub(crate) async fn create_marked(
        &self,
        files: Vec<UploadFile>,
        marker: (&str, &Value),
    ) -> Result<Arc<Session>> {
        let _admission = self.admission.lock().await;
        self.create_with(
            files,
            json!({"expires_at":now()+7200.}),
            self.max_log,
            Some(marker),
        )
        .await
    }
    pub(crate) async fn forget(&self, session: &str) {
        self.sessions.write().await.remove(session);
    }
    async fn create(&self, files: Vec<UploadFile>, meta: Value, max: u64) -> Result<Arc<Session>> {
        self.create_with(files, meta, max, None).await
    }
    async fn create_with(
        &self,
        files: Vec<UploadFile>,
        meta: Value,
        max: u64,
        marker: Option<(&str, &Value)>,
    ) -> Result<Arc<Session>> {
        let total = files
            .iter()
            .try_fold(0u64, |a, f| a.checked_add(f.size))
            .ok_or_else(|| bad("batch_too_large"))?;
        if total > max {
            return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "batch_too_large"));
        }
        if self.sessions.read().await.len() >= 32 {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "session_limit_reached",
            ));
        }
        // Until its manifest is on disk, an admitted session's route is invisible to a finish's
        // eviction guards (Python admits and creates under `registration_lock`). Callers hold
        // `admission` first; a finish takes only `registration`, so the order never inverts.
        let _registration = self.store.registration.lock().await;
        // Chunks are moved into place when finished, so no second copy needs room.
        let names = files.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
        let amount =
            crate::storage::admit(&self.store, total, &names, false, &BTreeSet::new(), None)
                .await?;
        let name = id();
        let path = self.store.root.join(".uploads").join(&name);
        // A session folder becomes visible complete or not at all, so startup never meets half of one.
        let staging = path.with_extension("creating");
        tokio::fs::create_dir(&staging).await?;
        let result = async {
            atomic_json(&staging.join("manifest.json"), &files).await?;
            atomic_json(
                &staging.join("reservation.json"),
                &json!({"bytes":amount,"expires_at":meta["expires_at"]}),
            )
            .await?;
            if meta.get("device_id").is_some() {
                atomic_json(&staging.join("device.json"), &meta).await?;
            }
            if let Some((name, value)) = marker {
                atomic_json(&staging.join(name), value).await?;
            }
            tokio::fs::rename(&staging, &path).await?;
            sync_dir(path.parent().unwrap()).await
        }
        .await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(e.into());
        }
        let session = Arc::new(Session::load(path, files, meta).await?);
        self.sessions.write().await.insert(name, session.clone());
        Ok(session)
    }
    pub(crate) async fn check_session(
        &self,
        s: &Session,
        device: bool,
        headers: &HeaderMap,
    ) -> Result<()> {
        if device {
            self.authorize(s.path.file_name().unwrap().to_str().unwrap(), headers)
                .await?;
        } else if s.meta.read().await.get("device_id").is_some() {
            return Err(missing("session_not_found"));
        }
        let meta = s.meta.read().await;
        if meta["expires_at"].as_f64().unwrap_or(0.) <= now()
            || now() - s.last_progress().await? > IDLE
        {
            return Err(ApiError(StatusCode::GONE, "session_expired"));
        }
        Ok(())
    }
    pub async fn status(&self, name: &str, headers: &HeaderMap) -> Result<Value> {
        let meta = self.authorize(name, headers).await?;
        if let Some(result) = meta.get("result") {
            return Ok(json!({"state":"completed","result":result}));
        }
        let s = self.get(name).await?;
        let _life = s.lifecycle.read().await;
        Ok(
            json!({"id":name,"state":"uploading","expires_at":meta["expires_at"],"files":s.offsets().await?}),
        )
    }
    pub async fn chunk(
        &self,
        name: &str,
        index: usize,
        offset: Option<u64>,
        body: &[u8],
        device: bool,
        headers: &HeaderMap,
    ) -> Result<Value> {
        let result = self
            .write_chunk(name, index, offset, body, device, headers)
            .await;
        if let Err(ApiError(status, _)) = &result
            && matches!(
                *status,
                StatusCode::INSUFFICIENT_STORAGE | StatusCode::INTERNAL_SERVER_ERROR
            )
        {
            self.release(name).await;
        }
        result
    }
    /// Python drops a session whose chunk failed on storage once no other request still uses it,
    /// releasing its reservation; the uploader then starts the batch again.
    async fn release(&self, name: &str) {
        let Ok(s) = self.get(name).await else {
            return;
        };
        let Ok(_life) = s.lifecycle.try_write() else {
            return;
        };
        if s.path.join("commit.json").exists() || remove_session(&s.path).await.is_err() {
            return;
        }
        self.sessions.write().await.remove(name);
    }
    async fn write_chunk(
        &self,
        name: &str,
        index: usize,
        offset: Option<u64>,
        body: &[u8],
        device: bool,
        headers: &HeaderMap,
    ) -> Result<Value> {
        if device {
            let meta = self.authorize(name, headers).await?;
            if meta.get("result").is_some() {
                return Err(conflict("session_completed"));
            }
        }
        let s = self.get(name).await?;
        let _life = s.lifecycle.read().await;
        let file = s
            .files
            .get(index)
            .ok_or_else(|| missing("session_not_found"))?;
        let start = std::time::Instant::now();
        let _file = s.file_locks[index].lock().await;
        let lock_wait = start.elapsed();
        self.check_session(&s, device, headers).await?;
        if s.meta.read().await.get("result").is_some() {
            return Err(conflict("session_completed"));
        }
        let offset = offset.ok_or_else(|| bad("invalid_offset"))?;
        let max = if device {
            s.meta.read().await["chunk_size"]
                .as_u64()
                .unwrap_or(256 * 1024)
        } else {
            2 * 1024 * 1024
        };
        let end = offset
            .checked_add(body.len() as u64)
            .ok_or_else(|| bad("invalid_chunk"))?;
        if body.is_empty() || body.len() as u64 > max || end > file.size {
            return Err(bad("invalid_chunk"));
        }
        let path = s.path.join(index.to_string());
        let current = match tokio::fs::metadata(&path).await {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        s.received[index].store(current, Ordering::Release);
        if offset < current && end <= current {
            let mut f = tokio::fs::File::open(&path).await?;
            f.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut existing = vec![0u8; body.len()];
            f.read_exact(&mut existing).await?;
            if existing != body {
                return Err(conflict("offset_conflict"));
            }
            return Ok(json!({"received":end}));
        }
        if offset != current {
            return Err(conflict("offset_conflict"));
        }
        if fs2::available_space(&self.store.root)? < body.len() as u64 + MARGIN {
            return Err(ApiError(
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_disk_space",
            ));
        }
        let write = std::time::Instant::now();
        let mut opts = tokio::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(path).await?;
        let outcome = async {
            f.write_all(body).await?;
            // tokio's sync_data records a failed background write instead of returning it; flush reports it.
            f.flush().await?;
            crate::fault::point("chunk_before_sync");
            f.sync_data().await
        }
        .await;
        if outcome.is_err() {
            // Bytes of a failed, unacknowledged write must not be offered as a resume offset.
            let _ = f.set_len(current).await;
        }
        s.received[index].store(f.metadata().await?.len(), Ordering::Release);
        outcome?;
        s.progressed_ms
            .fetch_max((now() * 1000.) as u64, Ordering::AcqRel);
        crate::fault::point("chunk_after_sync");
        tracing::info!(
            file_lock_wait_us = lock_wait.as_micros() as u64,
            file_write_us = write.elapsed().as_micros() as u64,
            bytes = body.len(),
            "upload_chunk_committed"
        );
        Ok(json!({"received":end}))
    }
    pub async fn cancel(&self, name: &str, device: bool, headers: &HeaderMap) -> Result<Value> {
        if device {
            let meta = self.authorize(name, headers).await?;
            if let Some(result) = meta.get("result") {
                return Ok(json!({"state":"completed","result":result}));
            }
        }
        let s = self.get(name).await?;
        let _life = s
            .lifecycle
            .try_write()
            .map_err(|_| conflict("upload_busy"))?;
        self.check_session(&s, device, headers).await?;
        if s.path.join("commit.json").exists() {
            return Err(conflict("upload_busy"));
        }
        remove_session(&s.path).await?;
        self.sessions.write().await.remove(name);
        Ok(json!({"deleted":name}))
    }
    pub async fn finish(
        &self,
        name: &str,
        device: bool,
        headers: &HeaderMap,
        body: Value,
    ) -> Result<(StatusCode, Value)> {
        if device {
            let meta = self.authorize(name, headers).await?;
            if let Some(result) = meta.get("result") {
                return Ok((StatusCode::OK, result.clone()));
            }
        }
        let s = self.get(name).await?;
        let _life = s
            .lifecycle
            .try_write()
            .map_err(|_| conflict("upload_busy"))?;
        self.check_session(&s, device, headers).await?;
        let plan_path = s.path.join("commit.json");
        // Registration is separate from all UI/authentication and chunk locks.
        let registration;
        let plan: Value = if plan_path.exists() {
            registration = self.store.registration.lock().await;
            read_json(&plan_path).await?
        } else {
            let streaming = s.meta.read().await["streaming"] == true;
            let hashes = if streaming {
                let hashes = body["sha256"]
                    .as_array()
                    .filter(|h| {
                        h.len() == s.files.len()
                            && h.iter().all(|v| v.as_str().is_some_and(valid_hash))
                    })
                    .ok_or_else(|| bad("invalid_file"))?;
                if body.as_object().is_none_or(|v| v.len() != 1) {
                    return Err(bad("invalid_file"));
                }
                // Python's device finish refuses a body contradicting the manifest before
                // registration starts, so this session stays.
                if s.files
                    .iter()
                    .zip(hashes)
                    .any(|(f, h)| f.sha256.as_deref().is_some_and(|v| Some(v) != h.as_str()))
                {
                    return Err(ApiError(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "checksum_mismatch",
                    ));
                }
                Some(hashes)
            } else {
                None
            };
            for (i, f) in s.files.iter().enumerate() {
                if tokio::fs::metadata(s.path.join(i.to_string()))
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0)
                    != f.size
                {
                    return Err(conflict("incomplete_upload"));
                }
            }
            // From here on Python's `finish_upload` removes the session in every outcome, so the
            // uploader's next begin of the batch uploads it again instead of meeting the same
            // stored bytes until the idle expiry. `upload_busy` is HARS's retryable wait.
            let verified = async {
                let mut digests = Vec::new();
                for (i, f) in s.files.iter().enumerate() {
                    let path = s.path.join(i.to_string());
                    let digest = file_hash(&path).await?;
                    let expected = hashes
                        .map(|v| v[i].as_str().unwrap())
                        .or(f.sha256.as_deref());
                    if expected.is_some_and(|v| v != digest) {
                        return Err(ApiError(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "checksum_mismatch",
                        ));
                    }
                    let (_, kind, _) = split_name(&f.name)?;
                    if kind.ends_with(".mp4") || kind.ends_with(".hevc") {
                        crate::media::validate_upload(&path, &kind)
                            .await
                            .map_err(|_| bad("invalid_file"))?;
                    }
                    digests.push(digest);
                }
                Ok(digests)
            }
            .await;
            let digests = match verified {
                Ok(v) => v,
                Err(e) => return Err(self.discard(name, &s, e).await),
            };
            // The archive is probed before any lock, and only for re-uploads of archived recordings.
            let archived = self.archived_matches(&digests).await;
            let reachable = self.archive_reachable(!archived.is_empty()).await;
            // Registration never holds a shared UI/authentication lock. Hashing already completed.
            registration = self.store.registration.lock().await;
            let planned = async {
                self.check_session(&s, device, headers).await?;
                let plan = self.plan(&s, &digests, &archived, reachable).await?;
                if ["records", "updates"]
                    .iter()
                    .any(|k| plan[k].as_array().is_some_and(|v| !v.is_empty()))
                {
                    // Everything is received and verified: only now may old recordings make room,
                    // never one this upload matched (Python `protected_ids`).
                    let names = s.files.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
                    let duplicates = plan["result"]["duplicates"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .chain(plan["updates"].as_array().into_iter().flatten())
                        .filter_map(|d| d["id"].as_str().map(str::to_owned))
                        .collect();
                    crate::storage::admit_reserved(
                        &self.store,
                        0,
                        &names,
                        true,
                        &duplicates,
                        Some(&registration),
                        Some(name),
                    )
                    .await?;
                }
                atomic_json(&plan_path, &plan).await?;
                Ok(plan)
            }
            .await;
            let plan = match planned {
                Ok(v) => v,
                Err(e) if plan_path.exists() => return Err(e),
                Err(e) => return Err(self.discard(name, &s, e).await),
            };
            crate::fault::point("finish_after_journal");
            plan
        };
        let result = self.apply_plan(name, &s, &plan, device).await?;
        drop(registration);
        Ok((StatusCode::CREATED, result))
    }
    /// Drops a session whose registration failed (the caller holds its lifecycle lock) and
    /// returns the failure to answer with.
    async fn discard(&self, name: &str, s: &Session, error: ApiError) -> ApiError {
        if error.1 == "upload_busy" {
            return error;
        }
        if remove_session(&s.path).await.is_ok() {
            self.sessions.write().await.remove(name);
        }
        error
    }
    async fn apply_plan(
        &self,
        name: &str,
        s: &Session,
        plan: &Value,
        device: bool,
    ) -> Result<Value> {
        for entry in plan["moves"].as_array().unwrap() {
            let source = s.path.join(entry["index"].as_u64().unwrap().to_string());
            let target = self
                .store
                .root
                .join(entry["id"].as_str().unwrap())
                .join(entry["kind"].as_str().unwrap());
            tokio::fs::create_dir_all(target.parent().unwrap()).await?;
            if source.exists() {
                tokio::fs::rename(&source, &target).await?;
                crate::fault::point("finish_after_move");
            } else if file_hash(&target).await? != entry["sha256"].as_str().unwrap() {
                return Err(conflict("checksum_mismatch"));
            }
        }
        for meta in plan["records"].as_array().unwrap() {
            self.store.save_meta(meta.clone()).await?;
        }
        let mut updated = vec![];
        for update in plan["updates"].as_array().into_iter().flatten() {
            updated.push(crate::recording_update::apply(&self.store, update).await?);
        }
        crate::fault::point("finish_before_receipt");
        let mut result = plan["result"].clone();
        result["updated"] = json!(updated);
        if device {
            let mut receipt = s.meta.read().await.clone();
            receipt["result"] = result.clone();
            self.auth.save_receipt(name, receipt.clone()).await?;
            *s.meta.write().await = receipt;
        }
        crate::fault::point("finish_after_receipt");
        // Receipt is durable before removing the resumable session and reservation.
        remove_session(&s.path).await?;
        self.sessions.write().await.remove(name);
        Ok(result)
    }
    pub async fn recover_commits(&self) -> Result<usize> {
        let sessions = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>();
        let mut recovered = 0;
        for (name, s) in sessions {
            let Ok(_life) = s.lifecycle.try_write() else {
                continue;
            };
            let path = s.path.join("commit.json");
            if !path.exists() {
                continue;
            }
            let _registration = self.store.registration.lock().await;
            let plan: Value = read_json(&path).await?;
            let device = s.meta.read().await.get("device_id").is_some();
            self.apply_plan(&name, &s, &plan, device).await?;
            recovered += 1;
        }
        Ok(recovered)
    }
    /// Whether the archive answers, decided before the registration lock. A re-upload of archived recordings probes
    /// it here: their files were just resolved through Drive's read trust, which outlasts the 10 s probe cache that
    /// `last_state` reads, so `last_state` alone refused every such upload once the cache ran out.
    async fn archive_reachable(&self, matched: bool) -> bool {
        let Some(archive) = self.store.archive() else {
            return false;
        };
        let connected = Some(crate::archive::State::Connected);
        if matched {
            use crate::archive::ArchiveRead;
            Some(archive.backend().probe(true).await.state) == connected
        } else {
            archive.last_state() == connected
        }
    }
    /// Python `archived_matches`: archived recordings whose log has one of these hashes, with the
    /// states of their archived files (resolved here, outside every lock).
    async fn archived_matches(&self, digests: &[String]) -> ArchivedMatches {
        let mut out = ArchivedMatches::new();
        let Some(archive) = self.store.archive() else {
            return out;
        };
        let metas = self
            .store
            .records
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for meta in metas.iter().filter(|m| crate::storage::archived(m)) {
            let entry = crate::archive::index::entry(meta, "rlog.zst");
            let digest = if crate::archive::index::is_archived_location(
                entry["location"].as_str().unwrap_or("local"),
            ) {
                entry["sha256"].as_str()
            } else {
                meta["content_hashes"]["rlog.zst"]["sha256"].as_str()
            };
            if digest.is_some_and(|d| digests.iter().any(|x| x == d)) {
                let id = meta["id"].as_str().unwrap_or_default().to_owned();
                out.insert(
                    id,
                    crate::recording_update::archived_states(&archive, meta).await,
                );
            }
        }
        out
    }
    async fn plan(
        &self,
        s: &Session,
        digests: &[String],
        archived: &ArchivedMatches,
        reachable: bool,
    ) -> Result<Value> {
        let mut groups: BTreeMap<String, Vec<(usize, String, String)>> = BTreeMap::new();
        for (i, f) in s.files.iter().enumerate() {
            let (group, kind, label) = split_name(&f.name)?;
            groups.entry(group).or_default().push((i, kind, label));
        }
        let mut existing = self
            .store
            .records
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        // Failed peer journals must be recovered before their records can be claimed.
        let peers = self
            .sessions
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for peer in peers {
            let p = peer.path.join("commit.json");
            if p.exists() {
                let plan: Value = read_json(&p).await?;
                let updating = plan["updates"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|u| existing.iter().find(|m| m["id"] == u["id"]));
                if plan["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .chain(updating)
                    .any(|m| {
                        digests
                            .iter()
                            .any(|d| m["content_hashes"]["rlog.zst"]["sha256"] == *d)
                    })
                {
                    return Err(conflict("upload_busy"));
                }
            }
        }
        let mut records = Vec::new();
        let mut moves = Vec::new();
        let mut duplicates = Vec::new();
        let mut updates = Vec::new();
        let mut planned_kinds: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for group in groups.values() {
            let mut kinds = BTreeSet::new();
            let mut cameras = BTreeSet::new();
            for (_, kind, _) in group {
                if !kinds.insert(kind) || camera(kind).is_some_and(|c| !cameras.insert(c)) {
                    return Err(bad("duplicate_file"));
                }
            }
            let log = group
                .iter()
                .find(|(_, kind, _)| kind == "rlog.zst")
                .ok_or_else(|| bad("rlog_required"))?;
            if let Some(old) = existing
                .iter_mut()
                .find(|m| m["content_hashes"]["rlog.zst"]["sha256"] == digests[log.0])
            {
                let old_id = old["id"].as_str().unwrap_or_default().to_owned();
                let folder = self.store.root.join(&old_id);
                let incoming = group
                    .iter()
                    .map(|(i, kind, _)| (kind.clone(), digests[*i].clone()))
                    .collect::<BTreeMap<_, _>>();
                let known = old["content_hashes"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(k, v)| Some((k.clone(), v["sha256"].as_str()?.to_owned())))
                    .collect::<BTreeMap<_, _>>();
                let planned = planned_kinds.entry(old_id.clone()).or_default();
                let added = crate::recording_update::additional_videos(
                    &folder, old, &incoming, &known, planned,
                );
                if added.is_empty() {
                    let video_differs = group.iter().any(|(i, k, _)| {
                        crate::recording_update::VIDEO_KINDS.contains(&k.as_str())
                            && known.get(k) != Some(&digests[*i])
                    });
                    duplicates
                        .push(json!({"id":old["id"],"name":log.2,"video_differs":video_differs}));
                    continue;
                }
                let states = archived.get(&old_id);
                if let Some(refused) = crate::recording_update::refusal(old, reachable, states) {
                    return Err(refused);
                }
                let restored =
                    crate::recording_update::restored(&folder, &added, &incoming, &known);
                let mut rows = vec![];
                for kind in &added {
                    let (i, _, _) = group.iter().find(|(_, k, _)| k == kind).unwrap();
                    let name = s.files[*i].name.rsplit('/').next().unwrap().to_owned();
                    moves.push(json!({"index":i,"id":old_id,"kind":kind,"sha256":digests[*i]}));
                    old["files"][kind] = json!(name);
                    old["content_hashes"][kind] = json!({"sha256":digests[*i]});
                    planned.insert(kind.clone());
                    rows.push((*i, kind.clone(), name, digests[*i].clone()));
                }
                updates.push(crate::recording_update::entry(
                    old,
                    restored,
                    &rows,
                    &crate::recording_update::available_originals(states),
                ));
                continue;
            }
            let record = id();
            let mut files = serde_json::Map::new();
            let mut content = serde_json::Map::new();
            let mut total = 0;
            for (i, kind, _) in group {
                files.insert(
                    kind.clone(),
                    json!(s.files[*i].name.rsplit('/').next().unwrap()),
                );
                let file_meta = tokio::fs::metadata(s.path.join(i.to_string())).await?;
                let mtime = file_meta
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                content.insert(
                    kind.clone(),
                    json!({"size":s.files[*i].size,"mtime_ns":mtime,"sha256":digests[*i]}),
                );
                total += s.files[*i].size;
                moves.push(json!({"index":i,"id":record,"kind":kind,"sha256":digests[*i]}));
            }
            let meta = json!({"id":record,"name":log.2,"uploaded":now(),"status":"unconverted","video":group.iter().any(|(_,k,_)|camera(k).is_some()),"bytes":total,"files":files,"error":null,"content_hashes":content});
            existing.push(meta.clone());
            records.push(meta);
        }
        // `updated` is filled in when the journal is applied: it shows the recordings as saved.
        Ok(
            json!({"records":records,"moves":moves,"updates":updates,"result":{"logs":records,"duplicates":duplicates,"updated":[]}}),
        )
    }
    pub async fn cleanup(&self) -> Result<usize> {
        let _admission = self.admission.lock().await;
        let peers = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>();
        let mut removed = 0;
        for (name, s) in peers {
            let Ok(_life) = s.lifecycle.try_write() else {
                continue;
            };
            if s.path.join("commit.json").exists() {
                continue;
            } // A registration journal is never expired/deleted.
            let meta = s.meta.read().await.clone();
            let revoked = match meta["device_id"].as_str() {
                Some(device) => self.auth.check_device(device).await.is_err(),
                None => false,
            };
            if revoked
                || meta["expires_at"].as_f64().unwrap_or(0.) <= now()
                || now() - s.last_progress().await? > IDLE
            {
                remove_session(&s.path).await?;
                self.sessions.write().await.remove(&name);
                removed += 1;
            }
        }
        Ok(removed)
    }
    /// Python `cleanup_uploads(force=True)`: every incomplete browser session goes now; device
    /// sessions only once expired. Sessions being registered or receiving a chunk are spared.
    pub async fn discard_incomplete(&self) -> Result<u64> {
        let _admission = self.admission.lock().await;
        let peers = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>();
        let mut removed = 0;
        for (name, s) in peers {
            let Ok(_life) = s.lifecycle.try_write() else {
                continue;
            };
            if s.path.join("commit.json").exists() {
                continue;
            }
            let meta = s.meta.read().await.clone();
            let expired = meta["expires_at"].as_f64().unwrap_or(0.) <= now()
                || now() - s.last_progress().await? > IDLE;
            if meta.get("device_id").is_some() && !expired {
                continue;
            }
            let path = s.path.clone();
            removed += tokio::task::spawn_blocking(move || crate::store::directory_size(&path))
                .await
                .map_err(std::io::Error::other)??;
            remove_session(&s.path).await?;
            self.sessions.write().await.remove(&name);
        }
        Ok(removed)
    }
    pub async fn snapshot(&self) -> (u64, Value) {
        let sessions = self
            .sessions
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut reserved = 0;
        let mut total = 0;
        let mut received = 0;
        for s in sessions {
            let size = s.files.iter().map(|f| f.size).sum::<u64>();
            let done = s
                .received
                .iter()
                .zip(&s.files)
                .map(|(v, f)| v.load(Ordering::Acquire).min(f.size))
                .sum::<u64>();
            reserved += size.saturating_sub(done) + 65536;
            if now() - s.progressed_ms.load(Ordering::Acquire) as f64 / 1000. < 30. {
                total += size;
                received += done;
            }
        }
        (
            reserved,
            if total == 0 {
                Value::Null
            } else {
                json!({"received_bytes":received,"total_bytes":total})
            },
        )
    }
}
async fn sync_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all()).await??;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
/// One rename hides the whole session; the folder's contents are deleted afterwards and,
/// after a crash, at the next startup.
pub(crate) async fn remove_session(path: &Path) -> std::io::Result<()> {
    let grave = path.with_extension("removing");
    tokio::fs::rename(path, &grave).await?;
    sync_dir(path.parent().unwrap()).await?;
    crate::fault::point("session_removing");
    let _ = tokio::fs::remove_dir_all(&grave).await;
    Ok(())
}
pub async fn file_hash(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0u8; 65536];
    let mut hash = Sha256::new();
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}
