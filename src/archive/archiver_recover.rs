//! Drive startup recovery and capacity moves (Python `_recover_remote`, `_drop_stale_remote`, `_recover_pending`,
//! `_recover_moved`, `_reclaim`, `StoragePolicy.request_reclaim`). Recovery never uploads and never changes the
//! index of a file Drive does not have; a local file goes only when its moved copy verifies.
use super::{
    ArchiveRead, ArchiveWrite, SessionState,
    archiver::{Archive, Ctx, CycleResult, Outcome, Reclaim, file_sha256, is_file, lock, stat},
    archiver_remote::resume_of,
    index::{self, ARCHIVE_ORDER},
    settings::Mode,
};
use crate::store::now;
use serde_json::Value;
use std::collections::BTreeSet;

impl Archive {
    /// Python `_recover_remote` (caller holds the cycle lock): interrupted uploads are resumed, adopted or dropped;
    /// a local file left by a move that stopped before its unlink goes once its copy verifies; files Drive does not
    /// have are counted, their index never changed.
    pub(crate) async fn recover_remote(&self, ctx: &Ctx) {
        for id in self.host.ids().await {
            self.checked();
            let Some(mut m) = self.host.meta(&id).await else {
                continue;
            };
            for kind in super::MEDIA_KINDS {
                let e = index::entry(&m, kind);
                let step = if e["location"] == "gdrive" {
                    self.recover_moved(ctx, &id, &m, kind, &e)
                        .await
                        .map(|()| true)
                } else if e["location"] == "local" {
                    if self.drop_stale_remote(&id, &m, kind).await {
                        match self.host.meta(&id).await {
                            Some(fresh) => m = fresh,
                            None => return,
                        }
                    }
                    let mut step = Ok(true);
                    if let Some(pending) = index::remote_pending(&m, kind)
                        && pending["backend"] == ctx.backend.caps().kind.as_str()
                    {
                        step = self.recover_pending(ctx, &id, &m, kind, &pending).await;
                    }
                    if matches!(step, Ok(true))
                        && let Some(block) = index::remote(&m, kind)
                    {
                        step = self
                            .check_remote(ctx, &id, kind, &block)
                            .await
                            .map(|_| true);
                    }
                    step
                } else {
                    Ok(true)
                };
                match step {
                    Ok(true) => {}
                    // Not recovered: the next connected cycle tries again.
                    Ok(false) => return,
                    Err(e) => {
                        self.remote_fail(ctx, &e);
                        return;
                    }
                }
            }
        }
        lock(&self.st).recovered = true;
    }

    /// Python `_drop_stale_remote`: a local file whose remote copy (or upload in progress) describes other content
    /// loses the block; nothing is deleted. A matching read records the hash. True when the index changed.
    async fn drop_stale_remote(&self, id: &str, m: &Value, kind: &str) -> bool {
        let (block, pending) = (index::remote(m, kind), index::remote_pending(m, kind));
        if block.is_none() && pending.is_none() {
            return false;
        }
        let local = self.host.folder(id).join(kind);
        let Ok((size, mtime)) = stat(&local) else {
            return false; // Gone locally: the copy stands in for it.
        };
        let processing = m["status"] == "processing";
        let mut digest = index::fresh_hash(m, kind, size, mtime).filter(|_| !processing);
        let read = digest.is_none();
        let mut differs = async |bytes: Option<u64>, sha: Option<&str>| -> std::io::Result<bool> {
            if bytes.is_some_and(|b| b != size) {
                return Ok(true);
            }
            let Some(sha) = sha else { return Ok(false) };
            if digest.is_none() {
                digest = Some(file_sha256(&local).await?);
            }
            Ok(digest.as_deref() != Some(sha))
        };
        let stale_block = match &block {
            Some(b) => match differs(b["bytes"].as_u64(), b["sha256"].as_str()).await {
                Ok(v) => v,
                Err(_) => return false,
            },
            None => false,
        };
        let stale_pending = match (&pending, stale_block) {
            (Some(p), false) => match differs(p["size"].as_u64(), p["sha256"].as_str()).await {
                Ok(v) => v,
                Err(_) => return false,
            },
            _ => false,
        };
        let record = !(stale_block || stale_pending) && read && digest.is_some() && !processing;
        if !(stale_block || stale_pending || record) {
            return false;
        }
        let digest = digest.unwrap_or_default();
        let changed = self
            .with_meta(id, |current| {
                if stat(&local).ok() != Some((size, mtime))
                    || index::location(current, kind) != "local"
                    || index::remote(current, kind) != block
                    || index::remote_pending(current, kind) != pending
                {
                    return None;
                }
                if stale_block {
                    index::drop_remote(current, kind, None);
                } else if stale_pending {
                    index::clear_remote_pending(current, kind);
                } else {
                    index::record_hash(current, kind, size, mtime, &digest);
                }
                Some(())
            })
            .await;
        if !matches!(changed, Some(Ok(()))) {
            return false;
        }
        if !record {
            tracing::info!(kind, "stale_remote_copy_dropped_kept_on_drive");
        }
        true
    }

