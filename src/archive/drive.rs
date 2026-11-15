//! Python `DriveClient`, `DriveBackend` and `DriveConnection`, split by write scope: [`DriveBackend`] writes only to
//! the HARS-owned folder; [`DriveLegacy`] reads a Python-owned folder and has no write method at all.
use super::{
    Availability, CACHE_SECONDS, Clock, FOLDER_MIME, OWNER_KEY, OWNER_VALUE, READ_TRUST_SECONDS,
    STAT_SECONDS, USAGE_REFRESH_SECONDS,
    backend::*,
    drive_auth::{DriveAuth, Poll},
    fsutil,
    http_client::{Http, HttpResponse, Request, error_reason},
};
use bytes::{Bytes, BytesMut};
use md5::Md5;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Resumable chunks must be multiples of 256 KiB, except the last; Google recommends 8 MiB or more.
pub const UPLOAD_CHUNK: usize = 16 << 20;
pub const FILE_FIELDS: &str = "id,size,sha256Checksum,md5Checksum,appProperties";
/// A 403 that only reconnecting fixes.
pub const AUTH_REASONS: [&str; 4] = [
    "insufficientPermissions",
    "authError",
    "access_denied",
    "forbidden",
];
/// The data root's `.archive.json` key for Drive's storage id (a NAS-era `storage_id` is the share's).
pub const LOCAL_KEY: &str = "gdrive_storage_id";

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn int_or(v: Option<&Value>) -> Option<u64> {
    v.and_then(super::drive_auth::int_of)
}
fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}
fn props(v: &Value) -> Map<String, Value> {
    v.get("appProperties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}
fn prop<'a>(p: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Python `_q`: a value inside a Drive query string literal.
pub fn q(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

fn quote(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn fail(r: &HttpResponse) -> BackendError {
    let reason = error_reason(r).unwrap_or_else(|| r.status.to_string());
    if r.status == 401 || r.status == 403 && AUTH_REASONS.contains(&reason.as_str()) {
        return BackendError::new(ErrorCode::AuthExpired, reason);
    }
    if matches!(r.status, 404 | 410) {
        return BackendError::new(ErrorCode::NotFound, reason);
    }
    BackendError::new(ErrorCode::Io, reason)
}

/// 308 `Range: bytes=0-N` -> N; no header -> -1 (nothing received yet).
pub fn received_end(r: &HttpResponse) -> i64 {
    r.header("range")
        .map(str::trim)
        .and_then(|v| v.strip_prefix("bytes=0-"))
        .filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
        .unwrap_or(-1)
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionAnswer {
    Incomplete(i64),
    Done(Value),
    Expired,
}

/// Drive v3 calls. Token handling: one refresh on 401, then auth_expired. Session URIs never reach an error.
pub struct DriveClient {
    pub http: Arc<Http>,
    pub auth: Arc<DriveAuth>,
    api_base: String,
    upload_base: String,
}
impl fmt::Debug for DriveClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DriveClient()")
    }
}

impl DriveClient {
    pub fn new(
        http: Arc<Http>,
        auth: Arc<DriveAuth>,
        api_base: Option<&str>,
        upload_base: Option<&str>,
    ) -> Self {
        let base = |v: Option<&str>, d: fn() -> String| {
            format!(
                "{}/drive/v3",
                v.map_or_else(d, str::to_owned).trim_end_matches('/')
            )
        };
        Self {
            http,
            auth,
            api_base: base(api_base, super::http_client::api_base_url),
            upload_base: base(upload_base, super::http_client::upload_base_url),
        }
    }

    async fn request(&self, mut req: Request, token: bool) -> Result<HttpResponse> {
        // quick (the probe): the short timeout and no retry of a timeout or socket error; Google's own error
        // answers (5xx, 429) keep their backoff.
        req.idempotent &= !req.quick;
        for attempt in 0..2 {
            let mut r = req.clone();
            if token {
                r = r.header(
                    "authorization",
                    format!("Bearer {}", self.auth.access_token(req.quick).await?),
                );
            }
            let answer = self.http.request(r).await?;
            if answer.status != 401 || !token || attempt == 1 {
                return Ok(answer);
            }
            // A 401 rejected the request unprocessed, so sending it again is safe.
            self.auth.expire();
        }
        unreachable!("the second attempt always returns")
    }

    fn url(&self, path: &str, params: &[(&str, &str)]) -> String {
        if params.is_empty() {
            return format!("{}{path}", self.api_base);
        }
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params)
            .finish();
        format!("{}{path}?{query}", self.api_base)
    }
    fn file_url(&self, file_id: &str, params: &[(&str, &str)]) -> String {
        self.url(&format!("/files/{}", quote(file_id)), params)
    }

    pub async fn get(&self, file_id: &str, fields: &str, quick: bool) -> Result<Option<Value>> {
        let mut req = Request::new(
            reqwest::Method::GET,
            self.file_url(file_id, &[("fields", fields)]),
        );
        req.idempotent = true;
        req.quick = quick;
        let r = self.request(req, true).await?;
        if r.status == 404 {
            return Ok(None);
        }
        if r.status != 200 {
            return Err(fail(&r));
        }
        Ok(Some(r.json().unwrap_or_else(|| json!({}))))
    }

    pub async fn list(&self, query: &str, fields: &str) -> Result<Vec<Value>> {
        let (mut found, mut token) = (Vec::new(), None::<String>);
        let fields = format!("nextPageToken,{fields}");
        loop {
            let mut params = vec![
                ("q", query),
                ("fields", fields.as_str()),
                ("spaces", "drive"),
                ("pageSize", "1000"),
            ];
            if let Some(t) = token.as_deref() {
                params.push(("pageToken", t));
            }
            let mut req = Request::new(reqwest::Method::GET, self.url("/files", &params));
            req.idempotent = true;
            let r = self.request(req, true).await?;
            if r.status != 200 {
                return Err(fail(&r));
            }
            let body = r.json().unwrap_or_else(|| json!({}));
            if let Some(files) = body.get("files").and_then(Value::as_array) {
                found.extend(files.iter().filter(|f| f.is_object()).cloned());
            }
            token = text(&body, "nextPageToken").map(str::to_owned);
            if token.is_none() {
                return Ok(found);
            }
        }
    }

    /// Not retried: after a failure the caller looks the folder up by its appProperties.
    pub async fn create_folder(&self, name: &str, app_props: Value) -> Result<String> {
        let body = json!({"name": name, "mimeType": FOLDER_MIME, "appProperties": app_props});
        let mut req = Request::new(
            reqwest::Method::POST,
            format!("{}/files?fields=id", self.api_base),
        )
        .header("content-type", "application/json; charset=UTF-8")
        .body(serde_json::to_vec(&body).unwrap_or_default());
        req.once = true;
        let r = self.request(req, true).await?;
        if r.status != 200 {
            return Err(fail(&r));
        }
        r.json()
            .as_ref()
            .and_then(|b| text(b, "id"))
            .map(str::to_owned)
            .ok_or_else(|| BackendError::new(ErrorCode::Io, "no_file_id"))
    }

    /// A session holds nothing until completed, so starting one again is harmless.
    pub async fn start_resumable(
        &self,
        name: &str,
        parents: &[&str],
        app_props: Value,
        size: u64,
    ) -> Result<SessionUri> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([("uploadType", "resumable"), ("fields", FILE_FIELDS)])
            .finish();
        let body = json!({"name": name, "parents": parents, "appProperties": app_props});
        let req = Request::new(
            reqwest::Method::POST,
            format!("{}/files?{query}", self.upload_base),
        )
        .header("content-type", "application/json; charset=UTF-8")
        .header("x-upload-content-type", "application/octet-stream")
        .header("x-upload-content-length", size.to_string())
        .body(serde_json::to_vec(&body).unwrap_or_default());
        let r = self.request(req, true).await?;
        if r.status != 200 {
            return Err(fail(&r));
        }
        r.header("location")
            .filter(|s| !s.is_empty())
            .map(|s| SessionUri(s.to_owned()))
            .ok_or_else(|| BackendError::new(ErrorCode::Io, "no_session"))
    }

    fn session_answer(r: &HttpResponse) -> Result<SessionAnswer> {
        match r.status {
            308 => Ok(SessionAnswer::Incomplete(received_end(r))),
            200 | 201 => Ok(SessionAnswer::Done(r.json().unwrap_or_else(|| json!({})))),
            404 | 410 => Ok(SessionAnswer::Expired),
            _ => Err(fail(r)),
        }
    }

    /// Not retried: after an error the caller asks session_status() how far the upload got.
    pub async fn put_chunk(
        &self,
        session: &SessionUri,
        data: Bytes,
        start: u64,
        total: u64,
    ) -> Result<SessionAnswer> {
        let range = if data.is_empty() {
            format!("bytes */{total}")
        } else {
            format!("bytes {start}-{}/{total}", start + data.len() as u64 - 1)
        };
        let mut req = Request::new(reqwest::Method::PUT, session.expose())
            .header("content-range", range)
            .body(data);
        req.once = true;
        Self::session_answer(&self.request(req, false).await?)
    }

    pub async fn session_status(&self, session: &SessionUri, total: u64) -> Result<SessionAnswer> {
        let mut req = Request::new(reqwest::Method::PUT, session.expose())
            .header("content-range", format!("bytes */{total}"));
        req.idempotent = true;
        Self::session_answer(&self.request(req, false).await?)
    }

    pub async fn download(
        &self,
        file_id: &str,
        start: Option<u64>,
        end: Option<u64>,
    ) -> Result<reqwest::Response> {
        let mut req = Request::new(
            reqwest::Method::GET,
            self.file_url(file_id, &[("alt", "media")]),
        );
        if let Some(start) = start {
            req = req.header(
                "range",
                format!(
                    "bytes={start}-{}",
                    end.map(|e| e.to_string()).unwrap_or_default()
                ),
            );
        }
        req.stream = true;
        req.idempotent = true;
        let r = self.request(req, true).await?;
        if r.status == 416 {
            return Err(BackendError::new(ErrorCode::Io, "range_not_satisfiable"));
        }
        if !matches!(r.status, 200 | 206) {
            return Err(fail(&r));
        }
        // A range answered with the whole file must not be streamed as if it were the range.
        if start.is_some() && r.status != 206 {
            return Err(BackendError::new(ErrorCode::Io, "range_ignored"));
        }
        r.into_stream()
            .ok_or_else(|| BackendError::new(ErrorCode::Io, "no_body"))
    }

    /// Resending is safe: a delete that went through answers 404 the second time.
    pub async fn delete(&self, file_id: &str) -> Result<bool> {
        let mut req = Request::new(reqwest::Method::DELETE, self.file_url(file_id, &[]));
        req.idempotent = true;
        let r = self.request(req, true).await?;
        if matches!(r.status, 200 | 204 | 404) {
            return Ok(true);
        }
        Err(fail(&r))
    }

    pub async fn about(&self) -> Result<Value> {
        let mut req = Request::new(
            reqwest::Method::GET,
            self.url("/about", &[("fields", "storageQuota")]),
        );
        req.idempotent = true;
        let r = self.request(req, true).await?;
        if r.status != 200 {
            return Err(fail(&r));
        }
        Ok(r.json()
            .and_then(|b| b.get("storageQuota").cloned())
            .unwrap_or_else(|| json!({})))
    }
}

fn remote_ref(meta: &Value) -> RemoteRef {
    RemoteRef {
        backend: BackendKind::Gdrive,
        file_id: text(meta, "id").map(str::to_owned),
        bytes: int_or(meta.get("size")).unwrap_or(0),
        sha256: text(meta, "sha256Checksum").map(str::to_owned),
        md5: text(meta, "md5Checksum").map(str::to_owned),
        completed: false,
        sent_md5: None,
    }
}
fn remote_stat(meta: &Value, trashed: bool) -> RemoteStat {
    RemoteStat {
        exists: !trashed,
        bytes: int_or(meta.get("size")).unwrap_or(0),
        sha256: text(meta, "sha256Checksum").map(str::to_owned),
        md5: text(meta, "md5Checksum").map(str::to_owned),
        trashed,
    }
}

/// Which files a view answers for: a folder and the storage id its files carry (Python's filters), and for a
/// HARS write root also the owner mark.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Scope {
    folder_id: String,
    storage_id: String,
    owned: bool,
}
impl Scope {
    fn accepts(&self, p: &Map<String, Value>) -> bool {
        prop(p, "rv_storage_id") == Some(self.storage_id.as_str())
            && (!self.owned || prop(p, OWNER_KEY) == Some(OWNER_VALUE))
    }
}

