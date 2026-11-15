use crate::error::{Result, bad, missing};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, RwLock};

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
pub fn valid_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn sha(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

pub async fn atomic_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let bytes = serde_json::to_vec(value)?;
    let temp = path.with_extension(format!("{}.tmp", id()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temp).await?;
    let outcome = async {
        file.write_all(&bytes).await?;
        // tokio's sync_all records a failed background write instead of returning it; flush reports it.
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, path).await?;
        #[cfg(unix)]
        {
            let parent = path.parent().unwrap().to_path_buf();
            tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all()).await??;
        }
        Ok(())
    }
    .await;
    if outcome.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    outcome
}

pub async fn read_json<T: DeserializeOwned>(path: &Path) -> std::io::Result<T> {
    serde_json::from_slice(&tokio::fs::read(path).await?).map_err(std::io::Error::other)
}

/// Resolve aliases through the nearest existing ancestor without creating anything.
pub async fn prospective_path(path: &Path) -> std::io::Result<PathBuf> {
    if path
        .components()
        .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "parent components are not allowed in data paths",
        ));
    }
    let mut probe = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut suffix = Vec::new();
    loop {
        match tokio::fs::canonicalize(&probe).await {
            Ok(mut real) => {
                for component in suffix.iter().rev() {
                    real.push(component);
                }
                return Ok(real);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    probe
                        .file_name()
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "invalid data path",
                            )
                        })?
                        .to_os_string(),
                );
                if !probe.pop() {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

pub struct Store {
    pub root: PathBuf,
    pub records: RwLock<BTreeMap<String, Value>>,
    record_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    pub registration: Mutex<()>,
    pub processing: RwLock<Value>,
    _instance: std::fs::File,
}

impl Store {
    pub async fn open(root: PathBuf) -> anyhow::Result<Arc<Self>> {
        let root = prospective_path(&root).await?;
        let python = prospective_path(Path::new("/data/roadviewer")).await?;
        anyhow::ensure!(
            !root.starts_with(&python) && !python.starts_with(&root),
            "HARS requires a separate data directory"
        );
        tokio::fs::create_dir_all(&root).await?;
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let instance = opts.open(root.join(".hars-instance.lock"))?;
        fs2::FileExt::try_lock_exclusive(&instance)?;
        let sweep = root.clone();
        tokio::task::spawn_blocking(move || sweep_temps(&sweep, 2)).await??;
        tokio::fs::create_dir_all(root.join(".uploads")).await?;
        let mut records = BTreeMap::new();
        let mut dirs = tokio::fs::read_dir(&root).await?;
        while let Some(entry) = dirs.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if valid_id(&name) && entry.file_type().await?.is_dir() {
                if !entry.path().join("meta.json").exists() {
                    // A durable upload journal may own this directory after a crash.
                    continue;
                }
                let meta: Value = read_json(&entry.path().join("meta.json")).await?;
                anyhow::ensure!(meta["id"] == name, "invalid recording metadata identifier");
                records.insert(name, meta);
            }
        }
        let processing_path = root.join(".processing-settings.json");
        let mut processing =
            json!({"concurrency":1,"auto_convert":true,"keep_original_video":true});
        if processing_path.exists() {
            let saved: Value = read_json(&processing_path).await?;
            let saved = saved
                .as_object()
                .ok_or_else(|| std::io::Error::other("invalid processing settings"))?;
            processing.as_object_mut().unwrap().extend(saved.clone());
        }
        anyhow::ensure!(
            processing["concurrency"]
                .as_u64()
                .is_some_and(|v| (1..=4).contains(&v))
                && processing["auto_convert"].is_boolean()
                && processing["keep_original_video"].is_boolean(),
            "invalid processing settings"
        );
        Ok(Arc::new(Self {
            root,
            records: RwLock::new(records),
            record_locks: Mutex::new(BTreeMap::new()),
            registration: Mutex::new(()),
            processing: RwLock::new(processing),
            _instance: instance,
        }))
    }

    pub async fn record_lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.record_locks
            .lock()
            .await
            .entry(id.to_owned())
            .or_default()
            .clone()
    }
    pub async fn folder(&self, name: &str) -> Result<PathBuf> {
        if !valid_id(name) || !self.records.read().await.contains_key(name) {
            return Err(missing("not_found"));
        }
        Ok(self.root.join(name))
    }
    pub async fn meta(&self, name: &str) -> Result<Value> {
        self.records
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| missing("not_found"))
    }
    pub async fn save_meta(&self, meta: Value) -> Result<()> {
        let name = meta["id"]
            .as_str()
            .filter(|v| valid_id(v))
            .ok_or_else(|| bad("invalid_recording"))?
            .to_owned();
        atomic_json(&self.root.join(&name).join("meta.json"), &meta).await?;
        self.records.write().await.insert(name, meta);
        Ok(())
    }
    pub async fn used(&self) -> std::io::Result<u64> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || directory_size(&root)).await?
    }
}

/// Deletes `atomic_json` temporaries left by a killed writer. The instance lock excludes live writers.
fn sweep_temps(dir: &Path, depth: u8) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if kind.is_dir() && depth > 0 {
            sweep_temps(&entry.path(), depth - 1)?;
        } else if kind.is_file()
            && name
                .strip_suffix(".tmp")
                .and_then(|stem| stem.rsplit_once('.'))
                .is_some_and(|(_, token)| valid_id(token))
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

pub fn directory_size(root: &Path) -> std::io::Result<u64> {
    let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    let mut total = 0;
    let entries = match std::fs::read_dir(root) {
        Ok(v) => v,
        // Upload cleanup or a deletion during the scan (as Python storage_used_bytes).
        Err(e) if gone(&e) => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let kind = match entry.file_type() {
            Ok(v) => v,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e),
        };
        if kind.is_file() {
            match entry.metadata() {
                Ok(m) => total += m.len(),
                Err(e) if gone(&e) => {}
                Err(e) => return Err(e),
            }
        } else if kind.is_dir() {
            total += directory_size(&entry.path())?;
        }
    }
    Ok(total)
}