    /// Python `_recover_pending`; Ok(false) or Err when recovery must stop.
    async fn recover_pending(
        &self,
        ctx: &Ctx,
        id: &str,
        m: &Value,
        kind: &str,
        pending: &Value,
    ) -> super::Result<bool> {
        match ctx.backend.session_status(&resume_of(pending)).await? {
            SessionState::Incomplete => Ok(true), // The next archiving of the file resumes it.
            SessionState::Done => {
                // Completed but not committed: the copy find() returns is verified and committed. Recovery never
                // uploads; when find() misses it the record stays and the cycle's resume asks the session.
                Ok(!matches!(
                    self.archive_remote(ctx, id, m, kind, true, None).await,
                    Outcome::Failed(_)
                ))
            }
            SessionState::Expired => {
                let _ = self
                    .with_meta(id, |current| {
                        if index::remote_pending(current, kind).as_ref() != Some(pending) {
                            return None;
                        }
                        index::clear_remote_pending(current, kind);
                        Some(())
                    })
                    .await;
                Ok(true)
            }
        }
    }

    /// True when Drive has the file; otherwise it is counted as missing (the index is kept).
    async fn check_remote(
        &self,
        ctx: &Ctx,
        id: &str,
        kind: &str,
        block: &Value,
    ) -> super::Result<bool> {
        let found = match self.remote_ref(ctx, block) {
            Some(r) => ctx.backend.stat(&r).await?,
            None => None,
        };
        if found.is_none_or(|f| !f.exists || f.trashed) {
            self.note_missing(id, kind);
            return Ok(false);
        }
        lock(&self.st)
            .missing
            .remove(&(id.to_owned(), kind.to_owned()));
        Ok(true)
    }

    /// Python `_recover_moved`: a moved file whose local file is still there goes only when every condition holds
    /// and it is the content that was moved.
    async fn recover_moved(
        &self,
        ctx: &Ctx,
        id: &str,
        m: &Value,
        kind: &str,
        e: &Value,
    ) -> super::Result<()> {
        let local = self.host.folder(id).join(kind);
        if !self.check_remote(ctx, id, kind, e).await? || !is_file(&local) {
            return Ok(());
        }
        let s = &ctx.settings;
        let Some(sha) = e["sha256"].as_str() else {
            return Ok(());
        };
        let bytes = index::bytes_of(e);
        if !(s.effective_mode() == Mode::Move && s.allow_local_delete && bytes > 0) {
            return Ok(());
        }
        let Ok(file) = stat(&local) else {
            return Ok(());
        };
        if file.0 != bytes {
            return Ok(());
        }
        let recorded = index::fresh_hash(m, kind, file.0, file.1).as_deref() == Some(sha);
        if !recorded && file_sha256(&local).await.ok().as_deref() != Some(sha) {
            tracing::warn!(kind, "local_differs_from_moved_copy_kept");
            return Ok(());
        }
        let Some(r) = self.remote_ref(ctx, e) else {
            return Ok(());
        };
        let verdict = self.verify(ctx, &r, sha, bytes, e["md5"].as_str()).await?;
        let revision = index::revision(m);
        let l = self.host.store.record_lock(id).await;
        let _g = l.lock().await;
        let Some(mut current) = self.host.meta(id).await else {
            return Ok(());
        };
        let same = stat(&local).ok() == Some(file)
            && index::entry(&current, kind) == *e
            && index::revision(&current) == revision
            && current["status"] == "ready"
            && !self.host.busy(id).await;
        if !same {
            return Ok(());
        }
        if verdict.is_none() {
            // The Drive copy is not the indexed content but the local file is: the index points at the local file
            // again. No remote block (that copy is not trusted); nothing is deleted.
            if let Some(storage) = current["storage"].as_object_mut() {
                storage.remove(kind);
            }
            index::record_hash(&mut current, kind, file.0, file.1, sha);
            let _ = self.host.save(current).await;
            tracing::warn!(kind, "drive_copy_differs_local_used_again");
            return Ok(());
        }
        let _ = std::fs::remove_file(&local);
        Ok(())
    }

    /// Python `_recheck_missing_remote`: the index is never changed here.
    pub(crate) async fn recheck_missing_remote(&self, ctx: &Ctx) {
        let missing: Vec<(String, String)> = lock(&self.st).missing.iter().cloned().collect();
        for (id, kind) in missing {
            let m = self.host.meta(&id).await;
            let block = m.as_ref().and_then(|m| {
                let e = index::entry(m, &kind);
                match e["location"].as_str() {
                    Some("gdrive") => Some(e),
                    Some("local") => index::remote(m, &kind),
                    _ => None,
                }
            });
            let Some(block) = block else {
                lock(&self.st).missing.remove(&(id, kind));
                continue;
            };
            if self.check_remote(ctx, &id, &kind, &block).await.is_err() {
                ctx.backend.invalidate();
                return;
            }
        }
    }

    // --- Capacity (Python `_reclaim` and `StoragePolicy.request_reclaim`).

