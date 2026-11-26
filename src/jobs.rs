//! Durable conversion scheduling. Native/CPU work runs in a disposable child.
use crate::{
    archive::index,
    store::{Store, atomic_json, id, now, read_json},
    upload::Uploads,
};
use anyhow::Context;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{Mutex, Notify, RwLock},
};

pub struct Jobs {
    store: Arc<Store>,
    uploads: Arc<Uploads>,
    active: Mutex<BTreeMap<String, Arc<Notify>>>,
    pending_nas: Mutex<BTreeSet<String>>,
    // Held while one admission measures, decides and reserves.
    memory: Mutex<Arc<MemoryGate>>,
    pub progress: RwLock<BTreeMap<String, Value>>,
}

const MIB: u64 = 1024 * 1024;
const MEMORY_HEADROOM: u64 = 256 * MIB;
// Preparation resolves inputs and stages one file at a time per active job.
// Allow a chunk, its file-read buffer, and Tokio's 2 MiB file-write buffer.
const PREPARATION_BUFFERS: u64 = 2 * crate::archive::CHUNK as u64 + 2 * MIB;
// A conversion child holds its decoded log until it exits, so it is admitted once for its
// whole run. Earlier process-tree peaks were about 350 MiB per job (docs/hars/PERFORMANCE.md);
// HAOS reconversions on 2026-11-20 raised app memory by about 300 MiB per job.
const FIRST_RESERVATION: u64 = 320 * MIB;
const MIN_RESERVATION: u64 = 256 * MIB;
const MAX_RESERVATION: u64 = 1024 * MIB;
const PEAK_HISTORY: usize = 8;
const RSS_SAMPLE: Duration = Duration::from_millis(500);
const REFUSAL_LOG: Duration = Duration::from_secs(10);

/// The next conversion's reservation: the largest of the recent successful peaks.
fn next_reservation(peaks: &VecDeque<u64>) -> u64 {
    peaks.iter().max().map_or(FIRST_RESERVATION, |peak| {
        (*peak).clamp(MIN_RESERVATION, MAX_RESERVATION)
    })
}

/// Whether one more conversion fits. A conversion sits well below its peak for most of its run
/// and peaks for a second or two, so the headroom is kept on top of half of what the running
/// ones may still allocate. If every one of them peaked at once they would still fit in what is
/// available, using the headroom.
fn fits(available: u64, preparation: u64, outstanding: u64, reserved: u64) -> bool {
    available >= MEMORY_HEADROOM + preparation + outstanding / 2 + reserved
        && available >= preparation + outstanding + reserved
}

fn record_peak(peaks: &mut VecDeque<u64>, peak: u64) {
    peaks.push_back(peak);
    while peaks.len() > PEAK_HISTORY {
        peaks.pop_front();
    }
}

/// Memory of one admitted conversion: what it was promised and what its process group holds.
struct Usage {
    reserved: u64,
    rss: AtomicU64,
    peak: AtomicU64,
}

impl Usage {
    fn sample(&self, rss: u64) {
        self.rss.store(rss, Ordering::Relaxed);
        self.peak.fetch_max(rss, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct GateState {
    preparation: u64,
    running: BTreeMap<u64, Arc<Usage>>,
    groups: BTreeMap<u64, u32>,
    waiting: VecDeque<u64>,
    next: u64,
    peaks: VecDeque<u64>,
}

impl GateState {
    /// The part of the running reservations not yet resident, and so not yet out of `available`.
    fn outstanding(&self) -> u64 {
        self.running
            .values()
            .map(|u| u.reserved.saturating_sub(u.rss.load(Ordering::Relaxed)))
            .sum()
    }
}

#[derive(Default)]
struct MemoryGate {
    state: std::sync::Mutex<GateState>,
    changed: Notify,
}

impl MemoryGate {
    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Admits the first conversion in line when it fits beside what the running ones may still
    /// allocate, or when none runs.
    fn decide(
        self: &Arc<Self>,
        ticket: u64,
        record: &str,
        available: Option<u64>,
        logged: &mut Option<tokio::time::Instant>,
    ) -> Option<ConversionPermit> {
        let mut state = self.state();
        let reserved = next_reservation(&state.peaks);
        let outstanding = state.outstanding();
        let running = state.running.len();
        let fits = available
            .is_some_and(|available| fits(available, state.preparation, outstanding, reserved));
        let mib = |bytes: u64| bytes / MIB;
        if !fits && running > 0 {
            if logged.is_none_or(|at| at.elapsed() >= REFUSAL_LOG) {
                *logged = Some(tokio::time::Instant::now());
                tracing::info!(
                    record,
                    available_mib = available.map(mib),
                    outstanding_mib = mib(outstanding),
                    reserved_mib = mib(reserved),
                    running,
                    waiting = state.waiting.len(),
                    "conversion_memory_waiting"
                );
            }
            return None;
        }
        state.waiting.retain(|t| *t != ticket);
        let id = state.next;
        state.next += 1;
        let usage = Arc::new(Usage {
            reserved,
            rss: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        });
        state.running.insert(id, Arc::clone(&usage));
        if !fits {
            tracing::warn!(
                record,
                available_mib = available.map(mib),
                reserved_mib = mib(reserved),
                "conversion_memory_forced"
            );
        }
        tracing::info!(
            record,
            available_mib = available.map(mib),
            outstanding_mib = mib(outstanding),
            reserved_mib = mib(reserved),
            running,
            waiting = state.waiting.len(),
            forced = !fits,
            "conversion_memory_admitted"
        );
        Some(ConversionPermit {
            gate: Arc::clone(self),
            id,
            usage,
            record: record.into(),
        })
    }

    /// Updates every running conversion's resident memory from its process group.
    async fn refresh(&self) {
        let groups = self.state().groups.clone();
        if groups.is_empty() {
            return;
        }
        let rss = group_rss().await;
        let state = self.state();
        for (permit, group) in groups {
            if let Some(usage) = state.running.get(&permit) {
                usage.sample(rss.get(&group).copied().unwrap_or(0));
            }
        }
    }
}

struct PreparationReservation {
    gate: Arc<MemoryGate>,
    bytes: u64,
}

impl Drop for PreparationReservation {
    fn drop(&mut self) {
        self.gate.state().preparation -= self.bytes;
        self.gate.changed.notify_waiters();
    }
}

/// A place in the admission queue, left when the waiting conversion is admitted or stops.
struct Waiting {
    gate: Arc<MemoryGate>,
    ticket: u64,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.gate.state().waiting.retain(|t| *t != self.ticket);
        self.gate.changed.notify_waiters();
    }
}

/// Admission of one conversion child, held until the child and its process group are reaped.
struct ConversionPermit {
    gate: Arc<MemoryGate>,
    id: u64,
    usage: Arc<Usage>,
    record: String,
}

impl ConversionPermit {
    /// The process group whose resident memory counts for this conversion.
    fn track(&self, group: u32) {
        self.gate.state().groups.insert(self.id, group);
    }

    /// Samples the process group until dropped.
    async fn sample(&self, group: u32) -> std::convert::Infallible {
        loop {
            let rss = group_rss().await;
            self.usage.sample(rss.get(&group).copied().unwrap_or(0));
            tokio::time::sleep(RSS_SAMPLE).await;
        }
    }

    /// A successful conversion's peak sizes the next reservations.
    fn succeeded(&self) {
        let peak = self.usage.peak.load(Ordering::Relaxed);
        if peak > 0 {
            record_peak(&mut self.gate.state().peaks, peak);
        }
    }
}

impl Drop for ConversionPermit {
    fn drop(&mut self) {
        {
            let mut state = self.gate.state();
            state.running.remove(&self.id);
            state.groups.remove(&self.id);
        }
        self.gate.changed.notify_waiters();
        tracing::info!(
            record = %self.record,
            peak_rss_mib = self.usage.peak.load(Ordering::Relaxed) / MIB,
            reserved_mib = self.usage.reserved / MIB,
            "conversion_memory_released"
        );
    }
}

/// Resident memory by process group, from `/proc`. Groups it cannot read are absent.
async fn group_rss() -> BTreeMap<u32, u64> {
    #[cfg(target_os = "linux")]
    {
        tokio::task::spawn_blocking(|| {
            // SAFETY: sysconf(3) reads a constant of the running system.
            let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
            let mut groups = BTreeMap::new();
            let Ok(entries) = std::fs::read_dir("/proc") else {
                return groups;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                {
                    continue;
                }
                let (Ok(stat), Ok(statm)) = (
                    std::fs::read_to_string(path.join("stat")),
                    std::fs::read_to_string(path.join("statm")),
                ) else {
                    continue;
                };
                // The command name may contain spaces and parentheses; fields after it are fixed.
                let group = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_ascii_whitespace().nth(2))
                    .and_then(|v| v.parse::<u32>().ok());
                let pages = statm
                    .split_ascii_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok());
                if let (Some(group), Some(pages)) = (group, pages) {
                    *groups.entry(group).or_insert(0) += pages * page;
                }
            }
            groups
        })
        .await
        .unwrap_or_default()
    }
    #[cfg(not(target_os = "linux"))]
    BTreeMap::new()
}

