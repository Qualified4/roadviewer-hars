//! Prepared generations live beside the local recording or in an owned, mounted share.
use crate::{
    archive::{self, State, nas},
    error::{Result, bad, conflict},
    store::{Store, atomic_json, id, read_json, valid_id},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Mutex, Semaphore, watch},
    time::{Instant, timeout},
};

const CONFIG: &str = ".prepared-storage.json";
const NAMESPACE: &str = ".roadviewer-prepared";
const OWNER: &str = "roadviewer-hars-prepared";
pub(crate) const PROBE: Duration = Duration::from_secs(3);
const TRACK_READ: Duration = Duration::from_secs(30);
const PLAYBACK: Duration = Duration::from_secs(45);
pub const MOVE_JOURNAL: &str = ".prepared-move.json";
const COPY_IDLE: Duration = Duration::from_secs(60);
pub const WAITING: &str = "prepared_storage";
pub const WAIT_MESSAGE: &str = "재생 데이터 NAS 연결을 기다리는 중입니다.";

type Sizes = BTreeMap<String, u64>;
type Digest = [u8; 32];

#[derive(Clone, Default, Deserialize, Serialize)]
struct Integrity {
    sizes: Sizes,
    sha256: BTreeMap<String, Digest>,
}

#[derive(Default)]
pub(crate) struct Manifest {
    integrity: Integrity,
    thumbnail: Value,
    summary: Value,
}

impl Manifest {
    fn read(path: &Path, integrity: Integrity) -> Self {
        let json_file = |name: &str| {
            nas::open_share_file(&path.join(name))
                .ok()
                .and_then(|f| serde_json::from_reader(std::io::BufReader::new(f)).ok())
                .unwrap_or(Value::Null)
        };
        Self {
            integrity,
            thumbnail: json_file("thumbnail.json"),
            summary: json_file("summary.json"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NasLocation {
    pub share: PathBuf,
    pub generation: String,
}

pub fn location(meta: &Value) -> io::Result<Option<NasLocation>> {
    meta.get("prepared_nas")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(io::Error::other)
}

pub fn size(meta: &Value, name: &str) -> Option<u64> {
    meta["prepared_files"][name].as_u64()
}

pub fn total(meta: &Value) -> u64 {
    meta["prepared_files"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, size)| size.as_u64())
        .sum()
}

pub(crate) fn set_location(meta: &mut Value, target: Option<&NasLocation>, manifest: Manifest) {
    if let Some(target) = target {
        meta["prepared_nas"] = json!(target);
        meta["prepared_files"] = json!(manifest.integrity.sizes);
        meta["prepared_thumbnail"] = manifest.thumbnail;
        if meta["prepared_stale"] != true {
            for key in crate::listing::LIST_FIELDS.into_iter().chain(["videos"]) {
                if let Some(value) = manifest.summary.get(key).filter(|v| !v.is_null()) {
                    meta[key] = value.clone();
                }
            }
        }
    } else if let Some(map) = meta.as_object_mut() {
        for key in ["prepared_nas", "prepared_files", "prepared_thumbnail"] {
            map.remove(key);
        }
    }
    meta.as_object_mut().unwrap().remove("prepared_stale");
}

#[derive(Clone, Deserialize, Serialize)]
struct Settings {
    storage_id: String,
    nas: bool,
    path: Option<PathBuf>,
    claimed: BTreeSet<PathBuf>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            storage_id: id(),
            nas: false,
            path: None,
            claimed: BTreeSet::new(),
        }
    }
}

pub struct Prepared {
    settings: Mutex<Settings>,
    configuration: Mutex<()>,
    allow_any_path: bool,
    // Blocked syscalls keep their slots even when HTTP has timed out.
    readers: Arc<Semaphore>,
    writer: Arc<Semaphore>,
    track_queries: Arc<Semaphore>,
    playback: Mutex<BTreeMap<String, (Instant, Arc<()>)>>,
    migration: Mutex<Value>,
}

fn unavailable() -> io::Error {
    io::Error::new(ErrorKind::NotConnected, "prepared_storage_unavailable")
}
pub(crate) fn api_error(e: io::Error) -> crate::error::ApiError {
    match e.kind() {
        ErrorKind::NotConnected => crate::error::ApiError(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "prepared_storage_unavailable",
        ),
        ErrorKind::NotFound => crate::error::missing("not_found"),
        _ => e.into(),
    }
}

impl Prepared {
    pub async fn open(root: &Path) -> io::Result<Self> {
        let settings = match read_json(&root.join(CONFIG)).await {
            Ok(settings) => settings,
            Err(e) if e.kind() == ErrorKind::NotFound => Settings::default(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            settings: Mutex::new(settings),
            configuration: Mutex::new(()),
            allow_any_path: archive::settings::allow_any_path(),
            readers: Arc::new(Semaphore::new(8)),
            writer: Arc::new(Semaphore::new(1)),
            track_queries: Arc::new(Semaphore::new(2)),
            playback: Mutex::default(),
            migration: Mutex::new(json!({"state":"idle"})),
        })
    }