/// Reading shared by both scopes (Python's `_Availability` cache plus stat/verify/open/find/usage).
struct DriveCore {
    client: DriveClient,
    enabled: bool,
    avail: Availability,
    clock: Clock,
    stat_cache: Mutex<HashMap<String, (f64, Option<RemoteStat>)>>,
    bulk: Mutex<Option<HashMap<String, RemoteStat>>>,
    usage_cached: Mutex<Option<(f64, Usage)>>,
    usage_seen: Mutex<Option<(f64, Usage)>>,
}

impl DriveCore {
    fn new(client: DriveClient, enabled: bool, clock: Clock) -> Self {
        Self {
            client,
            enabled,
            avail: Availability::new(clock.clone()),
            clock,
            stat_cache: Mutex::default(),
            bulk: Mutex::default(),
            usage_cached: Mutex::default(),
            usage_seen: Mutex::default(),
        }
    }
    fn auth(&self) -> &DriveAuth {
        &self.client.auth
    }
    fn invalidate(&self) {
        self.avail.invalidate();
        *lock(&self.usage_cached) = None;
        lock(&self.stat_cache).clear();
    }
    fn last_state(&self) -> Option<State> {
        if !self.enabled {
            return Some(State::Disabled);
        }
        self.avail.last_state()
    }