async fn available_memory() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        tokio::task::spawn_blocking(|| {
            let host = std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|info| memory_field(&info, "MemAvailable:"))
                .map(|kib| kib * 1024);
            let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
            host.into_iter()
                .chain(cgroup_available(
                    std::path::Path::new("/sys/fs/cgroup"),
                    &cgroup,
                ))
                .min()
        })
        .await
        .ok()
        .flatten()
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// Available memory, with the running conversions' resident memory measured alongside it.
async fn measure(gate: Arc<MemoryGate>) -> Option<u64> {
    gate.refresh().await;
    available_memory().await
}

#[cfg(target_os = "linux")]
fn memory_field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        if fields.next()? != name {
            return None;
        }
        fields.next()?.parse().ok()
    })
}

#[cfg(target_os = "linux")]
fn cgroup_available(root: &std::path::Path, membership: &str) -> Option<u64> {
    use std::path::{Component, Path};

    let group = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::/"))
        .map(Path::new)
        .filter(|path| path.components().all(|c| matches!(c, Component::Normal(_))));
    // A container usually sees its own cgroup at the mount root. On a host, an ancestor
    // can set a smaller limit than the process's immediate cgroup.
    let group = group.map_or_else(|| root.to_path_buf(), |path| root.join(path));
    group
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .filter_map(|dir| {
            let limit = std::fs::read_to_string(dir.join("memory.max"))
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()?;
            let current = std::fs::read_to_string(dir.join("memory.current"))
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok());
            // Inactive file cache can be reclaimed under pressure, unlike anonymous memory.
            let cache = std::fs::read_to_string(dir.join("memory.stat"))
                .ok()
                .and_then(|stats| memory_field(&stats, "inactive_file"))
                .unwrap_or(0);
            Some(current.map_or(0, |used| limit.saturating_sub(used.saturating_sub(cache))))
        })
        .min()
}