    fn share(&self, path: PathBuf, settings: &Settings) -> Share {
        Share {
            path,
            storage_id: settings.storage_id.clone(),
            allow_any_path: self.allow_any_path,
        }
    }

    pub async fn configure(&self, root: &Path, data: &Value) -> Result<()> {
        let _guard = self.configuration.lock().await;
        if self.migration.lock().await["state"] == "running" {
            return Err(conflict("prepared_migration_running"));
        }
        let mut next = self.settings.lock().await.clone();
        next.nas = match data["location"].as_str() {
            Some("local") => false,
            Some("nas") => true,
            _ => return Err(bad("invalid_prepared_location")),
        };
        if next.nas {
            let path = PathBuf::from(
                data["path"]
                    .as_str()
                    .map(str::trim)
                    .filter(|p| nas::valid_path(p, self.allow_any_path))
                    .ok_or_else(|| bad("invalid_prepared_path"))?,
            );
            next.path = Some(path.clone());
            if !next.claimed.contains(&path) {
                // Persist our ID and intended path before writing its marker. A restart can
                // recognize the same marker even if it stopped before recording the claim.
                atomic_json(&root.join(CONFIG), &next).await?;
                let share = self.share(path.clone(), &next);
                if self.quick(move || share.claim()).await.is_ok() {
                    next.claimed.insert(path);
                }
            }
        }
        atomic_json(&root.join(CONFIG), &next).await?;
        *self.settings.lock().await = next;
        Ok(())
    }

    pub async fn snapshot(&self, store: &Store) -> Value {
        let settings = self.settings.lock().await.clone();
        let bytes: u64 = store.records.read().await.values().map(total).sum();
        let state = match settings.path.clone() {
            None => "disabled",
            Some(path) => {
                let share = self.share(path, &settings);
                if self.quick(move || share.owned().map(|_| ())).await.is_ok() {
                    "connected"
                } else {
                    "unavailable"
                }
            }
        };
        json!({"location":if settings.nas {"nas"} else {"local"}, "path":settings.path, "state":state, "nas_bytes":bytes})
    }

    pub async fn target(&self, generation: &str) -> io::Result<Option<NasLocation>> {
        let settings = self.settings.lock().await;
        if !settings.nas {
            return Ok(None);
        }
        Ok(Some(NasLocation {
            share: settings.path.clone().ok_or_else(unavailable)?,
            generation: generation.to_owned(),
        }))
    }

    pub async fn check(&self, target: &NasLocation) -> io::Result<()> {
        let share = self.share(target.share.clone(), &*self.settings.lock().await);
        self.quick(move || share.owned().map(|_| ())).await
    }

    pub(crate) fn bounded<T: Send + 'static>(
        &self,
        op: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<tokio::task::JoinHandle<io::Result<T>>> {
        let slot = self
            .readers
            .clone()
            .try_acquire_owned()
            .map_err(|_| unavailable())?;
        Ok(tokio::task::spawn_blocking(move || {
            let _slot = slot;
            op()
        }))
    }

    pub(crate) async fn quick<T: Send + 'static>(
        &self,
        op: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        timeout(PROBE, self.bounded(op)?)
            .await
            .map_err(|_| unavailable())?
            .map_err(io::Error::other)?
    }

