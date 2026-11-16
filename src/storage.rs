//! Local storage policy (Python `storage_policy.py`) and crash-safe recording deletion.
//!
//! Every deletion is decided first (`Protection`, `Plan`), then journaled, then executed:
//! the recording folder is renamed to a hidden name before anything inside it is removed, so a
//! crash can never leave a half-deleted recording in the list. That rename is the commit point:
//! recovery finishes hidden folders only and never deletes a recording that is still whole, since
//! its protection may have changed since the decision. Journal and hidden folders are only
//! touched while `Store::registration` is held.
use crate::{
    error::{ApiError, Result},
    store::{Store, atomic_json, directory_size, now, read_json, valid_id},
    upload::KINDS,
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::Path,
    sync::LazyLock,
};
use tokio::sync::MutexGuard;

pub const MARGIN: u64 = 100 * 1024 * 1024;
pub const SETTINGS: &str = ".storage-settings.json";
/// HARS only: Python removes a local recording with a single `rmtree` and keeps no local journal
/// (its `.pending-nas-deletes.json` lists archived files only).
pub const JOURNAL: &str = ".hars-delete-journal.json";
pub const HIDDEN_PREFIX: &str = ".deleting-";
const METADATA_RESERVE: u64 = 65536;
const MEDIA_KINDS: [&str; 7] = [
    "rlog.zst",
    "qcamera.ts",
    "fcamera.hevc",
    "ecamera.hevc",
    "camera.mp4",
    "fcamera.mp4",
    "ecamera.mp4",
];

static ROUTE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^(?:[0-9a-fA-F]{8}--[0-9a-fA-F]{10}|\d{4}-\d{2}-\d{2}--\d{2}-\d{2}-\d{2})$")
        .unwrap()
});
static FLAT: LazyLock<regex::Regex> = LazyLock::new(|| {
    let kinds = KINDS
        .iter()
        .map(|k| regex::escape(k))
        .collect::<Vec<_>>()
        .join("|");
    regex::Regex::new(&format!(
        r"^(?P<route>.{{20}})--(?P<segment>\d+)--(?:{kinds})$"
    ))
    .unwrap()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    LimitExceeded,
    InsufficientDisk,
    NoDeletable,
}
impl StorageError {
    pub fn code(self) -> &'static str {
        match self {
            Self::LimitExceeded => "storage_limit_exceeded",
            Self::InsufficientDisk => "insufficient_disk_space",
            Self::NoDeletable => "no_deletable_logs",
        }
    }
}
impl From<StorageError> for ApiError {
    fn from(e: StorageError) -> Self {
        ApiError(StatusCode::INSUFFICIENT_STORAGE, e.code())
    }
}
/// Python `StoragePolicy.error` messages, sent beside the 507 code.
pub fn message(code: &str) -> Option<&'static str> {
    match code {
        "storage_limit_exceeded" => Some("설정한 저장공간 제한을 초과합니다."),
        "insufficient_disk_space" => Some(
            "예약·임시 파일과 안전 여유 공간을 제외하면 디스크 공간이 부족합니다. 기존 기록은 보존합니다.",
        ),
        "no_deletable_logs" => Some(
            "고정되었거나 사용 중이거나 Drive 파일이 있는 기록을 제외하면 확보할 공간이 없습니다.",
        ),
        _ => None,
    }
}

// ---------------------------------------------------------------- settings

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_bytes: u64,
    pub delete_oldest: bool,
}

