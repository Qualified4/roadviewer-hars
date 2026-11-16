//! Python `media_store.Archiver`: moves (or copies) files of converted recordings to the archive, one verified copy
//! at a time. The archive is only touched outside the record locks; under a recording's lock its index is re-read,
//! committed (fsync'd) and only then is the local file removed. Split by concern: [`super::archiver_nas`] (the
//! share), [`super::archiver_remote`] (Drive), [`super::archiver_manual`] (the 'archive everything' run).
use super::{
    ArchiveRead, ArchiveWrite, DriveConnection, NasBackend, State, WriteBackend,
    archiver_manual::Transfer,
    index::{self, ARCHIVE_ORDER},
    settings::{self, ArchiveSettings, Loaded, Mode},
};
use crate::{jobs::Jobs, store::Store, upload::Uploads};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Python `FAILURES`: reasons that count as a failed cycle.
pub const FAILURES: [&str; 7] = [
    "storage_unavailable",
    "nas_full",
    "hash_mismatch",
    "os_error",
    "auth_expired",
    "rate_limited",
    "remote_full",
];
pub const CACHE_SECONDS: f64 = 10.0;

pub type Factory = Arc<dyn Fn(&ArchiveSettings, &Path) -> WriteBackend + Send + Sync>;

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The recording store, uploads and conversions the archiver works beside (Python's `server` module).
#[derive(Clone)]
pub struct Host {
    pub store: Arc<Store>,
    pub uploads: Arc<Uploads>,
    pub jobs: Arc<Jobs>,
}

impl Host {
    pub fn folder(&self, id: &str) -> PathBuf {
        self.store.root.join(id)
    }
    pub async fn ids(&self) -> Vec<String> {
        self.store.records.read().await.keys().cloned().collect()
    }
    pub async fn meta(&self, id: &str) -> Option<Value> {
        self.store.meta(id).await.ok().filter(Value::is_object)
    }
    pub async fn save(&self, meta: Value) -> std::io::Result<()> {
        self.store
            .save_meta(meta)
            .await
            .map_err(|e| std::io::Error::other(format!("index not saved: {}", e.1)))
    }
    pub async fn busy(&self, id: &str) -> bool {
        self.jobs.busy(id).await
    }
    pub async fn upload_receiving(&self) -> bool {
        !self.uploads.snapshot().await.1.is_null()
    }
    pub async fn used(&self) -> u64 {
        self.store.used().await.unwrap_or(0)
    }
    pub async fn pins(&self) -> BTreeSet<String> {
        crate::storage::load_settings(&self.store.root)
            .await
            .map(|s| crate::storage::pins(&s))
            .unwrap_or_default()
    }
    /// Python `stored`: on the archive by the index, present locally, or a local entry whose verified remote copy
    /// stands in for a local file that is gone.
    pub fn stored(&self, id: &str, meta: &Value, kind: &str) -> bool {
        let where_ = index::location(meta, kind);
        index::is_archived_location(&where_)
            || where_ == "local"
                && (self.folder(id).join(kind).is_file() || index::remote(meta, kind).is_some())
    }
    pub fn has_video(&self, id: &str, meta: &Value) -> bool {
        index::VIDEO_KINDS.iter().any(|k| self.stored(id, meta, k))
    }
}

/// What the active backend and its settings were when a cycle started; they stay fixed while it runs (settings
/// saves wait for the cycle lock).
#[derive(Clone)]
pub struct Ctx {
    pub settings: ArchiveSettings,
    pub backend: Arc<WriteBackend>,
}
impl Ctx {
    pub fn remote(&self) -> bool {
        self.settings.remote()
    }
    pub fn nas(&self) -> Option<&NasBackend> {
        match &*self.backend {
            WriteBackend::Nas(n) => Some(n),
            WriteBackend::Drive(_) => None,
        }
    }
    /// Whether archiving deletes the local original: the NAS always moves; Drive only in move mode with deleting allowed.
    pub fn moves_local(&self) -> bool {
        !self.remote() || self.settings.effective_mode() == Mode::Move
    }
}

pub(crate) struct Active {
    pub settings: ArchiveSettings,
    pub settings_error: Option<&'static str>,
    pub migrated_from: String,
    pub backend: Arc<WriteBackend>,
    pub connection: Option<Arc<DriveConnection>>,
}

#[derive(Default)]
pub(crate) struct Status {
    pub running: bool,
    pub last_cycle_at: Option<f64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u64,
    pub archived_files_total: u64,
    pub requeued_deferred: u64,
    pub removed_files_total: u64,
    pub cleanup_last_reason: Option<&'static str>,
    pub cleanup_error: Option<&'static str>,
    pub recovered: bool,
    pub held_checked: bool,
    pub rechecked_held: u64,
    pub missing: BTreeSet<(String, String)>,
    pub empty: BTreeSet<(String, String)>,
    pub cycle_failed: bool,
    pub remote_copies_total: u64,
    pub last_auth_error: Option<String>,
    pub needs_activation: bool,
    pub manual_active: bool,
    pub checking: Option<(u64, u64)>,
    pub startup_failure: Option<&'static str>,
}

/// A capacity goal from upload admission (Python `request_reclaim`): `extra` bytes over the targets.
#[derive(Clone, Debug, Default)]
pub struct Reclaim {
    pub extra: u64,
    /// The committing upload whose own reservation is not counted (Python `exclude`).
    pub exclude: Option<String>,
    pub incoming: Vec<String>,
    pub protected: BTreeSet<String>,
}

/// The result a cycle reports (Python's `result` dict).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CycleResult {
    pub archived: u64,
    pub bytes: u64,
    pub stopped: Option<String>,
}