    /// Python `_probe` for a folder of `storage_id`; `owned` also requires the HARS owner mark on the folder.
    async fn probe_folder(
        &self,
        folder_id: Option<String>,
        storage_id: Option<String>,
        owned: bool,
    ) -> Probe {
        if !self.auth().connected() {
            return Probe::unavailable("not_connected");
        }
        let Some(folder_id) = folder_id else {
            return Probe::unavailable("folder_missing");
        };
        let folder = match self
            .client
            .get(&folder_id, "trashed,appProperties", true)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                return Probe::unavailable(if e.code == ErrorCode::Unavailable {
                    "network"
                } else {
                    e.code.as_str()
                });
            }
        };
        // 404: deleted for good, or reconnected with an account that cannot see it.
        let Some(folder) = folder else {
            return Probe::unavailable("folder_missing");
        };
        if folder
            .get("trashed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Probe::unavailable("folder_trashed");
        }
        let p = props(&folder);
        if owned && prop(&p, OWNER_KEY) != Some(OWNER_VALUE) {
            return Probe::unavailable("not_owned");
        }
        if storage_id.is_none() || prop(&p, "rv_storage_id") != storage_id.as_deref() {
            return Probe::unavailable("storage_id_mismatch");
        }
        Probe::connected()
    }

    async fn stat(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
        let Some(file_id) = r.file_id.as_deref() else {
            return Ok(None);
        };
        if let Some(hit) = lock(&self.bulk).as_ref().and_then(|b| b.get(file_id)) {
            return Ok(Some(hit.clone()));
        }
        let Some(meta) = self
            .client
            .get(file_id, "id,size,sha256Checksum,md5Checksum,trashed", false)
            .await?
        else {
            return Ok(None);
        };
        let trashed = meta
            .get("trashed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(Some(remote_stat(&meta, trashed)))
    }

    /// For playback only: every seek is a new range request. Verification and deletes always use stat().
    async fn stat_recent(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
        let key = r.file_id.clone().unwrap_or_default();
        let now = (self.clock)();
        if let Some((at, stat)) = lock(&self.stat_cache).get(&key)
            && now - at < STAT_SECONDS
        {
            return Ok(stat.clone());
        }
        let stat = self.stat(r).await?;
        let mut cache = lock(&self.stat_cache);
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(key, (now, stat.clone()));
        Ok(stat)
    }

    async fn find(&self, scope: Option<Scope>, id: &str, kind: &str) -> Result<Vec<RemoteRef>> {
        let Some(scope) = scope else {
            return Ok(Vec::new());
        };
        let query = format!(
            "appProperties has {{ key='rv_id' and value='{}' }} and appProperties has {{ key='rv_kind' and value='{}' }} and trashed=false and '{}' in parents",
            q(id),
            q(kind),
            q(&scope.folder_id)
        );
        let files = self
            .client
            .list(&query, &format!("files({FILE_FIELDS})"))
            .await?;
        Ok(files
            .iter()
            .filter(|f| scope.accepts(&props(f)))
            .map(remote_ref)
            .collect())
    }

    async fn find_all(
        &self,
        scope: Option<Scope>,
    ) -> Result<HashMap<(String, String), Vec<RemoteRef>>> {
        let Some(scope) = scope else {
            return Ok(HashMap::new());
        };
        let query = format!(
            "appProperties has {{ key='rv_storage_id' and value='{}' }} and trashed=false and '{}' in parents",
            q(&scope.storage_id),
            q(&scope.folder_id)
        );
        let mut found: HashMap<(String, String), Vec<RemoteRef>> = HashMap::new();
        for f in self
            .client
            .list(&query, &format!("files({FILE_FIELDS})"))
            .await?
        {
            let p = props(&f);
            let (Some(id), Some(kind)) = (prop(&p, "rv_id"), prop(&p, "rv_kind")) else {
                continue;
            };
            if scope.accepts(&p) {
                found
                    .entry((id.to_owned(), kind.to_owned()))
                    .or_default()
                    .push(remote_ref(&f));
            }
        }
        Ok(found)
    }

    /// Every file in the folder from one listing. A file missing here is never taken as gone: stat() asks.
    async fn snapshot(&self, folder_id: Option<String>) -> Result<HashMap<String, RemoteStat>> {
        let Some(folder_id) = folder_id else {
            return Ok(HashMap::new());
        };
        let query = format!("'{}' in parents and trashed=false", q(&folder_id));
        let files = self
            .client
            .list(&query, "files(id,size,sha256Checksum,md5Checksum)")
            .await?;
        Ok(files
            .iter()
            .filter_map(|f| Some((text(f, "id")?.to_owned(), remote_stat(f, false))))
            .collect())
    }

    async fn verify(
        &self,
        r: &RemoteRef,
        sha256: Option<&str>,
        size: Option<u64>,
        md5: Option<&str>,
    ) -> Result<Verdict> {
        let file_id = r.file_id.as_deref().unwrap_or_default();
        let meta = self
            .client
            .get(file_id, "size,sha256Checksum,md5Checksum,trashed", false)
            .await?;
        let Some(meta) =
            meta.filter(|m| !m.get("trashed").and_then(Value::as_bool).unwrap_or(false))
        else {
            return Err(BackendError::bare(ErrorCode::NotFound));
        };
        if size.is_some() && int_or(meta.get("size")) != size {
            return Ok(Verdict::Mismatch);
        }
        if let (Some(remote), Some(local)) = (text(&meta, "sha256Checksum"), sha256) {
            return Ok(if remote == local {
                Verdict::Ok
            } else {
                Verdict::Mismatch
            });
        }
        if let (Some(remote), Some(local)) = (text(&meta, "md5Checksum"), md5) {
            return Ok(if remote == local {
                Verdict::Ok
            } else {
                Verdict::Mismatch
            });
        }
        Ok(Verdict::Unverifiable)
    }

    async fn open(&self, r: &RemoteRef, start: u64, end: Option<u64>) -> Result<ChunkReader> {
        let file_id = r.file_id.clone().unwrap_or_default();
        let range = start > 0 || end.is_some();
        match self
            .client
            .download(&file_id, range.then_some(start), end)
            .await
        {
            Ok(response) => Ok(ChunkReader(Reader::Http(response))),
            Err(e) => {
                // Gone or unreadable: the next request asks Google again, and a trusted 'connected' is probed again.
                lock(&self.stat_cache).remove(&file_id);
                if e.code != ErrorCode::NotFound {
                    self.avail.invalidate();
                }
                Err(e)
            }
        }
    }

    async fn download_to(&self, r: &RemoteRef, dest: &Path, sha256: Option<&str>) -> Result<()> {
        let reader = self.open(r, 0, None).await?;
        stage(reader, dest, sha256.or(r.sha256.as_deref()), Some(r.bytes)).await
    }

    async fn usage(&self) -> Result<Usage> {
        if let Some((at, u)) = lock(&self.usage_cached).clone()
            && (self.clock)() - at < CACHE_SECONDS
        {
            return Ok(u);
        }
        let quota = self.client.about().await?;
        let usage = int_or(quota.get("usage")).unwrap_or(0);
        let limit = int_or(quota.get("limit")).filter(|l| *l > 0);
        let result = Usage {
            used: int_or(quota.get("usageInDrive")).unwrap_or(usage),
            free: limit.map(|l| l.saturating_sub(usage)),
            limit,
            reliable: limit.is_some(),
        };
        let entry = Some(((self.clock)(), result.clone()));
        *lock(&self.usage_cached) = entry.clone();
        *lock(&self.usage_seen) = entry;
        Ok(result)
    }
}

