//! The recording index's `storage` block (Python `media_store`: `entry`, `set_location`, `remote`, `remote_refs`,
//! `drop_remote`, `promote_remote`, `public_storage`, ...). Same keys and values as Python, so a recording imported
//! from Python keeps its archive state.
use super::MEDIA_KINDS;
use serde_json::{Map, Value, json};

/// Videos first, the original log last: it is the only input for reconversion.
pub const ARCHIVE_ORDER: [&str; 7] = [
    "fcamera.mp4",
    "ecamera.mp4",
    "camera.mp4",
    "fcamera.hevc",
    "ecamera.hevc",
    "qcamera.ts",
    "rlog.zst",
];
pub const VIDEO_KINDS: [&str; 6] = [
    "fcamera.mp4",
    "ecamera.mp4",
    "camera.mp4",
    "fcamera.hevc",
    "ecamera.hevc",
    "qcamera.ts",
];
/// Index locations whose file lives on the archive instead of locally.
pub const REMOTE_LOCATIONS: [&str; 2] = ["nas", "gdrive"];
/// Backends whose files are addressed by a file id.
pub const REMOTE_BACKENDS: [&str; 1] = ["gdrive"];
pub const REMOTE_FIELDS: [&str; 7] = [
    "backend",
    "file_id",
    "bytes",
    "sha256",
    "md5",
    "uploaded_at",
    "verified_by",
];
pub const REMOTE_PENDING_FIELDS: [&str; 6] = [
    "backend",
    "session",
    "started_at",
    "bytes_sent",
    "sha256",
    "size",
];
pub const PRIVATE_FIELDS: [&str; 2] = ["remote_pending", "file_id"];

pub fn known_kind(kind: &str) -> bool {
    MEDIA_KINDS.contains(&kind)
}

/// Python `_bytes`: a non-negative integer, else 0.
pub fn bytes_of(e: &Value) -> u64 {
    e.get("bytes").and_then(Value::as_u64).unwrap_or(0)
}

pub fn entry(meta: &Value, kind: &str) -> Value {
    match meta.get("storage").and_then(|s| s.get(kind)) {
        Some(e) if e.is_object() && e.get("location").is_some_and(truthy) => e.clone(),
        _ => json!({"location": "local"}),
    }
}
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn location(meta: &Value, kind: &str) -> String {
    match entry(meta, kind).get("location") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "local".to_owned(),
    }
}

fn storage_mut(meta: &mut Value) -> &mut Map<String, Value> {
    if !meta.get("storage").is_some_and(Value::is_object) {
        meta["storage"] = json!({});
    }
    meta["storage"].as_object_mut().expect("storage object")
}

/// `fields` replaces the kind's entry.
pub fn set_location(meta: &mut Value, kind: &str, fields: Value) {
    storage_mut(meta).insert(kind.to_owned(), fields);
}

pub fn is_archived_location(where_: &str) -> bool {
    REMOTE_LOCATIONS.contains(&where_)
}

fn block(meta: &Value, kind: &str, key: &str) -> Option<Value> {
    entry(meta, kind)
        .get(key)
        .filter(|v| v.is_object())
        .cloned()
}
pub fn remote(meta: &Value, kind: &str) -> Option<Value> {
    block(meta, kind, "remote")
}
pub fn remote_pending(meta: &Value, kind: &str) -> Option<Value> {
    block(meta, kind, "remote_pending")
}

/// The entry to add a block to; a missing or legacy entry becomes a local one.
pub fn editable<'a>(meta: &'a mut Value, kind: &str) -> &'a mut Map<String, Value> {
    let storage = storage_mut(meta);
    let valid = storage
        .get(kind)
        .is_some_and(|e| e.is_object() && e.get("location").is_some_and(truthy));
    if !valid {
        storage.insert(kind.to_owned(), json!({"location": "local"}));
    }
    storage
        .get_mut(kind)
        .and_then(Value::as_object_mut)
        .expect("entry object")
}

fn checked(fields: Value, allowed: &[&str]) -> Value {
    if let Some(o) = fields.as_object() {
        let unknown: Vec<&String> = o
            .keys()
            .filter(|k| !allowed.contains(&k.as_str()))
            .collect();
        assert!(unknown.is_empty(), "unknown index fields: {unknown:?}");
    }
    fields
}
pub fn set_remote(meta: &mut Value, kind: &str, fields: Value) {
    editable(meta, kind).insert("remote".into(), checked(fields, &REMOTE_FIELDS));
}
pub fn set_remote_pending(meta: &mut Value, kind: &str, fields: Value) {
    editable(meta, kind).insert(
        "remote_pending".into(),
        checked(fields, &REMOTE_PENDING_FIELDS),
    );
}
pub fn clear_remote_pending(meta: &mut Value, kind: &str) {
    if let Some(e) = meta
        .get_mut("storage")
        .and_then(|s| s.get_mut(kind))
        .and_then(Value::as_object_mut)
    {
        e.remove("remote_pending");
    }
}
/// Python `_editable(meta, kind)['error'] = ...`.
pub fn set_error(meta: &mut Value, kind: &str, error: &str) {
    editable(meta, kind).insert("error".into(), json!(error));
}

