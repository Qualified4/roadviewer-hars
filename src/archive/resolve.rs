//! Where a recording's file is now (Python `resolve`, `_resolve_remote`, `indexed_state`, `media_size`): a local
//! path, a remote reference, or a state saying why neither is reachable. A local file always wins over a remote
//! copy. Touches the share or Drive: never call it while holding a record lock.
use super::{
    ArchiveRead, RemoteRef, State, WriteBackend,
    archiver::Archive,
    index::{self, bytes_of},
};
use serde_json::Value;
use std::path::PathBuf;

/// A file only the remote backend has.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteFile {
    pub backend_kind: &'static str,
    pub reference: RemoteRef,
    pub kind: String,
    /// The index's size when it has one (served sizes are the index's).
    pub size: u64,
    pub etag: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    Local(PathBuf),
    Remote(RemoteFile),
    Nothing,
}

/// The state strings Python's `resolve` returns.
pub type ResolveState = &'static str;

async fn resolve_remote(
    backend: &WriteBackend,
    block: &Value,
    backend_kind: &str,
    kind: &str,
    mut corrupt: bool,
) -> (Resolved, ResolveState) {
    // Only the backend the file was stored with can answer for it (another kind may be active after a switch).
    let WriteBackend::Drive(drive) = backend else {
        return (Resolved::Nothing, "storage_unavailable");
    };
    if backend_kind != "gdrive" {
        return (Resolved::Nothing, "storage_unavailable");
    }
    match drive.read_state().await.state {
        State::Checking => return (Resolved::Nothing, "checking"),
        State::Connected => {}
        _ => return (Resolved::Nothing, "storage_unavailable"),
    }
    let Some(file_id) = block["file_id"].as_str().filter(|s| !s.is_empty()) else {
        return (Resolved::Nothing, "missing");
    };
    let reference = RemoteRef::new(
        drive.caps().kind,
        file_id,
        bytes_of(block),
        block["sha256"].as_str().map(str::to_owned),
        block["md5"].as_str().map(str::to_owned),
    );
    let stat = match drive.stat_recent(&reference).await {
        Ok(s) => s,
        Err(_) => return (Resolved::Nothing, "storage_unavailable"),
    };
    let Some(stat) = stat.filter(|s| s.exists && !s.trashed) else {
        return (Resolved::Nothing, "missing");
    };
    // The file must still be the one that was verified (dynamically; the index is not rewritten).
    if reference.bytes > 0 && stat.bytes != reference.bytes {
        corrupt = true;
    }
    if let (Some(a), Some(b)) = (&reference.sha256, &stat.sha256)
        && a != b
    {
        corrupt = true;
    }
    let size = if reference.bytes > 0 {
        reference.bytes
    } else {
        stat.bytes
    };
    let sha = reference.sha256.clone().unwrap_or_default();
    let etag = format!("\"{file_id}:{}\"", &sha[..sha.len().min(16)]);
    let file = RemoteFile {
        backend_kind: "gdrive",
        reference,
        kind: kind.to_owned(),
        size,
        etag,
    };
    (
        Resolved::Remote(file),
        if corrupt { "corrupt" } else { "available" },
    )
}

fn error_set(e: &Value) -> bool {
    e.get("error").is_some_and(|v| !v.is_null())
}

impl Archive {
    /// Python `resolve(p, meta, kind, store, backend)`.
    pub async fn resolve(&self, id: &str, meta: &Value, kind: &str) -> (Resolved, ResolveState) {
        assert!(index::known_kind(kind), "unknown media kind: {kind}");
        let backend = self.backend();
        let e = index::entry(meta, kind);
        let where_ = e["location"].as_str().unwrap_or("local");
        let local = self.host.folder(id).join(kind);
        match where_ {
            "removed" => return (Resolved::Nothing, "missing"),
            "gdrive" => return resolve_remote(&backend, &e, "gdrive", kind, error_set(&e)).await,
            "local" if !local.is_file() => {
                if let Some(block) = index::remote(meta, kind) {
                    let name = block["backend"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("gdrive")
                        .to_owned();
                    return resolve_remote(&backend, &block, &name, kind, error_set(&e)).await;
                }
            }
            _ => {}
        }
        let path = if where_ == "nas" {
            let WriteBackend::Nas(nas) = &*backend else {
                return (Resolved::Nothing, "storage_unavailable");
            };
            match nas.probe(false).await.state {
                State::Checking => return (Resolved::Nothing, "checking"),
                State::Connected => {}
                _ => return (Resolved::Nothing, "storage_unavailable"),
            }
            match nas.path_for(id, kind) {
                Ok(p) => p,
                Err(_) => return (Resolved::Nothing, "storage_unavailable"),
            }
        } else {
            local
        };
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => {}
            Ok(_) | Err(_) if where_ != "nas" => return (Resolved::Nothing, "missing"),
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return (Resolved::Nothing, "storage_unavailable");
            }
            _ => {
                // A share that dropped during the lookup looks the same: only a reachable share confirms missing.
                backend.invalidate();
                return match backend.probe(false).await.state {
                    State::Checking => (Resolved::Nothing, "checking"),
                    State::Connected => (Resolved::Nothing, "missing"),
                    _ => (Resolved::Nothing, "storage_unavailable"),
                };
            }
        }
        // A copy that did not verify (remote_mismatch) says nothing about the local file.
        if where_ == "local" && e["error"] == "remote_mismatch" {
            return (Resolved::Local(path), "available");
        }
        let state = if error_set(&e) {
            "corrupt"
        } else {
            "available"
        };
        (Resolved::Local(path), state)
    }

    /// Python `indexed_state`: the library list's state from the index and the archive's state (`share`), so the
    /// list never stats the archive per file.
    pub fn indexed_state(&self, id: &str, meta: &Value, kind: &str, share: State) -> ResolveState {
        let e = index::entry(meta, kind);
        let where_ = e["location"].as_str().unwrap_or("local");
        if where_ == "removed" {
            return "missing";
        }
        let local = self.host.folder(id).join(kind);
        let remote_only =
            where_ == "local" && index::remote(meta, kind).is_some() && !local.is_file();
        if !index::is_archived_location(where_) && !remote_only {
            if !local.is_file() {
                return "missing";
            }
            return if where_ == "local" && e["error"] == "remote_mismatch" || !error_set(&e) {
                "available"
            } else {
                "corrupt"
            };
        }
        match share {
            State::Connected if error_set(&e) => "corrupt",
            State::Connected => "available",
            State::Unavailable | State::Disabled => "storage_unavailable",
            State::Checking => "checking",
        }
    }

    /// Python `media_size`: archived sizes come from the index.
    pub fn media_size(&self, id: &str, meta: &Value, kind: &str) -> u64 {
        let e = index::entry(meta, kind);
        match e["location"].as_str() {
            Some("nas" | "gdrive") => bytes_of(&e),
            Some("removed") => 0,
            _ => std::fs::metadata(self.host.folder(id).join(kind))
                .map(|m| m.len())
                .unwrap_or_else(|_| index::remote(meta, kind).map_or(0, |b| bytes_of(&b))),
        }
    }
}
