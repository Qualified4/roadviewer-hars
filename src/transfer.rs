//! Export to one tar file and import back (Python `log_transfer.py`), multi-segment ZIP
//! downloads, the legacy multipart upload and the browser's upload failure report.
//!
//! The browser reads an export itself and imports one recording at a time through an ordinary
//! chunked upload session; the server never unpacks a tar, so member paths cannot escape.
use crate::{
    archive::{Archive, ArchiveRead, ErrorCode, resolve::RemoteFile},
    error::ApiError,
    http::App,
    listing::{self, file_size, has_video, location, original_bytes},
    media_read::{self, Source},
    parity::{
        Answer, Fail, RecordingOrder, SOURCES, STORED_KINDS, TAG_COLORS, download_name, local_time,
        message_disposition, nfc, number, other, py_strip, py_words,
    },
    storage,
    store::{atomic_json, now, read_json, valid_id},
    upload::{UploadFile, file_hash, remove_session},
};
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode, header as h},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path as FsPath, PathBuf},
    sync::{Arc, LazyLock},
};
use tokio::{
    io::AsyncReadExt,
    sync::{MutexGuard, mpsc},
};

pub const FORMAT: &str = "roadviewer-export";
pub const VERSION: u64 = 1;
const BLOCK: u64 = 512;
const RECORD_SLACK: usize = 4096;
const READ: usize = 1 << 20;
const EXPORT_IDS: usize = 150;
const IMPORT: &str = "import.json";
const EXPORTS: &str = ".exports";
const ENTRY_FIELDS: [(&str, &[&str]); 4] = [
    (
        "nas",
        &["bytes", "sha256", "archived_at", "restored_at", "error"],
    ),
    (
        "gdrive",
        &["file_id", "bytes", "sha256", "md5", "archived_at", "error"],
    ),
    ("removed", &["removed_at", "reason", "sha256", "bytes"]),
    ("local", &["remote", "error"]),
];
const REMOTE_FIELDS: [&str; 7] = [
    "backend",
    "file_id",
    "bytes",
    "sha256",
    "md5",
    "uploaded_at",
    "verified_by",
];
const RECORD_KEYS: [&str; 11] = [
    "id",
    "name",
    "uploaded",
    "files",
    "tags",
    "bookmarks",
    "pinned",
    "video_codecs",
    "video_origins",
    "originals",
    "storage",
];
fn re(p: &str) -> regex::Regex {
    regex::Regex::new(p).unwrap()
}
static SHA256: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^[0-9a-f]{64}$"));
static MD5: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^[0-9a-f]{32}$"));
static FILE_ID: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9_-]{1,256}$"));
static CODE: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^[a-z0-9_]{1,64}$"));
static ITEM_ID: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9_-]{1,64}$"));

/// Staging folders of interrupted imports and multipart uploads, and original-format files of
/// interrupted ZIP downloads; never a registered recording. Runs before any request is served.
pub fn cleanup_staging(root: &FsPath) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if (name.starts_with(".import-") || name.starts_with(".legacy-") || name == EXPORTS)
            && entry.file_type()?.is_dir()
        {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> Fail {
    Fail::bad("invalid_import", message)
}
fn request() -> Fail {
    Fail {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_request",
        message: None,
    }
}
fn dumps(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}
fn padding(size: u64) -> u64 {
    (BLOCK - size % BLOCK) % BLOCK
}
fn member_bytes(size: u64) -> u64 {
    BLOCK + size + padding(size)
}
fn tar_header(name: &str, size: u64, mtime: f64) -> Vec<u8> {
    let mut header = tar::Header::new_ustar();
    header
        .set_path(name)
        .expect("export member names are short");
    header.set_size(size);
    header.set_mtime(mtime.max(0.) as u64);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_username("roadviewer").unwrap();
    header.set_groupname("roadviewer").unwrap();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    header.as_bytes().to_vec()
}
fn mtime(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.)
}

type Tx = mpsc::Sender<std::io::Result<Bytes>>;
fn channel() -> (Tx, Body) {
    let (tx, rx) = mpsc::channel::<std::io::Result<Bytes>>(4);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    (tx, Body::from_stream(stream))
}
struct Closed;
async fn send(tx: &Tx, bytes: impl Into<Bytes>) -> Result<(), Closed> {
    tx.send(Ok(bytes.into())).await.map_err(|_| Closed)
}

async fn pinned_logs(app: &App) -> BTreeSet<String> {
    storage::load_settings(&app.store.root)
        .await
        .map(|s| storage::pins(&s))
        .unwrap_or_default()
}

/// The index entry as exported (whitelisted fields), or None for a plain local file.
fn export_entry(meta: &Value, kind: &str) -> Option<Value> {
    let e = meta["storage"][kind]
        .as_object()
        .filter(|e| e.get("location").is_some_and(|l| l.as_str().is_some()))?;
    let where_ = e["location"].as_str().unwrap();
    let fields = ENTRY_FIELDS.iter().find(|(l, _)| *l == where_)?.1;
    let mut out = Map::new();
    out.insert("location".into(), json!(where_));
    for key in fields.iter().filter(|k| **k != "remote") {
        if let Some(v) = e.get(*key) {
            out.insert((*key).into(), v.clone());
        }
    }
    if where_ == "local" {
        if let Some(block) = e.get("remote").and_then(Value::as_object) {
            let block = REMOTE_FIELDS
                .iter()
                .filter_map(|k| block.get(*k).map(|v| ((*k).to_owned(), v.clone())))
                .collect::<Map<_, _>>();
            out.insert("remote".into(), Value::Object(block));
        }
        if out.get("error").is_some_and(|v| v != "remote_mismatch") {
            out.remove("error");
        }
        return (out.len() > 1).then_some(Value::Object(out));
    }
    Some(Value::Object(out))
}
fn record(meta: &Value, originals: Value, pinned: bool) -> Value {
    let storage = STORED_KINDS
        .iter()
        .filter_map(|k| export_entry(meta, k).map(|e| ((*k).to_owned(), e)))
        .collect::<Map<_, _>>();
    let files = meta["files"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, v)| STORED_KINDS.contains(&k.as_str()) && v.is_string())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect::<Map<_, _>>();
    let or = |key: &str, default: Value| meta.get(key).cloned().unwrap_or(default);
    json!({"id":meta["id"],"name":or("name",json!("")),"uploaded":or("uploaded",json!(0)),"files":files,
        "tags":or("tags",json!([])),"bookmarks":or("bookmarks",json!([])),"pinned":pinned,
        "video_codecs":or("video_codecs",json!({})),"video_origins":or("video_origins",json!({})),
        "originals":originals,"storage":storage})
}

