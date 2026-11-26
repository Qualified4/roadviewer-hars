//! Python `gps_track.TrackBackfill`: routes for recordings converted before routes existed (imported from the Python
//! Road Viewer), newest first, one recording at a time in a low-priority child process, a pause between recordings.
//! Waits while uploads, conversions or an archive run are active. A recording whose rlog is only on Google Drive is
//! read only when asked (the setting `archive_gdrive_fill_tracks` or one pass by hand) and Drive is only read: one
//! verified download into a temporary folder under the data root, removed once the child is done.
use super::{
    ArchiveRead, ErrorCode, State, WriteBackend,
    archiver::{Archive, lock},
    index,
    resolve::Resolved,
};
use crate::store::now;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const TRACK_V: u64 = 2;
pub const FILE: &str = "track.json.gz";
pub(crate) const STAGED: &str = "track.json.gz.migrating";
const SUMMARY_FIELDS: [&str; 5] = [
    "track_v",
    "has_track",
    "track_distance_m",
    "track_bbox",
    "track_read",
];
const FAILURES: &str = "track_failures";
const MAX_FAILURES: u64 = 3;
/// Drive probe reasons that need the user: the Drive part of the pass stops instead of backing off.
const BLOCKING: [&str; 5] = [
    "not_connected",
    "auth_expired",
    "folder_missing",
    "folder_trashed",
    "storage_id_mismatch",
];
pub const TEMP_PREFIX: &str = ".track-";
const BACKOFF_START: u64 = 300;
const BACKOFF_MAX: u64 = 3600;

/// Reads one rlog into `prepared/track.json.gz.migrating`; the summary fields, or None when the read failed.
pub type Reader = Arc<
    dyn Fn(PathBuf, PathBuf) -> futures_util::future::BoxFuture<'static, Option<Value>>
        + Send
        + Sync,
>;

/// Python's START_DELAY, INTERVAL, BUSY_RETRY, PAUSE (tests shorten them).
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub start_delay: Duration,
    pub interval: Duration,
    pub busy_retry: Duration,
    pub pause: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            start_delay: Duration::from_secs(180),
            interval: Duration::from_secs(3600),
            busy_retry: Duration::from_secs(60),
            pause: Duration::from_secs(2),
        }
    }
}

#[derive(Default)]
struct St {
    /// A pass started by hand, until it ends.
    manual: bool,
    /// Stop asked during a pass: no more Drive recordings in it.
    halt: bool,
    /// A Drive pass is under way (waiting included).
    active: bool,
    waiting: Option<&'static str>,
    /// [total, done, failed] of the Drive-only recordings, from the latest scan.
    counts: Option<[u64; 3]>,
    last_error: Option<String>,
    retry_at: Option<Instant>,
    backoff: u64,
    finished_at: Option<f64>,
}

pub struct TrackFill {
    st: Mutex<St>,
    wake: tokio::sync::Notify,
    reader: Mutex<Reader>,
    timing: Mutex<Timing>,
}

impl Default for TrackFill {
    fn default() -> Self {
        Self {
            st: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            reader: Mutex::new(Arc::new(|rlog, prepared| {
                Box::pin(read_in_child(rlog, prepared))
            })),
            timing: Mutex::default(),
        }
    }
}