/// One file's archive attempt: archived, abandoned because the recording changed meanwhile, or failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Archived,
    Changed,
    Failed(&'static str),
}

pub struct Archive {
    pub host: Host,
    pub root: PathBuf,
    pub(crate) options: Map<String, Value>,
    pub(crate) allow_any_path: bool,
    factory: RwLock<Factory>,
    pub(crate) active: RwLock<Active>,
    /// One cycle (scheduled, manual, capacity, recovery) or settings save at a time.
    pub cycle: Arc<tokio::sync::Mutex<()>>,
    pub(crate) st: Mutex<Status>,
    pub(crate) reclaim_request: Mutex<Option<Reclaim>>,
    pub(crate) settings_changed: AtomicBool,
    pub(crate) stop: AtomicBool,
    pub(crate) wake: tokio::sync::Notify,
    pub(crate) job: Mutex<Option<Value>>,
    pub(crate) transfer: Mutex<Option<Arc<Transfer>>>,
    pub(crate) index_cache: Mutex<Option<(Instant, [u64; 5])>>,
    started: AtomicBool,
    /// Device-flow polling unit (tests shorten Google's interval).
    connection_interval: Mutex<Duration>,
    me: Weak<Archive>,
}

pub fn default_factory() -> Factory {
    Arc::new(|s: &ArchiveSettings, root: &Path| match s.backend {
        settings::Backend::Gdrive => {
            let http =
                Arc::new(super::Http::new(super::HttpConfig::default()).expect("HTTP client"));
            WriteBackend::Drive(super::make_drive(
                drive_settings(s),
                root,
                &s.gdrive_client_id,
                s.gdrive_client_secret.clone(),
                http,
            ))
        }
        settings::Backend::Smb => WriteBackend::Nas(Arc::new(NasBackend::new(
            nas_settings(s),
            root,
            super::monotonic_clock(),
        ))),
    })
}
pub fn drive_settings(s: &ArchiveSettings) -> super::DriveSettings {
    super::DriveSettings {
        enabled: s.enabled,
        error: s.error.clone(),
        folder_name: s.gdrive_folder_name.clone(),
    }
}
pub fn nas_settings(s: &ArchiveSettings) -> super::NasSettings {
    super::NasSettings {
        enabled: s.enabled,
        error: s.error.clone(),
        path: s.path.clone(),
        allow_any_path: s.allow_any_path,
    }
}