struct Plan {
    folder: PathBuf,
    meta: Value,
    files: Vec<(&'static str, u64, f64)>,
    record_size: usize,
    pinned: bool,
}
fn plan(folder: PathBuf, meta: Value, pinned: bool) -> Plan {
    let mut files = vec![];
    for kind in STORED_KINDS {
        if location(&meta, kind) != "local" {
            continue;
        }
        let Ok(m) = std::fs::metadata(folder.join(kind)) else {
            continue;
        };
        if m.is_file() && m.len() > 0 && m.len() < 8u64.pow(11) {
            files.push((kind, m.len(), mtime(&m)));
        }
    }
    let zeros = files
        .iter()
        .map(|(k, size, _)| {
            (
                (*k).to_owned(),
                json!({"bytes":size,"sha256":"0".repeat(64)}),
            )
        })
        .collect::<Map<_, _>>();
    let record_size = dumps(&record(&meta, Value::Object(zeros), pinned)).len() + RECORD_SLACK;
    Plan {
        folder,
        meta,
        files,
        record_size,
        pinned,
    }
}

pub async fn export(
    State(app): State<App>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Answer<Response> {
    let mut ids: Vec<String> = vec![];
    for id in q.get("ids").map(String::as_str).unwrap_or("").split(',') {
        if !id.is_empty() && !ids.iter().any(|i| i == id) {
            ids.push(id.to_owned());
        }
    }
    if ids.len() > EXPORT_IDS {
        return Err(request());
    }
    if ids.is_empty() {
        ids = app.store.records.read().await.keys().cloned().collect();
    }
    let pins = pinned_logs(&app).await;
    let mut plans = vec![];
    for id in &ids {
        let folder = app.store.folder(id).await?;
        let meta = app.store.meta(id).await?;
        let pinned = pins.contains(id);
        plans.push(
            tokio::task::spawn_blocking(move || plan(folder, meta, pinned))
                .await
                .map_err(|_| request())?,
        );
    }
    if plans.is_empty() {
        return Err(Fail::not_found());
    }
    plans.sort_by_cached_key(|p| RecordingOrder::of(&p.meta));
    let manifest = dumps(
        &json!({"format":FORMAT,"version":VERSION,"app_version":env!("CARGO_PKG_VERSION"),"exported_at":now(),
        "recordings":plans.iter().map(|p|json!({"id":p.meta["id"],"name":p.meta.get("name").cloned().unwrap_or(json!("")),
            "files":p.files.iter().map(|f|f.0).collect::<Vec<_>>(),"bytes":p.files.iter().map(|f|f.1).sum::<u64>()})).collect::<Vec<_>>()}),
    );
    let total = member_bytes(manifest.len() as u64)
        + plans
            .iter()
            .map(|p| {
                p.files.iter().map(|f| member_bytes(f.1)).sum::<u64>()
                    + member_bytes(p.record_size as u64)
            })
            .sum::<u64>()
        + 2 * BLOCK;
    let stamp = now();
    let (tx, body) = channel();
    let store = app.store.clone();
    tokio::spawn(async move {
        let _ = async {
            let mut head = tar_header("manifest.json", manifest.len() as u64, stamp);
            head.extend_from_slice(&manifest);
            head.resize(head.len() + padding(manifest.len() as u64) as usize, 0);
            send(&tx, head).await?;
            for plan in &plans {
                stream_recording(&tx, &store, plan, stamp).await?;
            }
            send(&tx, vec![0u8; 2 * BLOCK as usize]).await
        }
        .await;
    });
    let (y, mo, d, hh, mm, _) = local_time(stamp);
    let name = format!("roadviewer-export-{y:04}{mo:02}{d:02}-{hh:02}{mm:02}.tar");
    Ok((
        [
            (h::CONTENT_TYPE, "application/x-tar".to_owned()),
            (h::CONTENT_LENGTH, total.to_string()),
            (h::CONTENT_DISPOSITION, message_disposition(&name)),
            (h::CACHE_CONTROL, "no-store".to_owned()),
        ],
        body,
    )
        .into_response())
}
async fn stream_recording(
    tx: &Tx,
    store: &crate::store::Store,
    plan: &Plan,
    stamp: f64,
) -> Result<(), Closed> {
    let id = plan.meta["id"].as_str().unwrap_or("");
    let mut sent = Map::new();
    for (kind, size, file_mtime) in &plan.files {
        send(tx, tar_header(&format!("{id}/{kind}"), *size, *file_mtime)).await?;
        let mut digest = Sha256::new();
        let mut left = *size;
        // A file replaced or removed since planning is not exported: its bytes are zeros
        // and the record leaves it out, so the declared Content-Length still holds.
        if let Ok(mut source) = tokio::fs::File::open(plan.folder.join(kind)).await
            && source.metadata().await.is_ok_and(|m| m.len() == *size)
        {
            let mut buffer = vec![0u8; READ];
            while left > 0 {
                let want = (left as usize).min(READ);
                match source.read(&mut buffer[..want]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        digest.update(&buffer[..n]);
                        left -= n as u64;
                        send(tx, buffer[..n].to_vec()).await?;
                    }
                }
            }
        }
        if left > 0 {
            while left > 0 {
                let n = left.min(READ as u64);
                left -= n;
                send(tx, vec![0u8; n as usize]).await?;
            }
        } else {
            sent.insert(
                (*kind).into(),
                json!({"bytes":size,"sha256":hex::encode(digest.finalize())}),
            );
        }
        send(tx, vec![0u8; padding(*size) as usize]).await?;
    }
    let current = store.meta(id).await.unwrap_or_else(|_| plan.meta.clone());
    let mut data = dumps(&record(&current, Value::Object(sent.clone()), plan.pinned));
    if data.len() > plan.record_size {
        data = dumps(&record(&plan.meta, Value::Object(sent), plan.pinned));
    }
    data.resize(plan.record_size, b' ');
    let mut member = tar_header(&format!("{id}/record.json"), data.len() as u64, stamp);
    member.extend_from_slice(&data);
    member.resize(member.len() + padding(data.len() as u64) as usize, 0);
    send(tx, member).await
}

// --- Record validation (the record comes from the browser and is never trusted).

fn is_number(v: &Value, low: f64) -> bool {
    number(v).is_some_and(|n| n.is_finite() && n >= low)
}
fn text(value: &Value, limit: usize, what: &str) -> Answer<String> {
    let error = || invalid(format!("{what}이(가) 올바르지 않습니다."));
    let s = value.as_str().ok_or_else(error)?;
    let s = nfc(s);
    let s = py_strip(&s);
    if s.is_empty() || s.chars().count() > limit || s.chars().any(other) {
        return Err(error());
    }
    Ok(s.to_owned())
}
fn remote_block(kind: &str, block: &Value) -> Answer<Value> {
    let ok = block.as_object().is_some_and(|b| {
        b.keys().all(|k| REMOTE_FIELDS.contains(&k.as_str()))
            && b.get("backend").is_some_and(|v| v == "gdrive")
            && b.get("file_id")
                .and_then(Value::as_str)
                .is_some_and(|v| FILE_ID.is_match(v))
            && b.get("sha256")
                .and_then(Value::as_str)
                .is_some_and(|v| SHA256.is_match(v))
            && b.get("bytes").is_some_and(|v| v.is_u64())
            && b.get("md5")
                .is_none_or(|v| v.is_null() || v.as_str().is_some_and(|s| MD5.is_match(s)))
            && b.get("uploaded_at").is_none_or(|v| is_number(v, 0.))
            && b.get("verified_by")
                .is_none_or(|v| v.as_str().is_some_and(|s| CODE.is_match(s)))
    });
    if !ok {
        return Err(invalid(format!(
            "{kind}의 원격 사본 정보가 올바르지 않습니다."
        )));
    }
    Ok(block.clone())
}
fn entry(kind: &str, e: &Value) -> Answer<Value> {
    let error = || invalid(format!("{kind}의 저장 위치 정보가 올바르지 않습니다."));
    let o = e.as_object().ok_or_else(error)?;
    let where_ = o.get("location").and_then(Value::as_str).unwrap_or("");
    let fields = ENTRY_FIELDS
        .iter()
        .find(|(l, _)| *l == where_)
        .ok_or_else(error)?
        .1;
    if o.keys()
        .any(|k| k != "location" && !fields.contains(&k.as_str()))
    {
        return Err(error());
    }
    let mut out = Map::new();
    out.insert("location".into(), json!(where_));
    for (key, value) in o {
        if key == "location" {
            continue;
        }
        let ok = match key.as_str() {
            "bytes" => value.is_u64(),
            "sha256" => value.as_str().is_some_and(|v| SHA256.is_match(v)),
            "md5" => value.is_null() || value.as_str().is_some_and(|v| MD5.is_match(v)),
            "file_id" => value.as_str().is_some_and(|v| FILE_ID.is_match(v)),
            "error" | "reason" => value.as_str().is_some_and(|v| CODE.is_match(v)),
            "remote" => true,
            _ => is_number(value, 0.),
        };
        if !ok {
            return Err(error());
        }
        out.insert(key.clone(), value.clone());
    }
    if where_ == "local" {
        if out.get("error").is_some_and(|v| v != "remote_mismatch") {
            return Err(error());
        }
        if let Some(block) = out.get("remote").cloned() {
            out.insert("remote".into(), remote_block(kind, &block)?);
        }
    }
    if matches!(where_, "nas" | "gdrive")
        && !(out.contains_key("sha256") && out.contains_key("bytes"))
    {
        return Err(error());
    }
    if where_ == "gdrive" && !out.contains_key("file_id") {
        return Err(error());
    }
    Ok(Value::Object(out))
}

/// `log_transfer.parse_record`: the validated record, or a message for the user.
pub fn parse_record(value: &Value) -> Answer<Value> {
    let Some(v) = value.as_object().filter(|o| {
        o.keys().all(|k| RECORD_KEYS.contains(&k.as_str()))
            && ["id", "name", "originals"]
                .iter()
                .all(|k| o.contains_key(*k))
    }) else {
        return Err(invalid("기록 정보의 형식이 올바르지 않습니다."));
    };
    let id = v["id"]
        .as_str()
        .filter(|i| valid_id(i))
        .ok_or_else(|| invalid("기록 ID가 올바르지 않습니다."))?;
    let name = text(&v["name"], 300, "기록 이름")?;
    let uploaded = v.get("uploaded").cloned().unwrap_or_else(|| json!(now()));
    if !is_number(&uploaded, 0.) {
        return Err(invalid("업로드 시각이 올바르지 않습니다."));
    }
    let bad_files = || invalid("업로드 파일 이름이 올바르지 않습니다.");
    let mut files = Map::new();
    let raw_files = v.get("files").cloned().unwrap_or_else(|| json!({}));
    let raw_files = raw_files.as_object().ok_or_else(bad_files)?;
    if raw_files
        .keys()
        .any(|k| !STORED_KINDS.contains(&k.as_str()))
    {
        return Err(bad_files());
    }
    for (kind, file) in raw_files {
        let file = text(file, 255, "업로드 파일 이름")?;
        if file.contains('/') || file.contains('\\') || file == "." || file == ".." {
            return Err(bad_files());
        }
        files.insert(kind.clone(), json!(file));
    }
    let bad_tags = || invalid("태그 정보가 올바르지 않습니다.");
    let tags_value = v.get("tags").cloned().unwrap_or_else(|| json!([]));
    let tags_in = tags_value
        .as_array()
        .filter(|t| t.len() <= 100)
        .ok_or_else(bad_tags)?;
    let mut tags: Vec<(String, Value)> = vec![];
    for tag in tags_in {
        let o = tag
            .as_object()
            .filter(|o| o.keys().all(|k| k == "name" || k == "color"))
            .ok_or_else(bad_tags)?;
        let name = py_words(&text(
            o.get("name").unwrap_or(&Value::Null),
            32,
            "태그 이름",
        )?);
        let color = match o.get("color") {
            None => "mint",
            Some(c) => c
                .as_str()
                .filter(|c| TAG_COLORS.contains(c))
                .ok_or_else(|| invalid("태그 색상이 올바르지 않습니다."))?,
        };
        let key = name.to_lowercase();
        if !tags.iter().any(|(k, _)| *k == key) {
            tags.push((key, json!({"name":name,"color":color})));
        }
    }
    let bad_marks = || invalid("북마크 정보가 올바르지 않습니다.");
    let marks_value = v.get("bookmarks").cloned().unwrap_or_else(|| json!([]));
    let marks_in = marks_value
        .as_array()
        .filter(|t| t.len() <= 100)
        .ok_or_else(bad_marks)?;
    let mut marks: BTreeMap<String, Value> = BTreeMap::new();
    for b in marks_in {
        let o = b
            .as_object()
            .filter(|o| {
                o.len() == 4
                    && ["id", "time", "title", "note"]
                        .iter()
                        .all(|k| o.contains_key(*k))
            })
            .ok_or_else(bad_marks)?;
        let mark = o["id"]
            .as_str()
            .filter(|i| ITEM_ID.is_match(i))
            .ok_or_else(bad_marks)?;
        if !is_number(&o["time"], 0.) || number(&o["time"]).unwrap() > 86400. {
            return Err(bad_marks());
        }
        let note = nfc(o["note"].as_str().ok_or_else(bad_marks)?);
        let note = py_strip(&note);
        if note.chars().count() > 1000 || note.chars().any(|c| other(c) && c != '\n' && c != '\t') {
            return Err(bad_marks());
        }
        let title = py_words(&text(&o["title"], 80, "북마크 제목")?);
        marks.insert(
            mark.to_owned(),
            json!({"id":mark,"time":o["time"],"title":title,"note":note}),
        );
    }
    let mut bookmarks = marks.into_values().collect::<Vec<_>>();
    bookmarks.sort_by(|a, b| {
        number(&a["time"])
            .unwrap()
            .total_cmp(&number(&b["time"]).unwrap())
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let pinned = v.get("pinned").cloned().unwrap_or(json!(false));
    if !pinned.is_boolean() {
        return Err(invalid("고정 정보가 올바르지 않습니다."));
    }
    let sources = SOURCES.iter().map(|s| s.0).collect::<Vec<_>>();
    let codecs = v.get("video_codecs").cloned().unwrap_or_else(|| json!({}));
    if !codecs.as_object().is_some_and(|c| {
        c.iter()
            .all(|(k, v)| sources.contains(&k.as_str()) && (v == "h264" || v == "hevc"))
    }) {
        return Err(invalid("영상 코덱 정보가 올바르지 않습니다."));
    }
    let origins = v.get("video_origins").cloned().unwrap_or_else(|| json!({}));
    if !origins.as_object().is_some_and(|c| {
        c.iter()
            .all(|(k, v)| sources.contains(&k.as_str()) && is_number(v, f64::NEG_INFINITY))
    }) {
        return Err(invalid("영상 시작 정보가 올바르지 않습니다."));
    }
    let originals_in = v["originals"]
        .as_object()
        .filter(|o| o.keys().all(|k| STORED_KINDS.contains(&k.as_str())))
        .ok_or_else(|| invalid("원본 파일 목록이 올바르지 않습니다."))?;
    let mut originals = Map::new();
    for kind in STORED_KINDS {
        let Some(info) = originals_in.get(kind) else {
            continue;
        };
        let ok = info.as_object().is_some_and(|i| {
            i.len() == 2
                && i.get("bytes")
                    .and_then(Value::as_u64)
                    .is_some_and(|b| b > 0)
                && i.get("sha256")
                    .and_then(Value::as_str)
                    .is_some_and(|s| SHA256.is_match(s))
        });
        if !ok {
            return Err(invalid(format!("{kind} 파일 정보가 올바르지 않습니다.")));
        }
        originals.insert(kind.into(), info.clone());
    }
    let storage_in = v.get("storage").cloned().unwrap_or_else(|| json!({}));
    let storage_in = storage_in
        .as_object()
        .filter(|o| o.keys().all(|k| STORED_KINDS.contains(&k.as_str())))
        .ok_or_else(|| invalid("저장 위치 정보가 올바르지 않습니다."))?;
    let mut storage = Map::new();
    for kind in STORED_KINDS {
        let Some(raw) = storage_in.get(kind) else {
            continue;
        };
        let e = entry(kind, raw)?;
        if originals.contains_key(kind) && e["location"] != "local" {
            return Err(invalid(format!(
                "{kind}의 저장 위치가 원본 파일과 맞지 않습니다."
            )));
        }
        if e["location"] == "local" && !originals.contains_key(kind) && e.get("remote").is_none() {
            continue;
        }
        storage.insert(kind.into(), e);
    }
    let out = json!({"id":id,"name":name,"uploaded":uploaded,"files":files,
        "tags":tags.into_iter().map(|t|t.1).collect::<Vec<_>>(),"bookmarks":bookmarks,"pinned":pinned,
        "video_codecs":codecs,"video_origins":origins,"originals":originals,"storage":storage});
    if log_digest(&out).is_none() {
        return Err(invalid("원본 로그(rlog.zst)가 없는 기록입니다."));
    }
    Ok(out)
}
/// The recording's rlog.zst content, by which duplicates are found: sent, or kept elsewhere.
fn log_digest(record: &Value) -> Option<String> {
    if let Some(sha) = record["originals"]["rlog.zst"]["sha256"].as_str() {
        return Some(sha.into());
    }
    let e = &record["storage"]["rlog.zst"];
    match e["location"].as_str()? {
        "nas" | "gdrive" => e["sha256"].as_str().map(Into::into),
        "local" => e["remote"]["sha256"].as_str().map(Into::into),
        _ => None,
    }
}
async fn stored_log_digest(folder: &FsPath, meta: &Value) -> Option<String> {
    if matches!(location(meta, "rlog.zst"), "nas" | "gdrive") {
        return meta["storage"]["rlog.zst"]["sha256"]
            .as_str()
            .map(Into::into);
    }
    let path = folder.join("rlog.zst");
    let m = tokio::fs::metadata(&path).await.ok()?;
    let known = &meta["content_hashes"]["rlog.zst"];
    #[cfg(unix)]
    let fresh_mtime = {
        use std::os::unix::fs::MetadataExt;
        known["mtime_ns"]
            .as_i64()
            .is_none_or(|t| t == m.mtime() * 1_000_000_000 + m.mtime_nsec())
    };
    #[cfg(not(unix))]
    let fresh_mtime = true;
    if known["size"].as_u64() == Some(m.len())
        && fresh_mtime
        && let Some(sha) = known["sha256"].as_str()
    {
        return Some(sha.into());
    }
    file_hash(&path).await.ok()
}
/// Why a record is not imported: its id is taken, or a stored recording has the same log.
async fn skipped(app: &App, record: &Value) -> Option<&'static str> {
    let id = record["id"].as_str().unwrap();
    if app.store.root.join(id).exists() || app.store.records.read().await.contains_key(id) {
        return Some("exists");
    }
    let wanted = log_digest(record)?;
    let metas = app
        .store
        .records
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for meta in metas {
        let folder = app.store.root.join(meta["id"].as_str().unwrap_or(""));
        if stored_log_digest(&folder, &meta).await.as_deref() == Some(wanted.as_str()) {
            return Some("duplicate");
        }
    }
    None
}

fn import_body(body: &[u8]) -> Answer<Value> {
    let error = || invalid("불러오기 요청이 올바르지 않습니다.");
    let body: Value = serde_json::from_slice(body).map_err(|_| error())?;
    let o = body
        .as_object()
        .filter(|o| {
            o.len() == 3
                && ["format", "version", "record"]
                    .iter()
                    .all(|k| o.contains_key(*k))
        })
        .ok_or_else(error)?;
    if o["format"] != FORMAT {
        return Err(invalid("Road Viewer 내보내기 파일이 아닙니다."));
    }
    if o["version"] != VERSION || !o["version"].is_u64() {
        let shown = match &o["version"] {
            Value::String(s) => format!("'{s}'"),
            Value::Null => "None".into(),
            Value::Bool(b) => if *b { "True" } else { "False" }.into(),
            other => other.to_string(),
        };
        return Err(invalid(format!(
            "지원하지 않는 내보내기 형식 버전입니다 ({shown}). 앱을 업데이트한 뒤 다시 시도하세요."
        )));
    }
    parse_record(&o["record"])
}
fn skipped_answer(reason: &str, record: &Value) -> Response {
    Json(json!({"skipped":reason,"id":record["id"]})).into_response()
}

pub async fn begin(State(app): State<App>, body: Bytes) -> Answer<Response> {
    if body.len() > 1 << 20 {
        return Err(Fail::too_large());
    }
    let record = import_body(&body)?;
    if let Some(reason) = skipped(&app, &record).await {
        return Ok(skipped_answer(reason, &record));
    }
    let files = STORED_KINDS
        .iter()
        .filter_map(|kind| {
            let info = record["originals"].get(*kind)?;
            Some(UploadFile {
                name: (*kind).to_owned(),
                size: info["bytes"].as_u64().unwrap(),
                sha256: info["sha256"].as_str().map(Into::into),
            })
        })
        .collect::<Vec<_>>();
    if files.is_empty() {
        // Only references (every file is on Drive or the NAS): registered at once.
        crate::http_archive::list_state(&app.archive).await;
        let registration = app.store.registration.lock().await;
        let result = register(&app, &record, vec![], &registration).await;
        drop(registration);
        return imported(&app, result).await;
    }
    let id = record["id"].as_str().unwrap();
    if app.store.root.join(id).exists() {
        return Ok(skipped_answer("exists", &record));
    }
    let names = files.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
    // Session creation takes the admission and registration locks itself (storage::admit). The
    // id is checked again under the registration lock when the recording is published.
    let session = app.uploads.create_browser(files).await?;
    if let Err(e) = atomic_json(&session.path.join(IMPORT), &record).await {
        let _ = remove_session(&session.path).await;
        app.uploads
            .forget(&session.path.file_name().unwrap().to_string_lossy())
            .await;
        return Err(e.into());
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"session":session.path.file_name().unwrap().to_string_lossy(),"chunk_size":2*1024*1024,"files":names})),
    )
        .into_response())
}

