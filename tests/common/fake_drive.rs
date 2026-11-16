//! A loopback fake of Google's OAuth device flow and Drive v3 (a port of the Python `tests/fake_drive.py`), shared
//! by the archive test binaries. Nothing here reaches Google.
#![allow(dead_code)]
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use md5::Md5;
use roadviewer_hars::archive::{self, Clock, UploadResume};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, Once},
};

pub const FOLDER: &str = "application/vnd.google-apps.folder";
// --- The fake ---

pub struct DropConnection;

pub fn quiet_drops() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !info.payload().is::<DropConnection>() {
                default(info);
            }
        }));
    });
}

#[derive(Clone, Debug)]
pub struct FFile {
    pub name: String,
    pub mime: String,
    pub parents: Vec<String>,
    pub props: BTreeMap<String, String>,
    pub content: Vec<u8>,
    pub trashed: bool,
    pub md5: Option<String>,
    pub sha256: Option<String>,
    pub created: Option<String>,
}
impl FFile {
    pub fn media(content: &[u8], parents: &[&str], props: &[(&str, &str)]) -> Self {
        Self {
            name: "n".into(),
            mime: "application/octet-stream".into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            props: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            content: content.to_vec(),
            trashed: false,
            md5: Some(md5_hex(content)),
            sha256: Some(sha_hex(content)),
            created: None,
        }
    }
    pub fn folder(name: &str, props: &[(&str, &str)], created: &str) -> Self {
        Self {
            name: name.into(),
            mime: FOLDER.into(),
            created: Some(created.into()),
            md5: None,
            sha256: None,
            ..Self::media(b"", &[], props)
        }
    }
}

pub struct Sess {
    pub received: Vec<u8>,
    pub total: Option<u64>,
    pub meta: Value,
    pub file_id: Option<String>,
}

pub struct Rule {
    pub path: String,
    pub status: u16,
    pub reason: Option<String>,
    pub times: u32,
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
}

#[derive(Default)]
pub struct St {
    pub files: BTreeMap<String, FFile>,
    pub sessions: HashMap<String, Sess>,
    pub access: HashSet<String>,
    pub refresh: HashSet<String>,
    pub issued: HashMap<String, String>,
    pub revoked: Vec<String>,
    pub requests: Vec<Req>,
    pub failures: Vec<Rule>,
    pub device_poll: VecDeque<&'static str>,
    pub quota_limit: Option<u64>,
    pub lose_completion: bool,
    pub drop_chunk_after_store: u32,
    pub partial_accept: Option<usize>,
    pub completion_override: Option<Value>,
    pub list_lag: u32,
    /// The line dies for a session once it holds this many bytes: its PUTs answer 503.
    pub cut_after: Option<usize>,
    /// Full-file downloads left that send half the file and then break the connection.
    pub cut_media: u32,
    /// A pause before each 64 KiB of a full-file download (a slow link).
    pub media_delay: Option<std::time::Duration>,
    /// Bytes a full-file download sends beyond the listed size, without a length (a file that grew
    /// after it was listed, a hostile server), counted into `media_sent` as they are sent.
    pub media_overrun: usize,
    pub media_sent: Arc<std::sync::atomic::AtomicUsize>,
    pub no_auth: bool,
    /// A file whose content is answered only after this long (a slow Drive while a client gives up).
    pub media_hold: HashMap<String, std::time::Duration>,
    /// Content requests held by  so far.
    pub media_held: u32,
    pub file_count: u32,
    pub session_count: u32,
    pub token_count: u32,
    pub device_count: u32,
}

pub struct Fake {
    pub url: String,
    pub st: Mutex<St>,
}
pub type Shared = Arc<Fake>;