    pub(crate) async fn run_track_query<T: Send + 'static>(
        &self,
        op: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        timeout(TRACK_READ, async {
            let slot = self
                .track_queries
                .clone()
                .acquire_owned()
                .await
                .map_err(io::Error::other)?;
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                op()
            })
            .await
            .map_err(io::Error::other)?
        })
        .await
        .map_err(|_| unavailable())?
    }

    pub(crate) async fn run_track_warm<T: Send + 'static>(
        &self,
        op: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<Option<T>> {
        // Yield whenever a user track query is active so prewarming never competes with live requests.
        if self.track_queries.available_permits() < 2 {
            return Ok(None);
        }
        let Ok(slot) = self.track_queries.clone().try_acquire_owned() else {
            return Ok(None);
        };
        timeout(TRACK_READ, async move {
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                op()
            })
            .await
            .map_err(io::Error::other)?
        })
        .await
        .map_err(|_| unavailable())?
        .map(Some)
    }

    pub(crate) fn read_slot(&self) -> io::Result<tokio::sync::OwnedSemaphorePermit> {
        self.readers
            .clone()
            .try_acquire_owned()
            .map_err(|_| unavailable())
    }

    pub(crate) async fn share_for(&self, target: &NasLocation) -> Share {
        self.share(target.share.clone(), &*self.settings.lock().await)
    }

    pub async fn read_dir(&self, folder: &Path, meta: &Value) -> io::Result<PathBuf> {
        let Some(location) = location(meta)? else {
            return Ok(folder.join("prepared"));
        };
        let share = self.share_for(&location).await;
        let record = recording_id(folder)?.to_owned();
        self.quick(move || share.read_dir(&record, &location.generation))
            .await
    }

    pub(crate) async fn track_shares(
        &self,
        metas: &[Value],
    ) -> io::Result<BTreeMap<String, (NasLocation, Share)>> {
        let settings = self.settings.lock().await.clone();
        metas
            .iter()
            .filter(|m| m["status"] == "ready")
            .filter_map(|meta| {
                let record = meta["id"].as_str()?;
                match location(meta) {
                    Ok(Some(target)) => Some(Ok((
                        record.to_owned(),
                        (target.clone(), self.share(target.share, &settings)),
                    ))),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                }
            })
            .collect()
    }

    pub async fn touch_playback(&self, id: &str) -> Arc<()> {
        self.hold_location(id, PLAYBACK).await
    }

    pub(crate) async fn hold_location(&self, id: &str, linger: Duration) -> Arc<()> {
        let mut active = self.playback.lock().await;
        let now = Instant::now();
        active.retain(|_, (until, stream)| *until > now || Arc::strong_count(stream) > 1);
        let (until, stream) = active
            .entry(id.to_owned())
            .or_insert_with(|| (now, Arc::new(())));
        *until = (*until).max(now + linger);
        Arc::clone(stream)
    }

    async fn playing(&self, id: &str) -> bool {
        self.playback
            .lock()
            .await
            .get(id)
            .is_some_and(|(until, stream)| *until > Instant::now() || Arc::strong_count(stream) > 1)
    }

    pub(crate) async fn open_file(
        &self,
        target: &NasLocation,
        record: &str,
        name: String,
    ) -> io::Result<File> {
        let share = self.share_for(target).await;
        let record = record.to_owned();
        let generation = target.generation.clone();
        self.quick(move || {
            let pin = share.owned()?;
            let parent = pin.record(&record, false)?;
            let dir = parent.generation(&generation)?;
            checked_dir(&dir, false)?;
            nas::open_share_file(&dir.join(name))
        })
        .await
    }

    /// Local conversion stages and the archived source survive until the caller commits metadata.
    pub(crate) async fn install(
        &self,
        folder: &Path,
        source: &Path,
        target: &NasLocation,
    ) -> io::Result<Manifest> {
        let record = recording_id(folder)?.to_owned();
        let share = self.share_for(target).await;
        let generation = target.generation.clone();
        let source = source.to_owned();
        self.write(move |progress| {
            let pin = share.owned()?;
            progress.touch()?;
            let parent = pin.record(&record, true)?;
            let dir = parent.generation(&generation)?;
            let still_owned = || pin.confirm(&share.owned()?);
            let result = copy_generation(&source, &dir, &generation, &progress, still_owned);
            still_owned()?;
            Ok(Manifest::read(&source, result?))
        })
        .await
    }

    async fn write<T: Send + 'static>(
        &self,
        op: impl FnOnce(CopyProgress) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let slot = timeout(COPY_IDLE, self.writer.clone().acquire_owned())
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        let (activity, mut progress) = watch::channel(());
        let mut copy = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            op(CopyProgress(activity))
        });
        loop {
            tokio::select! {
                result = &mut copy => return result.map_err(io::Error::other)?,
                changed = progress.changed() => if changed.is_err() {
                    return copy.await.map_err(io::Error::other)?;
                },
                _ = tokio::time::sleep(COPY_IDLE) => return Err(unavailable()),
            }
        }
    }

    pub(crate) async fn remove_nas(&self, record: &str, target: &NasLocation) -> io::Result<()> {
        let share = self.share_for(target).await;
        let record = record.to_owned();
        let generation = target.generation.clone();
        self.quick(move || share.remove(&record, &generation)).await
    }

    /// Deletion already committed locally. Keep its hidden metadata until remote cleanup succeeds.
    pub(crate) async fn purge_hidden(&self, folder: &Path, record: &str) -> io::Result<()> {
        let meta: Value = match read_json(&folder.join("meta.json")).await {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut remote = location(&meta)?.into_iter().collect::<Vec<_>>();
        if let Some(work) = read_move(folder).await? {
            remote.extend([work.source, work.target].into_iter().flatten());
        }
        if let Ok(work) = read_json::<Value>(&folder.join(".conversion.json")).await {
            for key in ["prepared_previous", "prepared_target"] {
                if let Some(v) = work.get(key).filter(|v| !v.is_null()) {
                    remote.push(serde_json::from_value(v.clone())?);
                }
            }
        }
        for target in remote {
            // Deletion recovery must return promptly even while holding the registration lock.
            self.remove_nas(record, &target).await?;
        }
        Ok(())
    }

    pub async fn migration(&self) -> Value {
        self.migration.lock().await.clone()
    }

    pub async fn start_move(store: Arc<Store>, to_nas: bool) -> Result<Value> {
        let control = store.prepared.configuration.lock().await;
        let mut status = store.prepared.migration.lock().await;
        if status["state"] == "running" {
            return Err(conflict("prepared_migration_running"));
        }
        let settings = store.prepared.settings.lock().await.clone();
        let target = if to_nas {
            Some(settings.path.ok_or_else(|| bad("invalid_prepared_path"))?)
        } else {
            None
        };
        let records: Vec<_> = store.records.read().await.keys().cloned().collect();
        *status = json!({"state":"running","direction":if to_nas {"nas"} else {"local"},"total":records.len(),"moved":0,"skipped":0,"failed":0});
        let response = status.clone();
        drop(status);
        drop(control);
        tokio::spawn(async move {
            let mut failed = false;
            for record in records {
                let result = store
                    .prepared
                    .move_one(&store, &record, target.as_deref())
                    .await;
                let mut status = store.prepared.migration.lock().await;
                let key = match result {
                    Ok(MoveResult::Moved) => "moved",
                    Ok(MoveResult::Skipped) => "skipped",
                    Ok(MoveResult::Unchanged) => continue,
                    Err(e) => {
                        tracing::warn!(%record, kind=?e.kind(), "prepared_migration_failed");
                        failed = true;
                        status["error"] = json!(if e.kind() == ErrorKind::NotConnected {
                            "prepared_storage_unavailable"
                        } else {
                            "prepared_move_failed"
                        });
                        "failed"
                    }
                };
                status[key] = json!(status[key].as_u64().unwrap_or(0) + 1);
                if status["error"] == "prepared_storage_unavailable" {
                    break;
                }
            }
            store.prepared.migration.lock().await["state"] =
                json!(if failed { "failed" } else { "finished" });
        });
        Ok(response)
    }

    async fn move_one(
        &self,
        store: &Store,
        record: &str,
        to: Option<&Path>,
    ) -> io::Result<MoveResult> {
        let Ok(_guard) = store.record_lock(record).await.try_lock_owned() else {
            return Ok(MoveResult::Skipped);
        };
        let Ok(mut meta) = store.meta(record).await else {
            return Ok(MoveResult::Unchanged);
        };
        let folder = store.root.join(record);
        if self.playing(record).await
            || matches!(meta["status"].as_str(), Some("processing" | "queued"))
            || folder.join(".conversion.json").exists()
        {
            return Ok(MoveResult::Skipped);
        }
        let current = location(&meta)?;
        let pending = read_move(&folder).await?;
        if pending.is_none()
            && (meta["status"] != "ready" || current.is_none() && !folder.join("prepared").is_dir())
        {
            return Ok(MoveResult::Unchanged);
        }
        if let Some(work) = pending {
            self.complete_move(store, &folder, &mut meta, &work).await?;
            if work.target.as_ref().map(|l| l.share.as_path()) == to {
                return Ok(MoveResult::Moved);
            }
        } else if current.as_ref().map(|l| l.share.as_path()) == to {
            return Ok(MoveResult::Unchanged);
        }
        if to.is_some() && location(&meta)?.is_some() {
            // The supported directions are local to NAS and NAS to local. The latter
            // also works after the configured path has changed.
            return Ok(MoveResult::Skipped);
        }
        let generation = id();
        let work = Move {
            source: location(&meta)?,
            target: to.map(|share| NasLocation {
                share: share.to_owned(),
                generation: generation.clone(),
            }),
            generation,
            verified: None,
        };
        atomic_json(&folder.join(MOVE_JOURNAL), &work).await?;
        self.complete_move(store, &folder, &mut meta, &work).await?;
        Ok(MoveResult::Moved)
    }

    async fn complete_move(
        &self,
        store: &Store,
        folder: &Path,
        meta: &mut Value,
        work: &Move,
    ) -> io::Result<()> {
        let record = recording_id(folder)?;
        if !valid_id(&work.generation) {
            return Err(io::Error::other("invalid_prepared_move"));
        }
        if location(meta)? == work.target {
            // After an interruption the original may be partially deleted. Verify against
            // the durable manifest, including when that original no longer exists.
            let integrity = work
                .verified
                .clone()
                .ok_or_else(|| io::Error::other("prepared_verification_missing"))?;
            self.verify_destination(folder, work.target.as_ref(), integrity)
                .await?;
        } else {
            if location(meta)? != work.source {
                return Err(io::Error::other("prepared_move_conflict"));
            }
            let manifest = match (&work.source, &work.target) {
                (None, Some(target)) => {
                    self.install(folder, &folder.join("prepared"), target)
                        .await?
                }
                (Some(source), None) => {
                    let share = self.share_for(source).await;
                    let source = source.clone();
                    let folder = folder.to_owned();
                    let record = record.to_owned();
                    let generation = work.generation.clone();
                    self.write(move |progress| {
                        let pin = share.owned()?;
                        let parent = pin.record(&record, false)?;
                        let dir = parent.generation(&source.generation)?;
                        let dest = folder.join("prepared");
                        // A local remnant from an older generation is never adopted on name alone.
                        let still_owned = || pin.confirm(&share.owned()?);
                        let result =
                            copy_generation(&dir, &dest, &generation, &progress, still_owned);
                        still_owned()?;
                        Ok(Manifest::read(&dest, result?))
                    })
                    .await?
                }
                _ => return Err(io::Error::other("invalid_prepared_move")),
            };
            let mut verified = work.clone();
            verified.verified = Some(manifest.integrity.clone());
            atomic_json(&folder.join(MOVE_JOURNAL), &verified).await?;
            let stale = meta["prepared_stale"] == true;
            set_location(meta, work.target.as_ref(), manifest);
            if stale {
                meta["prepared_stale"] = json!(true);
            }
            store
                .save_meta(meta.clone())
                .await
                .map_err(|_| io::Error::other("prepared_metadata_save"))?;
            crate::fault::point("prepared_migration_committed");
            if let Some(target) = &work.target {
                self.check(target).await?;
            }
        }
        // The old copy can now go. A restart after metadata commit re-enters only this phase.
        if let Some(source) = &work.source {
            self.remove_nas(record, source).await?;
        } else {
            remove_local(folder.join("prepared")).await?;
        }
        tokio::fs::remove_file(folder.join(MOVE_JOURNAL)).await?;
        Ok(())
    }

    async fn verify_destination(
        &self,
        folder: &Path,
        target: Option<&NasLocation>,
        integrity: Integrity,
    ) -> io::Result<()> {
        let local = folder.join("prepared");
        let nas = if let Some(target) = target {
            Some((
                self.share_for(target).await,
                recording_id(folder)?.to_owned(),
                target.generation.clone(),
            ))
        } else {
            None
        };
        self.write(move |progress| {
            let mut buffer = vec![0; archive::CHUNK];
            if let Some((share, record, generation)) = nas {
                let pin = share.owned()?;
                let parent = pin.record(&record, false)?;
                verify(
                    &parent.generation(&generation)?,
                    &integrity,
                    &progress,
                    &mut buffer,
                )?;
                pin.confirm(&share.owned()?)
            } else {
                verify(&local, &integrity, &progress, &mut buffer)
            }
        })
        .await
    }

    pub(crate) async fn finish_pending(
        &self,
        store: &Store,
        folder: &Path,
        meta: &mut Value,
    ) -> io::Result<()> {
        if let Some(work) = read_move(folder).await? {
            self.complete_move(store, folder, meta, &work).await?;
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Move {
    source: Option<NasLocation>,
    target: Option<NasLocation>,
    generation: String,
    verified: Option<Integrity>,
}

enum MoveResult {
    Moved,
    Skipped,
    Unchanged,
}

async fn read_move(folder: &Path) -> io::Result<Option<Move>> {
    match read_json(&folder.join(MOVE_JOURNAL)).await {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) async fn remove_local(path: PathBuf) -> io::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn recording_id(folder: &Path) -> io::Result<&str> {
    folder
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|id| valid_id(id))
        .ok_or_else(|| io::Error::other("invalid_prepared_record"))
}

#[derive(Clone)]
pub(crate) struct Share {
    path: PathBuf,
    storage_id: String,
    allow_any_path: bool,
}

struct Pinned {
    dir: File,
    path: PathBuf,
}

impl Pinned {
    fn open(path: &Path) -> io::Result<Self> {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
        }
        let dir = options.open(path)?;
        #[cfg(target_os = "linux")]
        let path = {
            use std::os::fd::AsRawFd;
            // A lazy unmount cannot redirect a write to the underlying local directory.
            PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
        };
        #[cfg(not(target_os = "linux"))]
        let path = path.to_owned();
        Ok(Self { dir, path })
    }

    fn confirm(&self, other: &Self) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = self.dir.metadata()?;
            let b = other.dir.metadata()?;
            if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
                return Err(unavailable());
            }
        }
        Ok(())
    }

    fn child(&self, name: &str, create: bool) -> io::Result<Self> {
        let path = self.path.join(name);
        if create {
            checked_dir(&path, true)?;
        }
        Self::open(&path)
    }

    fn record(&self, record: &str, create: bool) -> io::Result<Self> {
        if !valid_id(record) {
            return Err(io::Error::other("invalid_prepared_record"));
        }
        self.child(record, create)
    }

    fn generation(&self, generation: &str) -> io::Result<PathBuf> {
        if !valid_id(generation) {
            return Err(io::Error::other("invalid_prepared_generation"));
        }
        Ok(self.path.join(generation))
    }
}