pub async fn is_import(app: &App, session: &str) -> bool {
    valid_id(session)
        && app
            .store
            .root
            .join(".uploads")
            .join(session)
            .join(IMPORT)
            .is_file()
}

pub async fn finish(
    State(app): State<App>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Answer<Response> {
    let session = app
        .uploads
        .get(&name)
        .await
        .map_err(|_| Fail::not_found())?;
    let _life = session.lifecycle.try_write().map_err(|_| {
        Fail::new(
            StatusCode::CONFLICT,
            "upload_busy",
            "업로드 조각을 전송 중입니다.",
        )
    })?;
    app.uploads.check_session(&session, false, &headers).await?;
    if !session.path.join(IMPORT).is_file() {
        return Err(Fail::not_found());
    }
    for (i, f) in session.files.iter().enumerate() {
        if file_size(&session.path.join(i.to_string())) != Some(f.size) {
            return Err(Fail::new(
                StatusCode::CONFLICT,
                "incomplete_upload",
                "아직 전송되지 않은 파일이 있습니다.",
            ));
        }
    }
    let result = finish_registered(&app, &session).await;
    // The session goes in every outcome, as Python's `finally`: a failed import is chosen again.
    let _ = remove_session(&session.path).await;
    app.uploads.forget(&name).await;
    result
}
async fn finish_registered(app: &App, session: &crate::upload::Session) -> Answer<Response> {
    let record = read_json::<Value>(&session.path.join(IMPORT))
        .await
        .ok()
        .and_then(|v| parse_record(&v).ok())
        .ok_or_else(|| invalid("불러오기 정보가 올바르지 않습니다."))?;
    let names = session
        .files
        .iter()
        .map(|f| f.name.as_str())
        .collect::<Vec<_>>();
    if names
        != STORED_KINDS
            .iter()
            .copied()
            .filter(|k| record["originals"].get(*k).is_some())
            .collect::<Vec<_>>()
    {
        return Err(invalid("불러오기 정보가 올바르지 않습니다."));
    }
    let mut received = vec![];
    for (i, f) in session.files.iter().enumerate() {
        let path = session.path.join(i.to_string());
        if file_hash(&path).await? != record["originals"][&f.name]["sha256"] {
            return Err(Fail::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "checksum_mismatch",
                format!("{} 파일이 내보낸 내용과 다릅니다 (체크섬 불일치).", f.name),
            ));
        }
        received.push((path, f.name.clone()));
    }
    if let Some(reason) = skipped(app, &record).await {
        return Ok(skipped_answer(reason, &record));
    }
    // Refreshed before locking: admission reads the archive state without probing the share.
    crate::http_archive::list_state(&app.archive).await;
    let registration = app.store.registration.lock().await;
    let result = register(app, &record, received, &registration).await;
    drop(registration);
    imported(app, result).await
}
/// Python `answer`: archived files of recordings evicted by admission go once the locks are released, and the
/// archive's index totals are counted again with the imported references.
async fn imported(app: &App, result: Answer<Response>) -> Answer<Response> {
    if result
        .as_ref()
        .is_ok_and(|r| r.status() == StatusCode::CREATED)
    {
        crate::http_archive::process_pending(&app.archive).await;
        app.archive.forget_index();
    }
    result
}