impl Archive {
    /// Only constructed here: the archive is first touched by the scheduler (`activate`), so a hung mount cannot
    /// stall startup.
    pub fn open(
        host: Host,
        options: Map<String, Value>,
        allow_any_path: bool,
        factory: Option<Factory>,
    ) -> Arc<Self> {
        let root = host.store.root.clone();
        let Loaded {
            settings,
            migrated_from,
            error,
        } = settings::load(&root, &options, allow_any_path);
        if let Some(e) = error {
            tracing::warn!(reason = e, "archive_settings_disabled");
        }
        let factory = factory.unwrap_or_else(default_factory);
        let backend = Arc::new(factory(&settings, &root));
        let archive = Arc::new_cyclic(|me| Self {
            host,
            root: root.clone(),
            options,
            allow_any_path,
            factory: RwLock::new(factory),
            active: RwLock::new(Active {
                settings,
                settings_error: error,
                migrated_from,
                backend,
                connection: None,
            }),
            cycle: Arc::new(tokio::sync::Mutex::new(())),
            st: Mutex::new(Status::default()),
            reclaim_request: Mutex::default(),
            settings_changed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
            job: Mutex::new(None),
            transfer: Mutex::default(),
            index_cache: Mutex::default(),
            started: AtomicBool::new(false),
            connection_interval: Mutex::new(Duration::from_secs(1)),
            me: me.clone(),
        });
        *lock(&archive.job) = archive.load_job();
        *archive
            .host
            .store
            .archive
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(&archive);
        archive
    }

    pub(crate) fn arc(&self) -> Arc<Self> {
        self.me.upgrade().expect("archive alive")
    }