#[derive(Clone, Debug)]
pub struct DriveSettings {
    pub enabled: bool,
    pub error: Option<String>,
    /// The HARS write folder's name; never Python's default (`Road Viewer`), so the two are told apart at a glance.
    pub folder_name: String,
}
impl Default for DriveSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            error: None,
            folder_name: super::DEFAULT_DRIVE_FOLDER.to_owned(),
        }
    }
}

/// The HARS-owned Drive write root. Uploads go only into a folder carrying `hars_owner=roadviewer-hars` and this
/// data root's storage id; deletes only touch media files carrying both and living in that folder.
pub struct DriveBackend {
    core: DriveCore,
    pub settings: DriveSettings,
    pub root: PathBuf,
    pub auth: Arc<DriveAuth>,
    upload_chunk: usize,
    owned_folder: Mutex<Option<String>>,
    usage_running: AtomicBool,
    usage_tried_at: Mutex<Option<f64>>,
}
impl fmt::Debug for DriveBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DriveBackend(connected={})", self.auth.connected())
    }
}

enum SendError {
    Expired,
    Backend(BackendError),
}
impl From<BackendError> for SendError {
    fn from(e: BackendError) -> Self {
        Self::Backend(e)
    }
}

/// The local file being uploaded: read on a blocking thread, hashed as it is read.
struct LocalFile {
    file: std::fs::File,
    h256: Sha256,
    hmd5: Md5,
}
impl LocalFile {
    async fn read(mut self, n: usize) -> Result<(Self, Vec<u8>)> {
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut buf = vec![0; n];
            let mut got = 0;
            while got < n {
                match self.file.read(&mut buf[got..]) {
                    Ok(0) => break,
                    Ok(k) => got += k,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(BackendError::from_io(&e)),
                }
            }
            buf.truncate(got);
            self.h256.update(&buf);
            self.hmd5.update(&buf);
            Ok((self, buf))
        })
        .await
        .map_err(|_| BackendError::new(ErrorCode::Io, "join"))?
    }
    /// Hashes `n` bytes without keeping them (the part a resumed session already holds).
    async fn skip(mut self, n: u64) -> Result<Self> {
        let mut left = n;
        while left > 0 {
            let (next, data) = self.read(left.min(UPLOAD_CHUNK as u64) as usize).await?;
            self = next;
            if data.is_empty() {
                return Err(BackendError::new(ErrorCode::Io, "local_shrank"));
            }
            left -= data.len() as u64;
        }
        Ok(self)
    }
}

impl DriveBackend {
    pub fn new(
        settings: DriveSettings,
        root: impl Into<PathBuf>,
        auth: Arc<DriveAuth>,
        client: DriveClient,
        clock: Clock,
    ) -> Self {
        Self {
            core: DriveCore::new(client, settings.enabled, clock),
            settings,
            root: root.into(),
            auth,
            upload_chunk: UPLOAD_CHUNK,
            owned_folder: Mutex::default(),
            usage_running: AtomicBool::new(false),
            usage_tried_at: Mutex::default(),
        }
    }

    pub fn client(&self) -> &DriveClient {
        &self.core.client
    }

    /// Multiples of 256 KiB only (tests use small chunks; production keeps 16 MiB).
    pub fn set_upload_chunk(&mut self, size: usize) {
        assert!(
            size > 0 && size.is_multiple_of(256 << 10),
            "resumable chunks are multiples of 256 KiB"
        );
        self.upload_chunk = size;
    }

    fn scope(&self) -> Option<Scope> {
        Some(Scope {
            folder_id: self.auth.folder_id()?,
            storage_id: self.storage_id()?,
            owned: true,
        })
    }

    pub fn last_result(&self) -> Option<Probe> {
        if !self.settings.enabled {
            return Some(Probe::disabled(self.settings.error.clone()));
        }
        self.core.avail.fresh()
    }
    pub fn known_result(&self) -> Option<Probe> {
        if !self.settings.enabled {
            return Some(Probe::disabled(self.settings.error.clone()));
        }
        self.core.avail.known()
    }

    /// For playback and downloads: a recent 'connected' is trusted instead of asking Google again.
    pub async fn read_state(&self) -> Probe {
        if self.core.avail.connected_within(READ_TRUST_SECONDS) {
            return Probe::connected();
        }
        self.probe(true).await
    }
    pub async fn stat_recent(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
        self.core.stat_recent(r).await
    }
    pub async fn find_all(&self) -> Result<HashMap<(String, String), Vec<RemoteRef>>> {
        self.core.find_all(self.scope()).await
    }
    pub async fn snapshot(&self) -> Result<HashMap<String, RemoteStat>> {
        self.core.snapshot(self.auth.folder_id()).await
    }
    /// Python `bulk_stats()`: stat() answers from one folder listing until end_bulk_stats(). verify() and delete()
    /// still ask about the file itself.
    pub async fn begin_bulk_stats(&self) -> Result<()> {
        let snapshot = self.snapshot().await?;
        *lock(&self.core.bulk) = Some(snapshot);
        Ok(())
    }
    pub fn end_bulk_stats(&self) {
        *lock(&self.core.bulk) = None;
    }