impl Jobs {
    pub async fn open(store: Arc<Store>, uploads: Arc<Uploads>) -> anyhow::Result<Arc<Self>> {
        let jobs = Arc::new(Self {
            store,
            uploads,
            active: Mutex::new(BTreeMap::new()),
            pending_nas: Mutex::default(),
            memory: Mutex::new(Arc::new(MemoryGate::default())),
            progress: RwLock::new(BTreeMap::new()),
        });
        let ids = jobs
            .store
            .records
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for record in ids {
            let folder = jobs
                .store
                .folder(&record)
                .await
                .map_err(|_| anyhow::anyhow!("invalid conversion record"))?;
            let journal = folder.join(".conversion.json");
            let mut journaled = None;
            let mut nas_pending = false;
            if journal.is_file() {
                let work: Value = read_json(&journal).await?;
                let generation = work["generation"]
                    .as_str()
                    .filter(|v| crate::store::valid_id(v))
                    .context("invalid conversion journal")?;
                // A NAS publication waits for the share after startup, with its local stage intact.
                nas_pending = work.get("prepared_target").is_some()
                    || work.get("prepared_previous").is_some();
                if nas_pending {
                    jobs.pending_nas.lock().await.insert(record.clone());
                } else {
                    jobs.publish(&record, generation).await?;
                }
                journaled = Some(format!(".convert-{generation}"));
            }
            // A conversion a killed process left before it completed: no journal names it, so it was
            // never published, and no conversion runs before this returns. A journaled generation
            // is `publish`'s (a cancelled one keeps its staging).
            let mut entries = tokio::fs::read_dir(&folder).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(".convert-")
                    && Some(&name) != journaled.as_ref()
                    && entry.file_type().await?.is_dir()
                    && tokio::fs::remove_dir_all(entry.path()).await.is_err()
                {
                    tracing::warn!(record = %record, "conversion_staging_cleanup_failed");
                }
            }
            let mut meta = jobs
                .store
                .meta(&record)
                .await
                .map_err(|_| anyhow::anyhow!("missing conversion record"))?;
            if meta["status"] == "ready"
                && meta.get("video_downloads").is_none()
                && meta.get("prepared_nas").is_none()
            {
                meta["video_downloads"] = download_entries(&folder).await?;
                jobs.store
                    .save_meta(meta.clone())
                    .await
                    .map_err(|_| anyhow::anyhow!("download metadata recovery failed"))?;
            }
            if meta["status"] == "processing" {
                if nas_pending {
                    meta["status"] = json!("unconverted");
                    meta["error"] = json!(crate::prepared::WAIT_MESSAGE);
                    meta["deferred"] = json!(crate::prepared::WAITING);
                } else {
                    meta["status"] = json!("queued");
                    meta["error"] = Value::Null;
                }
                jobs.store
                    .save_meta(meta)
                    .await
                    .map_err(|_| anyhow::anyhow!("conversion recovery metadata failed"))?;
            }
        }
        Ok(jobs)
    }
    pub async fn cancel(&self, record: &str) -> bool {
        if let Some(cancel) = self.active.lock().await.get(record) {
            cancel.notify_one();
            true
        } else {
            false
        }
    }
    pub async fn busy(&self, record: &str) -> bool {
        self.active.lock().await.contains_key(record)
    }

    async fn save_cancellation(&self, record: &str, generation: &str) -> anyhow::Result<()> {
        let lock = self.store.record_lock(record).await;
        let _guard = lock.lock().await;
        let mut meta = self
            .store
            .meta(record)
            .await
            .map_err(|_| anyhow::anyhow!("record unavailable"))?;
        if meta["conversion_id"] == generation {
            meta["status"] = json!("unconverted");
            meta["manual_conversion"] = json!(false);
            meta["auto_excluded"] = json!(true);
            meta["error"] = Value::Null;
            self.store
                .save_meta(meta)
                .await
                .map_err(|_| anyhow::anyhow!("save cancellation failed"))?;
        }
        Ok(())
    }

    async fn try_start(
        &self,
        record: &str,
        limit: usize,
        read_memory: impl std::ops::AsyncFnOnce() -> Option<u64>,
    ) -> Option<(Arc<Notify>, PreparationReservation)> {
        let gate = self.memory.lock().await;
        let active = self.active.lock().await;
        if active.len() >= limit || active.contains_key(record) {
            return None;
        }
        let idle = active.is_empty();
        drop(active);
        let available = read_memory().await;
        let started = {
            let mut state = gate.state();
            // Leave room for the conversion first in line for admission.
            let first_in_line = if state.waiting.is_empty() {
                0
            } else {
                next_reservation(&state.peaks)
            };
            let needed = MEMORY_HEADROOM
                + state.preparation
                + state.outstanding()
                + first_in_line
                + PREPARATION_BUFFERS;
            // With nothing else active, one job always starts.
            let started = idle || available.is_some_and(|available| available >= needed);
            if started {
                state.preparation += PREPARATION_BUFFERS;
            }
            started
        };
        if !started {
            return None;
        }
        let reservation = PreparationReservation {
            gate: Arc::clone(&gate),
            bytes: PREPARATION_BUFFERS,
        };
        let cancel = Arc::new(Notify::new());
        self.active
            .lock()
            .await
            .insert(record.into(), cancel.clone());
        Some((cancel, reservation))
    }

    /// Waits in arrival order until this conversion fits, then admits its child for its whole run.
    /// When no conversion runs, the first in line is admitted whatever the memory reads, so the
    /// queue always moves.
    async fn admit<Measured: Future<Output = Option<u64>>>(
        &self,
        record: &str,
        read_memory: impl Fn() -> Measured,
    ) -> ConversionPermit {
        let gate = Arc::clone(&*self.memory.lock().await);
        let waiting = {
            let mut state = gate.state();
            let ticket = state.next;
            state.next += 1;
            state.waiting.push_back(ticket);
            Waiting {
                gate: Arc::clone(&gate),
                ticket,
            }
        };
        let mut logged: Option<tokio::time::Instant> = None;
        let mut reported = false;
        loop {
            let changed = gate.changed.notified();
            let decision = self.memory.lock().await;
            let first = gate.state().waiting.front() == Some(&waiting.ticket);
            if first {
                let available = read_memory().await;
                if let Some(permit) = gate.decide(waiting.ticket, record, available, &mut logged) {
                    return permit;
                }
            }
            drop(decision);
            if !reported {
                reported = true;
                self.progress
                    .write()
                    .await
                    .insert(record.into(), json!({"stage":"waiting_memory","percent":0}));
            }
            // A conversion can finish or the system can recover memory independently.
            let _ = tokio::time::timeout(Duration::from_secs(1), changed).await;
        }
    }

    pub async fn tick(self: &Arc<Self>) -> anyhow::Result<()> {
        let settings = self.store.processing.read().await.clone();
        let limit = settings["concurrency"].as_u64().unwrap_or(1) as usize;
        let automatic = settings["auto_convert"] == true;
        let mut admission_blocked = false;
        let gate = Arc::clone(&*self.memory.lock().await);
        let (_, receiving) = self.uploads.snapshot().await;
        let mut rows = self
            .store
            .records
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| {
            a["queued_at"]
                .as_f64()
                .unwrap_or(0.)
                .total_cmp(&b["queued_at"].as_f64().unwrap_or(0.))
                .then_with(|| {
                    a["uploaded"]
                        .as_f64()
                        .unwrap_or(0.)
                        .total_cmp(&b["uploaded"].as_f64().unwrap_or(0.))
                })
        });
        let mut nas_ready = BTreeMap::new();
        let pending = self.pending_nas.lock().await.clone();
        if !pending.is_empty() {
            let records = self.store.records.read().await;
            self.pending_nas
                .lock()
                .await
                .retain(|id| records.contains_key(id));
        }
        for row in rows {
            let record = row["id"].as_str().context("missing conversion id")?;
            // A job deferred for its archive waits for the archiver to queue it again once the archive
            // answers: picked up here it would sit in the queue under the deferral warning, and loop
            // through the same deferral while the archive is away.
            let waiting = |m: &Value| m["status"] == "unconverted" && m["deferred"] == "storage";
            let prepared_wait = row["deferred"] == crate::prepared::WAITING;
            let resume = pending.contains(record);
            let eligible = resume
                || row["status"] == "queued"
                || (row["status"] == "unconverted"
                    && (automatic && row["auto_excluded"] != true
                        || prepared_wait && row["manual_conversion"] == true)
                    && !waiting(&row));
            if !eligible {
                continue;
            }
            let lock = self.store.record_lock(record).await;
            let Ok(_guard) = lock.try_lock() else {
                continue;
            };
            if self.busy(record).await {
                continue;
            }
            // Deleted while this tick waited for its lock.
            let Ok(mut meta) = self.store.meta(record).await else {
                continue;
            };
            if resume {
                let journal = self.store.root.join(record).join(".conversion.json");
                let work: Value = match read_json(&journal).await {
                    Ok(work) => work,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        self.pending_nas.lock().await.remove(record);
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                // Release the scheduler and let the publication lock serialize recovery with edits.
                let target: Option<crate::prepared::NasLocation> =
                    serde_json::from_value(work["prepared_target"].clone())?;
                let previous: Option<crate::prepared::NasLocation> =
                    serde_json::from_value(work["prepared_previous"].clone())?;
                let relevant = if meta["status"] == "ready" {
                    previous.as_ref()
                } else {
                    target.as_ref()
                };
                if meta["status"] == "error" && meta["conversion_id"] == work["generation"] {
                    // Keep the stage on a non-network failure. Retrying the conversion schedules
                    // publication again with its original generation instead of overwriting the journal.
                    continue;
                }
                if let Some(target) = relevant
                    && !self.prepared_ready(&mut nas_ready, target).await
                {
                    continue;
                }
                let generation = work["generation"]
                    .as_str()
                    .filter(|key| crate::store::valid_id(key))
                    .context("invalid conversion journal")?
                    .to_owned();
                let cancel = Arc::new(Notify::new());
                {
                    let mut active = self.active.lock().await;
                    if active.len() >= limit {
                        continue;
                    }
                    active.insert(record.to_owned(), cancel);
                }
                let jobs = self.clone();
                let record = record.to_owned();
                tokio::spawn(async move {
                    if let Err(error) = jobs.publish(&record, &generation).await {
                        jobs.publication_error(&record, &generation, error).await;
                    }
                    jobs.active.lock().await.remove(&record);
                });
                continue;
            }
            if !matches!(meta["status"].as_str(), Some("queued" | "unconverted")) || waiting(&meta)
            {
                continue;
            }
            if prepared_wait
                && let Some(target) = self.store.prepared.target("").await?
                && !self.prepared_ready(&mut nas_ready, &target).await
            {
                continue;
            }
            let manual = meta["manual_conversion"] == true;
            if !manual && !automatic {
                meta["status"] = json!("unconverted");
                self.store
                    .save_meta(meta)
                    .await
                    .map_err(|_| anyhow::anyhow!("save conversion state failed"))?;
                continue;
            }
            if meta["status"] != "queued" {
                meta["status"] = json!("queued");
                meta["queued_at"] = json!(now());
                self.store
                    .save_meta(meta.clone())
                    .await
                    .map_err(|_| anyhow::anyhow!("save conversion queue failed"))?;
            }
            if !manual && !receiving.is_null() {
                self.progress
                    .write()
                    .await
                    .insert(record.into(), json!({"stage":"waiting_upload","percent":0}));
                continue;
            }
            if admission_blocked {
                continue;
            }
            let Some((cancel, reservation)) = self
                .try_start(record, limit, || measure(gate.clone()))
                .await
            else {
                // Retry on the next tick. This pass can still update queued records.
                admission_blocked = true;
                continue;
            };
            let generation = id();
            if let Some(m) = meta.as_object_mut() {
                m.remove("deferred");
            }
            meta["status"] = json!("processing");
            meta["conversion_id"] = json!(generation);
            meta["error"] = Value::Null;
            if self.store.save_meta(meta).await.is_err() {
                self.active.lock().await.remove(record);
                anyhow::bail!("save active conversion failed; original data preserved");
            }
            self.progress
                .write()
                .await
                .insert(record.into(), json!({"stage":"log_prepare","percent":0}));
            let jobs = self.clone();
            let record = record.to_owned();
            tokio::spawn(async move {
                let result = jobs.run(&record, &generation, cancel, reservation).await;
                if let Err(error) = result {
                    jobs.publication_error(&record, &generation, error).await;
                }
                jobs.active.lock().await.remove(&record);
                jobs.progress.write().await.remove(&record);
            });
        }
        Ok(())
    }
    async fn prepared_ready(
        &self,
        ready: &mut BTreeMap<std::path::PathBuf, bool>,
        target: &crate::prepared::NasLocation,
    ) -> bool {
        if let Some(ready) = ready.get(&target.share) {
            return *ready;
        }
        let connected = self.store.prepared.check(target).await.is_ok();
        ready.insert(target.share.clone(), connected);
        connected
    }

    async fn publication_error(&self, record: &str, generation: &str, error: anyhow::Error) {
        let lock = self.store.record_lock(record).await;
        let _guard = lock.lock().await;
        if let Ok(mut meta) = self.store.meta(record).await
            && meta["conversion_id"] == generation
            && meta["status"] != "ready"
        {
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotConnected)
            {
                meta["status"] = json!("unconverted");
                meta["error"] = json!(crate::prepared::WAIT_MESSAGE);
                meta["deferred"] = json!(crate::prepared::WAITING);
            } else {
                tracing::warn!(%record, "conversion_worker_failed");
                meta["status"] = json!("error");
                meta["error"] = json!(error.to_string());
            }
            let _ = self.store.save_meta(meta).await;
        }
    }

    async fn run(
        &self,
        record: &str,
        generation: &str,
        cancel: Arc<Notify>,
        preparation: PreparationReservation,
    ) -> anyhow::Result<()> {
        let folder = self
            .store
            .folder(record)
            .await
            .map_err(|_| anyhow::anyhow!("record unavailable"))?;
        let staging = folder.join(format!(".convert-{generation}"));
        let mut meta = self
            .store
            .meta(record)
            .await
            .map_err(|_| anyhow::anyhow!("record unavailable"))?;
        if folder.join(crate::prepared::MOVE_JOURNAL).exists() {
            let lock = self.store.record_lock(record).await;
            let _guard = lock.lock().await;
            meta = self
                .store
                .meta(record)
                .await
                .map_err(|_| anyhow::anyhow!("record unavailable"))?;
            self.store
                .prepared
                .finish_pending(&self.store, &folder, &mut meta)
                .await?;
        }
        let target = self.store.prepared.target(generation).await?;
        if let Some(target) = &target {
            self.store.prepared.check(target).await?;
        }
        let previous = if meta["prepared_stale"] == true {
            None
        } else if crate::prepared::location(&meta)?.is_some() {
            Some(self.store.prepared.read_dir(&folder, &meta).await?)
        } else {
            None
        };
        let label = meta["name"].as_str().unwrap_or("recording").to_owned();
        // Python `run_job`: archived inputs are read where they are, Drive-only ones from staged verified copies.
        let inputs = match self.store.archive() {
            Some(archive) => {
                Some(crate::media_read::conversion_inputs(&archive, &folder, record, &meta).await)
            }
            None => None,
        };
        drop(preparation);
        let result = self
            .convert(
                record,
                generation,
                cancel,
                &folder,
                &staging,
                label,
                inputs.as_ref(),
                target,
                previous,
                crate::prepared::location(&meta)?,
            )
            .await;
        for path in inputs.iter().flat_map(|i| &i.staged) {
            crate::media_read::unstage(path).await;
        }
        result
    }
    #[allow(clippy::too_many_arguments)]
    async fn convert(
        &self,
        record: &str,
        generation: &str,
        cancel: Arc<Notify>,
        folder: &std::path::Path,
        staging: &std::path::Path,
        label: String,
        inputs: Option<&crate::media_read::Inputs>,
        target: Option<crate::prepared::NasLocation>,
        previous: Option<std::path::PathBuf>,
        previous_nas: Option<crate::prepared::NasLocation>,
    ) -> anyhow::Result<()> {
        if let Some(message) = inputs.and_then(|i| i.deferred) {
            // Converting without the archived files would replace a good result with one lacking its videos;
            // `deferred` marks it for requeueing once the archive answers (the archiver's requeue_deferred).
            let lock = self.store.record_lock(record).await;
            let _guard = lock.lock().await;
            if let Ok(mut meta) = self.store.meta(record).await
                && meta["conversion_id"] == generation
            {
                meta["status"] = json!("unconverted");
                meta["error"] = json!(message);
                meta["deferred"] = json!("storage");
                self.store
                    .save_meta(meta)
                    .await
                    .map_err(|_| anyhow::anyhow!("save conversion deferral failed"))?;
            }
            return Ok(());
        }
        let input = |kind: &str| {
            inputs
                .and_then(|i| i.paths.get(kind).cloned())
                .unwrap_or_else(|| folder.join(kind))
        };
        let available = |kind: &str| match inputs {
            Some(i) => i.states.get(kind) == Some(&"available"),
            None => folder.join(kind).is_file(),
        };
        let timeout = if available("fcamera.hevc") || available("ecamera.hevc") {
            1800
        } else {
            600
        };
        // Added input changes the generation and invalidates the current conversion.
        let superseded = async {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if self
                    .store
                    .meta(record)
                    .await
                    .is_ok_and(|m| m["conversion_id"] != generation)
                {
                    return;
                }
            }
        };
        tokio::pin!(superseded);
        let worker = std::env::current_exe()?;
        #[cfg(unix)]
        let mut command = {
            let mut c = Command::new("nice");
            c.args(["-n", "10"])
                .arg("ionice")
                .args(["-c", "2", "-n", "7"])
                .arg(worker);
            c
        };
        #[cfg(not(unix))]
        let mut command = Command::new(worker);
        let media = crate::convert::VIDEO_INPUTS
            .iter()
            .map(|k| ((*k).to_owned(), json!(input(k))))
            .collect::<serde_json::Map<_, _>>();
        if let Some(previous) = previous {
            command.env(crate::convert::PREPARED_DIR, previous);
        }
        command
            .arg("convert-log")
            .arg(input("rlog.zst"))
            .arg(staging)
            .arg(label)
            .env(
                crate::convert::MEDIA_PATHS,
                Value::Object(media).to_string(),
            )
            .env(crate::convert::RECORDING_DIR, folder)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Its media workers die with it (they also die with it on its own SIGKILL).
        crate::media::die_with_parent(&mut command);
        #[cfg(unix)]
        command.process_group(0);
        let gate = Arc::clone(&*self.memory.lock().await);
        let permit = tokio::select! {
            biased;
            _ = cancel.notified() => return self.save_cancellation(record, generation).await,
            _ = &mut superseded => return Ok(()),
            permit = self.admit(record, || measure(gate.clone())) => permit,
        };
        // The time limit counts from admission: waiting for memory is not converting.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
        let mut child = command
            .spawn()
            .context("conversion process could not start")?;
        let group = child.id();
        if let Some(group) = group {
            permit.track(group);
        }
        let stop = |child: &mut tokio::process::Child| {
            #[cfg(unix)]
            if let Some(pid) = group.and_then(|p| i32::try_from(p).ok()) {
                // SAFETY: kill(2) on the process group this conversion leads.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            child.start_kill()
        };
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let reaped = std::sync::atomic::AtomicBool::new(false);
        let run = async {
            while let Some(line) = lines.next_line().await? {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && v["stage"].as_str().is_some_and(|s| s.len() < 64)
                    && v["percent"]
                        .as_f64()
                        .is_some_and(|p| (0.0..=100.).contains(&p))
                {
                    self.progress.write().await.insert(record.into(), v);
                } else {
                    crate::media::relay_remux_event(record, &line);
                }
            }
            let status = child.wait().await?;
            reaped.store(true, std::sync::atomic::Ordering::Relaxed);
            anyhow::ensure!(
                status.success(),
                "conversion failed; original data preserved"
            );
            Ok::<_, anyhow::Error>(())
        };
        let future = async {
            let Some(group) = group else {
                return run.await;
            };
            tokio::select! {
                result = run => result,
                never = permit.sample(group) => match never {},
            }
        };
        tokio::select! {
            result=tokio::time::timeout_at(deadline,future)=>{
                if let Err(error)=result.context("conversion deadline exceeded; original data preserved").and_then(|r|r){
                    // Nothing of this generation was published yet: its partial output goes with its
                    // processes. A reaped leader's id may belong to another group by now; its media
                    // workers die with it (PDEATHSIG).
                    if !reaped.load(std::sync::atomic::Ordering::Relaxed) {
                        let _=stop(&mut child);
                        let _=child.wait().await;
                    }
                    let kept=read_json::<Value>(&staging.join(crate::convert::KEPT_PREVIOUS)).await.ok();
                    let _=tokio::fs::remove_dir_all(&staging).await;
                    if let Some(kept)=kept && self.keep_previous(record,generation,&kept["warning"]).await? {
                        return Ok(());
                    }
                    return Err(error);
                }
            },
            _=&mut superseded=>{
                stop(&mut child)?;
                child.wait().await?;
                let _=tokio::fs::remove_dir_all(&staging).await;
                return Ok(());
            }
            _=cancel.notified()=>{
                stop(&mut child)?;
                child.wait().await?;
                let _=tokio::fs::remove_dir_all(&staging).await;
                return self.save_cancellation(record, generation).await;
            }
        }
        permit.succeeded();
        drop(permit);
        let _summary: Value = read_json(&staging.join("summary.json"))
            .await
            .context("conversion summary missing")?;
        atomic_json(&staging.join(".generation"), &generation).await?;
        // MP4s of the published result this one took over: the journal carries the list, so
        // `prepared/` keeps the layout it always has.
        let carried_path = staging.join(crate::convert::CARRIED_PREVIOUS);
        // Only an absent list means nothing was taken over. A list that cannot be read must stop
        // the publication: read as empty, a carried MP4 would count as produced and, with originals
        // not retained, replace the original the engine refused.
        let carried = match read_json::<Value>(&carried_path).await {
            Ok(list) => {
                tokio::fs::remove_file(&carried_path).await?;
                list
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!([]),
            Err(e) => return Err(e).context("carried camera list unreadable"),
        };
        // The MP4s this generation wrote, by size: a resumed publication tells one it moved next to
        // its original from a stored MP4 that was there before (a conversion of MP4 inputs writes none).
        let mut produced = serde_json::Map::new();
        for (_, _, mp4) in CAMERA_FILES {
            if is_listed(&carried, mp4) {
                continue;
            }
            if let Ok(stat) = tokio::fs::metadata(staging.join(mp4)).await {
                produced.insert(mp4.into(), json!(stat.len()));
            }
        }
        let mut work = json!({"generation":generation,"produced":produced,"carried":carried});
        if let Some(target) = target {
            work["prepared_target"] = json!(target);
        }
        if let Some(previous) = previous_nas {
            work["prepared_previous"] = json!(previous);
        }
        atomic_json(&folder.join(".conversion.json"), &work).await?;
        if work.get("prepared_target").is_some() || work.get("prepared_previous").is_some() {
            self.pending_nas.lock().await.insert(record.to_owned());
        }
        self.publish(record, generation).await
    }
    async fn publish(&self, record: &str, generation: &str) -> anyhow::Result<()> {
        let contents = self.original_contents(record).await;
        let lock = self.store.record_lock(record).await;
        let _guard = lock.lock().await;
        let folder = self
            .store
            .folder(record)
            .await
            .map_err(|_| anyhow::anyhow!("record unavailable"))?;
        let mut meta = self
            .store
            .meta(record)
            .await
            .map_err(|_| anyhow::anyhow!("record unavailable"))?;
        let journal_path = folder.join(".conversion.json");
        let mut journal: Value = read_json(&journal_path).await?;
        let nas_target: Option<crate::prepared::NasLocation> =
            serde_json::from_value(journal["prepared_target"].clone())?;
        let nas_previous: Option<crate::prepared::NasLocation> =
            serde_json::from_value(journal["prepared_previous"].clone())?;
        if meta["cancel_requested"] == generation {
            meta["status"] = json!("unconverted");
            meta["auto_excluded"] = json!(true);
            meta["manual_conversion"] = json!(false);
            meta["error"] = Value::Null;
            self.store
                .save_meta(meta)
                .await
                .map_err(|_| anyhow::anyhow!("save conversion cancellation failed"))?;
            if let Some(target) = &nas_target {
                self.store.prepared.remove_nas(record, target).await?;
            }
            tokio::fs::remove_file(&journal_path).await?;
            return Ok(());
        }
        let local_prepared = folder.join("prepared");
        let staging = folder.join(format!(".convert-{generation}"));
        let backup = folder.join(format!(".previous-{generation}"));
        if meta["conversion_id"] != generation {
            // Superseded by a camera addition, which already cleared the replay data this
            // generation would replace (including a half-published one) and queued the record.
            for dir in [&staging, &backup] {
                match tokio::fs::remove_dir_all(dir).await {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                }
            }
            if let Some(target) = &nas_target {
                self.store.prepared.remove_nas(record, target).await?;
            }
            tokio::fs::remove_file(&journal_path).await?;
            return Ok(());
        }
        let prepared = if nas_target.is_some() {
            &staging
        } else {
            &local_prepared
        };
        let installed = read_json::<String>(&prepared.join(".generation"))
            .await
            .ok()
            .as_deref()
            == Some(generation);
        let committed = meta["status"] == "ready"
            && if let Some(target) = &nas_target {
                crate::prepared::location(&meta)?.as_ref() == Some(target)
            } else {
                installed
            };
        if committed {
            return self
                .finish_prepared_publication(
                    &folder,
                    &journal_path,
                    &journal,
                    &backup,
                    &staging,
                    nas_target.as_ref(),
                    nas_previous.as_ref(),
                )
                .await;
        }
        if !installed {
            anyhow::ensure!(
                nas_target.is_none(),
                "completed conversion generation is missing"
            );
            anyhow::ensure!(
                staging.join("summary.json").is_file(),
                "completed conversion staging is missing; old output preserved"
            );
            if prepared.exists() {
                tokio::fs::rename(prepared, &backup).await?;
            }
            tokio::fs::rename(&staging, prepared).await?;
            sync_dirs(&[&folder]).await?;
            crate::fault::point("publish_installed");
        }
        let summary: Value = read_json(&prepared.join("summary.json")).await?;
        let keep = self.store.processing.read().await["keep_original_video"] != false;
        let mut codecs = serde_json::Map::new();
        let mut origins = serde_json::Map::new();
        let mut originals = vec![];
        for (camera, raw, mp4) in CAMERA_FILES {
            if !summary["videos"][camera].is_object() {
                continue;
            }
            codecs.insert(camera.into(), summary["videos"][camera]["codec"].clone());
            if let Some(origin) = summary["videos"][camera].get("sourcePts") {
                origins.insert(camera.into(), origin.clone());
            }
            // An archived original stays the stored copy; its MP4 remains converted output in prepared/.
            let archived = index::location(&meta, raw) != "local"
                || index::is_archived_location(&index::location(&meta, mp4));
            // A restart resuming this generation's publication finds an MP4 it already moved next to
            // the original it was made from: the one this generation wrote (the journal's size), not a
            // stored MP4 the conversion read while its original was elsewhere.
            let moved = installed
                && !prepared.join(mp4).exists()
                && folder.join(raw).is_file()
                && std::fs::metadata(folder.join(mp4))
                    .is_ok_and(|m| m.is_file() && journal["produced"][mp4] == json!(m.len()));
            // A camera taken over from the previous result was not converted by this generation:
            // its MP4 and its original stay as that result left them.
            let carried = is_listed(&journal["carried"], mp4);
            if !keep && !archived && !carried && (moved || prepared.join(mp4).is_file()) {
                // An MP4 removed by archive cleanup exists again, locally: the index must not hide it.
                if index::location(&meta, mp4) == "removed"
                    && let Some(storage) = meta["storage"].as_object_mut()
                {
                    storage.remove(mp4);
                }
                // The conversion verified frame count, timestamps and compressed
                // bitstream before reaching its durable publication journal.
                if !moved {
                    tokio::fs::rename(prepared.join(mp4), folder.join(mp4)).await?;
                    crate::fault::point("publish_mp4_moved");
                }
                // The conversion synced the MP4 itself; its new name must be durable before the original goes.
                sync_dirs(&[&folder, prepared]).await?;
                // The MP4 is new content: a copy of the previous one no longer describes it.
                if index::drop_remote(&mut meta, mp4, None).is_some() {
                    tracing::info!(kind = mp4, "remote_copy_superseded_left_on_archive");
                }
                if index::remote(&meta, raw).is_some() {
                    // The original goes: its copy becomes the stored file if it is the same content.
                    let content = std::fs::metadata(folder.join(raw)).ok().map(|stat| {
                        let known = contents
                            .get(raw)
                            .filter(|(size, mtime, _)| {
                                (*size, *mtime) == (stat.len(), mtime_ns(&stat))
                            })
                            .map(|(_, _, sha)| sha.clone())
                            .unwrap_or_default();
                        (stat.len(), known)
                    });
                    let content = content.as_ref().map(|(s, h)| (*s, h.as_str()));
                    if !index::promote_remote(&mut meta, raw, content, now()) {
                        tracing::info!(kind = raw, "remote_copy_mismatch_left_on_archive");
                    }
                }
                if folder.join(raw).is_file() {
                    originals.push(raw);
                }
            }
        }
        meta["video_codecs"] = json!(codecs);
        meta["video_origins"] = json!(origins);
        meta["video_downloads"] = download_entries_from(&folder, prepared).await?;
        let manifest = if let Some(target) = &nas_target {
            self.store
                .prepared
                .install(&folder, prepared, target)
                .await?
        } else {
            Default::default()
        };
        crate::prepared::set_location(&mut meta, nas_target.as_ref(), manifest);
        apply_summary(&mut meta, &summary);
        meta["status"] = json!("ready");
        meta.as_object_mut().unwrap().remove("deferred");
        meta["error"] = Value::Null;
        meta["manual_conversion"] = json!(false);
        meta["decoder_version"] = json!("hars-v28-runtime-geometry");
        // Originals go only after the index that describes their absence (a promoted copy, a visible
        // MP4) is durable; until then every step above is redone the same on a restart. Once it is,
        // the journal is what still names them.
        journal["generation"] = json!(generation);
        journal["originals"] = json!(originals);
        atomic_json(&journal_path, &journal).await?;
        self.store
            .save_meta(meta)
            .await
            .map_err(|_| anyhow::anyhow!("conversion metadata publication failed"))?;
        crate::fault::point("publish_meta_saved");
        self.finish_prepared_publication(
            &folder,
            &journal_path,
            &journal,
            &backup,
            &staging,
            nas_target.as_ref(),
            nas_previous.as_ref(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_prepared_publication(
        &self,
        folder: &std::path::Path,
        journal_path: &std::path::Path,
        journal: &Value,
        backup: &std::path::Path,
        staging: &std::path::Path,
        target: Option<&crate::prepared::NasLocation>,
        previous: Option<&crate::prepared::NasLocation>,
    ) -> anyhow::Result<()> {
        let record = folder
            .file_name()
            .and_then(|n| n.to_str())
            .context("invalid recording id")?;
        if let Some(target) = target {
            let f = self
                .store
                .prepared
                .open_file(target, record, ".generation".into())
                .await?;
            let published: String = self
                .store
                .prepared
                .quick(move || {
                    serde_json::from_reader(std::io::BufReader::new(f))
                        .map_err(std::io::Error::other)
                })
                .await?;
            anyhow::ensure!(
                published == target.generation,
                "prepared publication marker mismatch"
            );
        }
        if let Some(previous) = previous {
            self.store.prepared.remove_nas(record, previous).await?;
        }
        if target.is_some() {
            crate::prepared::remove_local(folder.join("prepared")).await?;
            crate::prepared::remove_local(staging.to_owned()).await?;
        }
        self.finish_publication(folder, journal_path, journal, backup)
            .await
    }

    /// The rest of a publication whose metadata is durable: the originals its journal names, then
    /// the journal and the previous output. Each original goes only with its MP4 next to it.
    async fn finish_publication(
        &self,
        folder: &std::path::Path,
        journal_path: &std::path::Path,
        journal: &Value,
        backup: &std::path::Path,
    ) -> anyhow::Result<()> {
        for (_, raw, mp4) in CAMERA_FILES {
            let listed = journal["originals"]
                .as_array()
                .is_some_and(|l| l.iter().any(|k| k == raw));
            if !listed || !folder.join(mp4).is_file() {
                continue;
            }
            match tokio::fs::remove_file(folder.join(raw)).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                Err(_) => {}
                Ok(()) => crate::fault::point("publish_original_deleted"),
            }
        }
        sync_dirs(&[folder]).await?;
        tokio::fs::remove_file(journal_path).await?;
        // Only this job's old, derived directory is removed, after its replacement
        // and metadata are durable. Raw sources are always retained.
        if backup.is_dir() {
            tokio::fs::remove_dir_all(backup).await?;
        }
        Ok(())
    }
}

fn is_listed(list: &Value, name: &str) -> bool {
    list.as_array().is_some_and(|l| l.iter().any(|k| k == name))
}

const CAMERA_FILES: [(&str, &str, &str); 3] = [
    ("qcamera", "qcamera.ts", "camera.mp4"),
    ("front", "fcamera.hevc", "fcamera.mp4"),
    ("wide", "ecamera.hevc", "ecamera.mp4"),
];

fn apply_summary(meta: &mut Value, summary: &Value) {
    for key in [
        "duration",
        "warnings",
        "model_frames",
        "videos",
        "drive_summary",
        "replay_v",
        "started_at",
        "start_source",
        "first_mono",
        "gps_offset",
        "clock_offset",
        "start_v",
        "has_track",
    ] {
        if let Some(v) = summary.get(key) {
            meta[key] = v.clone();
        }
    }
    meta["video"] = json!(summary["video"].is_object());
}

impl Jobs {
    /// A conversion that stopped for a resource failure rather than drop a camera the published
    /// result plays: the record is that result again, `ready`, with the failure among its warnings,
    /// so its videos stay playable. False when that result is gone or another generation owns the record.
    async fn keep_previous(
        &self,
        record: &str,
        generation: &str,
        warning: &Value,
    ) -> anyhow::Result<bool> {
        let lock = self.store.record_lock(record).await;
        let _guard = lock.lock().await;
        let (Ok(folder), Ok(mut meta)) = (
            self.store.folder(record).await,
            self.store.meta(record).await,
        ) else {
            return Ok(false);
        };
        if meta["prepared_stale"] == true {
            return Ok(false);
        }
        let prepared = self.store.prepared.read_dir(&folder, &meta).await?;
        let Ok(summary) = read_json::<Value>(&prepared.join("summary.json")).await else {
            return Ok(false);
        };
        if meta["conversion_id"] != generation {
            return Ok(false);
        }
        apply_summary(&mut meta, &summary);
        let mut warnings = summary["warnings"].as_array().cloned().unwrap_or_default();
        warnings.push(warning.clone());
        meta["warnings"] = json!(warnings);
        if crate::prepared::location(&meta)?.is_none() {
            meta["video_downloads"] = download_entries(&folder).await?;
        }
        meta["status"] = json!("ready");
        meta.as_object_mut().unwrap().remove("deferred");
        meta["error"] = Value::Null;
        meta["manual_conversion"] = json!(false);
        self.store
            .save_meta(meta)
            .await
            .map_err(|_| anyhow::anyhow!("save kept conversion result failed"))?;
        Ok(true)
    }
    /// Python `local_content`, before the publication lock: (size, mtime, sha256) of each local original with a
    /// remote copy the publication may delete, so an identical copy can stand in for it (`promote_remote`).
    async fn original_contents(&self, record: &str) -> BTreeMap<&'static str, (u64, u64, String)> {
        let mut out = BTreeMap::new();
        let (Ok(meta), Ok(folder)) = (
            self.store.meta(record).await,
            self.store.folder(record).await,
        ) else {
            return out;
        };
        for raw in ["qcamera.ts", "fcamera.hevc", "ecamera.hevc"] {
            if index::remote(&meta, raw).is_none() {
                continue;
            }
            let path = folder.join(raw);
            let Ok(stat) = tokio::fs::metadata(&path).await else {
                continue;
            };
            let (size, mtime) = (stat.len(), mtime_ns(&stat));
            let sha = match index::fresh_hash(&meta, raw, size, mtime) {
                Some(sha) => sha,
                None => match crate::upload::file_hash(&path).await {
                    Ok(sha) => sha,
                    // Unverifiable: the copy is not promoted; the conversion itself goes on.
                    Err(_) => continue,
                },
            };
            out.insert(raw, (size, mtime, sha));
        }
        out
    }
}
async fn sync_dirs(dirs: &[&std::path::Path]) -> std::io::Result<()> {
    let dirs = dirs.iter().map(|d| d.to_path_buf()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        for dir in &dirs {
            std::fs::File::open(dir)?.sync_all()?;
        }
        #[cfg(not(unix))]
        let _ = dirs;
        Ok(())
    })
    .await?
}
fn mtime_ns(stat: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        stat.mtime() as u64 * 1_000_000_000 + stat.mtime_nsec() as u64
    }
    #[cfg(not(unix))]
    {
        stat.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64)
    }
}