impl TrackFill {
    pub fn set_reader(&self, reader: Reader) {
        *lock(&self.reader) = reader;
    }
    pub fn set_timing(&self, timing: Timing) {
        *lock(&self.timing) = timing;
    }
    pub fn wake(&self) {
        self.wake.notify_one();
    }
    /// Python `request_run`: one pass over the Drive recordings now, setting on or off; false while one started by
    /// hand is under way.
    pub fn request_run(&self) -> bool {
        let mut st = lock(&self.st);
        if st.manual {
            return false;
        }
        st.manual = true;
        st.halt = false;
        st.retry_at = None;
        drop(st);
        self.wake();
        true
    }
    /// Python `request_stop`: stops after the current recording (the setting, if on, starts again on its next run).
    pub fn request_stop(&self) -> bool {
        let mut st = lock(&self.st);
        let was = st.manual || st.active;
        st.manual = false;
        st.halt = st.active;
        was
    }
    fn end_pass(&self) {
        let mut st = lock(&self.st);
        if st.manual || st.active {
            st.finished_at = Some(now());
        }
        st.manual = false;
        st.halt = false;
        st.active = false;
        st.waiting = None;
    }
    fn retry_left(&self) -> Option<Duration> {
        lock(&self.st)
            .retry_at
            .map(|at| at.saturating_duration_since(Instant::now()))
            .filter(|d| !d.is_zero())
    }
    async fn sleep(&self, d: Duration) {
        tokio::select! {
            _ = self.wake.notified() => {}
            _ = tokio::time::sleep(d) => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Needs a route; the rlog is here or on the NAS.
    Local,
    /// The rlog is only on Drive and was not read yet.
    Drive,
    Done,
    Failed,
}

enum Halt {
    /// Drive cannot be read until someone acts: nothing is marked; the next run tries again.
    Blocked(String),
    /// A network, rate-limit or server error: tried again after a back-off; nothing is marked.
    Later(&'static str),
}

/// Python `summary_fields`; `read` is false for a recording whose rlog could not be read here.
pub fn empty_fields(read: bool) -> Value {
    json!({"track_v": TRACK_V, "has_track": false, "track_distance_m": null, "track_bbox": null, "track_read": read})
}

fn read_summary(folder: &Path) -> Option<Value> {
    let bytes = std::fs::read(folder.join("prepared/summary.json")).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
}

fn converted(folder: &Path, m: &Value) -> bool {
    m["status"] == "ready"
        && m.get("prepared_nas").is_none()
        && folder.join("prepared/data.json.gz").is_file()
}

/// Python `drive_only`: moved to Drive, or a Drive copy whose local file is gone.
pub fn drive_only(folder: &Path, m: &Value) -> bool {
    let where_ = index::location(m, "rlog.zst");
    where_ == "gdrive"
        || where_ == "local"
            && index::remote(m, "rlog.zst").is_some()
            && !folder.join("rlog.zst").is_file()
}

/// Python `track_read`: the rlog was read for its route (with or without a fix).
fn was_read(summary: &Value) -> bool {
    summary["track_v"] == TRACK_V && (summary["has_track"] == true || summary["track_read"] == true)
}

/// Python `TrackBackfill.needs`.
fn needs(folder: &Path, m: &Value) -> bool {
    converted(folder, m) && read_summary(folder).is_some_and(|s| s["track_v"] != TRACK_V)
}

/// Python `TrackBackfill.kind`.
fn kind_of(folder: &Path, m: &Value) -> Option<Kind> {
    if !converted(folder, m) {
        return None;
    }
    let summary = read_summary(folder)?;
    if !drive_only(folder, m) {
        return (summary["track_v"] != TRACK_V).then_some(Kind::Local);
    }
    if was_read(&summary) {
        return Some(Kind::Done);
    }
    Some(if summary[FAILURES].as_u64().unwrap_or(0) >= MAX_FAILURES {
        Kind::Failed
    } else {
        Kind::Drive
    })
}

impl Archive {
    pub fn track_fill(&self) -> &TrackFill {
        &self.track_fill
    }

    /// Python `TrackBackfill.status()`.
    pub async fn track_fill_status(&self) -> Value {
        if lock(&self.track_fill.st).counts.is_none() {
            self.track_scan().await;
        }
        let enabled = self.settings().gdrive_fill_tracks;
        let retry = self.track_fill.retry_left();
        let st = lock(&self.track_fill.st);
        let c = st.counts;
        json!({
            "enabled": enabled, "manual": st.manual, "running": st.active, "stopping": st.halt && st.active,
            "waiting": st.waiting, "total": c.map(|c| c[0]), "done": c.map(|c| c[1]), "failed": c.map(|c| c[2]),
            "pending": c.map(|c| c[0].saturating_sub(c[1] + c[2])), "last_error": st.last_error,
            "retry_in": retry.map(|d| d.as_secs_f64().round() as u64), "finished_at": st.finished_at,
        })
    }

    /// Python `TrackBackfill.start`: temporary folders left by a restart mid-read go, then the loop.
    pub(crate) fn start_track_fill(&self) {
        let root = self.root.clone();
        let me = self.arc();
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || remove_temps(&root)).await;
            me.track_loop().await;
        });
    }