    /// The latest measured usage whatever its age; None before the first measurement and while signed out.
    pub fn last_usage(&self) -> Option<Usage> {
        lock(&self.core.usage_seen)
            .as_ref()
            .filter(|_| self.auth.connected())
            .map(|(_, u)| u.clone())
    }
    /// Measures in the background at most once per USAGE_REFRESH_SECONDS, even when Drive fails. True while one runs.
    pub fn refresh_usage(self: &Arc<Self>) -> bool {
        if !self.settings.enabled || !self.auth.connected() {
            return false;
        }
        if self.usage_running.load(Ordering::SeqCst) {
            return true;
        }
        let now = (self.core.clock)();
        let seen = lock(&self.core.usage_seen).as_ref().map(|(at, _)| *at);
        let tried = *lock(&self.usage_tried_at);
        if seen.is_some_and(|at| now - at < USAGE_REFRESH_SECONDS)
            || tried.is_some_and(|at| now - at < USAGE_REFRESH_SECONDS)
        {
            return false;
        }
        if self.usage_running.swap(true, Ordering::SeqCst) {
            return true;
        }
        *lock(&self.usage_tried_at) = Some(now);
        let me = self.clone();
        tokio::spawn(async move {
            let _ = me.core.usage().await;
            me.usage_running.store(false, Ordering::SeqCst);
        });
        true
    }

    async fn probe_now(&self) -> Probe {
        let folder_id = self.auth.folder_id();
        let result = self
            .core
            .probe_folder(folder_id.clone(), self.storage_id(), true)
            .await;
        *lock(&self.owned_folder) = folder_id.filter(|_| result.state == State::Connected);
        result
    }

    /// The bound folder, once it is confirmed to be this data root's HARS folder.
    async fn owned_folder(&self) -> Result<(String, String)> {
        let (Some(folder_id), Some(storage_id)) = (self.auth.folder_id(), self.storage_id()) else {
            return Err(BackendError::new(ErrorCode::Unavailable, "folder_missing"));
        };
        if lock(&self.owned_folder).as_deref() == Some(folder_id.as_str()) {
            return Ok((folder_id, storage_id));
        }
        let Some(folder) = self
            .core
            .client
            .get(&folder_id, "trashed,appProperties,mimeType", false)
            .await?
        else {
            return Err(BackendError::new(ErrorCode::Unavailable, "folder_missing"));
        };
        let p = props(&folder);
        if text(&folder, "mimeType") != Some(FOLDER_MIME)
            || prop(&p, OWNER_KEY) != Some(OWNER_VALUE)
            || prop(&p, "rv_storage_id") != Some(storage_id.as_str())
        {
            return Err(BackendError::foreign());
        }
        if folder
            .get("trashed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(BackendError::new(ErrorCode::Unavailable, "folder_trashed"));
        }
        *lock(&self.owned_folder) = Some(folder_id.clone());
        Ok((folder_id, storage_id))
    }

    async fn find_folder(&self, storage_id: &str) -> Result<Vec<String>> {
        let query = format!(
            "appProperties has {{ key='rv_storage_id' and value='{}' }} and appProperties has {{ key='{OWNER_KEY}' and value='{OWNER_VALUE}' }} and mimeType='{FOLDER_MIME}' and trashed=false",
            q(storage_id)
        );
        let mut found: Vec<(String, String)> = self
            .core
            .client
            .list(&query, "files(id,createdTime,appProperties)")
            .await?
            .iter()
            .filter(|f| prop(&props(f), OWNER_KEY) == Some(OWNER_VALUE))
            .filter_map(|f| {
                Some((
                    text(f, "createdTime").unwrap_or_default().to_owned(),
                    text(f, "id")?.to_owned(),
                ))
            })
            .collect();
        // Several (a create retried by hand, a lagging list): always the oldest, so every caller picks the same.
        found.sort();
        Ok(found.into_iter().map(|(_, id)| id).collect())
    }

    async fn try_activate(&self) -> Result<()> {
        if self.auth.folder_id().is_some() {
            return Ok(());
        }
        let local = self.storage_id();
        let mut storage_id = local.clone().or_else(|| self.auth.storage_id());
        let mut found = match &storage_id {
            Some(s) => self.find_folder(s).await?,
            None => Vec::new(),
        };
        if found.is_empty()
            && local.is_none()
            && let Some(s) = &storage_id
        {
            // About to create: a folder whose create answer was lost may not be listed yet, so look once more.
            self.core.client.http.backoff(0).await;
            found = self.find_folder(s).await?;
        }
        let folder_id = if let Some(first) = found.into_iter().next() {
            Some(first)
        } else if local.is_none() {
            let sid = match &storage_id {
                Some(s) => s.clone(),
                None => {
                    // Recorded before the create: a create whose answer was lost is found next time.
                    let sid = uuid::Uuid::new_v4().simple().to_string();
                    self.auth.remember(None, Some(&sid))?;
                    storage_id = Some(sid.clone());
                    sid
                }
            };
            let name = if self.settings.folder_name.trim().is_empty() {
                super::DEFAULT_DRIVE_FOLDER
            } else {
                self.settings.folder_name.trim()
            };
            Some(
                self.core
                    .client
                    .create_folder(name, json!({"rv_storage_id": sid, OWNER_KEY: OWNER_VALUE}))
                    .await?,
            )
        } else {
            // Our id but no HARS folder carrying it: stays unavailable, never claims another folder.
            None
        };
        if let (Some(folder_id), Some(sid)) = (folder_id, storage_id) {
            if local.is_none() {
                fsutil::write_local_id(&self.root, LOCAL_KEY, &sid)
                    .map_err(|e| BackendError::from_io(&e))?;
            }
            self.auth.remember(Some(&folder_id), Some(&sid))?;
        }
        Ok(())
    }

    fn completed(
        &self,
        meta: &Value,
        size: u64,
        resume: &mut UploadResume,
        on_progress: Progress<'_>,
        sent_md5: Option<String>,
    ) -> Result<RemoteRef> {
        let mut r = remote_ref(meta);
        r.completed = sent_md5.is_some();
        r.sent_md5 = sent_md5;
        if r.file_id.is_none() {
            return Err(BackendError::new(ErrorCode::Io, "no_file_id"));
        }
        if r.bytes != size {
            return Err(BackendError::new(ErrorCode::Mismatch, "size"));
        }
        resume.bytes_sent = size;
        on_progress(size, resume)?;
        Ok(r)
    }