pub(crate) async fn download_entries(folder: &std::path::Path) -> std::io::Result<Value> {
    download_entries_from(folder, &folder.join("prepared")).await
}

async fn download_entries_from(
    folder: &std::path::Path,
    prepared: &std::path::Path,
) -> std::io::Result<Value> {
    let mut downloads = vec![];
    for (source, raw, mp4) in [
        ("qcamera", "qcamera.ts", "camera.mp4"),
        ("front", "fcamera.hevc", "fcamera.mp4"),
        ("wide", "ecamera.hevc", "ecamera.mp4"),
    ] {
        let retained = folder.join(mp4);
        let derived = prepared.join(mp4);
        let original = folder.join(raw);
        let (kind, path) = if retained.is_file() {
            (mp4, retained)
        } else if derived.is_file() {
            (mp4, derived)
        } else if original.is_file() {
            (raw, original)
        } else {
            continue;
        };
        downloads.push(
            json!({"source":source,"kind":kind,"bytes":tokio::fs::metadata(path).await?.len()}),
        );
    }
    Ok(json!(downloads))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn conversion_memory_follows_the_tightest_limit() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let group = root.join("system.slice/app");
        std::fs::create_dir_all(&group).unwrap();
        for (dir, limit, used, cache) in [
            (root, 3072, 2048, 0),
            (&root.join("system.slice"), 2048, 1900, 256),
            (&group, 2048, 512, 0),
        ] {
            std::fs::write(dir.join("memory.max"), format!("{}\n", limit * MIB)).unwrap();
            std::fs::write(dir.join("memory.current"), format!("{}\n", used * MIB)).unwrap();
            std::fs::write(
                dir.join("memory.stat"),
                format!("anon 0\ninactive_file {}\n", cache * MIB),
            )
            .unwrap();
        }
        let membership = "0::/system.slice/app\n";
        let host = Some(
            memory_field(
                "MemTotal: 3145728 kB\nMemAvailable: 1572864 kB\n",
                "MemAvailable:",
            )
            .unwrap()
                * 1024,
        );
        let available = || {
            host.into_iter()
                .chain(cgroup_available(root, membership))
                .min()
        };

        assert_eq!(available(), Some(404 * MIB));
        std::fs::write(
            root.join("system.slice/memory.current"),
            (1024 * MIB).to_string(),
        )
        .unwrap();
        assert_eq!(available(), Some(1024 * MIB));
        std::fs::write(root.join("memory.max"), "max\n").unwrap();
        assert_eq!(available(), Some(1280 * MIB));
    }

    async fn open_jobs() -> (tempfile::TempDir, Arc<Jobs>) {
        let fixture = tempfile::tempdir().unwrap();
        let store = Store::open(fixture.path().to_path_buf()).await.unwrap();
        let auth = Arc::new(crate::auth::Auth::open(&store.root).await.unwrap());
        let uploads = Uploads::open(store.clone(), auth, MIB).await.unwrap();
        (fixture, Jobs::open(store, uploads).await.unwrap())
    }

    /// The host as the gate sees it: what an admitted conversion holds is out of `available`
    /// and is its process group's RSS.
    struct Machine {
        available: std::sync::atomic::AtomicI64,
        lowest: std::sync::atomic::AtomicI64,
    }

    impl Machine {
        fn new(mib: u64) -> Arc<Self> {
            let bytes = (mib * MIB) as i64;
            Arc::new(Self {
                available: bytes.into(),
                lowest: bytes.into(),
            })
        }
        fn sample(&self) -> Option<u64> {
            Some(self.available.load(Ordering::SeqCst).max(0) as u64)
        }
        fn hold(&self, permit: &ConversionPermit, mib: u64) {
            let before = permit.usage.rss.load(Ordering::SeqCst) as i64;
            let after = (mib * MIB) as i64;
            let left =
                self.available.fetch_sub(after - before, Ordering::SeqCst) - (after - before);
            self.lowest.fetch_min(left, Ordering::SeqCst);
            permit.usage.sample(mib * MIB);
        }
    }

    fn gate_state(jobs: &Jobs) -> (usize, usize, u64) {
        let gate = Arc::clone(&*jobs.memory.try_lock().unwrap());
        let state = gate.state();
        (state.running.len(), state.waiting.len(), state.preparation)
    }

    #[tokio::test]
    async fn sixteen_conversions_finish_when_each_is_admitted_once() {
        // Each child keeps about 180 MiB of decoded log until it exits. 2.0.12 returned the log
        // reservation after reading and asked again before each later phase: with this model its
        // children waiting with their logs held left too little for any phase, and the queue stopped.
        let (_fixture, jobs) = open_jobs().await;
        let machine = Machine::new(1331);
        let (running, most) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let mut conversions = tokio::task::JoinSet::new();
        for number in 0..16 {
            let (jobs, machine) = (jobs.clone(), machine.clone());
            let (running, most) = (running.clone(), most.clone());
            conversions.spawn(async move {
                let permit = jobs
                    .admit(&number.to_string(), || std::future::ready(machine.sample()))
                    .await;
                most.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                machine.hold(&permit, 180);
                tokio::time::sleep(Duration::from_millis(20)).await;
                machine.hold(&permit, 300);
                tokio::time::sleep(Duration::from_millis(20)).await;
                permit.succeeded();
                running.fetch_sub(1, Ordering::SeqCst);
                machine.hold(&permit, 0); // Reaped before the permit is returned.
            });
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(done) = conversions.join_next().await {
                done.unwrap();
            }
        })
        .await
        .expect("every conversion finishes");
        let (most, lowest) = (
            most.load(Ordering::SeqCst),
            machine.lowest.load(Ordering::SeqCst),
        );
        println!(
            "most_concurrent={most} lowest_available_mib={}",
            lowest / MIB as i64
        );
        assert!(most >= 2, "{most}");
        assert!(lowest >= 0, "{lowest}");
        assert_eq!(gate_state(&jobs), (0, 0, 0));
        let gate = Arc::clone(&*jobs.memory.lock().await);
        assert_eq!(next_reservation(&gate.state().peaks), 300 * MIB);
    }

    #[tokio::test]
    async fn the_first_in_line_runs_alone_when_memory_is_short() {
        let (_fixture, jobs) = open_jobs().await;
        let (running, most) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let mut conversions = tokio::task::JoinSet::new();
        for number in 0..3 {
            let jobs = jobs.clone();
            let (running, most) = (running.clone(), most.clone());
            conversions.spawn(async move {
                let permit = jobs
                    .admit(&number.to_string(), || std::future::ready(Some(483 * MIB)))
                    .await;
                most.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                drop(permit);
            });
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(done) = conversions.join_next().await {
                done.unwrap();
            }
        })
        .await
        .expect("the queue moves one at a time");
        assert_eq!(most.load(Ordering::SeqCst), 1);
        assert_eq!(gate_state(&jobs), (0, 0, 0));
    }

    #[tokio::test]
    async fn room_for_one_admits_one_and_a_returned_permit_admits_the_next() {
        let (_fixture, jobs) = open_jobs().await;
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let (sender, mut admitted) = tokio::sync::mpsc::unbounded_channel();
        let mut conversions = tokio::task::JoinSet::new();
        for number in 0..16 {
            let (jobs, barrier, sender) = (jobs.clone(), barrier.clone(), sender.clone());
            conversions.spawn(async move {
                barrier.wait().await;
                let sample = || std::future::ready(Some(MEMORY_HEADROOM + FIRST_RESERVATION));
                let _ = sender.send(jobs.admit(&number.to_string(), sample).await);
            });
        }
        let first = tokio::time::timeout(Duration::from_secs(2), admitted.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            admitted.try_recv().is_err(),
            "admitted twice for one reservation"
        );
        assert_eq!(gate_state(&jobs), (1, 15, 0));

        drop(first); // A cancelled conversion's child was reaped.
        let second = tokio::time::timeout(Duration::from_secs(2), admitted.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            admitted.try_recv().is_err(),
            "admitted two after one returned"
        );
        assert_eq!(gate_state(&jobs), (1, 14, 0));

        // Conversions cancelled while waiting leave the queue.
        conversions.abort_all();
        while conversions.join_next().await.is_some() {}
        assert_eq!(gate_state(&jobs), (1, 0, 0));
        drop(second);
        assert_eq!(gate_state(&jobs), (0, 0, 0));
    }

    #[tokio::test]
    async fn admission_counts_the_unused_part_of_running_reservations() {
        let (_fixture, jobs) = open_jobs().await;
        let available = AtomicU64::new(64 * 1024 * MIB);
        let sample = async || Some(available.load(Ordering::SeqCst));
        let running = jobs.admit("running", sample).await;
        assert_eq!(running.usage.reserved, 320 * MIB);
        running.usage.sample(200 * MIB);

        // Half of the 120 MiB the running conversion may still allocate, on top of the headroom.
        let threshold = MEMORY_HEADROOM + 60 * MIB + 320 * MIB;
        available.store(threshold - 1, Ordering::SeqCst);
        let next = jobs.admit("next", sample);
        tokio::pin!(next);
        tokio::select! {
            biased;
            _ = &mut next => panic!("admitted below the threshold"),
            _ = std::future::ready(()) => {}
        }
        assert_eq!(
            jobs.progress.read().await["next"]["stage"],
            "waiting_memory"
        );
        available.store(threshold, Ordering::SeqCst);
        Arc::clone(&*jobs.memory.lock().await)
            .changed
            .notify_waiters();
        let next = tokio::time::timeout(Duration::from_millis(500), next)
            .await
            .expect("admitted at the threshold");
        assert_eq!(gate_state(&jobs), (2, 0, 0));
        drop((running, next));
    }

    #[test]
    fn admission_keeps_headroom_over_half_the_pending_growth_and_room_for_all_of_it() {
        let (headroom, reserved) = (MEMORY_HEADROOM, 320 * MIB);
        // Nothing running: the headroom and the new reservation.
        assert!(fits(headroom + reserved, 0, 0, reserved));
        assert!(!fits(headroom + reserved - 1, 0, 0, reserved));
        // A little pending growth: the headroom stays on top of half of it.
        let pending = 120 * MIB;
        assert!(fits(
            headroom + pending / 2 + reserved,
            0,
            pending,
            reserved
        ));
        assert!(!fits(
            headroom + pending / 2 + reserved - 1,
            0,
            pending,
            reserved
        ));
        // A lot of pending growth: all of it must still fit, headroom included.
        let pending = 640 * MIB;
        assert!(headroom + pending / 2 + reserved < pending + reserved);
        assert!(fits(pending + reserved, 0, pending, reserved));
        assert!(!fits(pending + reserved - 1, 0, pending, reserved));
        // Preparation buffers count in full.
        assert!(!fits(headroom + reserved, MIB, 0, reserved));
    }

    #[test]
    fn reservations_follow_recent_successful_peaks() {
        let mut peaks = VecDeque::new();
        assert_eq!(next_reservation(&peaks), 320 * MIB);
        record_peak(&mut peaks, 200 * MIB);
        assert_eq!(next_reservation(&peaks), 256 * MIB);
        record_peak(&mut peaks, 300 * MIB);
        assert_eq!(next_reservation(&peaks), 300 * MIB);
        record_peak(&mut peaks, 2048 * MIB);
        assert_eq!(next_reservation(&peaks), 1024 * MIB);
        for _ in 0..PEAK_HISTORY {
            record_peak(&mut peaks, 100 * MIB);
        }
        assert_eq!(peaks.len(), PEAK_HISTORY);
        assert_eq!(next_reservation(&peaks), 256 * MIB);
    }
}