impl Share {
    fn pin(&self) -> io::Result<Pinned> {
        let check_mount = || {
            self.path
                .to_str()
                .is_some_and(|p| nas::valid_path(p, self.allow_any_path))
                && nas::mounted(&self.path, self.allow_any_path)
                && fs::canonicalize(&self.path).ok().as_deref() == Some(self.path.as_path())
        };
        if !check_mount() {
            return Err(unavailable());
        }
        let pin = Pinned::open(&self.path).map_err(|_| unavailable())?;
        if !check_mount() {
            return Err(unavailable());
        }
        pin.confirm(&Pinned::open(&self.path).map_err(|_| unavailable())?)?;
        Ok(pin)
    }

    fn owned(&self) -> io::Result<Pinned> {
        let root = self
            .pin()?
            .child(NAMESPACE, false)
            .map_err(|_| unavailable())?;
        self.check_marker(&root.path)?;
        Ok(root)
    }

    fn check_marker(&self, root: &Path) -> io::Result<()> {
        if !fs::symlink_metadata(root.join(archive::SENTINEL)).is_ok_and(|m| m.is_file())
            || nas::probe_sync(Some(root), Some(self.storage_id.clone()), Some(OWNER)).state
                != State::Connected
        {
            return Err(unavailable());
        }
        Ok(())
    }