/// Builds the recording in a staging folder under the data root and publishes it with one rename,
/// so an interrupted import leaves nothing in the list (staging is removed at startup).
async fn register(
    app: &App,
    record: &Value,
    received: Vec<(PathBuf, String)>,
    registration: &MutexGuard<'_, ()>,
) -> Answer<Response> {
    let id = record["id"].as_str().unwrap().to_owned();
    let temp = app
        .store
        .root
        .join(format!(".import-{}", crate::store::id()));
    let result = register_in(app, record, &received, &temp, &id, registration).await;
    let _ = tokio::fs::remove_dir_all(&temp).await;
    result
}
async fn register_in(
    app: &App,
    record: &Value,
    received: &[(PathBuf, String)],
    temp: &FsPath,
    id: &str,
    registration: &MutexGuard<'_, ()>,
) -> Answer<Response> {
    let stage = temp.join(id);
    tokio::fs::create_dir_all(&stage).await?;
    let mut hashes = Map::new();
    for (path, kind) in received {
        let target = stage.join(kind);
        tokio::fs::rename(path, &target).await?;
        let m = tokio::fs::metadata(&target).await?;
        #[cfg(unix)]
        let mtime_ns = {
            use std::os::unix::fs::MetadataExt;
            m.mtime() * 1_000_000_000 + m.mtime_nsec()
        };
        #[cfg(not(unix))]
        let mtime_ns = 0;
        hashes.insert(
            kind.clone(),
            json!({"size":m.len(),"mtime_ns":mtime_ns,"sha256":record["originals"][kind]["sha256"]}),
        );
        if kind.ends_with(".hevc") || kind.ends_with(".mp4") {
            let format = if kind.ends_with(".hevc") {
                "HEVC"
            } else {
                "MP4"
            };
            if crate::media::validate_upload(&target, kind).await.is_err() {
                return Err(invalid(format!(
                    "{kind}: 올바른 {format} 영상 파일이 아닙니다."
                )));
            }
        }
    }
    let automatic = app.store.processing.read().await["auto_convert"] == true;
    let status = if automatic { "queued" } else { "unconverted" };
    let mut meta = json!({"id":id,"name":record["name"],"uploaded":record["uploaded"],"status":status,
        "manual_conversion":false,"files":record["files"],"error":null,"content_hashes":hashes,
        "tags":record["tags"],"bookmarks":record["bookmarks"]});
    for key in ["video_codecs", "video_origins", "storage"] {
        if record[key].as_object().is_some_and(|o| !o.is_empty()) {
            meta[key] = record[key].clone();
        }
    }
    meta["video"] = json!(has_video(&stage, &meta));
    meta["bytes"] = json!(original_bytes(&stage, &meta));
    let root = app.store.root.join(id);
    if root.exists() || app.store.records.read().await.contains_key(id) {
        return Ok(skipped_answer("exists", record));
    }
    // Python `register`: the received files are already counted as used, so only the metadata
    // reserve is admitted, and this commit may evict old recordings under the policy.
    let kinds = received.iter().map(|(_, k)| k.clone()).collect::<Vec<_>>();
    storage::admit(
        &app.store,
        0,
        &kinds,
        true,
        &BTreeSet::new(),
        Some(registration),
    )
    .await?;
    atomic_json(&stage.join("meta.json"), &meta).await?;
    tokio::fs::rename(&stage, &root).await?;
    #[cfg(unix)]
    {
        let parent = app.store.root.clone();
        tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
            .await
            .map_err(|_| {
                Fail::from(ApiError(StatusCode::INTERNAL_SERVER_ERROR, "storage_error"))
            })??;
    }
    app.store.records.write().await.insert(id.to_owned(), meta);
    crate::track::invalidate_recording(&app.store.root.join(id));
    if record["pinned"] == true {
        let mut settings = storage::load_settings(&app.store.root).await?;
        let mut pins = storage::pins(&settings);
        pins.insert(id.to_owned());
        settings["pinned_logs"] = json!(pins);
        storage::save_settings(&app.store.root, &settings).await?;
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"imported":id,"status":status})),
    )
        .into_response())
}