    /// The file is read once: the part the session holds is only hashed, the rest hashed and sent. The whole file
    /// is checked before the last chunk goes out, so a session carrying a wrong rv_sha256 is never completed.
    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        local: &Path,
        session: &SessionUri,
        offset: u64,
        size: u64,
        sha256: Option<&str>,
        md5: Option<&str>,
        resume: &mut UploadResume,
        on_progress: Progress<'_>,
    ) -> std::result::Result<(Value, Option<String>), SendError> {
        let client = &self.core.client;
        let file = std::fs::File::open(local).map_err(|e| BackendError::from_io(&e))?;
        let mut reader = LocalFile {
            file,
            h256: Sha256::new(),
            hmd5: Md5::new(),
        }
        .skip(offset)
        .await?;
        let (mut read, mut sent, mut buf, mut stalls, mut checked) =
            (offset, offset, Bytes::new(), 0u32, None::<String>);
        loop {
            while buf.len() < self.upload_chunk && read < size {
                let want = ((self.upload_chunk - buf.len()) as u64).min(size - read) as usize;
                let (next, data) = reader.read(want).await?;
                reader = next;
                if data.is_empty() {
                    return Err(BackendError::new(ErrorCode::Io, "local_shrank").into());
                }
                read += data.len() as u64;
                buf = if buf.is_empty() {
                    Bytes::from(data)
                } else {
                    let mut joined = BytesMut::from(&buf[..]);
                    joined.extend_from_slice(&data);
                    joined.freeze()
                };
            }
            if read == size && checked.is_none() {
                let (next, extra) = reader.read(1).await?;
                reader = next;
                let sha_ok =
                    sha256.is_none_or(|s| hex::encode(reader.h256.clone().finalize()) == s);
                let digest = hex::encode(reader.hmd5.clone().finalize());
                if !extra.is_empty() || !sha_ok || md5.is_some_and(|m| digest != m) {
                    return Err(BackendError::new(ErrorCode::Mismatch, "local_changed").into());
                }
                checked = Some(digest);
            }
            // Signed out meanwhile: the session URI needs no token, so stop here, before the next chunk. The
            // persisted session stays for a later resume.
            if !self.auth.connected() {
                return Err(BackendError::new(ErrorCode::AuthExpired, "signed_out").into());
            }
            let answer = match client.put_chunk(session, buf.clone(), sent, size).await {
                Ok(a) => a,
                // The chunk may or may not have arrived: ask the session rather than resending blindly.
                Err(e) if matches!(e.code, ErrorCode::Unavailable | ErrorCode::RateLimited) => {
                    client.session_status(session, size).await?
                }
                Err(e) => return Err(e.into()),
            };
            let value = match answer {
                SessionAnswer::Expired => return Err(SendError::Expired),
                SessionAnswer::Done(meta) => return Ok((meta, checked)),
                SessionAnswer::Incomplete(v) => v,
            };
            let end = value + 1;
            if end < sent as i64 || end > (sent + buf.len() as u64) as i64 {
                return Err(BackendError::new(ErrorCode::Io, "bad_range").into());
            }
            let end = end as u64;
            if end == sent {
                stalls += 1;
                if stalls >= client.http.attempts {
                    return Err(BackendError::new(ErrorCode::Unavailable, "no_progress")
                        .retryable()
                        .into());
                }
                client.http.backoff(stalls - 1).await;
            } else {
                stalls = 0;
            }
            buf = buf.slice((end - sent) as usize..);
            sent = end;
            resume.bytes_sent = sent;
            on_progress(sent, resume)?;
        }
    }
}

impl ArchiveRead for DriveBackend {
    fn caps(&self) -> &'static Capabilities {
        &DRIVE_CAPS
    }
    async fn probe(&self, wait: bool) -> Probe {
        if !self.settings.enabled {
            return Probe::disabled(self.settings.error.clone());
        }
        self.core.avail.probe(wait, || self.probe_now()).await
    }
    fn last_state(&self) -> Option<State> {
        self.core.last_state()
    }
    fn invalidate(&self) {
        self.core.invalidate();
        *lock(&self.owned_folder) = None;
    }
    fn storage_id(&self) -> Option<String> {
        fsutil::local_id(&self.root, LOCAL_KEY)
    }
    async fn stat(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
        self.core.stat(r).await
    }
    async fn find(&self, id: &str, kind: &str) -> Result<Vec<RemoteRef>> {
        self.core.find(self.scope(), id, kind).await
    }
    async fn verify(
        &self,
        r: &RemoteRef,
        sha256: Option<&str>,
        size: Option<u64>,
        md5: Option<&str>,
    ) -> Result<Verdict> {
        self.core.verify(r, sha256, size, md5).await
    }
    async fn open(&self, r: &RemoteRef, start: u64, end: Option<u64>) -> Result<ChunkReader> {
        self.core.open(r, start, end).await
    }
    async fn download_to(&self, r: &RemoteRef, dest: &Path, sha256: Option<&str>) -> Result<()> {
        self.core.download_to(r, dest, sha256).await
    }
    async fn usage(&self) -> Result<Usage> {
        self.core.usage().await
    }
}

impl ArchiveWrite for DriveBackend {
    /// Creates the folder only on a fresh setup (no folder bound, no local storage id); otherwise only verifies.
    /// A folder whose binding was lost is found again by its storage id and owner mark, never recreated.
    async fn activate(&self) {
        if !self.settings.enabled || !self.auth.connected() {
            return;
        }
        self.invalidate();
        let _ = self.try_activate().await;
        self.invalidate();
    }