    async fn track_loop(self: Arc<Self>) {
        self.track_scan().await;
        let timing = *lock(&self.track_fill.timing);
        self.track_fill.sleep(timing.start_delay).await;
        while !self.stopping() {
            let more = self.track_run_once().await;
            let timing = *lock(&self.track_fill.timing);
            let mut wait = if more {
                timing.busy_retry
            } else {
                timing.interval
            };
            if more
                && lock(&self.track_fill.st).waiting.is_none()
                && let Some(left) = self.track_fill.retry_left()
            {
                wait = wait.max(left);
            }
            self.track_fill.sleep(wait).await;
        }
    }

    /// Python `busy` without playback: uploads, conversions or an archive run.
    async fn track_busy(&self) -> bool {
        self.running()
            || self.manual_active()
            || self.host.upload_receiving().await
            || self.host.jobs_running().await
    }

    fn drive_on(&self) -> bool {
        let st = lock(&self.track_fill.st);
        (st.manual || self.settings().gdrive_fill_tracks) && !st.halt && self.settings().remote()
    }

    /// Python `scan`: (id, kind) of the recordings to read, newest first; counts the Drive-only ones.
    async fn track_scan(&self) -> Vec<(String, Kind)> {
        let metas: Vec<Value> = self
            .host
            .store
            .records
            .read()
            .await
            .values()
            .cloned()
            .collect();
        let root = self.root.clone();
        let (rows, counts) = tokio::task::spawn_blocking(move || {
            let mut dated = Vec::new();
            for m in metas {
                let Some(id) = m["id"].as_str().map(str::to_owned) else {
                    continue;
                };
                let folder = root.join(&id);
                if let Ok(t) = std::fs::metadata(&folder).and_then(|s| s.modified()) {
                    dated.push((t, id, folder, m));
                }
            }
            dated.sort_by_key(|d| std::cmp::Reverse(d.0));
            let (mut rows, mut counts) = (Vec::new(), [0u64; 3]);
            for (_, id, folder, m) in dated {
                match kind_of(&folder, &m) {
                    Some(Kind::Done) => {
                        counts[0] += 1;
                        counts[1] += 1;
                    }
                    Some(Kind::Failed) => {
                        counts[0] += 1;
                        counts[2] += 1;
                    }
                    Some(k) => {
                        counts[0] += u64::from(k == Kind::Drive);
                        rows.push((id, k));
                    }
                    None => {}
                }
            }
            (rows, counts)
        })
        .await
        .unwrap_or_default();
        lock(&self.track_fill.st).counts = Some(counts);
        rows
    }

    /// Python `run_once`: whether work remains for soon (busy, a NAS that could not be read, Drive backing off).
    pub async fn track_run_once(&self) -> bool {
        let more = self.track_pass().await;
        if !more {
            self.track_fill.end_pass();
        }
        more
    }

