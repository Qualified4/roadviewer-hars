//! Python `PendingDeletes` (`.pending-nas-deletes.json`): archived files of deleted recordings that could not be
//! removed yet. Rows list NAS files by kind and a remote backend's files by reference (`refs`); only rows of an
//! explicit user deletion (`explicit`) may remove Drive files.
use super::{
    ArchiveRead, ArchiveWrite, BackendKind, MEDIA_KINDS, NasBackend, RemoteRef, State, fsutil,
    index::{REMOTE_BACKENDS, Ref},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::LazyLock,
};

pub const PENDING_DELETES: &str = ".pending-nas-deletes.json";
const REF_KEYS: [&str; 5] = ["kind", "backend", "file_id", "bytes", "sha256"];

/// One file shared by deletion requests and the archiver.
static LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

fn valid_ref(r: &Value) -> bool {
    let Some(o) = r.as_object() else {
        return false;
    };
    o.len() == REF_KEYS.len()
        && REF_KEYS.iter().all(|k| o.contains_key(*k))
        && o["kind"].as_str().is_some_and(|k| MEDIA_KINDS.contains(&k))
        && o["backend"]
            .as_str()
            .is_some_and(|b| REMOTE_BACKENDS.contains(&b))
        && o["file_id"].as_str().is_some_and(|f| !f.is_empty())
        && (o["bytes"].is_u64() || o["bytes"].is_i64())
        && (o["sha256"].is_null() || o["sha256"].is_string())
}

fn valid_row(r: &Value) -> Option<Value> {
    let id = r.get("id")?.as_str()?;
    if !crate::store::valid_id(id) {
        return None;
    }
    let kinds = r.get("kinds")?.as_array()?;
    let mut row = json!({"id": id, "kinds": kinds.iter().filter(|k| k.as_str().is_some_and(|k| MEDIA_KINDS.contains(&k))).cloned().collect::<Vec<_>>()});
    if r.get("explicit") == Some(&json!(true)) {
        row["explicit"] = json!(true);
    }
    if let Some(refs) = r.get("refs") {
        row["refs"] = json!(
            refs.as_array()
                .map(|a| a
                    .iter()
                    .filter(|x| valid_ref(x))
                    .cloned()
                    .collect::<Vec<_>>())
                .unwrap_or_default()
        );
    }
    Some(row)
}

