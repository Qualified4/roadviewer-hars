//! Durable conversion scheduling. Native/CPU work runs in a disposable child.
use crate::{
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
            if journal.is_file() {
                let work: Value = read_json(&journal).await?;
                let generation = work["generation"]
                    .as_str()
                    .filter(|v| crate::store::valid_id(v))
                    .context("invalid conversion journal")?;
                jobs.publish(&record, generation).await?;
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
        let label = self
            .store
            .meta(record)
            .await
            .map_err(|_| anyhow::anyhow!("record unavailable"))?["name"]
            .as_str()
            .unwrap_or("recording")
            .to_owned();
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
        command
            .arg("convert-log")
            .arg(folder.join("rlog.zst"))
            .arg(&staging)
            .arg(label)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .context("conversion process could not start")?;
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let future = async {
            while let Some(line) = lines.next_line().await? {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && v["stage"].as_str().is_some_and(|s| s.len() < 64)
                    && v["percent"]
                        .as_f64()
                        .is_some_and(|p| (0.0..=100.).contains(&p))
                {
                    self.progress.write().await.insert(record.into(), v);
                }
            }
            anyhow::ensure!(
                child.wait().await?.success(),
                "conversion failed; original data preserved"
            );
            Ok::<_, anyhow::Error>(())
        };
        let timeout =
            if folder.join("fcamera.hevc").is_file() || folder.join("ecamera.hevc").is_file() {
                1800
            } else {
                600
            };
        tokio::select! {
            result=tokio::time::timeout(std::time::Duration::from_secs(timeout),future)=>{result.context("conversion deadline exceeded; original data preserved")??;},
            _=cancel.notified()=>{
                child.kill().await?;
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
            if !keep && prepared.join(mp4).is_file() {
                // The conversion verified frame count, timestamps and compressed
                // bitstream before reaching its durable publication journal.
                tokio::fs::rename(prepared.join(mp4), folder.join(mp4)).await?;
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

async fn download_entries(folder: &std::path::Path) -> std::io::Result<Value> {
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