    async fn track_pass(&self) -> bool {
        let (mut retry, mut blocked) = (false, false);
        let pause = lock(&self.track_fill.timing).pause;
        for (id, kind) in self.track_scan().await {
            if self.stopping() {
                return false;
            }
            let Some(m) = self.host.meta(&id).await else {
                continue;
            };
            let folder = self.host.folder(&id);
            let kind = if kind == Kind::Drive {
                kind_of(&folder, &m)
            } else {
                Some(kind)
            };
            match kind {
                Some(Kind::Local) => {
                    if !needs(&folder, &m) {
                        continue;
                    }
                    if self.track_busy().await {
                        lock(&self.track_fill.st).waiting = Some("busy");
                        return true;
                    }
                    lock(&self.track_fill.st).waiting = None;
                    match self.track_source(&id, &folder, &m).await {
                        Err(()) => {
                            retry = true;
                            continue;
                        }
                        Ok(rlog) => self.track_process(&id, &m, rlog).await,
                    }
                }
                Some(Kind::Drive) => {
                    if blocked || !self.drive_on() {
                        // Not asked to read Drive: a recording never given a route gets has_track false (and
                        // track_read false), and is read once either is on.
                        if needs(&folder, &m) {
                            self.track_process(&id, &m, None).await;
                        }
                        continue;
                    }
                    if self.track_fill.retry_left().is_some() {
                        retry = true;
                        continue;
                    }
                    lock(&self.track_fill.st).active = true;
                    if self.track_busy().await {
                        lock(&self.track_fill.st).waiting = Some("busy");
                        return true;
                    }
                    lock(&self.track_fill.st).waiting = None;
                    match self.track_process_drive(&id, &m).await {
                        Ok(()) => {
                            let mut st = lock(&self.track_fill.st);
                            st.backoff = 0;
                            st.last_error = None;
                        }
                        Err(Halt::Blocked(e)) => {
                            tracing::warn!(reason = %e, "drive_maps_paused");
                            let mut st = lock(&self.track_fill.st);
                            st.last_error = Some(e);
                            // A pass started by hand ends here; the setting tries again next run.
                            st.manual = false;
                            st.active = false;
                            blocked = true;
                            continue;
                        }
                        Err(Halt::Later(e)) => {
                            let mut st = lock(&self.track_fill.st);
                            st.backoff = BACKOFF_MAX.min(BACKOFF_START.max(st.backoff * 2));
                            st.retry_at = Some(Instant::now() + Duration::from_secs(st.backoff));
                            tracing::warn!(
                                reason = e,
                                seconds = st.backoff,
                                "drive_maps_backing_off"
                            );
                            st.last_error = Some(e.to_owned());
                            retry = true;
                            continue;
                        }
                    }
                }
                _ => continue,
            }
            if !pause.is_zero() {
                tokio::time::sleep(pause).await;
            }
        }
        retry
    }

