//! Durable conversion scheduling. Native/CPU work runs in a disposable child.
use crate::{
    archive::index,
    convert::{MemoryEvent, MemoryPhase},
    store::{Store, atomic_json, id, now, read_json},
    upload::Uploads,
};
use anyhow::Context;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Mutex, Notify, RwLock},
};

pub struct Jobs {
    store: Arc<Store>,
    uploads: Arc<Uploads>,
    active: Mutex<BTreeMap<String, Arc<Notify>>>,
    memory: Mutex<Arc<PhaseReservations>>,
    pub progress: RwLock<BTreeMap<String, Value>>,
}

const MIB: u64 = 1024 * 1024;
// HAOS reconversions on 2026-11-20 briefly raised app memory by about 300 MiB per job.
// Earlier process-tree peaks were about 350 MiB per job and 1,467 MiB for four jobs
// (docs/hars/PERFORMANCE.md). This is the estimate for the next conversion.
const MEMORY_PER_CONVERSION: u64 = 320 * MIB;
const MEMORY_HEADROOM: u64 = 256 * MIB;

impl MemoryPhase {
    fn estimate(self) -> u64 {
        match self {
            // The remux worker's address-space allowance, used as an estimate for a camera phase.
            Self::Video => crate::pure_remux::MEMORY_BUDGET_MIB * MIB,
            // The measured whole conversion peaked near 350 MiB, mostly during log analysis.
            Self::Replay => 350 * MIB,
            Self::Thumbnail => crate::thumbnail::MEMORY_BUDGET_MIB * MIB,
        }
    }
}

#[derive(Default)]
struct PhaseReservations {
    bytes: AtomicU64,
    waiting: std::sync::Mutex<Vec<u64>>,
    changed: Notify,
}

impl PhaseReservations {
    fn waiting(&self) -> std::sync::MutexGuard<'_, Vec<u64>> {
        self.waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct MemoryReservation {
    phases: Arc<PhaseReservations>,
    bytes: u64,
}

impl MemoryReservation {
    async fn try_new(
        phases: &tokio::sync::MutexGuard<'_, Arc<PhaseReservations>>,
        bytes: u64,
        priority: u64,
        only_conversion: bool,
        read_memory: impl std::ops::AsyncFnOnce() -> Option<u64>,
    ) -> Option<Self> {
        // Snapshot existing promises before measuring. If one ends during the read,
        // the sample may predate its allocations, so keep its promise for this decision.
        let promised = phases.bytes.load(Ordering::Acquire);
        match read_memory().await {
            Some(available) if available >= MEMORY_HEADROOM + promised + bytes + priority => {}
            None if only_conversion && promised == 0 && priority == 0 => {}
            _ => return None,
        }
        phases.bytes.fetch_add(bytes, Ordering::AcqRel);
        Some(Self {
            phases: Arc::clone(phases),
            bytes,
        })
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.phases.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        self.phases.changed.notify_waiters();
    }
}

struct WaitingPhase<'a> {
    phases: &'a PhaseReservations,
    bytes: u64,
}

impl<'a> WaitingPhase<'a> {
    fn new(phases: &'a PhaseReservations, bytes: u64) -> Self {
        phases.waiting().push(bytes);
        Self { phases, bytes }
    }
}