impl Fake {
    pub fn st(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap()
    }
    pub fn fail_next(
        &self,
        path: &str,
        status: u16,
        reason: Option<&str>,
        times: u32,
        headers: &[(&str, &str)],
    ) {
        self.st().failures.push(Rule {
            path: path.into(),
            status,
            reason: reason.map(str::to_owned),
            times,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
    }
    pub fn requests(&self, since: usize) -> Vec<Req> {
        self.st().requests[since..].to_vec()
    }
    pub fn mark(&self) -> usize {
        self.st().requests.len()
    }
    pub fn ranges(&self, since: usize) -> Vec<String> {
        self.requests(since)
            .into_iter()
            .filter(|r| r.method == "PUT")
            .map(|r| r.headers.get("content-range").cloned().unwrap_or_default())
            .collect()
    }
    pub fn count(&self, since: usize, method: &str, path_start: &str) -> usize {
        self.requests(since)
            .iter()
            .filter(|r| r.method == method && r.path.starts_with(path_start))
            .count()
    }
    pub fn folders(&self) -> BTreeMap<String, FFile> {
        self.st()
            .files
            .iter()
            .filter(|(_, f)| f.mime == FOLDER)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    pub fn file(&self, id: &str) -> Option<FFile> {
        self.st().files.get(id).cloned()
    }
    pub fn put(&self, id: &str, f: FFile) {
        self.st().files.insert(id.into(), f);
    }
}

pub fn error_body(path: &str, status: u16, reason: Option<&str>) -> Value {
    let path = path.split('?').next().unwrap_or_default();
    if ["/device/code", "/token", "/revoke"].contains(&path) {
        return json!({"error": reason.unwrap_or("error")});
    }
    json!({"error": {"code": status, "message": reason.unwrap_or("error"), "errors": [{"reason": reason.unwrap_or("error"), "domain": "global"}]}})
}

pub fn file_json(id: &str, f: &FFile) -> Value {
    let mut out = json!({"kind": "drive#file", "id": id, "name": f.name, "mimeType": f.mime, "parents": f.parents,
                         "appProperties": f.props, "trashed": f.trashed});
    if let Some(c) = &f.created {
        out["createdTime"] = json!(c);
    }
    if f.mime != FOLDER {
        out["size"] = json!(f.content.len().to_string());
        out["md5Checksum"] = json!(f.md5);
        out["sha256Checksum"] = json!(f.sha256);
    }
    out
}

pub fn matches(f: &FFile, q: &str) -> bool {
    let re = |p: &str| regex::Regex::new(p).unwrap();
    for c in re(r"appProperties has \{\s*key='([^']*)' and value='([^']*)'\s*\}").captures_iter(q) {
        if f.props.get(&c[1]).map(String::as_str) != Some(&c[2]) {
            return false;
        }
    }
    for c in re(r"'([^']*)' in parents").captures_iter(q) {
        if !f.parents.iter().any(|p| p == &c[1]) {
            return false;
        }
    }
    for c in re(r"trashed\s*=\s*(true|false)").captures_iter(q) {
        if f.trashed != (&c[1] == "true") {
            return false;
        }
    }
    for c in re(r"mimeType\s*=\s*'([^']*)'").captures_iter(q) {
        if f.mime != c[1] {
            return false;
        }
    }
    true
}

pub fn reply(status: u16, body: Option<Value>, headers: &[(String, String)]) -> Response {
    let mut map = HeaderMap::new();
    let data = match body {
        Some(b) => {
            map.insert(
                "content-type",
                "application/json; charset=UTF-8".parse().unwrap(),
            );
            serde_json::to_vec(&b).unwrap()
        }
        None => Vec::new(),
    };
    for (k, v) in headers {
        map.insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    (StatusCode::from_u16(status).unwrap(), map, data).into_response()
}
pub fn ok(body: Value) -> Response {
    reply(200, Some(body), &[])
}

pub fn issue(st: &mut St, refresh: Option<String>) -> Value {
    st.token_count += 1;
    let access = format!("at-{}-secretaccess", st.token_count);
    st.access.insert(access.clone());
    let mut body = json!({"access_token": access, "expires_in": 3599, "token_type": "Bearer", "scope": archive::drive_auth::DRIVE_SCOPE});
    let refresh = refresh.unwrap_or_else(|| {
        let r = format!("rt-{}-secretrefresh", st.token_count);
        st.refresh.insert(r.clone());
        body["refresh_token"] = json!(r);
        r
    });
    st.issued.insert(access, refresh);
    body
}

pub fn new_file(st: &mut St, meta: &Value, content: Vec<u8>) -> String {
    st.file_count += 1;
    let id = format!("f{}", st.file_count);
    let props = meta["appProperties"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let f = FFile {
        name: meta["name"].as_str().unwrap_or("Untitled").into(),
        mime: meta["mimeType"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .into(),
        parents: meta["parents"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        props,
        md5: Some(md5_hex(&content)),
        sha256: Some(sha_hex(&content)),
        content,
        trashed: false,
        created: Some(format!("2026-11-03T00:00:00.{:06}Z", st.file_count)),
    };
    st.files.insert(id.clone(), f);
    id
}

pub async fn handle(
    State(fake): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let full = uri
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    let path = uri.path().to_owned();
    let query: HashMap<String, String> =
        url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
            .into_owned()
            .collect();
    let hdr = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let delay = {
        let mut st = fake.st();
        let delay = path
            .strip_prefix("/drive/v3/files/")
            .filter(|_| query.get("alt").map(String::as_str) == Some("media"))
            .and_then(|id| st.media_hold.get(id).copied());
        st.media_held += u32::from(delay.is_some());
        delay
    };
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    let mut guard = fake.st();
    let st = &mut *guard;
    st.requests.push(Req {
        method: method.to_string(),
        path: full.clone(),
        headers: headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_lowercase(),
                    v.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
    });
    if let Some(rule) = st
        .failures
        .iter_mut()
        .find(|r| r.times > 0 && full.contains(&r.path))
    {
        rule.times -= 1;
        let (status, reason, extra) = (rule.status, rule.reason.clone(), rule.headers.clone());
        return reply(
            status,
            Some(error_body(&full, status, reason.as_deref())),
            &extra,
        );
    }
    if ["/device/code", "/token", "/revoke"].contains(&path.as_str()) {
        let form: HashMap<String, String> =
            url::form_urlencoded::parse(&body).into_owned().collect();
        let f = |k: &str| form.get(k).cloned().unwrap_or_default();
        return match path.as_str() {
            "/device/code" => {
                if f("client_id").is_empty() {
                    return reply(401, Some(json!({"error": "invalid_client"})), &[]);
                }
                st.device_count += 1;
                ok(
                    json!({"device_code": format!("dc-{}", st.device_count), "user_code": "ABCD-EFGH",
                          "verification_url": "https://www.google.com/device", "expires_in": 1800, "interval": 5}),
                )
            }
            "/token" => match f("grant_type").as_str() {
                archive::drive_auth::DEVICE_GRANT => {
                    if !f("device_code").starts_with("dc-") {
                        return reply(400, Some(json!({"error": "invalid_grant"})), &[]);
                    }
                    match st.device_poll.pop_front().unwrap_or("granted") {
                        "granted" => ok(issue(st, None)),
                        step => {
                            let status = match step {
                                "authorization_pending" => 428,
                                "expired_token" => 400,
                                _ => 403,
                            };
                            reply(status, Some(json!({"error": step})), &[])
                        }
                    }
                }
                "refresh_token" => {
                    let refresh = f("refresh_token");
                    if !st.refresh.contains(&refresh) {
                        return reply(
                            400,
                            Some(
                                json!({"error": "invalid_grant", "error_description": "Token has been expired or revoked."}),
                            ),
                            &[],
                        );
                    }
                    ok(issue(st, Some(refresh)))
                }
                _ => reply(400, Some(json!({"error": "unsupported_grant_type"})), &[]),
            },
            _ => {
                let token = f("token");
                st.revoked.push(token.clone());
                if st.refresh.remove(&token) {
                    let gone: Vec<String> = st
                        .issued
                        .iter()
                        .filter(|(_, r)| **r == token)
                        .map(|(a, _)| a.clone())
                        .collect();
                    for a in gone {
                        st.access.remove(&a);
                    }
                } else if !st.access.remove(&token) {
                    return reply(400, Some(json!({"error": "invalid_token"})), &[]);
                }
                ok(json!({}))
            }
        };
    }
    let session_put = method == Method::PUT && query.contains_key("upload_id");
    if !st.no_auth && !session_put {
        let auth = hdr("authorization");
        if !auth
            .strip_prefix("Bearer ")
            .is_some_and(|t| st.access.contains(t))
        {
            return reply(401, Some(error_body(&path, 401, Some("authError"))), &[]);
        }
    }
    if path == "/drive/v3/about" {
        let used: usize = st.files.values().map(|f| f.content.len()).sum();
        let mut quota = json!({"usage": used.to_string(), "usageInDrive": used.to_string()});
        if let Some(l) = st.quota_limit {
            quota["limit"] = json!(l.to_string());
        }
        return ok(json!({"storageQuota": quota}));
    }
    if path == "/drive/v3/files" {
        if method == Method::GET {
            let q = query.get("q").cloned().unwrap_or_default();
            let mut found: Vec<Value> = st
                .files
                .iter()
                .filter(|(_, f)| matches(f, &q))
                .map(|(i, f)| file_json(i, f))
                .collect();
            if st.list_lag > 0 {
                st.list_lag -= 1;
                found.clear();
            }
            return ok(json!({"kind": "drive#fileList", "files": found}));
        }
        let meta: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        let id = new_file(st, &meta, Vec::new());
        return ok(file_json(&id, &st.files[&id]));
    }
    if let Some(id) = path.strip_prefix("/drive/v3/files/") {
        let id = id.to_owned();
        let Some(f) = st.files.get(&id).cloned() else {
            return reply(404, Some(error_body(&path, 404, Some("notFound"))), &[]);
        };
        if method == Method::DELETE {
            st.files.remove(&id);
            return reply(204, None, &[]);
        }
        if query.get("alt").map(String::as_str) != Some("media") {
            return ok(file_json(&id, &f));
        }
        let range = hdr("range");
        let Some(caps) = regex::Regex::new(r"^bytes=(\d+)-(\d*)$")
            .unwrap()
            .captures(&range)
        else {
            if st.media_overrun > 0 {
                return overrun_stream(f.content.clone(), st.media_overrun, st.media_sent.clone());
            }
            let cut = st.cut_media > 0;
            if cut {
                st.cut_media -= 1;
            }
            if !cut && st.media_delay.is_none() {
                return (StatusCode::OK, f.content.clone()).into_response();
            }
            return media_stream(f.content.clone(), cut, st.media_delay);
        };
        let len = f.content.len();
        let start: usize = caps[1].parse().unwrap();
        let end = caps[2]
            .parse::<usize>()
            .unwrap_or(len.saturating_sub(1))
            .min(len.saturating_sub(1));
        if start >= len {
            return reply(
                416,
                Some(error_body(&path, 416, Some("requestedRangeNotSatisfiable"))),
                &[("content-range".into(), format!("bytes */{len}"))],
            );
        }
        return (
            StatusCode::PARTIAL_CONTENT,
            [("content-range", format!("bytes {start}-{end}/{len}"))],
            f.content[start..=end].to_vec(),
        )
            .into_response();
    }
    if path == "/upload/drive/v3/files"
        && query.get("uploadType").map(String::as_str) == Some("resumable")
    {
        if method == Method::POST && !query.contains_key("upload_id") {
            st.session_count += 1;
            let upload_id = format!("up{}-secretsession", st.session_count);
            let total = hdr("x-upload-content-length").parse().ok();
            let meta: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            st.sessions.insert(
                upload_id.clone(),
                Sess {
                    received: Vec::new(),
                    total,
                    meta,
                    file_id: None,
                },
            );
            let location = format!(
                "{}/upload/drive/v3/files?uploadType=resumable&upload_id={upload_id}",
                fake.url
            );
            return reply(200, None, &[("location".into(), location)]);
        }
        if session_put {
            let upload_id = query["upload_id"].clone();
            let header = hdr("content-range");
            let (partial, drop_after) = (
                st.partial_accept.take(),
                st.drop_chunk_after_store > 0 && !body.is_empty(),
            );
            let Some(s) = st.sessions.get_mut(&upload_id) else {
                return reply(404, Some(error_body(&path, 404, Some("notFound"))), &[]);
            };
            if let Some(fid) = s.file_id.clone() {
                return ok(file_json(&fid, &st.files[&fid]));
            }
            if st.cut_after.is_some_and(|n| s.received.len() >= n) {
                return reply(503, Some(error_body(&path, 503, Some("backendError"))), &[]);
            }
            let status = regex::Regex::new(r"^bytes \*/(\d+|\*)$")
                .unwrap()
                .captures(&header)
                .map(|c| c[1].parse::<u64>().ok());
            let chunk = regex::Regex::new(r"^bytes (\d+)-(\d+)/(\d+|\*)$")
                .unwrap()
                .captures(&header);
            let total = if let Some(total) = status {
                total
            } else if let Some(c) = chunk {
                let (start, end): (usize, usize) = (c[1].parse().unwrap(), c[2].parse().unwrap());
                if start != s.received.len() || end - start + 1 != body.len() {
                    return reply(
                        400,
                        Some(error_body(&path, 400, Some("badContentRange"))),
                        &[],
                    );
                }
                let take = partial.unwrap_or(body.len()).min(body.len());
                s.received.extend_from_slice(&body[..take]);
                c[3].parse().ok()
            } else {
                return reply(
                    400,
                    Some(error_body(&path, 400, Some("badContentRange"))),
                    &[],
                );
            };
            if total.is_some() {
                s.total = total;
            }
            if drop_after {
                st.drop_chunk_after_store -= 1;
                drop(guard);
                std::panic::panic_any(DropConnection);
            }
            let s = st.sessions.get_mut(&upload_id).unwrap();
            if s.total.is_none_or(|t| (s.received.len() as u64) < t) {
                let h: Vec<(String, String)> = if s.received.is_empty() {
                    vec![]
                } else {
                    vec![("range".into(), format!("bytes=0-{}", s.received.len() - 1))]
                };
                return reply(308, None, &h);
            }
            let (meta, received) = (s.meta.clone(), s.received.clone());
            let used: usize = st.files.values().map(|f| f.content.len()).sum();
            if st
                .quota_limit
                .is_some_and(|l| (used + received.len()) as u64 > l)
            {
                return reply(
                    403,
                    Some(error_body(&path, 403, Some("storageQuotaExceeded"))),
                    &[],
                );
            }
            let fid = new_file(st, &meta, received);
            st.sessions.get_mut(&upload_id).unwrap().file_id = Some(fid.clone());
            let mut out = file_json(&fid, &st.files[&fid]);
            if let Some(Value::Object(o)) = st.completion_override.take() {
                for (k, v) in o {
                    out[k] = v;
                }
            }
            if std::mem::take(&mut st.lose_completion) {
                drop(guard);
                std::panic::panic_any(DropConnection);
            }
            return ok(out);
        }
    }
    reply(404, Some(error_body(&path, 404, Some("notFound"))), &[])
}

/// A full-file answer streamed in pieces: slowed by , or broken off halfway () after promising the
/// whole length, as a dropped connection looks to the client.
fn media_stream(content: Vec<u8>, cut: bool, delay: Option<std::time::Duration>) -> Response {
    let len = content.len();
    let stop = if cut { len / 2 } else { len };
    let pieces: Vec<Bytes> = content[..stop]
        .chunks(64 << 10)
        .map(Bytes::copy_from_slice)
        .collect();
    let stream =
        futures_util::stream::unfold((pieces.into_iter(), cut), move |(mut it, cut)| async move {
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            match it.next() {
                Some(b) => Some((Ok::<_, std::io::Error>(b), (it, cut))),
                None if cut => Some((Err(std::io::Error::other("cut")), (it, false))),
                None => None,
            }
        });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-length", len.to_string())
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

/// The file and then `extra` zero bytes, in 64 KiB pieces made as they are sent, with no length.
fn overrun_stream(
    content: Vec<u8>,
    extra: usize,
    sent: Arc<std::sync::atomic::AtomicUsize>,
) -> Response {
    const PIECE: usize = 64 << 10;
    let total = content.len() + extra;
    let stream = futures_util::stream::unfold(0usize, move |at| {
        let (content, sent) = (content.clone(), sent.clone());
        async move {
            if at >= total {
                return None;
            }
            let end = (at + PIECE).min(total);
            let piece: Vec<u8> = (at..end)
                .map(|i| content.get(i).copied().unwrap_or(0))
                .collect();
            sent.fetch_add(piece.len(), std::sync::atomic::Ordering::SeqCst);
            Some((Ok::<_, std::io::Error>(Bytes::from(piece)), end))
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

pub async fn start_fake() -> Shared {
    quiet_drops();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fake = Arc::new(Fake {
        url: format!("http://{}", listener.local_addr().unwrap()),
        st: Mutex::default(),
    });
    let app = Router::new().fallback(handle).with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    fake
}

// --- Helpers ---

pub fn sha_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}
pub fn md5_hex(b: &[u8]) -> String {
    hex::encode(Md5::digest(b))
}
pub fn bytes_of(size: usize, seed: u64) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..size)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}
pub fn clock_of(c: &Arc<Mutex<f64>>) -> Clock {
    let c = c.clone();
    Arc::new(move || *c.lock().unwrap())
}
pub fn no_progress() -> impl FnMut(u64, &UploadResume) -> archive::Result<()> + Send {
    |_, _| Ok(())
}