/// A remote backend's file the index refers to (Python `remote_refs` rows).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub kind: String,
    pub backend: String,
    pub file_id: String,
    pub bytes: u64,
    pub sha256: Option<String>,
}
impl Ref {
    pub fn to_json(&self) -> Value {
        json!({"kind": self.kind, "backend": self.backend, "file_id": self.file_id, "bytes": self.bytes, "sha256": self.sha256})
    }
}

/// Moved files and, with `copies`, verified copies of the remote backends (NAS files are listed by kind).
pub fn remote_refs(meta: &Value, copies: bool) -> Vec<Ref> {
    let mut refs = Vec::new();
    for kind in MEDIA_KINDS {
        let e = entry(meta, kind);
        let loc = e["location"].as_str().unwrap_or("");
        let (block, name) = if loc == "gdrive" {
            (Some(e.clone()), Some("gdrive".to_owned()))
        } else if loc == "local" && copies {
            let b = remote(meta, kind);
            let n = b
                .as_ref()
                .and_then(|b| b.get("backend").and_then(Value::as_str).map(str::to_owned));
            (b, n)
        } else {
            (None, None)
        };
        let (Some(block), Some(name)) = (block, name) else {
            continue;
        };
        if !REMOTE_BACKENDS.contains(&name.as_str()) {
            continue;
        }
        let Some(file_id) = block
            .get("file_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        refs.push(Ref {
            kind: kind.to_owned(),
            backend: name,
            file_id: file_id.to_owned(),
            bytes: bytes_of(&block),
            sha256: block
                .get("sha256")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    refs
}

/// A remote copy describes one content: once the file holds other content its block, an upload in progress and a
/// failed verification go (the Drive file stays, an orphan `find()` adopts again). Returns the dropped block.
pub fn drop_remote(meta: &mut Value, kind: &str, sha256: Option<&str>) -> Option<Value> {
    let storage = meta.get_mut("storage")?.as_object_mut()?;
    let e = storage.get_mut(kind)?.as_object_mut()?;
    if e.get("location").and_then(Value::as_str) != Some("local") {
        return None;
    }
    let block = e.get("remote").filter(|b| b.is_object()).cloned();
    if let (Some(b), Some(sha)) = (&block, sha256)
        && b.get("sha256").and_then(Value::as_str) == Some(sha)
    {
        return None;
    }
    let mismatch = e.get("error").and_then(Value::as_str) == Some("remote_mismatch");
    if !(e.contains_key("remote") || e.contains_key("remote_pending") || mismatch) {
        return None;
    }
    e.remove("remote");
    e.remove("remote_pending");
    if mismatch {
        e.remove("error");
    }
    if e.len() == 1 {
        storage.remove(kind);
    }
    block
}

/// A local file about to be deleted whose verified remote copy is the same content: the copy becomes the stored
/// file (location gdrive). `content`: (size, sha256) of the local file, or None when it is gone already.
pub fn promote_remote(
    meta: &mut Value,
    kind: &str,
    content: Option<(u64, &str)>,
    now: f64,
) -> bool {
    let e = entry(meta, kind);
    if let Some(block) = remote(meta, kind) {
        let name = block.get("backend").and_then(Value::as_str).unwrap_or("");
        let file_id = block.get("file_id").and_then(Value::as_str).unwrap_or("");
        let sha = block.get("sha256").and_then(Value::as_str);
        if e["location"] == "local"
            && REMOTE_BACKENDS.contains(&name)
            && REMOTE_LOCATIONS.contains(&name)
            && !file_id.is_empty()
            && let Some(sha) = sha
            && content.is_none_or(|(size, digest)| size == bytes_of(&block) && digest == sha)
        {
            let archived_at = block
                .get("uploaded_at")
                .filter(|v| truthy(v))
                .cloned()
                .unwrap_or(json!(now));
            set_location(
                meta,
                kind,
                json!({"location": name, "file_id": file_id, "bytes": bytes_of(&block), "sha256": sha,
                       "md5": block.get("md5").cloned().unwrap_or(Value::Null), "archived_at": archived_at}),
            );
            return true;
        }
    }
    drop_remote(meta, kind, None);
    false
}

/// The index as API responses show it: an upload's session URI is a credential and file ids are not needed by the UI.
pub fn public_storage(storage: &Value) -> Value {
    let Some(map) = storage.as_object() else {
        return storage.clone();
    };
    let mut out = Map::new();
    for (kind, e) in map {
        let value = match e.as_object() {
            Some(o) => {
                let mut o: Map<String, Value> = o
                    .iter()
                    .filter(|(k, _)| !PRIVATE_FIELDS.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if let Some(Value::Object(r)) = o.get("remote").cloned() {
                    o.insert(
                        "remote".into(),
                        Value::Object(
                            r.into_iter()
                                .filter(|(k, _)| !PRIVATE_FIELDS.contains(&k.as_str()))
                                .collect(),
                        ),
                    );
                }
                Value::Object(o)
            }
            None => e.clone(),
        };
        out.insert(kind.clone(), value);
    }
    Value::Object(out)
}

pub fn archived(meta: &Value) -> bool {
    MEDIA_KINDS
        .iter()
        .any(|k| is_archived_location(&location(meta, k)))
}
pub fn archived_files(meta: &Value) -> u64 {
    MEDIA_KINDS
        .iter()
        .filter(|k| is_archived_location(&location(meta, k)))
        .count() as u64
}
pub fn archived_bytes(meta: &Value) -> u64 {
    MEDIA_KINDS
        .iter()
        .map(|k| entry(meta, k))
        .filter(|e| is_archived_location(e["location"].as_str().unwrap_or("")))
        .map(|e| bytes_of(&e))
        .sum()
}
/// Local files that also have a verified remote copy: (files, bytes).
pub fn remote_copy_totals(meta: &Value) -> (u64, u64) {
    let blocks: Vec<Value> = MEDIA_KINDS
        .iter()
        .filter(|k| location(meta, k) == "local")
        .filter_map(|k| remote(meta, k))
        .collect();
    (blocks.len() as u64, blocks.iter().map(bytes_of).sum())
}
pub fn mismatch_count(meta: &Value) -> u64 {
    MEDIA_KINDS
        .iter()
        .filter(|k| entry(meta, k).get("error").and_then(Value::as_str) == Some("remote_mismatch"))
        .count() as u64
}

/// Python `_same`: flags added to the entry meanwhile (removing, error) do not make it another copy.
pub fn same(a: &Value, b: &Value) -> bool {
    ["location", "sha256", "bytes"]
        .iter()
        .all(|k| a.get(*k) == b.get(*k))
}
/// Python `_kept`: a removal keeps what identified the copy.
pub fn kept(e: &Value) -> Map<String, Value> {
    ["sha256", "bytes"]
        .iter()
        .filter_map(|k| e.get(*k).map(|v| ((*k).to_owned(), v.clone())))
        .collect()
}

/// Python `content_hashes[kind]` as register_files writes it: fresh while the file keeps this size and mtime.
pub fn record_hash(meta: &mut Value, kind: &str, size: u64, mtime_ns: u64, sha256: &str) {
    if !meta.get("content_hashes").is_some_and(Value::is_object) {
        meta["content_hashes"] = json!({});
    }
    meta["content_hashes"][kind] = json!({"size": size, "mtime_ns": mtime_ns, "sha256": sha256});
}
/// The recorded sha256 when the file still has the recorded size and mtime.
pub fn fresh_hash(meta: &Value, kind: &str, size: u64, mtime_ns: u64) -> Option<String> {
    let known = meta.get("content_hashes")?.get(kind)?;
    (known.get("size").and_then(Value::as_u64) == Some(size)
        && known.get("mtime_ns").and_then(Value::as_u64) == Some(mtime_ns))
    .then(|| {
        known
            .get("sha256")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
    .flatten()
}
pub fn known_hash(meta: &Value, kind: &str) -> Option<(Option<u64>, String)> {
    let known = meta.get("content_hashes")?.get(kind)?;
    Some((
        known.get("size").and_then(Value::as_u64),
        known.get("sha256")?.as_str()?.to_owned(),
    ))
}

/// The conversion a file was archived under; a commit is refused once it changed (Python `conversion_revision`,
/// plus HARS's per-run `conversion_id`).
pub fn revision(meta: &Value) -> Value {
    json!([
        meta.get("conversion_revision").cloned().unwrap_or(json!(0)),
        meta.get("conversion_id").cloned().unwrap_or(Value::Null)
    ])
}