    /// The local targets are met again (limit and threshold, plus the bytes a refused upload needs).
    pub(crate) async fn enough(&self, ctx: &Ctx, r: &Reclaim) -> bool {
        let root = &self.host.store.root;
        let Ok(reserved) = crate::storage::reserved_except(root, r.exclude.as_deref()).await else {
            return false;
        };
        let limit = crate::storage::load_settings(root)
            .await
            .map(|s| crate::storage::limits(&s).max_bytes)
            .unwrap_or(0);
        let targets: Vec<u64> = [limit, ctx.settings.local_threshold_bytes]
            .into_iter()
            .filter(|v| *v > 0)
            .collect();
        let used = self.host.used().await;
        let free = fs2::available_space(root).unwrap_or(0);
        (targets.is_empty() || used + reserved + r.extra <= *targets.iter().min().unwrap_or(&0))
            && free >= reserved + r.extra + crate::storage::MARGIN
    }

    /// Pinned recordings are eligible too (moving is not deleting); routes still received or converting are not.
    pub(crate) async fn eligible(&self, meta: &Value, r: &Reclaim) -> bool {
        let id = meta["id"].as_str().unwrap_or_default();
        if r.protected.contains(id) || meta["status"] != "ready" || self.host.busy(id).await {
            return false;
        }
        let route = crate::storage::route_key(meta);
        let Ok(guards) =
            crate::storage::guards(&self.host.store.root, &r.incoming, &BTreeSet::new()).await
        else {
            return false;
        };
        if guards.receiving_routes.contains(&route) {
            return false;
        }
        let others: Vec<Value> = self
            .host
            .store
            .records
            .read()
            .await
            .values()
            .cloned()
            .collect();
        for m in others {
            if crate::storage::route_key(&m) == route
                && (matches!(m["status"].as_str(), Some("queued" | "processing"))
                    || self.host.busy(m["id"].as_str().unwrap_or_default()).await)
            {
                return false;
            }
        }
        true
    }

    /// Python `_reclaim`: the same cycle lock and move engine as scheduled archiving; no recovery or pending
    /// deletions are part of capacity admission.
    pub(crate) async fn reclaim(
        &self,
        ctx: &Ctx,
        result: &mut CycleResult,
        r: &Reclaim,
    ) -> Option<&'static str> {
        {
            let mut st = lock(&self.st);
            st.cycle_failed = false;
            st.last_cycle_at = Some(now());
        }
        let s = &ctx.settings;
        if !ctx.remote() || s.effective_mode() != Mode::Move || !s.allow_local_delete {
            return Some("disabled");
        }
        if self.enough(ctx, r).await {
            return Some("space_available");
        }
        if let Some(f) = self.remote_state(ctx).await {
            return Some(self.failed(f));
        }
        // Pinned recordings move too, but only after every other candidate (a stable sort keeps the upload order).
        let mut rows = self.candidates(ctx).await;
        let pins = self.host.pins().await;
        rows.sort_by_key(|(id, _)| pins.contains(id));
        for (id, meta) in rows {
            if !self.eligible(&meta, r).await {
                continue;
            }
            for kind in ARCHIVE_ORDER {
                if self.enough(ctx, r).await {
                    return Some("space_available");
                }
                if result.archived >= s.batch as u64 {
                    return Some("batch_limit");
                }
                if index::location(&meta, kind) != "local" || self.done_remotely(ctx, &meta, kind) {
                    continue;
                }
                let size = match std::fs::metadata(self.host.folder(&id).join(kind)) {
                    Ok(m) => m.len(),
                    Err(_) => continue,
                };
                if size == 0 {
                    continue;
                }
                match self
                    .archive_remote(ctx, &id, &meta, kind, false, Some(r))
                    .await
                {
                    Outcome::Archived => {
                        result.archived += 1;
                        result.bytes += size;
                        lock(&self.st).archived_files_total += 1;
                    }
                    Outcome::Changed => break,
                    Outcome::Failed(reason) => return Some(self.failed(reason)),
                }
                if self.stopping() {
                    return Some("shutdown");
                }
            }
        }
        Some(if self.enough(ctx, r).await {
            "space_available"
        } else {
            "no_eligible_files"
        })
    }

    /// Python `request_reclaim`: one coalesced goal for the existing archiver, never a wait under admission locks.
    pub fn request_reclaim(
        &self,
        extra: u64,
        incoming: &[String],
        protected: &BTreeSet<String>,
        exclude: Option<&str>,
    ) {
        let s = self.settings();
        if !s.enabled || s.effective_mode() != Mode::Move {
            return;
        }
        self.start_cycle(Some(Reclaim {
            extra,
            exclude: exclude.map(str::to_owned),
            incoming: incoming.to_vec(),
            protected: protected.clone(),
        }));
    }

    /// Whether admission takes Python's `drive_move` branch, and the local threshold it uses.
    pub fn drive_move(&self) -> Option<u64> {
        let s = self.settings();
        (s.remote() && s.effective_mode() == Mode::Move).then_some(s.local_threshold_bytes)
    }
}