    fn claim(&self) -> io::Result<()> {
        let pin = self.pin()?.child(NAMESPACE, true)?;
        let root = &pin.path;
        let marker = root.join(archive::SENTINEL);
        if self.check_marker(root).is_ok() {
            return Ok(());
        }
        // Claim only an empty namespace; an existing foreign or damaged marker is left intact.
        if fs::read_dir(root)?.next().is_some() {
            return Err(unavailable());
        }
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)?;
        serde_json::to_writer(&mut f, &json!({"owner":OWNER,"storage_id":self.storage_id}))?;
        f.sync_all()?;
        sync_directory(root)?;
        self.check_marker(root)
    }

    pub(crate) fn read_dir(&self, record: &str, generation: &str) -> io::Result<PathBuf> {
        let pin = self.owned()?;
        let parent = pin.record(record, false)?;
        let dir = parent.generation(generation)?;
        checked_dir(&dir, false)?;
        let real = self.path.join(NAMESPACE).join(record).join(generation);
        if fs::canonicalize(&real)? != real {
            return Err(unavailable());
        }
        Ok(real)
    }

    fn remove(&self, record: &str, generation: &str) -> io::Result<()> {
        let pin = self.owned()?;
        let parent = match pin.record(record, false) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            result => result?,
        };
        let dir = parent.generation(generation)?;
        self.check_marker(&pin.path)?;
        for path in [
            dir.clone(),
            dir.with_file_name(format!(".part-{generation}")),
        ] {
            match fs::remove_dir_all(path) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        sync_directory(dir.parent().unwrap())
    }
}