fn kinds(row: &Value) -> Vec<String> {
    row["kinds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| k.as_str().map(str::to_owned))
        .collect()
}
fn refs(row: &Value) -> Vec<Value> {
    row.get("refs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[derive(Clone, Debug)]
pub struct PendingDeletes {
    pub root: PathBuf,
    pub path: PathBuf,
}

impl PendingDeletes {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            path: root.join(PENDING_DELETES),
        }
    }

    /// Caller holds LOCK. Any read error other than a missing file propagates: the list is never overwritten blind.
    fn load(&self) -> std::io::Result<Vec<Value>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let rows: Option<Vec<Value>> = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v.as_array().cloned());
        let valid: Vec<Value> = rows.iter().flatten().filter_map(valid_row).collect();
        if rows.as_ref() != Some(&valid) {
            // Unreadable content is kept aside for inspection instead of being replaced by the next save.
            let stamp = crate::store::now() as u64;
            std::fs::rename(
                &self.path,
                self.root
                    .join(format!(".pending-nas-deletes.corrupt-{stamp}.json")),
            )?;
            tracing::warn!("pending_deletes_corrupt_kept_aside");
            if !valid.is_empty() {
                fsutil::atomic_json(&self.path, &json!(valid))?;
            }
        }
        Ok(valid)
    }

    fn save(&self, rows: &[Value]) -> std::io::Result<()> {
        if rows.is_empty() {
            match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            fsutil::sync_dir(&self.path)
        } else {
            fsutil::atomic_json(&self.path, &json!(rows))
        }
    }

    /// refs: remote files; a row gets the field only when it has some.
    pub async fn add(
        &self,
        id: &str,
        new_kinds: &[String],
        new_refs: &[Ref],
        explicit: bool,
    ) -> std::io::Result<()> {
        let _guard = LOCK.lock().await;
        let mut rows = self.load()?;
        let index = match rows.iter().position(|r| r["id"] == id) {
            Some(i) => i,
            None => {
                rows.push(json!({"id": id, "kinds": []}));
                rows.len() - 1
            }
        };
        let row = &mut rows[index];
        let mut have = kinds(row);
        for k in new_kinds {
            if !have.contains(k) {
                have.push(k.clone());
            }
        }
        row["kinds"] = json!(have);
        if explicit && !new_refs.is_empty() {
            row["explicit"] = json!(true);
        }
        if !new_refs.is_empty() {
            let mut list = refs(row);
            let known: BTreeSet<String> = list
                .iter()
                .filter_map(|r| r["file_id"].as_str().map(str::to_owned))
                .collect();
            list.extend(
                new_refs
                    .iter()
                    .filter(|r| !known.contains(&r.file_id))
                    .map(Ref::to_json),
            );
            row["refs"] = json!(list);
        }
        self.save(&rows)
    }

    pub async fn rows(&self) -> std::io::Result<Vec<Value>> {
        let _guard = LOCK.lock().await;
        self.load()
    }

    /// Archived files still waiting to be removed.
    pub async fn count(&self) -> std::io::Result<usize> {
        Ok(self
            .rows()
            .await?
            .iter()
            .map(|r| kinds(r).len() + refs(r).len())
            .sum())
    }

    fn deleted(&self, id: &str) -> bool {
        // A deletion stopped between recording the files and removing the recording still uses them, and a
        // recording that cannot be checked now is never taken for deleted.
        matches!(self.root.join(id).join("meta.json").try_exists(), Ok(false))
    }

    /// Rows of recordings that are still whole (their deletion was abandoned): nothing of theirs is removed later.
    pub async fn forget(&self, ids: &[String]) -> std::io::Result<()> {
        let _guard = LOCK.lock().await;
        let rows = self.load()?;
        let kept: Vec<Value> = rows
            .iter()
            .filter(|r| {
                let id = r["id"].as_str().unwrap_or_default();
                !(ids.iter().any(|i| i == id)
                    && matches!(self.root.join(id).join("meta.json").try_exists(), Ok(true)))
            })
            .cloned()
            .collect();
        if kept.len() == rows.len() {
            return Ok(());
        }
        self.save(&kept)
    }

    /// The NAS: the rows' kinds are removed from the HARS write root. Returns the number of files removed.
    pub async fn process_nas(&self, nas: &NasBackend) -> std::io::Result<usize> {
        if nas.probe(false).await.state != State::Connected {
            return Ok(0);
        }
        let rows = self.rows().await?;
        let (mut total, mut done) = (0, BTreeMap::<String, BTreeSet<String>>::new());
        for row in rows {
            let id = row["id"].as_str().unwrap_or_default().to_owned();
            if !self.deleted(&id) {
                continue;
            }
            let row_kinds = kinds(&row);
            if row_kinds.is_empty() && !refs(&row).is_empty() {
                continue;
            }
            let mut gone = BTreeSet::new();
            for kind in row_kinds {
                let present = match nas.path_for(&id, &kind) {
                    Ok(p) => tokio::task::spawn_blocking(move || {
                        std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
                    })
                    .await
                    .unwrap_or(false),
                    Err(_) => false,
                };
                let r = RemoteRef::new(BackendKind::Smb, format!("{id}/{kind}"), 0, None, None);
                match nas.delete(&r).await {
                    Ok(true) => {
                        total += usize::from(present);
                        gone.insert(kind);
                    }
                    Ok(false) => {}
                    // Not a HARS root (anymore): nothing on it is touched; the row waits.
                    Err(_) => {}
                }
            }
            done.insert(id, gone);
        }
        if !done.is_empty() {
            self.merge(&done, &BTreeMap::new()).await?;
        }
        Ok(total)
    }

    /// Python `_process_remote`: only rows of an explicit deletion, only refs of this backend kind.
    pub async fn process_remote<B: ArchiveWrite>(&self, backend: &B) -> std::io::Result<usize> {
        if backend.probe(false).await.state != State::Connected {
            return Ok(0);
        }
        let kind = backend.caps().kind.as_str();
        let rows = self.rows().await?;
        let (mut total, mut done) = (0, BTreeMap::<String, BTreeSet<String>>::new());
        for row in rows {
            let id = row["id"].as_str().unwrap_or_default().to_owned();
            // Old intents have no provenance: preserved for inspection, never guessed to be a user's delete.
            let targets: Vec<Value> = if row.get("explicit") == Some(&json!(true)) {
                refs(&row)
                    .into_iter()
                    .filter(|r| r["backend"] == kind)
                    .collect()
            } else {
                Vec::new()
            };
            if targets.is_empty() || !self.deleted(&id) {
                continue;
            }
            let mut gone = BTreeSet::new();
            for r in targets {
                let file_id = r["file_id"].as_str().unwrap_or_default().to_owned();
                let target = RemoteRef::new(
                    backend.caps().kind,
                    file_id.clone(),
                    r["bytes"].as_u64().unwrap_or(0),
                    r["sha256"].as_str().map(str::to_owned),
                    None,
                );
                match backend.delete(&target).await {
                    Ok(true) => {
                        gone.insert(file_id);
                        total += 1;
                    }
                    Ok(false) => {}
                    Err(e) if e.is_foreign() => {
                        // Not ours (another instance's file, the folder itself): never deleted, not tried again.
                        tracing::warn!(kind = %r["kind"].as_str().unwrap_or(""), "pending_drive_delete_skipped_foreign");
                        gone.insert(file_id);
                    }
                    Err(_) => {}
                }
            }
            done.insert(id, gone);
        }
        if !done.is_empty() {
            self.merge(&BTreeMap::new(), &done).await?;
        }
        Ok(total)
    }

    /// Drops what was removed from the current list; rows added meanwhile are kept.
    async fn merge(
        &self,
        kinds_done: &BTreeMap<String, BTreeSet<String>>,
        refs_done: &BTreeMap<String, BTreeSet<String>>,
    ) -> std::io::Result<()> {
        let _guard = LOCK.lock().await;
        let current = self.load()?;
        let mut left = Vec::new();
        for r in &current {
            let id = r["id"].as_str().unwrap_or_default();
            let mut row = r.clone();
            let no = BTreeSet::new();
            let k_done = kinds_done.get(id).unwrap_or(&no);
            row["kinds"] = json!(
                kinds(r)
                    .into_iter()
                    .filter(|k| !k_done.contains(k))
                    .collect::<Vec<_>>()
            );
            if r.get("refs").is_some() {
                let f_done = refs_done.get(id).unwrap_or(&no);
                row["refs"] = json!(
                    refs(r)
                        .into_iter()
                        .filter(|x| !f_done.contains(x["file_id"].as_str().unwrap_or("")))
                        .collect::<Vec<_>>()
                );
            }
            if !kinds(&row).is_empty() || !refs(&row).is_empty() {
                left.push(row);
            }
        }
        if left != current {
            self.save(&left)?;
        }
        Ok(())
    }
}