    async fn upload(
        &self,
        local: &Path,
        id: &str,
        kind: &str,
        sha256: Option<&str>,
        md5: Option<&str>,
        size: u64,
        resume: &mut UploadResume,
        on_progress: Progress<'_>,
    ) -> Result<RemoteRef> {
        let (folder_id, storage_id) = self.owned_folder().await?;
        let mut session = resume.session.clone();
        let mut offset = 0;
        // A session's rv_sha256 was fixed when it started: one made for other content is never asked or continued.
        if session.is_some()
            && (resume.sha256.as_deref() != sha256 || resume.size.is_some_and(|s| s != size))
        {
            session = None;
            resume.session = None;
            resume.bytes_sent = 0;
        }
        if let Some(s) = &session {
            match self.core.client.session_status(s, size).await? {
                SessionAnswer::Done(meta) => {
                    return self.completed(&meta, size, resume, on_progress, None);
                }
                SessionAnswer::Incomplete(v) => offset = (v + 1) as u64,
                SessionAnswer::Expired => session = None,
            }
        }
        let app_props = json!({"rv_id": id, "rv_kind": kind, "rv_sha256": sha256.unwrap_or(""), "rv_storage_id": storage_id, OWNER_KEY: OWNER_VALUE});
        // One fresh session if the current one expires part-way.
        for _ in 0..2 {
            let current = match session.take() {
                Some(s) => s,
                None => {
                    let s = self
                        .core
                        .client
                        .start_resumable(
                            &format!("{id}--{kind}"),
                            &[&folder_id],
                            app_props.clone(),
                            size,
                        )
                        .await?;
                    offset = 0;
                    *resume = UploadResume {
                        session: Some(s.clone()),
                        bytes_sent: 0,
                        sha256: sha256.map(str::to_owned),
                        size: Some(size),
                    };
                    on_progress(0, resume)?;
                    s
                }
            };
            match self
                .send(
                    local,
                    &current,
                    offset,
                    size,
                    sha256,
                    md5,
                    resume,
                    on_progress,
                )
                .await
            {
                Ok((meta, sent_md5)) => {
                    return self.completed(&meta, size, resume, on_progress, sent_md5);
                }
                Err(SendError::Expired) => continue,
                Err(SendError::Backend(e)) => {
                    // The session carries the old rv_sha256: drop it so a persisted resume cannot complete it later.
                    if e.reason() == Some("local_changed") {
                        resume.session = None;
                        resume.bytes_sent = 0;
                    }
                    return Err(e);
                }
            }
        }
        Err(BackendError::new(ErrorCode::Unavailable, "session_expired").retryable())
    }

    /// One status query for a persisted upload; nothing is sent. Without a session or size: expired.
    async fn session_status(&self, resume: &UploadResume) -> Result<SessionState> {
        let (Some(session), Some(size)) = (
            resume.session.as_ref().filter(|s| !s.0.is_empty()),
            resume.size,
        ) else {
            return Ok(SessionState::Expired);
        };
        Ok(
            match self.core.client.session_status(session, size).await? {
                SessionAnswer::Done(_) => SessionState::Done,
                SessionAnswer::Incomplete(_) => SessionState::Incomplete,
                SessionAnswer::Expired => SessionState::Expired,
            },
        )
    }

    async fn delete(&self, r: &RemoteRef) -> Result<bool> {
        let file_id = r.file_id.clone().unwrap_or_default();
        lock(&self.core.stat_cache).remove(&file_id);
        let folder_id = self.auth.folder_id();
        if file_id.is_empty() || folder_id.as_deref() == Some(file_id.as_str()) {
            return Err(BackendError::foreign());
        }
        let meta = match self
            .core
            .client
            .get(&file_id, "appProperties,trashed,mimeType,parents", false)
            .await
        {
            Ok(Some(meta)) => meta,
            Ok(None) => return Ok(true),
            Err(_) => return Ok(false),
        };
        let p = props(&meta);
        let in_folder = folder_id.as_deref().is_some_and(|f| {
            meta.get("parents")
                .and_then(Value::as_array)
                .is_some_and(|ps| ps.iter().any(|x| x.as_str() == Some(f)))
        });
        let ours = self
            .storage_id()
            .is_some_and(|local| prop(&p, "rv_storage_id") == Some(local.as_str()))
            && prop(&p, "rv_id").is_some()
            && prop(&p, OWNER_KEY) == Some(OWNER_VALUE)
            && text(&meta, "mimeType") != Some(FOLDER_MIME)
            && in_folder;
        if !ours {
            return Err(BackendError::foreign());
        }
        Ok(self.core.client.delete(&file_id).await.unwrap_or(false))
    }
}

/// A Drive folder written by the Python Road Viewer, read with Python's own filters (`rv_storage_id` of the
/// Python data root). It has no upload, delete or activate: nothing in it is ever changed.
pub struct DriveLegacy {
    core: DriveCore,
    storage_id: String,
    folder_id: Mutex<Option<String>>,
}
impl fmt::Debug for DriveLegacy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DriveLegacy()")
    }
}

impl DriveLegacy {
    /// `storage_id`: Python's `.archive.json` `gdrive_storage_id`; `folder_id`: Python's bound folder when known,
    /// else found by locate().
    pub fn new(
        client: DriveClient,
        storage_id: &str,
        folder_id: Option<&str>,
        clock: Clock,
    ) -> Self {
        Self {
            core: DriveCore::new(client, true, clock),
            storage_id: storage_id.to_owned(),
            folder_id: Mutex::new(folder_id.map(str::to_owned)),
        }
    }
    /// Reads only the storage id from a Python data root (never its token file).
    pub fn python_storage_id(python_root: &Path) -> Option<String> {
        fsutil::local_id(python_root, LOCAL_KEY)
    }
    pub fn folder_id(&self) -> Option<String> {
        lock(&self.folder_id).clone()
    }
    /// Python's `_find_folder`: the oldest folder carrying the storage id; a HARS-marked folder is never taken.
    pub async fn locate(&self) -> Result<Option<String>> {
        let query = format!(
            "appProperties has {{ key='rv_storage_id' and value='{}' }} and mimeType='{FOLDER_MIME}' and trashed=false",
            q(&self.storage_id)
        );
        let mut found: Vec<(String, String)> = self
            .core
            .client
            .list(&query, "files(id,createdTime,appProperties)")
            .await?
            .iter()
            .filter(|f| prop(&props(f), OWNER_KEY).is_none())
            .filter_map(|f| {
                Some((
                    text(f, "createdTime").unwrap_or_default().to_owned(),
                    text(f, "id")?.to_owned(),
                ))
            })
            .collect();
        found.sort();
        let first = found.into_iter().next().map(|(_, id)| id);
        if first.is_some() {
            *lock(&self.folder_id) = first.clone();
            self.core.invalidate();
        }
        Ok(first)
    }
    fn scope(&self) -> Option<Scope> {
        Some(Scope {
            folder_id: self.folder_id()?,
            storage_id: self.storage_id.clone(),
            owned: false,
        })
    }
    pub async fn find_all(&self) -> Result<HashMap<(String, String), Vec<RemoteRef>>> {
        self.core.find_all(self.scope()).await
    }
    pub async fn stat_recent(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
        self.core.stat_recent(r).await
    }
}