/// Published NAS generations are immutable. Only cache misses need to open the
/// share and acquire a NAS reader slot.
pub(crate) struct TrackReads {
    cached: crate::track::NasSegments,
    cold: BTreeMap<String, (NasLocation, Share)>,
    detail: bool,
}

impl TrackReads {
    pub(crate) fn new(
        root: &Path,
        mut shares: BTreeMap<String, (NasLocation, Share)>,
        detail: bool,
    ) -> Self {
        let mut cached = BTreeMap::new();
        shares.retain(|record, (target, _)| {
            let sig = crate::track::SegmentSig::Nas(target.clone());
            if let Some(data) = crate::track::cached_segment(&root.join(record), &sig, detail) {
                cached.insert(record.clone(), Some((sig, data)));
                false
            } else {
                true
            }
        });
        Self {
            cached,
            cold: shares,
            detail,
        }
    }

    pub(crate) fn needs_nas(&self) -> bool {
        !self.cold.is_empty()
    }

    pub(crate) fn load(
        self,
        root: &Path,
        require: Option<&str>,
    ) -> io::Result<crate::track::NasSegments> {
        use crate::track::{SegmentSig, read_segment};

        let Self {
            cached: mut segments,
            cold: shares,
            detail,
        } = self;
        let mut pins = BTreeMap::new();
        let mut cold = Vec::with_capacity(shares.len());
        for (record, (target, share)) in shares {
            pins.entry(target.share.clone())
                .or_insert_with(|| share.owned());
            let folder = root.join(&record);
            cold.push((record, folder, target));
        }
        // At most four readers per query, with two NAS queries allowed at once.
        let read = |(record, folder, target): &(String, PathBuf, NasLocation)| {
            let sig = SegmentSig::Nas(target.clone());
            let result = (|| {
                let pin = pins[&target.share].as_ref().map_err(|_| unavailable())?;
                let parent = pin.record(record, false)?;
                let dir = parent.generation(&target.generation)?;
                // Keep the generation pinned while opening its files; reject directory symlinks.
                let prepared = Pinned::open(&dir)?;
                // Older prepared generations can lack the track files. As with local
                // recordings, omit them and leave a later request free to try again.
                Ok(
                    read_segment(folder, &prepared.path, sig.clone(), detail, true)
                        .ok()
                        .map(|data| (sig, data)),
                )
            })();
            (record.to_owned(), result)
        };
        let loaded = std::thread::scope(|scope| {
            cold.chunks(cold.len().div_ceil(4).max(1))
                .map(|chunk| scope.spawn(|| chunk.iter().map(&read).collect::<Vec<_>>()))
                .collect::<Vec<_>>()
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        for (record, result) in loaded {
            let data = match result {
                Ok(data) => data,
                Err(e) if require == Some(record.as_str()) => return Err(e),
                Err(_) => None,
            };
            segments.insert(record, data);
        }
        Ok(segments)
    }
}

fn checked_dir(path: &Path, create: bool) -> io::Result<()> {
    if create {
        match fs::create_dir(path) {
            Ok(()) => sync_directory(path.parent().unwrap())?,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(io::Error::other("invalid_prepared_directory"));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn files(path: &Path) -> io::Result<Sizes> {
    checked_dir(path, false)?;
    fs::read_dir(path)?
        .map(|entry| {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| io::Error::other("invalid_prepared_file"))?;
            let meta = fs::symlink_metadata(entry.path())?;
            if !meta.is_file() {
                return Err(io::Error::other("invalid_prepared_file"));
            }
            Ok((name, meta.len()))
        })
        .collect()
}

struct CopyProgress(watch::Sender<()>);

impl CopyProgress {
    fn touch(&self) -> io::Result<()> {
        self.0.send(()).map_err(|_| unavailable())
    }

    fn copy(
        &self,
        mut from: impl Read,
        mut to: impl Write,
        buffer: &mut [u8],
    ) -> io::Result<Digest> {
        use sha2::Digest;
        let mut digest = Sha256::new();
        loop {
            self.touch()?;
            let n = match from.read(buffer) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                result => result?,
            };
            if n == 0 {
                return Ok(digest.finalize().into());
            }
            to.write_all(&buffer[..n])?;
            digest.update(&buffer[..n]);
        }
    }
}

fn verify(
    dest: &Path,
    integrity: &Integrity,
    progress: &CopyProgress,
    buffer: &mut [u8],
) -> io::Result<()> {
    if files(dest)? != integrity.sizes {
        return Err(io::Error::other("prepared_size_mismatch"));
    }
    for name in integrity.sizes.keys() {
        let digest = progress.copy(nas::open_share_file(&dest.join(name))?, io::sink(), buffer)?;
        if integrity.sha256.get(name) != Some(&digest) {
            return Err(io::Error::other("prepared_sha256_mismatch"));
        }
    }
    Ok(())
}

fn copy_generation(
    source: &Path,
    target: &Path,
    generation: &str,
    progress: &CopyProgress,
    still_owned: impl Fn() -> io::Result<()>,
) -> io::Result<Integrity> {
    progress.touch()?;
    let sizes = files(source)?;
    if !sizes.contains_key("summary.json")
        && !sizes.contains_key("data.json.gz")
        && !sizes.contains_key("data.json")
    {
        return Err(io::Error::other("incomplete_prepared_generation"));
    }
    let parent = target.parent().unwrap();
    let part = parent.join(format!(".part-{generation}"));
    let mut buffer = vec![0; archive::CHUNK];
    let mut integrity = Integrity {
        sizes,
        sha256: BTreeMap::new(),
    };
    // After rename but before metadata commit, only a verified identical target is adopted.
    match fs::symlink_metadata(target) {
        Ok(_) => {
            for name in integrity.sizes.keys() {
                let file = nas::open_share_file(&source.join(name))?;
                integrity
                    .sha256
                    .insert(name.clone(), progress.copy(file, io::sink(), &mut buffer)?);
            }
            verify(target, &integrity, progress, &mut buffer)?;
            sync_directory(parent)?;
            return Ok(integrity);
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match fs::remove_dir_all(&part) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    fs::create_dir(&part)?;
    for name in integrity.sizes.keys() {
        let from = nas::open_share_file(&source.join(name))?;
        let mut to = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(part.join(name))?;
        integrity
            .sha256
            .insert(name.clone(), progress.copy(from, &mut to, &mut buffer)?);
        to.sync_all()?;
    }
    verify(&part, &integrity, progress, &mut buffer)?;
    sync_directory(&part)?;
    still_owned()?;
    progress.touch()?;
    fs::rename(&part, target)?;
    sync_directory(parent)?;
    crate::fault::point("prepared_generation_installed");
    Ok(integrity)
}
