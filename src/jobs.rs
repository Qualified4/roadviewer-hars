//! Durable conversion scheduling. Native/CPU work runs in a disposable child.
use crate::{
    archive::index,
    store::{Store, atomic_json, id, now, read_json},
    upload::Uploads,
};
use anyhow::Context;
use serde_json::{Value, json};
use std::{collections::BTreeMap, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{Mutex, Notify, RwLock},
};

pub struct Jobs {
    store: Arc<Store>,
    uploads: Arc<Uploads>,
    active: Mutex<BTreeMap<String, Arc<Notify>>>,
    pub progress: RwLock<BTreeMap<String, Value>>,
}
impl Jobs {
    pub async fn open(store: Arc<Store>, uploads: Arc<Uploads>) -> anyhow::Result<Arc<Self>> {
        let jobs = Arc::new(Self {
            store,
            uploads,
            active: Mutex::new(BTreeMap::new()),
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
    pub async fn tick(self: &Arc<Self>) -> anyhow::Result<()> {
        let settings = self.store.processing.read().await.clone();
        let limit = settings["concurrency"].as_u64().unwrap_or(1) as usize;
        let automatic = settings["auto_convert"] == true;
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
            if self.busy(record).await {
                continue;
            }
            let eligible = row["status"] == "queued"
                || (row["status"] == "unconverted" && automatic && row["auto_excluded"] != true);
            if !eligible {
                continue;
            }
            let lock = self.store.record_lock(record).await;
            let _guard = lock.lock().await;
            // Deleted while this tick waited for its lock.
            let Ok(mut meta) = self.store.meta(record).await else {
                continue;
            };
            if !matches!(meta["status"].as_str(), Some("queued" | "unconverted")) {
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
            if self.active.lock().await.len() >= limit {
                continue;
            }
            let generation = id();
            let cancel = Arc::new(Notify::new());
            self.active
                .lock()
                .await
                .insert(record.into(), cancel.clone());
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
                let result = jobs.run(&record, &generation, cancel).await;
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
    async fn run(&self, record: &str, generation: &str, cancel: Arc<Notify>) -> anyhow::Result<()> {
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
            .kill_on_drop(true)
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
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let reaped = std::sync::atomic::AtomicBool::new(false);
        let future = async {
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
                    let _=tokio::fs::remove_dir_all(&staging).await;
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
        let _summary: Value = read_json(&staging.join("summary.json"))
            .await
            .context("conversion summary missing")?;
        atomic_json(&staging.join(".generation"), &generation).await?;
        atomic_json(
            &folder.join(".conversion.json"),
            &json!({"generation":generation}),
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
        let installed = read_json::<String>(&prepared.join(".generation"))
            .await
            .ok()
            .as_deref()
            == Some(generation);
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
        }
        let summary: Value = read_json(&prepared.join("summary.json")).await?;
        let keep = self.store.processing.read().await["keep_original_video"] != false;
        let mut codecs = serde_json::Map::new();
        let mut origins = serde_json::Map::new();
        for (camera, raw, mp4) in [
            ("qcamera", "qcamera.ts", "camera.mp4"),
            ("front", "fcamera.hevc", "fcamera.mp4"),
            ("wide", "ecamera.hevc", "ecamera.mp4"),
        ] {
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
            // A restart resuming this generation's publication finds the MP4s it already moved.
            let moved = installed && !prepared.join(mp4).exists() && folder.join(mp4).is_file();
            if !keep && !archived && (moved || prepared.join(mp4).is_file()) {
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
                    tokio::fs::remove_file(folder.join(raw)).await?;
                }
            }
        }
        meta["video_codecs"] = json!(codecs);
        meta["video_origins"] = json!(origins);
        meta["video_downloads"] = download_entries(&folder).await?;
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
        meta["status"] = json!("ready");
        meta["error"] = Value::Null;
        meta["manual_conversion"] = json!(false);
        meta["decoder_version"] = json!("hars-v28-runtime-geometry");
        self.store
            .save_meta(meta)
            .await
            .map_err(|_| anyhow::anyhow!("conversion metadata publication failed"))?;
        tokio::fs::remove_file(folder.join(".conversion.json")).await?;
        // Only this job's old, derived directory is removed, after its replacement
        // and metadata are durable. Raw sources are always retained.
        if backup.is_dir() {
            tokio::fs::remove_dir_all(backup).await?;
        }
        Ok(())
    }
}

impl Jobs {
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