impl ArchiveRead for DriveLegacy {
    fn caps(&self) -> &'static Capabilities {
        &DRIVE_CAPS
    }
    async fn probe(&self, wait: bool) -> Probe {
        self.core
            .avail
            .probe(wait, || {
                self.core
                    .probe_folder(self.folder_id(), Some(self.storage_id.clone()), false)
            })
            .await
    }
    fn last_state(&self) -> Option<State> {
        self.core.last_state()
    }
    fn invalidate(&self) {
        self.core.invalidate();
    }
    fn storage_id(&self) -> Option<String> {
        Some(self.storage_id.clone())
    }
    async fn stat(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
        self.core.stat(r).await
    }
    async fn find(&self, id: &str, kind: &str) -> Result<Vec<RemoteRef>> {
        self.core.find(self.scope(), id, kind).await
    }
    async fn verify(
        &self,
        r: &RemoteRef,
        sha256: Option<&str>,
        size: Option<u64>,
        md5: Option<&str>,
    ) -> Result<Verdict> {
        self.core.verify(r, sha256, size, md5).await
    }
    async fn open(&self, r: &RemoteRef, start: u64, end: Option<u64>) -> Result<ChunkReader> {
        self.core.open(r, start, end).await
    }
    async fn download_to(&self, r: &RemoteRef, dest: &Path, sha256: Option<&str>) -> Result<()> {
        self.core.download_to(r, dest, sha256).await
    }
    async fn usage(&self) -> Result<Usage> {
        self.core.usage().await
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingCode {
    pub user_code: String,
    pub verification_url: String,
    pub expires_at: f64,
}

struct Cancel {
    set: AtomicBool,
    notify: tokio::sync::Notify,
}
impl Cancel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            set: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        })
    }
    fn cancel(&self) {
        self.set.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    fn is_set(&self) -> bool {
        self.set.load(Ordering::SeqCst)
    }
    async fn wait(&self, d: Duration) -> bool {
        let notified = self.notify.notified();
        if self.is_set() {
            return true;
        }
        tokio::select! {
            _ = notified => true,
            _ = tokio::time::sleep(d) => self.is_set(),
        }
    }
}

struct ConnState {
    pending: Option<PendingCode>,
    cancel: Arc<Cancel>,
    last_error: Option<String>,
}

pub type OnChange = Arc<dyn Fn(bool) + Send + Sync>;

/// Python `DriveConnection`: signing in with the device flow, one code at a time. A task polls at the interval
/// Google asks for until the grant, a refusal or expiry; after the grant the folder is set up and probed.
pub struct DriveConnection {
    pub backend: Arc<DriveBackend>,
    on_change: Option<OnChange>,
    state: Mutex<ConnState>,
    start_gate: tokio::sync::Mutex<()>,
    /// One second of Google's polling interval (tests shorten it).
    pub interval_unit: Duration,
}
impl fmt::Debug for DriveConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DriveConnection(pending={})", self.pending().is_some())
    }
}

impl DriveConnection {
    pub fn new(backend: Arc<DriveBackend>, on_change: Option<OnChange>) -> Arc<Self> {
        Arc::new(Self {
            backend,
            on_change,
            state: Mutex::new(ConnState {
                pending: None,
                cancel: Cancel::new(),
                last_error: None,
            }),
            start_gate: tokio::sync::Mutex::new(()),
            interval_unit: Duration::from_secs(1),
        })
    }
    pub fn with_interval_unit(
        backend: Arc<DriveBackend>,
        on_change: Option<OnChange>,
        unit: Duration,
    ) -> Arc<Self> {
        let mut c = Self::new(backend, on_change);
        Arc::get_mut(&mut c).expect("fresh").interval_unit = unit;
        c
    }

    /// The code the panel may show (never the device code).
    pub fn pending(&self) -> Option<PendingCode> {
        lock(&self.state)
            .pending
            .clone()
            .filter(|p| (self.backend.auth.clock)() < p.expires_at)
    }
    /// A code (access_denied, code_expired, Google's error token), never a message.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.state).last_error.clone()
    }

    pub async fn start(self: &Arc<Self>) -> Result<PendingCode> {
        let _gate = self.start_gate.lock().await;
        if let Some(p) = self.pending() {
            return Ok(p);
        }
        // A task left from an expired code stops at its next wait.
        lock(&self.state).cancel.cancel();
        let flow = match self.backend.auth.start_device_flow(None).await {
            Ok(f) => f,
            Err(e) => {
                lock(&self.state).last_error =
                    Some(e.reason().unwrap_or(e.code.as_str()).to_owned());
                return Err(e);
            }
        };
        let cancel = Cancel::new();
        let pending = PendingCode {
            user_code: flow.user_code,
            verification_url: flow.verification_url,
            expires_at: flow.expires_at,
        };
        {
            let mut state = lock(&self.state);
            state.cancel = cancel.clone();
            state.pending = Some(pending.clone());
            state.last_error = None;
        }
        let me = self.clone();
        tokio::spawn(async move { me.poll(cancel).await });
        Ok(pending)
    }

    async fn poll(self: Arc<Self>, cancel: Arc<Cancel>) {
        let auth = &self.backend.auth;
        let result = loop {
            let interval = auth.interval().filter(|i| *i > 0).unwrap_or(5);
            if cancel.wait(self.interval_unit * interval as u32).await {
                return;
            }
            let result = match auth.poll_once().await {
                Ok(p) => p.as_str().to_owned(),
                // Google or the network failing for now: the code is still valid, so the next poll follows.
                Err(e) if matches!(e.code, ErrorCode::Unavailable | ErrorCode::RateLimited) => {
                    "pending".to_owned()
                }
                Err(e) => e.reason().unwrap_or(e.code.as_str()).to_owned(),
            };
            if cancel.is_set() {
                return;
            }
            if result != "pending" {
                break result;
            }
        };
        let granted = result == Poll::Granted.as_str();
        if granted {
            self.backend.activate().await;
            self.backend.probe(false).await;
            let _ = self.backend.usage().await;
        }
        {
            let mut state = lock(&self.state);
            if cancel.is_set() {
                return;
            }
            state.pending = None;
            state.last_error = (!granted).then(|| match result.as_str() {
                "denied" => "access_denied".to_owned(),
                "expired" => "code_expired".to_owned(),
                other => other.to_owned(),
            });
        }
        if let Some(f) = &self.on_change {
            f(granted);
        }
    }

    /// Forgets the grant here at once; Google is told in the background and a revoke that fails changes nothing.
    pub fn disconnect(self: &Arc<Self>) {
        let token = {
            let mut state = lock(&self.state);
            state.cancel.cancel();
            state.pending = None;
            state.last_error = None;
            self.backend.auth.forget()
        };
        self.backend.invalidate();
        if token.is_some() {
            let auth = self.backend.auth.clone();
            tokio::spawn(async move { auth.revoke(token).await });
        }
        if let Some(f) = &self.on_change {
            f(false);
        }
    }
}