impl Drop for WaitingPhase<'_> {
    fn drop(&mut self) {
        let mut waiting = self.phases.waiting();
        if let Some(index) = waiting.iter().position(|bytes| *bytes == self.bytes) {
            waiting.swap_remove(index);
        }
    }
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
            memory: Mutex::new(Arc::new(PhaseReservations::default())),
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
            if journal.is_file() {
                let work: Value = read_json(&journal).await?;
                let generation = work["generation"]
                    .as_str()
                    .filter(|v| crate::store::valid_id(v))
                    .context("invalid conversion journal")?;
                jobs.publish(&record, generation).await?;
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
            if meta["status"] == "ready" && meta.get("video_downloads").is_none() {
                meta["video_downloads"] = download_entries(&folder).await?;
                jobs.store
                    .save_meta(meta.clone())
                    .await
                    .map_err(|_| anyhow::anyhow!("download metadata recovery failed"))?;
            }
            if meta["status"] == "processing" {
                meta["status"] = json!("queued");
                meta["error"] = Value::Null;
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

    async fn try_start(
        &self,
        record: &str,
        limit: usize,
        read_memory: impl std::ops::AsyncFnOnce() -> Option<u64>,
    ) -> Option<(Arc<Notify>, MemoryReservation)> {
        let phases = self.memory.lock().await;
        let active = self.active.lock().await;
        if active.len() >= limit || active.contains_key(record) {
            return None;
        }
        let idle = active.is_empty();
        drop(active);
        // Leave room for any one already-waiting phase. Its estimate becomes a
        // reservation only if the phase is granted under this same decision lock.
        let priority = phases.waiting().iter().copied().max().unwrap_or(0);
        let reservation =
            MemoryReservation::try_new(&phases, MEMORY_PER_CONVERSION, priority, idle, read_memory)
                .await?;
        let cancel = Arc::new(Notify::new());
        self.active
            .lock()
            .await
            .insert(record.into(), cancel.clone());
        Some((cancel, reservation))
    }

    async fn reserve_phase(
        &self,
        phase: MemoryPhase,
        read_memory: impl std::ops::AsyncFn() -> Option<u64>,
    ) -> Option<MemoryReservation> {
        let optional = phase.is_optional();
        let mut phases = self.memory.lock().await;
        let state = Arc::clone(&phases);
        let bytes = phase.estimate();
        let _waiting = (!optional).then(|| WaitingPhase::new(&state, bytes));
        loop {
            let changed = state.changed.notified();
            let alone = self.active.lock().await.len() == 1;
            let reservation =
                MemoryReservation::try_new(&phases, bytes, 0, alone, &read_memory).await;
            // An optional phase gets one attempt. The recording can be published immediately.
            if optional || reservation.is_some() {
                return reservation;
            }
            drop(phases);
            // A phase can finish or the system can recover memory independently.
            let _ = tokio::time::timeout(Duration::from_secs(1), changed).await;
            phases = self.memory.lock().await;
        }
    }

    pub async fn tick(self: &Arc<Self>) -> anyhow::Result<()> {
        let settings = self.store.processing.read().await.clone();
        let limit = settings["concurrency"].as_u64().unwrap_or(1) as usize;
        let automatic = settings["auto_convert"] == true;
        let mut admission_blocked = false;
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
        for row in rows {
            let record = row["id"].as_str().context("missing conversion id")?;
            // A job deferred for its archive waits for the archiver to queue it again once the archive
            // answers: picked up here it would sit in the queue under the deferral warning, and loop
            // through the same deferral while the archive is away.
            let waiting = |m: &Value| m["status"] == "unconverted" && m["deferred"] == "storage";
            let eligible = row["status"] == "queued"
                || (row["status"] == "unconverted"
                    && automatic
                    && row["auto_excluded"] != true
                    && !waiting(&row));
            if !eligible {
                continue;
            }
            let lock = self.store.record_lock(record).await;
            let _guard = lock.lock().await;
            if self.busy(record).await {
                continue;
            }
            // Deleted while this tick waited for its lock.
            let Ok(mut meta) = self.store.meta(record).await else {
                continue;
            };
            if !matches!(meta["status"].as_str(), Some("queued" | "unconverted")) || waiting(&meta)
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
            let Some((cancel, reservation)) = self.try_start(record, limit, available_memory).await
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
                    tracing::warn!(record=%record,"conversion_worker_failed");
                    let lock = jobs.store.record_lock(&record).await;
                    let _guard = lock.lock().await;
                    if let Ok(mut meta) = jobs.store.meta(&record).await
                        && meta["conversion_id"] == generation
                    {
                        meta["status"] = json!("error");
                        meta["error"] = json!(error.to_string());
                        let _ = jobs.store.save_meta(meta).await;
                    }
                }
                jobs.active.lock().await.remove(&record);
                jobs.progress.write().await.remove(&record);
            });
        }
        Ok(())
    }
    async fn run(
        &self,
        record: &str,
        generation: &str,
        cancel: Arc<Notify>,
        reservation: MemoryReservation,
    ) -> anyhow::Result<()> {
        let folder = self
            .store
            .folder(record)
            .await
            .map_err(|_| anyhow::anyhow!("record unavailable"))?;
        let staging = folder.join(format!(".convert-{generation}"));
        let meta = self
            .store
            .meta(record)
            .await
            .map_err(|_| anyhow::anyhow!("record unavailable"))?;
        let label = meta["name"].as_str().unwrap_or("recording").to_owned();
        // Python `run_job`: archived inputs are read where they are, Drive-only ones from staged verified copies.
        let inputs = match self.store.archive() {
            Some(archive) => {
                Some(crate::media_read::conversion_inputs(&archive, &folder, record, &meta).await)
            }
            None => None,
        };
        let result = self
            .convert(
                record,
                generation,
                cancel,
                &folder,
                &staging,
                label,
                inputs.as_ref(),
                reservation,
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
        reservation: MemoryReservation,
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
            .env(crate::convert::MEMORY_CONTROL, "1")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Its media workers die with it (they also die with it on its own SIGKILL).
        crate::media::die_with_parent(&mut command);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .context("conversion process could not start")?;
        let group = child.id();
        let stop = |child: &mut tokio::process::Child| {
            #[cfg(unix)]
            if let Some(pid) = group.and_then(|p| i32::try_from(p).ok()) {
                // SAFETY: kill(2) on the process group this conversion leads.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            child.start_kill()
        };
        let mut permit_pipe = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let reaped = std::sync::atomic::AtomicBool::new(false);
        let mut reservation = Some(reservation);
        let future = async {
            while let Some(line) = lines.next_line().await? {
                if let Ok(event) = serde_json::from_str::<MemoryEvent>(&line) {
                    match event {
                        MemoryEvent::End => {
                            anyhow::ensure!(
                                reservation.take().is_some(),
                                "no conversion memory phase to end"
                            );
                        }
                        MemoryEvent::Begin(phase) => {
                            anyhow::ensure!(
                                reservation.is_none(),
                                "conversion memory phase already active"
                            );
                            let sample = move || async move {
                                if phase.is_optional()
                                    && crate::fault::injected("thumbnail_memory_747_mib")
                                {
                                    Some(747 * MIB)
                                } else {
                                    available_memory().await
                                }
                            };
                            reservation = tokio::select! {
                                permit = self.reserve_phase(phase, sample) => permit,
                                status = child.wait() => {
                                    status?;
                                    reaped.store(true, std::sync::atomic::Ordering::Relaxed);
                                    anyhow::bail!("conversion exited while waiting for memory");
                                }
                            };
                            permit_pipe
                                .write_all(&[u8::from(reservation.is_some())])
                                .await?;
                        }
                    }
                } else if let Ok(v) = serde_json::from_str::<Value>(&line)
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
        let timeout = if available("fcamera.hevc") || available("ecamera.hevc") {
            1800
        } else {
            600
        };
        // A camera added to the recording (Python terminates its process) gives the record another
        // generation; this output would lack the new camera, so it is dropped without a status.
        let superseded = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
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
        tokio::select! {
            result=tokio::time::timeout(std::time::Duration::from_secs(timeout),future)=>{
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
            _=superseded=>{
                stop(&mut child)?;
                child.wait().await?;
                let _=tokio::fs::remove_dir_all(&staging).await;
                return Ok(());
            }
            _=cancel.notified()=>{
                stop(&mut child)?;
                child.wait().await?;
                let _=tokio::fs::remove_dir_all(&staging).await;
                let lock=self.store.record_lock(record).await;let _guard=lock.lock().await;
                let mut meta=self.store.meta(record).await.map_err(|_|anyhow::anyhow!("record unavailable"))?;
                meta["status"]=json!("unconverted");meta["manual_conversion"]=json!(false);meta["auto_excluded"]=json!(true);meta["error"]=Value::Null;
                self.store.save_meta(meta).await.map_err(|_|anyhow::anyhow!("save cancellation failed"))?;
                return Ok(());
            }
        }
        drop(reservation);
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
        atomic_json(
            &folder.join(".conversion.json"),
            &json!({"generation":generation,"produced":produced,"carried":carried}),
        )
        .await?;
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
        if meta["cancel_requested"] == generation {
            meta["status"] = json!("unconverted");
            meta["auto_excluded"] = json!(true);
            meta["manual_conversion"] = json!(false);
            meta["error"] = Value::Null;
            self.store
                .save_meta(meta)
                .await
                .map_err(|_| anyhow::anyhow!("save conversion cancellation failed"))?;
            tokio::fs::remove_file(folder.join(".conversion.json")).await?;
            return Ok(());
        }
        let prepared = folder.join("prepared");
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
            tokio::fs::remove_file(folder.join(".conversion.json")).await?;
            return Ok(());
        }
        let journal_path = folder.join(".conversion.json");
        let journal: Value = read_json(&journal_path).await.unwrap_or(Value::Null);
        let installed = read_json::<String>(&prepared.join(".generation"))
            .await
            .ok()
            .as_deref()
            == Some(generation);
        if installed && meta["status"] == "ready" {
            // The metadata of this publication is durable: only the originals it lists remain to go.
            return self
                .finish_publication(&folder, &journal_path, &journal, &backup)
                .await;
        }
        if !installed {
            anyhow::ensure!(
                staging.join("summary.json").is_file(),
                "completed conversion staging is missing; old output preserved"
            );
            if prepared.exists() {
                tokio::fs::rename(&prepared, &backup).await?;
            }
            tokio::fs::rename(&staging, &prepared).await?;
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
                sync_dirs(&[&folder, &prepared]).await?;
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
        meta["video_downloads"] = download_entries(&folder).await?;
        apply_summary(&mut meta, &summary);
        meta["status"] = json!("ready");
        meta["error"] = Value::Null;
        meta["manual_conversion"] = json!(false);
        meta["decoder_version"] = json!("hars-v28-runtime-geometry");
        // Originals go only after the index that describes their absence (a promoted copy, a visible
        // MP4) is durable; until then every step above is redone the same on a restart. Once it is,
        // the journal is what still names them.
        let mut journal = journal;
        journal["generation"] = json!(generation);
        journal["originals"] = json!(originals);
        atomic_json(&journal_path, &journal).await?;
        self.store
            .save_meta(meta)
            .await
            .map_err(|_| anyhow::anyhow!("conversion metadata publication failed"))?;
        crate::fault::point("publish_meta_saved");
        self.finish_publication(&folder, &journal_path, &journal, &backup)
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
        let Ok(summary) = read_json::<Value>(&folder.join("prepared/summary.json")).await else {
            return Ok(false);
        };
        if meta["conversion_id"] != generation {
            return Ok(false);
        }
        apply_summary(&mut meta, &summary);
        let mut warnings = summary["warnings"].as_array().cloned().unwrap_or_default();
        warnings.push(warning.clone());
        meta["warnings"] = json!(warnings);
        meta["video_downloads"] = download_entries(&folder).await?;
        meta["status"] = json!("ready");
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
    let mut downloads = vec![];
    for (source, raw, mp4) in [
        ("qcamera", "qcamera.ts", "camera.mp4"),
        ("front", "fcamera.hevc", "fcamera.mp4"),
        ("wide", "ecamera.hevc", "ecamera.mp4"),
    ] {
        let retained = folder.join(mp4);
        let derived = folder.join("prepared").join(mp4);
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

    #[tokio::test]
    async fn conversion_admission_reserves_uncharged_starts_at_limit_sixteen() {
        let (_fixture, jobs) = open_jobs().await;
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut attempts = tokio::task::JoinSet::new();
        for number in 0..16 {
            let (jobs, barrier) = (jobs.clone(), barrier.clone());
            attempts.spawn(async move {
                barrier.wait().await;
                jobs.try_start(&number.to_string(), 16, async || {
                    tokio::task::yield_now().await;
                    Some(MEMORY_HEADROOM + MEMORY_PER_CONVERSION)
                })
                .await
            });
        }
        let mut admitted = Vec::new();
        while let Some(result) = attempts.join_next().await {
            admitted.extend(result.unwrap());
        }
        assert_eq!(admitted.len(), 1);
        assert_eq!(jobs.active.lock().await.len(), 1);
        drop(admitted); // The decoded data is now included in the measured availability.
        assert!(
            jobs.try_start("next", 1, async || panic!("at the configured limit"))
                .await
                .is_none()
        );
        assert!(jobs.try_start("next", 16, async || None).await.is_none());
        jobs.active.lock().await.clear();
        assert!(jobs.try_start("next", 16, async || None).await.is_some());
    }

    #[tokio::test]
    async fn conversion_admission_starts_fifth_when_waiting_phase_also_fits() {
        let (_fixture, jobs) = open_jobs().await;
        let available = AtomicU64::new(1861 * MIB);
        let samples = std::cell::Cell::new(0);
        let sample = async || {
            samples.set(samples.get() + 1);
            Some(available.load(Ordering::Relaxed))
        };
        let mut preparing = Vec::new();
        for record in ["first", "second", "third", "replay"] {
            preparing.push(jobs.try_start(record, 16, sample).await.unwrap());
        }
        drop(preparing.pop().unwrap()); // The Replay worker finished log preparation.
        assert_eq!(jobs.active.lock().await.len(), 4);

        // Three startups plus a granted Replay leave too little (1861 < 1886).
        let replay = jobs
            .reserve_phase(MemoryPhase::Replay, sample)
            .await
            .unwrap();
        assert!(jobs.try_start("fifth", 16, sample).await.is_none());
        drop(replay);

        // A mandatory Replay request sleeps at lower availability (1500 < 1566).
        available.store(1500 * MIB, Ordering::Relaxed);
        let replay = jobs.reserve_phase(MemoryPhase::Replay, sample);
        tokio::pin!(replay);
        tokio::select! {
            biased;
            _ = &mut replay => panic!("replay fits only after memory recovers"),
            _ = std::future::ready(()) => {}
        }
        available.store(1861 * MIB, Ordering::Relaxed);
        let before = samples.get();
        assert!(jobs.try_start("fifth", 16, sample).await.is_none());
        assert_eq!(
            samples.get(),
            before + 1,
            "must measure even while a phase waits"
        );

        // With one more startup settled, both fit (1861 >= 1566) with four active.
        drop(preparing.pop().unwrap());
        let fifth = jobs.try_start("fifth", 16, sample).await.unwrap();
        assert_eq!(jobs.active.lock().await.len(), 5);
        let replay = tokio::time::timeout(Duration::from_secs(2), replay)
            .await
            .unwrap()
            .unwrap();
        let phases = Arc::clone(&*jobs.memory.lock().await);
        assert_eq!(phases.bytes.load(Ordering::Relaxed), (3 * 320 + 350) * MIB);
        drop((fifth, replay, preparing));
    }

    #[tokio::test]
    async fn conversion_admission_coordinates_later_phases_and_recovers() {
        let (_fixture, jobs) = open_jobs().await;
        for record in ["camera", "replay"] {
            // Both jobs completed log reading. Their retained memory is in the sample.
            drop(
                jobs.try_start(record, 16, async || Some(1700 * MIB))
                    .await
                    .unwrap(),
            );
        }
        let available = AtomicU64::new(768 * MIB);
        let sample = async || Some(available.load(Ordering::Relaxed));
        let camera = jobs
            .reserve_phase(MemoryPhase::Video, sample)
            .await
            .unwrap();
        assert!(jobs.try_start("next", 16, sample).await.is_none());

        let replay = jobs.reserve_phase(MemoryPhase::Replay, sample);
        tokio::pin!(replay);
        tokio::select! {
            biased;
            _ = &mut replay => panic!("reused the camera's uncharged memory"),
            _ = std::future::ready(()) => {}
        }
        let threshold = MEMORY_HEADROOM + MemoryPhase::Replay.estimate();
        available.store(threshold - 1, Ordering::Relaxed);
        drop(camera);
        tokio::select! {
            biased;
            _ = &mut replay => panic!("approved below the replay threshold"),
            _ = std::future::ready(()) => {}
        }
        available.store(threshold, Ordering::Relaxed);
        let phases = Arc::clone(&*jobs.memory.lock().await);
        phases.changed.notify_waiters();
        let replay = replay.await.unwrap();

        available.store(768 * MIB, Ordering::Relaxed);
        assert!(
            jobs.reserve_phase(MemoryPhase::Thumbnail, sample)
                .await
                .is_none(),
            "reused the replay's reservation"
        );
        assert!(phases.waiting().is_empty());
        let cancel = jobs.active.lock().await.get("replay").unwrap().clone();
        let stopped = tokio::spawn(async move {
            cancel.notified().await;
            // The conversion owns the reservation until the killed child is reaped.
            drop(replay);
        });
        assert!(jobs.cancel("replay").await);
        stopped.await.unwrap();
        let thumbnail = jobs
            .reserve_phase(MemoryPhase::Thumbnail, sample)
            .await
            .unwrap();

        // With enough additional memory, the thumbnail can overlap another camera.
        available.store(1024 * MIB, Ordering::Relaxed);
        let camera = jobs
            .reserve_phase(MemoryPhase::Video, sample)
            .await
            .unwrap();
        assert_eq!(
            phases.bytes.load(Ordering::Relaxed),
            MemoryPhase::Thumbnail.estimate() + MemoryPhase::Video.estimate()
        );
        drop((thumbnail, camera));
        assert_eq!(phases.bytes.load(Ordering::Relaxed), 0);
    }
}