/// The settings file with Python's defaults; unknown keys are kept for the next save.
pub async fn load_settings(root: &Path) -> std::io::Result<Value> {
    let mut value = match read_json::<Value>(&root.join(SETTINGS)).await {
        Ok(v) => v,
        Err(e) if e.kind() == ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e),
    };
    let fields = value
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("invalid storage settings"))?;
    fields.entry("max_bytes").or_insert(json!(0));
    fields.entry("policy").or_insert(json!("reject_new"));
    fields.entry("pinned_logs").or_insert(json!([]));
    Ok(value)
}
pub async fn save_settings(root: &Path, settings: &Value) -> std::io::Result<()> {
    atomic_json(&root.join(SETTINGS), settings).await
}
pub fn limits(settings: &Value) -> Limits {
    Limits {
        max_bytes: settings["max_bytes"].as_u64().unwrap_or(0),
        delete_oldest: settings["policy"] == "delete_oldest",
    }
}
pub fn pins(settings: &Value) -> BTreeSet<String> {
    settings["pinned_logs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}
/// Python `api` POST validation: exactly these two keys, an integer limit and a known policy.
pub fn parse_settings_update(body: &[u8]) -> Option<(u64, &'static str)> {
    let body: Value = serde_json::from_slice(body).ok()?;
    let fields = body.as_object()?;
    if fields.len() != 2 || !fields.contains_key("max_bytes") || !fields.contains_key("policy") {
        return None;
    }
    let max = fields["max_bytes"].as_u64().filter(|v| *v < (1u64 << 53))?;
    let policy = match fields["policy"].as_str()? {
        "reject_new" => "reject_new",
        "delete_oldest" => "delete_oldest",
        _ => return None,
    };
    Some((max, policy))
}

/// Python `migrate_pins`: route pins become pins of the segments that exist now, so a segment
/// uploaded later is not protected by an old route pin.
pub async fn migrate_pins(store: &Store) -> std::io::Result<bool> {
    let mut settings = load_settings(&store.root).await?;
    let Some(routes) = settings.get("pinned_routes").cloned() else {
        return Ok(false);
    };
    let routes: BTreeSet<String> = routes
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let mut pinned = pins(&settings);
    for meta in store.records.read().await.values() {
        if routes.contains(&route_key(meta)) {
            pinned.insert(meta["id"].as_str().unwrap_or_default().to_owned());
        }
    }
    let fields = settings.as_object_mut().unwrap();
    fields.remove("pinned_routes");
    fields.insert("pinned_logs".into(), json!(pinned));
    save_settings(&store.root, &settings).await?;
    Ok(true)
}

// ---------------------------------------------------------------- pure policy

/// Why automatic cleanup keeps a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protection {
    Pinned,
    /// Registered by an upload in progress (a duplicate it matched, or a pending commit journal).
    Uploading,
    /// A segment of the same route is queued or converting.
    Converting,
    /// A segment of the same route is still being received.
    RouteReceiving,
    /// A remote (Drive) file or verified copy belongs to the recording.
    RemoteCopy,
    /// Archived files could not be removed with the recording while the archive is unreachable.
    ArchiveUnreachable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordFacts {
    pub id: String,
    pub route: String,
    pub uploaded: f64,
    pub status: String,
    pub pinned: bool,
    pub bytes: u64,
    pub remote_refs: bool,
    pub archived: bool,
}
impl RecordFacts {
    pub fn from_meta(meta: &Value, pinned: &BTreeSet<String>, bytes: u64) -> Self {
        let id = meta["id"].as_str().unwrap_or_default().to_owned();
        Self {
            route: route_key(meta),
            uploaded: meta["uploaded"].as_f64().unwrap_or(0.),
            status: meta["status"].as_str().unwrap_or_default().to_owned(),
            pinned: pinned.contains(&id),
            bytes,
            remote_refs: has_remote_refs(meta),
            archived: archived(meta),
            id,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Guards {
    pub receiving_routes: BTreeSet<String>,
    pub busy_ids: BTreeSet<String>,
    /// HARS has no archive yet; a recording with archived files is then never evicted.
    pub archive_reachable: bool,
}

/// Python `admit`'s eviction rule. A route is kept whole while any segment of it is received,
/// registered or converted; otherwise each segment is judged alone (pins are per segment).
pub fn eviction_protection(
    record: &RecordFacts,
    route: &[RecordFacts],
    guards: &Guards,
) -> Option<Protection> {
    if guards.receiving_routes.contains(&record.route) {
        return Some(Protection::RouteReceiving);
    }
    if route.iter().any(|m| guards.busy_ids.contains(&m.id)) {
        return Some(Protection::Uploading);
    }
    if route
        .iter()
        .any(|m| matches!(m.status.as_str(), "queued" | "processing"))
    {
        return Some(Protection::Converting);
    }
    if record.pinned {
        Some(Protection::Pinned)
    } else if record.remote_refs {
        Some(Protection::RemoteCopy)
    } else if record.archived && !guards.archive_reachable {
        Some(Protection::ArchiveUnreachable)
    } else {
        None
    }
}
/// A user's explicit deletion only waits for a registration that refers to the recording.
pub fn manual_protection(id: &str, guards: &Guards) -> Option<Protection> {
    guards
        .busy_ids
        .contains(id)
        .then_some(Protection::Uploading)
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedRoute {
    pub route: String,
    /// The oldest upload of the whole route, pinned segments included (Python sort key).
    pub oldest: f64,
    pub records: Vec<(String, u64)>,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Deletion order: oldest route first; each route is deleted whole before space is measured again.
    pub routes: Vec<PlannedRoute>,
    pub kept: BTreeMap<String, Protection>,
    pub reclaim: u64,
}
impl Plan {
    pub fn ids(&self) -> Vec<String> {
        self.routes
            .iter()
            .flat_map(|r| r.records.iter().map(|(id, _)| id.clone()))
            .collect()
    }
}
pub fn plan_eviction(records: &[RecordFacts], guards: &Guards) -> Plan {
    let mut groups: BTreeMap<&str, Vec<&RecordFacts>> = BTreeMap::new();
    for r in records {
        groups.entry(&r.route).or_default().push(r);
    }
    let mut plan = Plan::default();
    for (route, rows) in groups {
        let owned = rows.iter().map(|r| (*r).clone()).collect::<Vec<_>>();
        let mut deletable = Vec::new();
        for r in &rows {
            match eviction_protection(r, &owned, guards) {
                Some(p) => {
                    plan.kept.insert(r.id.clone(), p);
                }
                None => deletable.push((r.id.clone(), r.bytes)),
            }
        }
        if deletable.is_empty() {
            continue;
        }
        deletable.sort();
        plan.reclaim += deletable.iter().map(|(_, b)| b).sum::<u64>();
        plan.routes.push(PlannedRoute {
            route: route.to_owned(),
            oldest: rows
                .iter()
                .map(|r| r.uploaded)
                .fold(f64::INFINITY, f64::min),
            records: deletable,
        });
    }
    plan.routes.sort_by(|a, b| {
        a.oldest
            .total_cmp(&b.oldest)
            .then_with(|| a.route.cmp(&b.route))
    });
    plan
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub used: u64,
    pub reserved: u64,
    pub free: u64,
}
/// Multipart uploads are copied once more while registering; finished chunks are moved instead.
pub fn needed(total: u64, staging: bool) -> u64 {
    total
        .saturating_mul(if staging { 2 } else { 1 })
        .saturating_add(METADATA_RESERVE)
}
pub fn enough(limits: Limits, space: Space, needed: u64) -> bool {
    let (used, reserved, free, needed) = (
        space.used as u128,
        space.reserved as u128,
        space.free as u128,
        needed as u128,
    );
    (limits.max_bytes == 0 || used + reserved + needed <= limits.max_bytes as u128)
        && free >= reserved + needed + MARGIN as u128
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Fits,
    NeedsEviction,
}
/// Python `admit` up to its candidate scan.
pub fn check(
    limits: Limits,
    space: Space,
    needed: u64,
) -> std::result::Result<Verdict, StorageError> {
    let (used, reserved, free, need, limit) = (
        space.used as u128,
        space.reserved as u128,
        space.free as u128,
        needed as u128,
        limits.max_bytes as u128,
    );
    if free < reserved + need + MARGIN as u128 {
        return Err(StorageError::InsufficientDisk);
    }
    if enough(limits, space, needed) {
        return Ok(Verdict::Fits);
    }
    let code = if limit > 0 && used + reserved + need > limit {
        StorageError::LimitExceeded
    } else {
        StorageError::InsufficientDisk
    };
    if !limits.delete_oldest {
        return Err(code);
    }
    if limit > 0 && need > limit {
        return Err(StorageError::LimitExceeded);
    }
    Ok(Verdict::NeedsEviction)
}
/// Existing data is never deleted when even every candidate could not make room.
pub fn preflight(
    limits: Limits,
    space: Space,
    needed: u64,
    reclaim: u64,
) -> std::result::Result<(), StorageError> {
    let (used, reserved, free, need, limit, reclaim) = (
        space.used as i128,
        space.reserved as i128,
        space.free as i128,
        needed as i128,
        limits.max_bytes as i128,
        reclaim as i128,
    );
    if (limit > 0 && used - reclaim + reserved + need > limit)
        || free + reclaim < reserved + need + MARGIN as i128
    {
        return Err(StorageError::NoDeletable);
    }
    Ok(())
}

// ---------------------------------------------------------------- recording facts

/// Python `route_key`: the flat upload filename is authoritative, never shortened UI text.
pub fn route_key(meta: &Value) -> String {
    let name = meta["files"]["rlog.zst"].as_str().unwrap_or("");
    if let Some(c) = FLAT.captures(name) {
        return c["route"].to_owned();
    }
    let label = meta["name"].as_str().unwrap_or("");
    let label = label.split(" / 구간 ").next().unwrap_or("");
    if ROUTE.is_match(label) {
        label.to_owned()
    } else {
        format!("log:{}", meta["id"].as_str().unwrap_or(""))
    }
}
fn incoming_route(name: &str) -> Option<String> {
    let clean = name.replace('\\', "/");
    let base = clean.rsplit('/').next().unwrap_or("");
    FLAT.captures(base).map(|c| c["route"].to_owned())
}
fn entry(meta: &Value, kind: &str) -> Value {
    let e = &meta["storage"][kind];
    if e.is_object() && e["location"].as_str().is_some_and(|v| !v.is_empty()) {
        e.clone()
    } else {
        json!({"location":"local"})
    }
}
/// Python `media_store.remote_refs(meta, copies=True)` is non-empty.
pub fn has_remote_refs(meta: &Value) -> bool {
    MEDIA_KINDS.iter().any(|kind| {
        let e = entry(meta, kind);
        let block = match e["location"].as_str() {
            Some("gdrive") => e.clone(),
            Some("local") if e["remote"].is_object() => e["remote"].clone(),
            _ => return false,
        };
        let backend = if e["location"] == "gdrive" {
            "gdrive"
        } else {
            block["backend"].as_str().unwrap_or("")
        };
        backend == "gdrive" && block["file_id"].as_str().is_some_and(|v| !v.is_empty())
    })
}
pub fn archived(meta: &Value) -> bool {
    MEDIA_KINDS.iter().any(|kind| {
        matches!(
            entry(meta, kind)["location"].as_str(),
            Some("nas" | "gdrive")
        )
    })
}

fn tolerant_size(path: &Path) -> std::io::Result<u64> {
    match directory_size(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(0),
        other => other,
    }
}
/// Python `reserved`: the outstanding part of every upload reservation (received chunks are
/// already counted as used).
pub async fn reserved(root: &Path) -> std::io::Result<u64> {
    let uploads = root.join(".uploads");
    tokio::task::spawn_blocking(move || {
        let mut total = 0u64;
        let entries = match std::fs::read_dir(&uploads) {
            Ok(v) => v,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            let bytes = match std::fs::read(path.join("reservation.json")) {
                Ok(v) => serde_json::from_slice::<Value>(&v)
                    .ok()
                    .and_then(|v| v["bytes"].as_u64())
                    .ok_or_else(|| std::io::Error::other("invalid upload reservation"))?,
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                    continue;
                }
                Err(e) => return Err(e),
            };
            total += bytes.saturating_sub(tolerant_size(&path)?);
        }
        Ok(total)
    })
    .await?
}
pub async fn space(store: &Store, commit: bool) -> std::io::Result<Space> {
    Ok(Space {
        used: store.used().await?,
        // At commit, other upload intentions must not trigger extra eviction.
        reserved: if commit {
            0
        } else {
            reserved(&store.root).await?
        },
        free: fs2::available_space(&store.root)?,
    })
}

/// Routes still being received (every session manifest plus the incoming names) and the
/// recordings named by pending registration journals.
pub async fn guards(
    root: &Path,
    incoming: &[String],
    protected_ids: &BTreeSet<String>,
) -> std::io::Result<Guards> {
    let uploads = root.join(".uploads");
    let incoming = incoming.to_vec();
    let protected_ids = protected_ids.clone();
    tokio::task::spawn_blocking(move || {
        let mut g = Guards {
            busy_ids: protected_ids,
            ..Guards::default()
        };
        g.receiving_routes
            .extend(incoming.iter().filter_map(|n| incoming_route(n)));
        let entries = match std::fs::read_dir(&uploads) {
            Ok(v) => v,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(g),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            let read = |name: &str| match std::fs::read(path.join(name)) {
                Ok(v) => serde_json::from_slice::<Value>(&v)
                    .map(Some)
                    .map_err(std::io::Error::other),
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                    Ok(None)
                }
                Err(e) => Err(e),
            };
            if let Some(files) = read("manifest.json")? {
                for f in files.as_array().into_iter().flatten() {
                    if let Some(route) = f["name"].as_str().and_then(incoming_route) {
                        g.receiving_routes.insert(route);
                    }
                }
            }
            if let Some(plan) = read("commit.json")? {
                // A journaled camera addition protects the recording it adds to.
                for m in ["records", "updates"]
                    .iter()
                    .flat_map(|k| plan[k].as_array().into_iter().flatten())
                {
                    if let Some(id) = m["id"].as_str() {
                        g.busy_ids.insert(id.to_owned());
                    }
                }
            }
        }
        Ok(g)
    })
    .await?
}
pub async fn facts(store: &Store) -> std::io::Result<Vec<RecordFacts>> {
    let pinned = pins(&load_settings(&store.root).await?);
    let metas = store
        .records
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let root = store.root.clone();
    tokio::task::spawn_blocking(move || {
        metas
            .iter()
            .map(|m| {
                let bytes = tolerant_size(&root.join(m["id"].as_str().unwrap_or_default()))?;
                Ok(RecordFacts::from_meta(m, &pinned, bytes))
            })
            .collect()
    })
    .await?
}
/// Dry run: what automatic cleanup would delete now, and why the rest is kept.
pub async fn eviction_plan(
    store: &Store,
    incoming: &[String],
    protected_ids: &BTreeSet<String>,
) -> std::io::Result<Plan> {
    let guards = eviction_guards(store, incoming, protected_ids).await?;
    Ok(plan_eviction(&facts(store).await?, &guards))
}
async fn eviction_guards(
    store: &Store,
    incoming: &[String],
    protected_ids: &BTreeSet<String>,
) -> std::io::Result<Guards> {
    let mut guards = guards(&store.root, incoming, protected_ids).await?;
    // Only a recent "connected" result (never a probe under the registration lock) makes archived recordings evictable.
    guards.archive_reachable = store
        .archive()
        .is_some_and(|a| a.last_state() == Some(crate::archive::State::Connected));
    Ok(guards)
}

/// Python `admit`. `commit` carries the held registration lock: only then may recordings be
/// deleted, and only after every incoming file is received and verified.
pub async fn admit(
    store: &Store,
    total: u64,
    incoming: &[String],
    staging: bool,
    protected_ids: &BTreeSet<String>,
    commit: Option<&MutexGuard<'_, ()>>,
) -> Result<u64> {
    let settings = load_settings(&store.root).await?;
    let limits = limits(&settings);
    let needed = needed(total, staging);
    if let Some(archive) = store.archive()
        && let Some(threshold) = archive.drive_move()
    {
        return drive_move_admit(
            store,
            &archive,
            limits,
            threshold,
            needed,
            incoming,
            protected_ids,
        )
        .await;
    }
    let mut current = space(store, commit.is_some()).await?;
    if check(limits, current, needed)? == Verdict::Fits {
        return Ok(needed);
    }
    let guards = eviction_guards(store, incoming, protected_ids).await?;
    let plan = plan_eviction(&facts(store).await?, &guards);
    preflight(limits, current, needed, plan.reclaim)?;
    let Some(registration) = commit else {
        return Ok(needed);
    };
    let pinned = pins(&load_settings(&store.root).await?);
    for route in &plan.routes {
        let ids = route
            .records
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let members = store
            .records
            .read()
            .await
            .values()
            .filter(|m| route_key(m) == route.route)
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        // `convert` and the scheduler queue a segment under its record lock alone, so the plan's
        // rule is applied again to what the locks hold now.
        delete_records(
            store,
            registration,
            &ids,
            &members,
            "storage_limit",
            |m, locked| {
                let route = locked
                    .iter()
                    .map(|m| RecordFacts::from_meta(m, &pinned, 0))
                    .collect::<Vec<_>>();
                Judgement::of(eviction_protection(
                    &RecordFacts::from_meta(m, &pinned, 0),
                    &route,
                    &guards,
                ))
            },
        )
        .await?;
        current = space(store, true).await?;
        if enough(limits, current, needed) {
            return Ok(needed);
        }
    }
    Err(StorageError::NoDeletable.into())
}

/// Python `admit`'s `drive_move` branch: with Drive in move mode the quota and the local threshold are a MOVE
/// trigger, never a reason to discard data or refuse an otherwise safe upload. Only a physical shortage refuses.
async fn drive_move_admit(
    store: &Store,
    archive: &crate::archive::Archive,
    limits: Limits,
    threshold: u64,
    needed: u64,
    incoming: &[String],
    protected_ids: &BTreeSet<String>,
) -> Result<u64> {
    let used = store.used().await? as u128;
    let reserved = reserved(&store.root).await? as u128;
    let free = fs2::available_space(&store.root)? as u128;
    let need = needed as u128;
    let disk_full = free < reserved + need + MARGIN as u128;
    let limit = limits.max_bytes as u128;
    if disk_full
        || limit > 0 && used + reserved + need >= limit
        || threshold > 0 && used + reserved + need >= threshold as u128
    {
        archive.request_reclaim(if disk_full { needed } else { 0 }, incoming, protected_ids);
    }
    if disk_full {
        return Err(StorageError::InsufficientDisk.into());
    }
    Ok(needed)
}

/// Archived files of a recording about to be deleted (Python `delete_recording`): NAS files by kind, and for an
/// explicit deletion also the Drive files it refers to. Recorded before the local commit point, so a crash
/// leaves the recording whole (its row is skipped while its meta.json exists) or the row ready to finish.
async fn queue_archived(root: &Path, meta: &Value, explicit: bool) -> std::io::Result<()> {
    use crate::archive::index;
    let id = meta["id"].as_str().unwrap_or_default();
    let kinds: Vec<String> = MEDIA_KINDS
        .iter()
        .filter(|k| index::location(meta, k) == "nas")
        .map(|k| (*k).to_owned())
        .collect();
    let refs = if explicit {
        index::remote_refs(meta, true)
    } else {
        Vec::new()
    };
    if kinds.is_empty() && refs.is_empty() {
        return Ok(());
    }
    crate::archive::pending::PendingDeletes::new(root)
        .add(id, &kinds, &refs, explicit)
        .await
}

// ---------------------------------------------------------------- deletion journal

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalEntry {
    pub id: String,
    pub reason: String,
    pub at: f64,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct Journal {
    version: u32,
    deletes: Vec<JournalEntry>,
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
pub async fn journal_entries(root: &Path) -> std::io::Result<Vec<JournalEntry>> {
    let path = root.join(JOURNAL);
    let bytes = match tokio::fs::read(&path).await {
        Ok(v) => v,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice::<Journal>(&bytes) {
        Ok(j) if j.version == 1 && j.deletes.iter().all(|e| valid_id(&e.id)) => Ok(j.deletes),
        _ => {
            // Kept for inspection; hidden folders still finish without it.
            let stamp = now() as u64;
            for n in 0u32.. {
                let aside = root.join(match n {
                    0 => format!(".hars-delete-journal.corrupt-{stamp}.json"),
                    n => format!(".hars-delete-journal.corrupt-{stamp}-{n}.json"),
                });
                if !tokio::fs::try_exists(&aside).await? {
                    tokio::fs::rename(&path, aside).await?;
                    break;
                }
            }
            tracing::warn!("delete_journal_corrupt_kept_aside");
            Ok(vec![])
        }
    }
}
async fn journal_save(root: &Path, deletes: Vec<JournalEntry>) -> std::io::Result<()> {
    let path = root.join(JOURNAL);
    if deletes.is_empty() {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => sync_dir(root).await,
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    } else {
        atomic_json(
            &path,
            &Journal {
                version: 1,
                deletes,
            },
        )
        .await
    }
}
/// Step 1: the decision is durable before anything is touched.
pub async fn journal_add(root: &Path, ids: &[String], reason: &str) -> std::io::Result<()> {
    let mut deletes = journal_entries(root).await?;
    for id in ids {
        if !deletes.iter().any(|e| e.id == *id) {
            deletes.push(JournalEntry {
                id: id.clone(),
                reason: reason.to_owned(),
                at: now(),
            });
        }
    }
    journal_save(root, deletes).await
}
async fn journal_remove(root: &Path, ids: &[String]) -> std::io::Result<()> {
    let deletes = journal_entries(root).await?;
    let kept = deletes
        .iter()
        .filter(|e| !ids.contains(&e.id))
        .cloned()
        .collect::<Vec<_>>();
    if kept.len() == deletes.len() {
        return Ok(());
    }
    journal_save(root, kept).await
}
pub fn hidden_path(root: &Path, id: &str) -> std::path::PathBuf {
    root.join(format!("{HIDDEN_PREFIX}{id}"))
}
/// Step 2, the commit point: one rename takes the recording out of the list on disk, and the
/// memory list follows before the sync, which may fail after the rename already happened.
pub async fn hide(store: &Store, id: &str) -> std::io::Result<()> {
    {
        let mut records = store.records.write().await;
        match tokio::fs::rename(store.root.join(id), hidden_path(&store.root, id)).await {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        records.remove(id);
    }
    sync_dir(&store.root).await
}
/// Step 3: remove the hidden folder. Safe to repeat after a partial removal.
pub async fn purge(root: &Path, id: &str) -> std::io::Result<()> {
    match tokio::fs::remove_dir_all(hidden_path(root, id)).await {
        Ok(()) => sync_dir(root).await,
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
async fn gone(root: &Path, id: &str) -> std::io::Result<bool> {
    Ok(!tokio::fs::try_exists(root.join(id)).await?
        && !tokio::fs::try_exists(hidden_path(root, id)).await?)
}

/// What a deletion decides for one of its targets, judged again under the record locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judgement {
    Delete,
    /// This recording stays; the other targets are judged on their own.
    Skip,
    /// A protection of the whole group (a route being converted): nothing is deleted.
    KeepAll,
}
impl Judgement {
    /// The judgement `eviction_protection` gives: route-wide protections keep the whole route, per-recording ones
    /// (pins, remote copies, an unreachable archive) only that recording (Python `admit`).
    pub fn of(protection: Option<Protection>) -> Self {
        match protection {
            None => Self::Delete,
            Some(Protection::RouteReceiving | Protection::Uploading | Protection::Converting) => {
                Self::KeepAll
            }
            Some(_) => Self::Skip,
        }
    }
}

/// Delete `ids` while holding the record locks of them and of `group`. `judge` sees each target and
/// every locked recording again under those locks. Returns the deleted ids. A failure before a
/// recording is hidden leaves it whole and listed; a hidden one is finished now or by the periodic
/// `recover`.
pub async fn delete_records(
    store: &Store,
    registration: &MutexGuard<'_, ()>,
    ids: &[String],
    group: &[String],
    reason: &str,
    judge: impl Fn(&Value, &[Value]) -> Judgement,
) -> Result<Vec<String>> {
    let mut locked = ids.iter().chain(group).cloned().collect::<Vec<_>>();
    locked.sort();
    locked.dedup();
    let mut held = Vec::new();
    for id in &locked {
        held.push(store.record_lock(id).await.lock_owned().await);
    }
    let mut metas = Vec::new();
    for id in &locked {
        if let Ok(meta) = store.meta(id).await {
            metas.push(meta);
        }
    }
    let mut targets = Vec::new();
    for meta in metas.iter().filter(|m| ids.iter().any(|i| m["id"] == *i)) {
        let judgement = match judge(meta, &metas) {
            // Drive is extension storage: only an explicit complete deletion may remove its files.
            Judgement::Delete if reason != "manual" && has_remote_refs(meta) => Judgement::Skip,
            j => j,
        };
        match judgement {
            Judgement::KeepAll => return Ok(vec![]),
            Judgement::Skip => {}
            Judgement::Delete => targets.push(meta["id"].as_str().unwrap_or_default().to_owned()),
        }
    }
    if targets.is_empty() {
        return Ok(targets);
    }
    for id in &targets {
        if let Ok(meta) = store.meta(id).await {
            queue_archived(&store.root, &meta, reason == "manual").await?;
        }
    }
    journal_add(&store.root, &targets, reason).await?;
    for id in &targets {
        if let Err(e) = hide(store, id).await {
            drop(held);
            if recover(store, registration).await.is_err() {
                tracing::warn!("delete_recovery_incomplete");
            }
            return Err(e.into());
        }
    }
    for id in &targets {
        purge(&store.root, id).await?;
    }
    for id in &targets {
        if !gone(&store.root, id).await? {
            return Err(std::io::Error::other("recording folder still present").into());
        }
    }
    journal_remove(&store.root, &targets).await?;
    drop(held);
    Ok(targets)
}

/// Finish every hidden deletion (startup and periodic retry). A journal entry whose recording was
/// never hidden is discarded: that recording is still whole and listed, and its deletion is not
/// decided again here. Returns how many recordings were completed; a hidden folder that cannot
/// be removed yet stays journaled.
pub async fn recover(store: &Store, _registration: &MutexGuard<'_, ()>) -> std::io::Result<usize> {
    let journal = journal_entries(&store.root).await?;
    let mut hidden = BTreeSet::new();
    let mut dirs = tokio::fs::read_dir(&store.root).await?;
    while let Some(entry) = dirs.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_prefix(HIDDEN_PREFIX)
            && valid_id(id)
        {
            hidden.insert(id.to_owned());
        }
    }
    let mut done = Vec::new();
    let mut failure = None;
    for id in &hidden {
        let lock = store.record_lock(id).await;
        let _guard = lock.lock().await;
        match purge(&store.root, id).await {
            Ok(()) => done.push(id.clone()),
            Err(e) => failure = Some(e),
        }
    }
    let mut settled = done.clone();
    for entry in &journal {
        if !hidden.contains(&entry.id) {
            tracing::warn!(reason = %entry.reason, "delete_journal_entry_discarded");
            settled.push(entry.id.clone());
        }
    }
    journal_remove(&store.root, &settled).await?;
    match failure {
        Some(e) => Err(e),
        None => Ok(done.len()),
    }
}
pub async fn pending(root: &Path) -> std::io::Result<bool> {
    if tokio::fs::try_exists(root.join(JOURNAL)).await? {
        return Ok(true);
    }
    let mut dirs = tokio::fs::read_dir(root).await?;
    while let Some(entry) = dirs.next_entry().await? {
        if entry
            .file_name()
            .to_string_lossy()
            .strip_prefix(HIDDEN_PREFIX)
            .is_some_and(valid_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
/// Startup: finish interrupted deletions, then migrate legacy route pins.
pub async fn open(store: &Store) -> anyhow::Result<()> {
    let registration = store.registration.lock().await;
    if let Err(e) = recover(store, &registration).await {
        // The journal stays; the periodic retry finishes once the folder can be removed.
        tracing::warn!(kind = ?e.kind(), "delete_recovery_incomplete");
    }
    migrate_pins(store).await?;
    Ok(())
}