    pub fn settings(&self) -> ArchiveSettings {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settings
            .clone()
    }
    pub fn backend(&self) -> Arc<WriteBackend> {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .backend
            .clone()
    }
    pub fn ctx(&self) -> Ctx {
        let a = self
            .active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ctx {
            settings: a.settings.clone(),
            backend: a.backend.clone(),
        }
    }
    pub fn settings_error(&self) -> Option<&'static str> {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settings_error
    }

    /// Replaces how backends are built (tests point Drive at a loopback fake) and rebuilds the active one.
    pub fn set_factory(&self, factory: Factory) {
        let mut a = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        a.backend = Arc::new(factory(&a.settings, &self.root));
        a.connection = None;
        *self
            .factory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = factory;
        let mut st = lock(&self.st);
        st.needs_activation = true;
        st.recovered = false;
        st.held_checked = false;
    }

    /// Python's settings POST swap (caller holds the cycle lock): the backend is rebuilt when what it is bound to
    /// changed; checks done against the old one no longer count.
    pub(crate) fn swap_settings(&self, updated: ArchiveSettings) {
        let mut a = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old = &a.settings;
        let rebuild = (updated.backend, &updated.path, &updated.gdrive_folder_name)
            != (old.backend, &old.path, &old.gdrive_folder_name);
        let refresh = rebuild
            || updated.enabled != old.enabled
            || updated.error != old.error
            || updated.gdrive_client_id != old.gdrive_client_id;
        let enabled_now = updated.enabled && !old.enabled;
        if refresh {
            let factory = self
                .factory
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            a.backend = Arc::new(factory(&updated, &self.root));
            a.connection = None;
        }
        a.settings = updated;
        a.settings_error = None;
        drop(a);
        let mut st = lock(&self.st);
        if rebuild {
            st.recovered = false;
            st.held_checked = false;
            st.missing.clear();
            st.empty.clear();
            st.last_auth_error = None;
        }
        st.needs_activation |= rebuild || enabled_now || refresh;
        drop(st);
        *lock(&self.index_cache) = None;
        self.settings_changed.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    pub fn migrated_from(&self) -> String {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .migrated_from
            .clone()
    }

    /// The device-flow sign-in of the active Drive backend (None for the NAS), made on first use.
    pub fn connection(&self) -> Option<Arc<DriveConnection>> {
        let mut a = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let WriteBackend::Drive(drive) = &*a.backend else {
            return None;
        };
        if a.connection.is_none() {
            let me = self.me.clone();
            let on_change: super::drive::OnChange = Arc::new(move |_| {
                if let Some(me) = me.upgrade() {
                    lock(&me.st).last_auth_error = None;
                }
            });
            a.connection = Some(DriveConnection::with_interval_unit(
                drive.clone(),
                Some(on_change),
                *lock(&self.connection_interval),
            ));
        }
        a.connection.clone()
    }

    pub fn set_connection_interval(&self, unit: Duration) {
        *lock(&self.connection_interval) = unit;
        self.active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connection = None;
    }
    pub fn running(&self) -> bool {
        lock(&self.st).running
    }
    pub fn manual_active(&self) -> bool {
        lock(&self.st).manual_active
    }
    pub fn last_auth_error(&self) -> Option<String> {
        lock(&self.st).last_auth_error.clone()
    }
    pub fn forget_index(&self) {
        *lock(&self.index_cache) = None;
    }

    /// The latest probe result without probing (Python `archive_last_state`); None when stale.
    pub fn last_state(&self) -> Option<State> {
        let ctx = self.ctx();
        if !ctx.settings.enabled {
            return Some(State::Disabled);
        }
        ctx.backend.last_state()
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }
    pub(crate) fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    // --- Scheduling.

    /// Python `start`: the loop waits a full interval before activation and recovery.
    pub fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        lock(&self.st).needs_activation = true;
        let me = self.arc();
        tokio::spawn(async move { me.run_loop().await });
    }

    async fn run_loop(self: Arc<Self>) {
        let mut deadline = Instant::now() + Duration::from_secs(self.settings().interval_seconds);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(left.min(Duration::from_secs(1))) => {}
            }
            if self.stopping() {
                return;
            }
            if self.settings_changed.swap(false, Ordering::SeqCst) {
                deadline = Instant::now() + Duration::from_secs(self.settings().interval_seconds);
            }
            if Instant::now() < deadline {
                continue;
            }
            let _ = self.run_cycle().await;
            deadline = Instant::now() + Duration::from_secs(self.settings().interval_seconds);
        }
    }

    /// One scheduled cycle now (Python `run_cycle`): busy while another cycle runs.
    pub async fn run_cycle(&self) -> CycleResult {
        if !self.settings().enabled {
            return CycleResult {
                stopped: Some("disabled".into()),
                ..CycleResult::default()
            };
        }
        let Ok(guard) = self.cycle.clone().try_lock_owned() else {
            return CycleResult {
                stopped: Some("busy".into()),
                ..CycleResult::default()
            };
        };
        self.run_locked(guard).await
    }

    /// Python `start_cycle`: a capacity goal is published for the running cycle, or a cycle starts for it.
    pub fn start_cycle(&self, reclaim: Option<Reclaim>) -> bool {
        if let Some(r) = reclaim {
            *lock(&self.reclaim_request) = Some(r);
        }
        let Ok(guard) = self.cycle.clone().try_lock_owned() else {
            return false;
        };
        lock(&self.st).running = true;
        let me = self.arc();
        tokio::spawn(async move {
            me.run_locked(guard).await;
        });
        true
    }

    pub(crate) async fn run_locked(&self, guard: tokio::sync::OwnedMutexGuard<()>) -> CycleResult {
        let mut result = CycleResult::default();
        let ctx = self.ctx();
        if !ctx.settings.enabled {
            self.finish_job("disabled");
            result.stopped = Some("disabled".into());
            self.release(guard);
            return result;
        }
        lock(&self.st).running = true;
        let activate = std::mem::take(&mut lock(&self.st).needs_activation);
        if activate {
            self.activate(&ctx).await;
        }
        let reclaim;
        if self.manual_active() {
            result.stopped = Some(self.manual_cycle(&ctx, &mut result).await.to_owned());
            reclaim = None;
        } else {
            reclaim = lock(&self.reclaim_request).take();
            let stopped = match &reclaim {
                Some(r) => self.reclaim(&ctx, &mut result, r).await,
                None => self.cycle_once(&ctx, &mut result).await,
            };
            result.stopped = stopped.map(str::to_owned);
        }
        if reclaim.is_some() && !lock(&self.st).cycle_failed {
            let mut st = lock(&self.st);
            st.consecutive_failures = 0;
            st.last_error = None;
        }
        loop {
            self.forget_index();
            let next = lock(&self.reclaim_request).take();
            let Some(r) = next else { break };
            if result.archived >= ctx.settings.batch as u64 {
                result.stopped = Some("batch_limit".into());
                break;
            }
            result.stopped = self.reclaim(&ctx, &mut result, &r).await.map(str::to_owned);
            if !lock(&self.st).cycle_failed {
                let mut st = lock(&self.st);
                st.consecutive_failures = 0;
                st.last_error = None;
            }
        }
        self.release(guard);
        result
    }

    fn release(&self, guard: tokio::sync::OwnedMutexGuard<()>) {
        self.forget_index();
        lock(&self.st).running = false;
        drop(guard);
    }

    pub(crate) async fn activate(&self, ctx: &Ctx) {
        ctx.backend.activate().await;
        let probe = ctx.backend.probe(true).await;
        if probe.state != State::Connected {
            tracing::warn!(
                backend = ctx.settings.backend.as_str(),
                reason = probe.reason.as_deref().unwrap_or(""),
                "archive_not_connected"
            );
        }
    }

    pub(crate) async fn cycle_once(
        &self,
        ctx: &Ctx,
        result: &mut CycleResult,
    ) -> Option<&'static str> {
        if ctx.remote() {
            return self.remote_cycle(ctx, result).await;
        }
        lock(&self.st).cycle_failed = false;
        let nas = ctx.nas()?;
        if !self.nas_available(nas).await {
            return Some(self.failed("storage_unavailable"));
        }
        lock(&self.st).last_cycle_at = Some(crate::store::now());
        self.startup(ctx).await;
        // The share lost during the startup checks: nothing else is decided on it this cycle.
        if !self.nas_available(nas).await {
            return Some(self.failed("storage_unavailable"));
        }
        if super::pending::PendingDeletes::new(&self.root)
            .process_nas(nas)
            .await
            .is_err()
        {
            tracing::warn!("pending_archive_deletions_failed");
        }
        self.cleanup_nas(ctx).await;
        self.requeue_deferred().await;
        // Nothing more is moved to a share that is still over its limit.
        let cleanup_error = lock(&self.st).cleanup_error;
        if let Some(e) = cleanup_error {
            return Some(self.failed(e));
        }
        self.archive_pass(ctx, result).await
    }

    /// The common tail of a NAS or Drive cycle: uploads and conversions come first, then the local threshold.
    pub(crate) async fn archive_pass(
        &self,
        ctx: &Ctx,
        result: &mut CycleResult,
    ) -> Option<&'static str> {
        if self.host.upload_receiving().await {
            return Some("uploading");
        }
        if self.host.jobs_running().await {
            return Some("processing");
        }
        // A manual run archives everything eligible, whatever the local usage.
        if !self.manual_active() && self.host.used().await <= ctx.settings.local_threshold_bytes {
            return Some("below_threshold");
        }
        let reason = if ctx.remote() {
            self.archive_remote_files(ctx, result).await
        } else {
            self.archive_nas_files(ctx, result).await
        };
        if let Some(r) = reason
            && FAILURES.contains(&r)
        {
            return Some(self.failed(r));
        }
        // A recording the startup re-check could not process keeps its error until a cycle has none.
        let mut st = lock(&self.st);
        if !st.cycle_failed {
            st.consecutive_failures = 0;
            st.last_error = None;
        }
        reason
    }

    pub(crate) fn failed(&self, reason: &'static str) -> &'static str {
        let mut st = lock(&self.st);
        st.last_error = Some(reason.to_owned());
        if !st.cycle_failed {
            st.consecutive_failures += 1;
        }
        st.cycle_failed = true;
        reason
    }

    pub(crate) async fn nas_available(&self, nas: &NasBackend) -> bool {
        nas.probe(false).await.state == State::Connected
    }

    /// Python `candidates`: ready, idle recordings past the minimum age with a local file left to archive, oldest
    /// first. Pinned recordings are archived too: pinning protects from deletion, not from moving.
    pub(crate) async fn candidates(&self, ctx: &Ctx) -> Vec<(String, Value)> {
        let now = crate::store::now();
        let mut rows = Vec::new();
        for id in self.host.ids().await {
            let Some(m) = self.host.meta(&id).await else {
                continue;
            };
            if m["status"] != "ready" || self.host.busy(&id).await {
                continue;
            }
            let Some(uploaded) = m["uploaded"].as_f64() else {
                continue;
            };
            if now - uploaded < ctx.settings.min_age_seconds as f64 {
                continue;
            }
            let folder = self.host.folder(&id);
            if ARCHIVE_ORDER.iter().any(|k| {
                index::location(&m, k) == "local"
                    && folder.join(k).is_file()
                    && !(ctx.remote() && self.done_remotely(ctx, &m, k))
            }) {
                rows.push((id, m));
            }
        }
        rows.sort_by(|a, b| {
            a.1["uploaded"]
                .as_f64()
                .unwrap_or(0.)
                .total_cmp(&b.1["uploaded"].as_f64().unwrap_or(0.))
                .then_with(|| a.0.cmp(&b.0))
        });
        rows
    }

    /// Conversions deferred while the archive was unreachable, now that it answers (Python `requeue_deferred`).
    pub(crate) async fn requeue_deferred(&self) -> u64 {
        let automatic = self.host.store.processing.read().await["auto_convert"] == true;
        let mut rows = Vec::new();
        for id in self.host.ids().await {
            if let Some(m) = self.host.meta(&id).await
                && m["deferred"] == "storage"
                && m["status"] == "unconverted"
                && m["auto_excluded"] != true
                && (automatic || m["manual_conversion"] == true)
            {
                rows.push((m["uploaded"].as_f64().unwrap_or(0.), id));
            }
        }
        rows.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut queued = 0;
        for (_, id) in rows {
            let lock_ = self.host.store.record_lock(&id).await;
            let _g = lock_.lock().await;
            let Some(mut m) = self.host.meta(&id).await else {
                continue;
            };
            if m["deferred"] != "storage" || m["status"] != "unconverted" {
                continue;
            }
            m["status"] = json!("queued");
            m["error"] = Value::Null;
            m["queued_at"] = json!(crate::store::now());
            if let Some(o) = m.as_object_mut() {
                o.remove("deferred");
            }
            // One recording that cannot be saved does not hold back the others; it stays deferred.
            if self.host.save(m).await.is_ok() {
                queued += 1;
            }
        }
        lock(&self.st).requeued_deferred += queued;
        queued
    }

    /// Python `_startup`: crash recovery until it completes once with the archive reachable, then the files it did
    /// not find are looked for again each connected cycle.
    pub(crate) async fn startup(&self, ctx: &Ctx) {
        lock(&self.st).startup_failure = None;
        if !self.connected(ctx).await {
            return;
        }
        let (recovered, missing) = {
            let mut st = lock(&self.st);
            // HARS has no results held back at startup for archived inputs (Python `requeue_startup`), so the
            // held re-check has nothing to do.
            st.held_checked = true;
            (st.recovered, !st.missing.is_empty())
        };
        if recovered && !missing {
            return;
        }
        let total = if recovered {
            0
        } else {
            self.host.ids().await.len() as u64
        };
        lock(&self.st).checking = (total > 0).then_some((0, total));
        if ctx.remote() {
            if !recovered {
                self.recover_remote(ctx).await;
            } else {
                self.recheck_missing_remote(ctx).await;
            }
        } else if !recovered {
            self.recover_nas(ctx).await;
        } else {
            self.recheck_missing_nas(ctx).await;
        }
        lock(&self.st).checking = None;
    }

    pub(crate) fn checked(&self) {
        let mut st = lock(&self.st);
        if let Some((done, total)) = st.checking {
            st.checking = Some(((done + 1).min(total), total));
        }
    }

    pub(crate) async fn connected(&self, ctx: &Ctx) -> bool {
        ctx.backend.probe(true).await.state == State::Connected
    }

    /// After a crash (Python `recover_startup`): runs the startup checks now with the cycle lock.
    pub async fn recover_startup(&self) {
        let ctx = self.ctx();
        if !ctx.settings.enabled {
            return;
        }
        let _guard = self.cycle.lock().await;
        if std::mem::take(&mut lock(&self.st).needs_activation) {
            self.activate(&ctx).await;
        }
        self.startup(&ctx).await;
    }

    pub(crate) fn note_missing(&self, id: &str, kind: &str) {
        let mut st = lock(&self.st);
        if st.missing.insert((id.to_owned(), kind.to_owned())) {
            tracing::warn!(kind, "archived_file_missing_index_kept");
        }
    }

    /// Re-reads the recording under its lock; `check` decides on the current index and may change it, returning
    /// Some(value) to save it. None from check (or a gone recording) saves nothing.
    pub(crate) async fn with_meta<T>(
        &self,
        id: &str,
        check: impl FnOnce(&mut Value) -> Option<T>,
    ) -> Option<std::io::Result<T>> {
        let l = self.host.store.record_lock(id).await;
        let _g = l.lock().await;
        let mut current = self.host.meta(id).await?;
        let out = check(&mut current)?;
        Some(self.host.save(current).await.map(|()| out))
    }

    /// Python `status()`.
    pub fn status(&self) -> Value {
        let ctx = self.ctx();
        let job = self.job_status();
        let st = lock(&self.st);
        json!({
            "last_cycle_at": st.last_cycle_at, "last_error": st.last_error, "consecutive_failures": st.consecutive_failures,
            "running": st.running, "archived_files_total": st.archived_files_total, "requeued_deferred": st.requeued_deferred,
            "removed_files_total": st.removed_files_total, "cleanup_last_reason": st.cleanup_last_reason,
            "rechecked_held": st.rechecked_held, "missing_archived": st.missing.len(),
            "backend": ctx.settings.backend.as_str(), "effective_mode": ctx.settings.effective_mode().as_str(),
            "remote_copies_total": st.remote_copies_total, "last_auth_error": st.last_auth_error,
            "manual": job, "manual_active": st.manual_active,
            "checking": st.checking.map(|(d, t)| json!({"done": d, "total": t})),
        })
    }

    /// The index totals of every recording: [archived files, archived bytes, local files with a remote copy, their
    /// bytes, files whose copy did not verify]; reused for CACHE_SECONDS (the settings panel polls).
    pub async fn index_totals(&self) -> [u64; 5] {
        if let Some((at, totals)) = *lock(&self.index_cache)
            && at.elapsed().as_secs_f64() < CACHE_SECONDS
        {
            return totals;
        }
        let mut t = [0u64; 5];
        for m in self.host.store.records.read().await.values() {
            t[0] += index::archived_files(m);
            t[1] += index::archived_bytes(m);
            let (n, b) = index::remote_copy_totals(m);
            t[2] += n;
            t[3] += b;
            t[4] += index::mismatch_count(m);
        }
        *lock(&self.index_cache) = Some((Instant::now(), t));
        t
    }
}