    /// Python `source`: a local path of the rlog, None when it cannot be read here, Err when the NAS is down.
    async fn track_source(
        &self,
        id: &str,
        folder: &Path,
        m: &Value,
    ) -> Result<Option<PathBuf>, ()> {
        match index::location(m, "rlog.zst").as_str() {
            "local" => Ok(Some(folder.join("rlog.zst")).filter(|p| p.is_file())),
            "nas" => match self.resolve(id, m, "rlog.zst").await {
                (_, "storage_unavailable" | "checking") => Err(()),
                (Resolved::Local(p), _) => Ok(Some(p)),
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    async fn track_read(&self, id: &str, rlog: PathBuf) -> Option<Value> {
        let record_lock = self.host.store.record_lock(id).await;
        let guard = record_lock.lock().await;
        let meta = self.host.meta(id).await?;
        let folder = self.host.folder(id);
        if !converted(&folder, &meta) {
            return None;
        }
        let _location = self
            .host
            .store
            .prepared
            .hold_location(id, Duration::ZERO)
            .await;
        drop(guard);
        let reader = lock(&self.track_fill.reader).clone();
        reader(rlog, folder.join("prepared")).await
    }

    async fn track_process(&self, id: &str, m: &Value, rlog: Option<PathBuf>) {
        let fields = match rlog {
            Some(p) => self.track_read(id, p).await,
            None => None,
        };
        self.track_commit(id, m, fields.unwrap_or_else(|| empty_fields(false)), None)
            .await;
    }

    /// Python `process_drive`: probe, the file's metadata and one verified download; never a write to Drive.
    async fn track_process_drive(&self, id: &str, m: &Value) -> Result<(), Halt> {
        let backend = self.backend();
        if !matches!(&*backend, WriteBackend::Drive(_)) {
            return Err(Halt::Blocked("gdrive_not_selected".into()));
        }
        let probe = backend.probe(true).await;
        match probe.state {
            State::Connected => {}
            State::Disabled => return Err(Halt::Blocked("archive_disabled".into())),
            State::Checking => return Err(Halt::Later("checking")),
            State::Unavailable => {
                let reason = probe.reason.unwrap_or_default();
                return Err(if BLOCKING.contains(&reason.as_str()) {
                    Halt::Blocked(reason)
                } else if reason == "rate_limited" {
                    Halt::Later("rate_limited")
                } else {
                    Halt::Later("network")
                });
            }
        }
        let (found, state) = self.resolve(id, m, "rlog.zst").await;
        match state {
            // Gone from Drive: there is no route to read.
            "missing" => {
                self.track_done(id, m, empty_fields(true)).await;
                return Ok(());
            }
            "storage_unavailable" | "checking" => return Err(Halt::Later("network")),
            "available" => {}
            other => {
                self.track_fail(id, m, other).await;
                return Ok(());
            }
        }
        let fields = match found {
            // A local file came back meanwhile.
            Resolved::Local(p) => self.track_read(id, p).await,
            Resolved::Remote(file) => {
                let temp = self
                    .root
                    .join(format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4().simple()));
                if tokio::fs::create_dir(&temp).await.is_err() {
                    return Err(Halt::Later("network"));
                }
                let dest = temp.join("rlog.zst");
                let downloaded = backend
                    .download_to(&file.reference, &dest, file.reference.sha256.as_deref())
                    .await;
                let fields = match downloaded {
                    Ok(()) => Ok(self.track_read(id, dest).await),
                    Err(e) => Err(e),
                };
                let _ = tokio::fs::remove_dir_all(&temp).await;
                match fields {
                    Ok(f) => f,
                    Err(e) => {
                        return match e.code {
                            ErrorCode::NotFound => {
                                self.track_done(id, m, empty_fields(true)).await;
                                Ok(())
                            }
                            ErrorCode::AuthExpired => Err(Halt::Blocked("auth_expired".into())),
                            ErrorCode::Mismatch | ErrorCode::Io => {
                                self.track_fail(id, m, e.code.as_str()).await;
                                Ok(())
                            }
                            ErrorCode::RateLimited | ErrorCode::QuotaExceeded => {
                                Err(Halt::Later("rate_limited"))
                            }
                            ErrorCode::Unavailable => Err(Halt::Later("network")),
                        };
                    }
                }
            }
            Resolved::Nothing => {
                self.track_fail(id, m, state).await;
                return Ok(());
            }
        };
        match fields {
            Some(f) => self.track_done(id, m, f).await,
            None => self.track_fail(id, m, "read_failed").await,
        }
        Ok(())
    }

    async fn track_done(&self, id: &str, m: &Value, fields: Value) {
        if self
            .track_commit(id, m, fields, Some(json!({FAILURES: null})))
            .await
            && let Some(c) = lock(&self.track_fill.st).counts.as_mut()
        {
            c[1] += 1;
        }
    }

    /// Counted in the summary; after MAX_FAILURES the recording is skipped (and marked unread).
    async fn track_fail(&self, id: &str, m: &Value, why: &str) {
        tracing::warn!(reason = why, "drive_track_failed");
        let prepared = self.host.folder(id).join("prepared");
        let mut count = 0;
        let committed = self
            .commit_prepared(id, m, |meta| {
                let file = prepared.join("summary.json");
                let mut summary: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
                let n = summary[FAILURES].as_u64().unwrap_or(0) + 1;
                count = n;
                summary[FAILURES] = json!(n);
                if n >= MAX_FAILURES && summary["track_v"] != TRACK_V {
                    merge(&mut summary, &empty_fields(false));
                    meta["has_track"] = json!(false);
                }
                super::fsutil::atomic_json(&file, &summary)?;
                Ok(())
            })
            .await;
        if committed
            && count >= MAX_FAILURES
            && let Some(c) = lock(&self.track_fill.st).counts.as_mut()
        {
            c[2] += 1;
        }
    }

    /// Python `commit`: the staged route replaces the old one (or both go without a fix) and the summary takes the
    /// fields; the list's `has_track` lives in the index here, so it is written with them.
    async fn track_commit(&self, id: &str, m: &Value, fields: Value, extra: Option<Value>) -> bool {
        let prepared = self.host.folder(id).join("prepared");
        let staged = prepared.join(STAGED);
        let ok = self
            .commit_prepared(id, m, |meta| {
                if staged.is_file() && fields["has_track"] == true {
                    std::fs::rename(&staged, prepared.join(FILE))?;
                } else {
                    remove_missing_ok(&staged)?;
                    remove_missing_ok(&prepared.join(FILE))?;
                }
                let file = prepared.join("summary.json");
                let mut summary: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
                for k in SUMMARY_FIELDS {
                    summary[k] = fields.get(k).cloned().unwrap_or(Value::Null);
                }
                if let Some(extra) = &extra {
                    merge(&mut summary, extra);
                }
                if summary.get(FAILURES).is_some_and(Value::is_null)
                    && let Some(o) = summary.as_object_mut()
                {
                    o.remove(FAILURES);
                }
                super::fsutil::atomic_json(&file, &summary)?;
                meta["has_track"] = summary["has_track"].clone();
                Ok(())
            })
            .await;
        if ok {
            crate::track::invalidate_recording(&self.host.folder(id));
        } else {
            let _ = std::fs::remove_file(&staged);
        }
        ok
    }

    /// Python `commit_prepared`: `write` runs under the record lock only while prepared/ still holds the conversion
    /// `m` was read with; the index it changed is saved after it. Returns whether it ran.
    async fn commit_prepared(
        &self,
        id: &str,
        m: &Value,
        write: impl FnOnce(&mut Value) -> std::io::Result<()>,
    ) -> bool {
        let l = self.host.store.record_lock(id).await;
        let _g = l.lock().await;
        let Some(mut current) = self.host.meta(id).await else {
            return false;
        };
        let revision = |v: &Value| v["conversion_revision"].as_u64().unwrap_or(0);
        if current["status"] != "ready"
            || current.get("prepared_nas").is_some()
            || revision(&current) != revision(m)
            || self.host.busy(id).await
            || !self.host.folder(id).join("prepared").is_dir()
        {
            return false;
        }
        let before = current.clone();
        if write(&mut current).is_err() {
            return false;
        }
        !(current != before && self.host.save(current).await.is_err())
    }
}

fn merge(a: &mut Value, b: &Value) {
    if let (Some(a), Some(b)) = (a.as_object_mut(), b.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
}

fn remove_missing_ok(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn remove_temps(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Python's child `gps_track.py RLOG PREPARED`: one recording per child, so the server never holds a decoded log.
async fn read_in_child(rlog: PathBuf, prepared: PathBuf) -> Option<Value> {
    let worker = std::env::current_exe().ok()?;
    #[cfg(unix)]
    let mut command = {
        let mut c = tokio::process::Command::new("nice");
        c.args(["-n", "10"]).arg(worker);
        c
    };
    #[cfg(not(unix))]
    let mut command = tokio::process::Command::new(worker);
    command
        .arg("track-log")
        .arg(&rlog)
        .arg(&prepared)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(600), command.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        tracing::warn!("track_read_failed");
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.lines().last()?).ok()
}

/// The child's work (Python `extract`): the whole rlog is read; the route goes to `prepared/track.json.gz.migrating`
/// when it has fixes. Its timeline origin is where the replay's first frame sits: that frame's camera time minus its t.
pub fn extract(rlog: &Path, prepared: &Path) -> anyhow::Result<Value> {
    let data = crate::logs::read_gzip(&prepared.join("data.json.gz"))?;
    let anchor = data["frames"].as_array().and_then(|frames| {
        frames
            .iter()
            .find_map(|f| Some((f["id"].as_u64()?, f["t"].as_f64()?)))
    });
    let decoded = crate::logs::decode(rlog)?;
    let origin =
        anchor.and_then(|(id, t)| crate::replay::frame_clock(&decoded, id).map(|clock| clock - t));
    let staged = prepared.join(STAGED);
    let track = origin.map(|o| crate::track::collect(&decoded, o));
    let points = track
        .as_ref()
        .and_then(|t| t["points"].as_array())
        .is_some_and(|p| !p.is_empty());
    if points {
        let track = track.expect("points");
        crate::logs::write_gzip(&staged, &track)?;
        Ok(crate::track::summary(&track))
    } else {
        remove_missing_ok(&staged)?;
        Ok(empty_fields(true))
    }
}