// --- ZIP downloads (stored, streamed with ZIP64 data descriptors like Python's zipfile).

struct ZipEntry {
    name: String,
    crc: u32,
    size: u64,
    offset: u64,
    time: u16,
    date: u16,
}
#[derive(Default)]
struct Zip {
    offset: u64,
    entries: Vec<ZipEntry>,
}
fn dos(stamp: Option<f64>) -> (u16, u16) {
    let Some(stamp) = stamp else {
        return (0, (1 << 5) | 1);
    };
    let (y, mo, d, hh, mm, ss) = local_time(stamp.max(315532800.));
    (
        ((hh << 11) | (mm << 5) | (ss / 2)) as u16,
        ((((y - 1980).max(0) as u32) << 9) | (mo << 5) | d) as u16,
    )
}
fn flags(name: &str) -> u16 {
    if name.is_ascii() { 0x08 } else { 0x0808 }
}
impl Zip {
    fn local(&mut self, name: &str, (time, date): (u16, u16)) -> Vec<u8> {
        let mut b = vec![];
        b.extend(0x04034b50u32.to_le_bytes());
        b.extend(45u16.to_le_bytes());
        b.extend(flags(name).to_le_bytes());
        b.extend(0u16.to_le_bytes());
        b.extend(time.to_le_bytes());
        b.extend(date.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(u32::MAX.to_le_bytes());
        b.extend(u32::MAX.to_le_bytes());
        b.extend((name.len() as u16).to_le_bytes());
        b.extend(20u16.to_le_bytes());
        b.extend(name.as_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend([0u8; 16]);
        self.entries.push(ZipEntry {
            name: name.into(),
            crc: 0,
            size: 0,
            offset: self.offset,
            time,
            date,
        });
        self.offset += b.len() as u64;
        b
    }
    fn descriptor(&mut self, crc: u32, size: u64) -> Vec<u8> {
        let e = self.entries.last_mut().unwrap();
        e.crc = crc;
        e.size = size;
        self.offset += size;
        let mut b = vec![];
        b.extend(0x08074b50u32.to_le_bytes());
        b.extend(crc.to_le_bytes());
        b.extend(size.to_le_bytes());
        b.extend(size.to_le_bytes());
        self.offset += b.len() as u64;
        b
    }
    fn finish(&self) -> Vec<u8> {
        let mut b = vec![];
        for e in &self.entries {
            b.extend(0x02014b50u32.to_le_bytes());
            b.extend(((3u16 << 8) | 45).to_le_bytes());
            b.extend(45u16.to_le_bytes());
            b.extend(flags(&e.name).to_le_bytes());
            b.extend(0u16.to_le_bytes());
            b.extend(e.time.to_le_bytes());
            b.extend(e.date.to_le_bytes());
            b.extend(e.crc.to_le_bytes());
            b.extend(u32::MAX.to_le_bytes());
            b.extend(u32::MAX.to_le_bytes());
            b.extend((e.name.len() as u16).to_le_bytes());
            b.extend(28u16.to_le_bytes());
            b.extend(0u16.to_le_bytes());
            b.extend(0u16.to_le_bytes());
            b.extend(0u16.to_le_bytes());
            b.extend((0o100644u32 << 16).to_le_bytes());
            b.extend(u32::MAX.to_le_bytes());
            b.extend(e.name.as_bytes());
            b.extend(1u16.to_le_bytes());
            b.extend(24u16.to_le_bytes());
            b.extend(e.size.to_le_bytes());
            b.extend(e.size.to_le_bytes());
            b.extend(e.offset.to_le_bytes());
        }
        let cd_offset = self.offset;
        let cd_size = b.len() as u64;
        let n = self.entries.len() as u64;
        let eocd64 = cd_offset + cd_size;
        b.extend(0x06064b50u32.to_le_bytes());
        b.extend(44u64.to_le_bytes());
        b.extend(((3u16 << 8) | 45).to_le_bytes());
        b.extend(45u16.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(n.to_le_bytes());
        b.extend(n.to_le_bytes());
        b.extend(cd_size.to_le_bytes());
        b.extend(cd_offset.to_le_bytes());
        b.extend(0x07064b50u32.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(eocd64.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend(0x06054b50u32.to_le_bytes());
        b.extend(0u16.to_le_bytes());
        b.extend(0u16.to_le_bytes());
        b.extend((n.min(0xFFFF) as u16).to_le_bytes());
        b.extend((n.min(0xFFFF) as u16).to_le_bytes());
        b.extend((cd_size.min(u32::MAX as u64) as u32).to_le_bytes());
        b.extend(u32::MAX.to_le_bytes());
        b.extend(0u16.to_le_bytes());
        b
    }
}
/// Opened only when its turn comes, so a request holds one descriptor however many members it has.
struct Member {
    path: PathBuf,
    /// A file only the remote backend has, read from it as the ZIP is written (`path` is unused).
    remote: Option<RemoteFile>,
    name: String,
    /// Whether the member carries its file's modification time (Python `zip_stream`).
    stamped: bool,
    temporary: bool,
}
async fn remove_temporaries(members: &[Member]) {
    for m in members.iter().filter(|m| m.temporary) {
        let _ = tokio::fs::remove_file(&m.path).await;
    }
}
async fn stream_zip(tx: Tx, members: Vec<Member>, archive: Arc<Archive>) {
    let mut zip = Zip::default();
    let outcome = async {
        for m in &members {
            if let Some(file) = &m.remote {
                // A failure ends the stream (an incomplete ZIP), except a file deleted meanwhile, which is left out
                // as a removed local one is (Python `zip_stream`).
                let backend = archive.backend();
                let mut reader = match backend.open(&file.reference, 0, None).await {
                    Ok(r) => r,
                    Err(e) if e.code == ErrorCode::NotFound => continue,
                    Err(e) => {
                        let _ = tx.send(Err(std::io::Error::other(e.code.as_str()))).await;
                        return Err(Closed);
                    }
                };
                send(&tx, zip.local(&m.name, dos(m.stamped.then(now)))).await?;
                let mut crc = crc32fast::Hasher::new();
                let mut size = 0u64;
                loop {
                    match reader.next_chunk().await {
                        Ok(Some(chunk)) => {
                            crc.update(&chunk);
                            size += chunk.len() as u64;
                            send(&tx, chunk).await?;
                        }
                        Ok(None) => break,
                        Err(e) => {
                            let _ = tx.send(Err(std::io::Error::other(e.code.as_str()))).await;
                            return Err(Closed);
                        }
                    }
                }
                send(&tx, zip.descriptor(crc.finalize(), size)).await?;
                continue;
            }
            // Sizes and offsets are written after each member's data (data descriptors), so a file
            // removed since the request was planned is left out, as Python's `zip_stream` does.
            let Ok(mut file) = tokio::fs::File::open(&m.path).await else {
                continue;
            };
            let stamp = if m.stamped {
                file.metadata().await.map(|v| mtime(&v)).ok()
            } else {
                None
            };
            send(&tx, zip.local(&m.name, dos(stamp))).await?;
            let mut crc = crc32fast::Hasher::new();
            let mut size = 0u64;
            let mut buffer = vec![0u8; READ];
            loop {
                match file.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(n) => {
                        crc.update(&buffer[..n]);
                        size += n as u64;
                        send(&tx, buffer[..n].to_vec()).await?;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return Err(Closed);
                    }
                }
            }
            send(&tx, zip.descriptor(crc.finalize(), size)).await?;
        }
        send(&tx, zip.finish()).await
    }
    .await;
    let _ = outcome;
    // Every member's export goes, including those a disconnected client never reached.
    remove_temporaries(&members).await;
}
fn zip_response(name_header: String, body: Body) -> Response {
    (
        [
            (h::CONTENT_TYPE, "application/zip".to_owned()),
            (h::CONTENT_DISPOSITION, name_header),
            (h::CACHE_CONTROL, "no-store".to_owned()),
        ],
        body,
    )
        .into_response()
}

const DOWNLOAD_PARTS: [&str; 4] = ["rlog", "front", "wide", "qcamera"];
pub async fn download_zip(
    State(app): State<App>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Answer<Response> {
    let mut ids: Vec<String> = vec![];
    for id in q.get("ids").map(String::as_str).unwrap_or("").split(',') {
        if !id.is_empty() && !ids.iter().any(|i| i == id) {
            ids.push(id.to_owned());
        }
    }
    let parts = q
        .get("parts")
        .map(String::as_str)
        .unwrap_or("rlog")
        .split(',')
        .collect::<BTreeSet<_>>();
    if ids.is_empty() || ids.len() > 150 || parts.iter().any(|p| !DOWNLOAD_PARTS.contains(p)) {
        return Err(request());
    }
    if let Some(format) = q.get("format") {
        return download_selected(&app, &ids, &parts, format).await;
    }
    let mut members = vec![];
    let mut names = BTreeSet::new();
    for id in &ids {
        let folder = app.store.folder(id).await?;
        let meta = app.store.meta(id).await?;
        let mut kinds = vec![];
        if parts.contains("rlog") {
            kinds.push("rlog.zst".to_owned());
        }
        for row in listing::video_downloads(&folder, &meta) {
            if parts.contains(row["source"].as_str().unwrap()) {
                kinds.push(row["kind"].as_str().unwrap().to_owned());
            }
        }
        for kind in kinds {
            // Every file is resolved before streaming: one unreachable file refuses the whole ZIP.
            let (path, remote) = match media_read::media_file(&app.archive, id, &meta, &kind).await
            {
                Err(e) => return Ok(e.into_response()),
                Ok(None) => continue,
                Ok(Some(Source::Path(p))) => (p, None),
                Ok(Some(Source::Remote(r))) => (PathBuf::new(), Some(r)),
            };
            let mut name = download_name(&meta, &kind);
            if names.contains(&name) {
                name = format!("{}-{name}", &id[..8]);
            }
            names.insert(name.clone());
            members.push(Member {
                path,
                remote,
                name,
                stamped: true,
                temporary: false,
            });
        }
    }
    if members.is_empty() {
        return Err(Fail::not_found());
    }
    let (y, mo, d, hh, mm, ss) = local_time(now());
    let header = format!(
        "attachment; filename=\"roadviewer-{}-segments-{y:04}{mo:02}{d:02}-{hh:02}{mm:02}{ss:02}.zip\"",
        ids.len()
    );
    let (tx, body) = channel();
    tokio::spawn(stream_zip(tx, members, app.archive.clone()));
    Ok(zip_response(header, body))
}

async fn download_selected(
    app: &App,
    ids: &[String],
    parts: &BTreeSet<&str>,
    format: &str,
) -> Answer<Response> {
    if format != "mp4" && format != "original" {
        return Err(request());
    }
    type Planned = (Source, String, Option<(&'static str, f64)>, Value, PathBuf);
    let mut plans: Vec<Planned> = vec![];
    for id in ids {
        let folder = app.store.folder(id).await?;
        let meta = app.store.meta(id).await?;
        // Resolve only the file actually sent: an unreachable MP4 must not block its reachable original.
        let file = |kind: &'static str| media_read::media_file(&app.archive, id, &meta, kind);
        if parts.contains("rlog") {
            match file("rlog.zst").await {
                Err(e) => return Ok(e.into_response()),
                Ok(Some(source)) => plans.push((
                    source,
                    "rlog.zst".into(),
                    None,
                    meta.clone(),
                    folder.clone(),
                )),
                Ok(None) => {}
            }
        }
        for source in ["front", "wide", "qcamera"] {
            if !parts.contains(source) {
                continue;
            }
            let (_, original, mp4) = *SOURCES.iter().find(|s| s.0 == source).unwrap();
            let kept = || async {
                Ok::<_, media_read::MediaError>(match file(mp4).await? {
                    Some(found) => Some(found),
                    None => Some(folder.join("prepared").join(mp4))
                        .filter(|p| p.is_file())
                        .map(Source::Path),
                })
            };
            if format == "mp4" {
                match kept().await {
                    Err(e) => return Ok(e.into_response()),
                    Ok(Some(found)) => {
                        plans.push((found, mp4.into(), None, meta.clone(), folder.clone()))
                    }
                    Ok(None) => {}
                }
                continue;
            }
            match file(original).await {
                Err(e) => return Ok(e.into_response()),
                Ok(Some(found)) => {
                    plans.push((found, original.into(), None, meta.clone(), folder.clone()));
                    continue;
                }
                Ok(None) => {}
            }
            let found = match kept().await {
                Err(e) => return Ok(e.into_response()),
                Ok(Some(found)) => found,
                Ok(None) => continue,
            };
            let option = listing::download_options(&folder, &meta)
                .into_iter()
                .find(|r| r["source"] == source);
            if option.is_none_or(|o| o["original"] != true) {
                return Err(Fail::new(
                    StatusCode::CONFLICT,
                    "original_unavailable",
                    "요청한 원본 형식 파일을 만들 수 없습니다.",
                ));
            }
            let offset = meta["video_origins"][source].as_f64().unwrap_or(0.);
            plans.push((
                found,
                original.into(),
                Some((original, offset)),
                meta.clone(),
                folder.clone(),
            ));
        }
    }
    if plans.is_empty() {
        return Err(Fail::not_found());
    }
    let mut members = vec![];
    let mut names = BTreeSet::new();
    let exports = app.store.root.join(EXPORTS);
    // The exports made so far go with this request if it is dropped before the ZIP stream takes them over.
    let mut made = vec![];
    for (found, kind, export, meta, folder) in plans {
        let (path, remote, temporary) = match (export, found) {
            (Some((original, offset)), found) => {
                let ext = if original == "qcamera.ts" {
                    "ts"
                } else {
                    "hevc"
                };
                // One to export from a path is staged first, verified (Python `open_downloads`).
                let (input, staged) = match found {
                    Source::Path(p) => (p, None),
                    Source::Remote(r) => match media_read::stage(&app.archive, &folder, &r).await {
                        Ok(copy) => (copy.path().to_owned(), Some(copy)),
                        Err(e) => {
                            remove_temporaries(&members).await;
                            return Ok(e.into_response());
                        }
                    },
                };
                let output = exports.join(format!("{}.{ext}", crate::store::id()));
                let exported = match tokio::fs::create_dir_all(&exports).await {
                    Ok(()) => media_read::export_original(
                        input,
                        staged,
                        original.to_owned(),
                        output,
                        offset,
                    )
                    .await
                    .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "media_export_failed")),
                    Err(e) => Err(e.into()),
                };
                let output = match exported {
                    Ok(output) => output,
                    Err(e) => {
                        remove_temporaries(&members).await;
                        return Err(e.into());
                    }
                };
                let path = output.path().to_owned();
                made.push(output);
                (path, None, true)
            }
            (None, Source::Path(p)) => (p, None, false),
            (None, Source::Remote(r)) => (PathBuf::new(), Some(r), false),
        };
        let id = meta["id"].as_str().unwrap_or("");
        let mut name = download_name(&meta, &kind);
        if names.contains(&name) {
            name = format!("{}-{name}", &id[..8.min(id.len())]);
        }
        names.insert(name.clone());
        members.push(Member {
            path,
            remote,
            name,
            stamped: false,
            temporary,
        });
    }
    let header = message_disposition(&format!("roadviewer-{}-segments-{format}.zip", ids.len()));
    let (tx, body) = channel();
    // stream_zip removes every temporary member itself, whether or not the client stays.
    for export in made {
        export.keep();
    }
    tokio::spawn(stream_zip(tx, members, app.archive.clone()));
    Ok(zip_response(header, body))
}

// --- Legacy multipart upload and failure reports.

pub async fn legacy_upload(
    State(app): State<App>,
    headers: HeaderMap,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Answer<Response> {
    let Some(length) = headers
        .get(h::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
    else {
        return Err(Fail {
            status: StatusCode::LENGTH_REQUIRED,
            code: "content_length_required",
            message: None,
        });
    };
    let mut multipart = multipart.map_err(|_| request())?;
    // Python admits the whole request with a staging copy before registering. Its body waits in a
    // temporary folder outside the data root; here it is received into the data root, so the same
    // verdict is reached before the first byte is written. The session admission after receiving
    // then counts the received copy as used.
    {
        let _registration = app.store.registration.lock().await;
        storage::admit(&app.store, length, &[], true, &BTreeSet::new(), None).await?;
    }
    let stage = app
        .store
        .root
        .join(format!(".legacy-{}", crate::store::id()));
    tokio::fs::create_dir(&stage).await?;
    let result = legacy_in(&app, &headers, &mut multipart, &stage).await;
    let _ = tokio::fs::remove_dir_all(&stage).await;
    result
}
async fn legacy_in(
    app: &App,
    headers: &HeaderMap,
    multipart: &mut Multipart,
    stage: &FsPath,
) -> Answer<Response> {
    use tokio::io::AsyncWriteExt;
    let mut files = vec![];
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(Fail::too_large()),
            Err(_) => return Err(request()),
        };
        if field.name() != Some("files") {
            continue;
        }
        if files.len() >= 100 {
            return Err(Fail::bad(
                "invalid_files",
                "한 번에 1~100개 파일을 선택하세요.",
            ));
        }
        let name = field.file_name().unwrap_or("").to_owned();
        let path = stage.join(files.len().to_string());
        let mut out = tokio::fs::File::create(&path).await?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut field = field;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    hash.update(&chunk);
                    size += chunk.len() as u64;
                    out.write_all(&chunk).await?;
                }
                Ok(None) => break,
                Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                    return Err(Fail::too_large());
                }
                Err(_) => return Err(request()),
            }
        }
        // tokio's sync_all records a failed background write instead of returning it; flush reports it.
        out.flush().await?;
        out.sync_all().await?;
        files.push(UploadFile {
            name,
            size,
            sha256: Some(hex::encode(hash.finalize())),
        });
    }
    if files.is_empty() {
        return Err(Fail::bad(
            "invalid_files",
            "한 번에 1~100개 파일을 선택하세요.",
        ));
    }
    for f in &files {
        crate::upload::split_name(&f.name)?;
        if f.size == 0 {
            return Err(ApiError(StatusCode::BAD_REQUEST, "invalid_file").into());
        }
    }
    let session = app.uploads.create_browser(files).await?;
    let name = session
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    for i in 0..session.files.len() {
        tokio::fs::rename(stage.join(i.to_string()), session.path.join(i.to_string())).await?;
    }
    drop(session);
    match app.uploads.finish(&name, false, headers, Value::Null).await {
        Ok((status, value)) => Ok((status, Json(value)).into_response()),
        Err(e) => {
            if let Ok(s) = app.uploads.get(&name).await
                && !s.path.join("commit.json").exists()
            {
                let _ = remove_session(&s.path).await;
                app.uploads.forget(&name).await;
            }
            Err(e.into())
        }
    }
}

pub async fn failure(Path(id): Path<String>, body: Bytes) -> Answer<Json<Value>> {
    if !valid_id(&id) {
        return Err(Fail::not_found());
    }
    if body.len() > 2048 {
        return Err(Fail::too_large());
    }
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&body) else {
        return Err(request());
    };
    // No filenames or arbitrary client text in server logs beyond these short fields.
    let fields = [
        "stage",
        "attempt",
        "offset",
        "status",
        "errorName",
        "errorCode",
        "elapsedMs",
        "loaded",
        "total",
        "visibility",
    ]
    .iter()
    .map(|k| {
        let text = match body.get(*k) {
            None | Some(Value::Null) => "None".to_owned(),
            Some(Value::String(s)) => s.clone(),
            Some(Value::Bool(b)) => if *b { "True" } else { "False" }.to_owned(),
            Some(v) => v.to_string(),
        };
        (
            (*k).to_owned(),
            json!(text.chars().take(80).collect::<String>()),
        )
    })
    .collect::<Map<_, _>>();
    let details = serde_json::Value::Object(fields).to_string();
    tracing::warn!(session=%id,details=%details,"upload_failure_reported");
    Ok(Json(json!({"recorded":true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tar_header_is_python_ustar() {
        let h = tar_header("0123/rlog.zst", 5, 10.);
        assert_eq!(h.len(), 512);
        assert_eq!(&h[257..263], b"ustar\0");
        assert_eq!(&h[265..275], b"roadviewer");
    }
    #[test]
    fn record_rejects_escapes_and_foreign_kinds() {
        let id = "0".repeat(32);
        let ok = json!({"id":id,"name":"n","originals":{"rlog.zst":{"bytes":1,"sha256":"a".repeat(64)}}});
        assert!(parse_record(&ok).is_ok());
        for (key, value) in [
            ("id", json!("../x")),
            ("files", json!({"rlog.zst":"../rlog.zst"})),
            ("files", json!({"rlog.zst":"/abs"})),
            ("files", json!({"meta.json":"x"})),
            (
                "originals",
                json!({"../meta.json":{"bytes":1,"sha256":"a".repeat(64)}}),
            ),
            (
                "storage",
                json!({"rlog.zst":{"location":"gdrive","file_id":"x","bytes":1,"sha256":"a".repeat(64)}}),
            ),
        ] {
            let mut bad = ok.clone();
            bad[key] = value;
            assert!(parse_record(&bad).is_err(), "{key}");
        }
    }
}