impl Host {
    pub async fn jobs_running(&self) -> bool {
        for id in self.ids().await {
            if self.busy(&id).await {
                return true;
            }
        }
        false
    }
}

/// sha256 of a file on a blocking thread.
pub(crate) async fn file_sha256(path: &Path) -> std::io::Result<String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use sha2::Digest;
        let mut f = std::fs::File::open(path)?;
        let mut h = sha2::Sha256::new();
        std::io::copy(&mut f, &mut h)?;
        Ok(hex::encode(h.finalize()))
    })
    .await
    .map_err(std::io::Error::other)?
}
/// sha256 and md5 in one read.
pub(crate) async fn file_digests(path: &Path) -> std::io::Result<(String, String)> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use md5::Md5;
        use sha2::Digest;
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let (mut a, mut b) = (sha2::Sha256::new(), Md5::new());
        let mut buf = vec![0; super::CHUNK];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            a.update(&buf[..n]);
            b.update(&buf[..n]);
        }
        Ok((hex::encode(a.finalize()), hex::encode(b.finalize())))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// (size, mtime_ns) of a local file.
pub(crate) fn stat(path: &Path) -> std::io::Result<(u64, u64)> {
    let m = std::fs::metadata(path)?;
    let mtime = m
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    Ok((m.len(), mtime))
}

/// A share path's metadata, read off the runtime's workers: a hung mount holds a blocking thread, not a worker.
pub(crate) async fn share_stat(path: &Path) -> std::io::Result<std::fs::Metadata> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || std::fs::metadata(path))
        .await
        .unwrap_or_else(|_| Err(std::io::Error::other("join")))
}
pub(crate) async fn share_remove(path: &Path) -> std::io::Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || std::fs::remove_file(path))
        .await
        .unwrap_or_else(|_| Err(std::io::Error::other("join")))
}

pub(crate) fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

pub(crate) fn kind_static(kind: &str) -> &'static str {
    super::MEDIA_KINDS
        .iter()
        .find(|k| **k == kind)
        .copied()
        .unwrap_or("rlog.zst")
}
