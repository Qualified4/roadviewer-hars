//! The Drive side of the archiver (Python `archive_remote`, `_archive_remote_files`, `_recover_remote`, `_reclaim`).
//! Drive is only called outside the record locks; under a recording's lock its index is re-read and committed, and
//! a local file goes only after its verified copy is committed, in move mode with deleting explicitly allowed.
use super::{
    ArchiveRead, ArchiveWrite, BackendError, ErrorCode, RemoteRef, SessionUri, State, UploadResume,
    Verdict,
    archiver::{
        Archive, Ctx, CycleResult, Outcome, Reclaim, file_digests, kind_static, lock, stat,
    },
    completion_matches,
    index::{self, ARCHIVE_ORDER},
    settings::Mode,
};
use crate::store::now;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Instant,
};

/// Drive uploads in flight at once (scheduled and manual runs).
pub const UPLOAD_WORKERS: usize = 3;
/// An upload in progress is recorded when its session starts, then at most once per this many bytes or seconds.
pub const PROGRESS_BYTES: u64 = 8 << 20;
pub const PROGRESS_SECONDS: f64 = 30.0;

/// Python `REMOTE_ERRORS`: a backend error code as a cycle failure.
pub fn remote_reason(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Unavailable => "storage_unavailable",
        ErrorCode::AuthExpired => "auth_expired",
        ErrorCode::RateLimited => "rate_limited",
        ErrorCode::QuotaExceeded => "remote_full",
        ErrorCode::Mismatch => "hash_mismatch",
        ErrorCode::Io | ErrorCode::NotFound => "os_error",
    }
}

/// One file's way to Drive. Until `Verified` is committed the local file is the only trusted copy.
enum Stage {
    Start,
    Hashed {
        size: u64,
        mtime: u64,
        sha256: String,
        md5: Option<String>,
    },
    /// A copy on Drive (adopted or just uploaded), not yet checked against the local content.
    Located {
        size: u64,
        mtime: u64,
        sha256: String,
        md5: Option<String>,
        remote: RemoteRef,
    },
    Verified {
        size: u64,
        mtime: u64,
        sha256: String,
        md5: Option<String>,
        remote: RemoteRef,
        verified_by: &'static str,
    },
    Done,
}

/// What the upload's progress callback hands to the task that records it in the index.
#[derive(Default)]
struct Progress {
    latest: Option<UploadResume>,
    changed: bool,
    done: bool,
}

pub(crate) fn resume_of(pending: &Value) -> UploadResume {
    UploadResume {
        session: pending["session"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| SessionUri(s.to_owned())),
        bytes_sent: pending["bytes_sent"].as_u64().unwrap_or(0),
        sha256: pending["sha256"].as_str().map(str::to_owned),
        size: pending["size"].as_u64(),
    }
}

impl Archive {
    pub(crate) async fn remote_cycle(
        &self,
        ctx: &Ctx,
        result: &mut CycleResult,
    ) -> Option<&'static str> {
        lock(&self.st).cycle_failed = false;
        if let Some(f) = self.remote_state(ctx).await {
            return Some(self.failed(f));
        }
        lock(&self.st).last_cycle_at = Some(now());
        self.startup(ctx).await;
        let failure = lock(&self.st).startup_failure;
        if let Some(f) = failure.or(self.remote_state(ctx).await) {
            return Some(self.failed(f));
        }
        // The settings panel shows the figure measured here; a request never asks.
        let _ = ctx.backend.usage().await;
        if super::pending::PendingDeletes::new(&self.root)
            .process_remote(&*ctx.backend)
            .await
            .is_err()
        {
            tracing::warn!("pending_archive_deletions_failed");
        }
        {
            let mut st = lock(&self.st);
            st.cleanup_last_reason = None;
            st.cleanup_error = None;
        }
        self.requeue_deferred().await;
        // Kept in copy mode too: copying starts once local storage passes the threshold (it frees nothing).
        self.archive_pass(ctx, result).await
    }

    /// None when the backend is connected, else the cycle's failure.
    pub(crate) async fn remote_state(&self, ctx: &Ctx) -> Option<&'static str> {
        let probe = ctx.backend.probe(true).await;
        if probe.state == State::Connected {
            lock(&self.st).last_auth_error = None;
            return None;
        }
        match probe.reason.as_deref() {
            Some("auth_expired") => {
                lock(&self.st).last_auth_error = Some("auth_expired".into());
                Some("auth_expired")
            }
            Some("rate_limited") => Some("rate_limited"),
            _ => Some("storage_unavailable"),
        }
    }

    pub(crate) fn remote_fail(&self, ctx: &Ctx, e: &BackendError) -> &'static str {
        let reason = remote_reason(e.code);
        tracing::warn!(error = %e, "archive_to_drive_failed");
        if reason == "auth_expired" {
            lock(&self.st).last_auth_error = Some(e.reason().unwrap_or("auth_expired").to_owned());
        }
        if matches!(reason, "storage_unavailable" | "os_error" | "auth_expired") {
            ctx.backend.invalidate();
        }
        reason
    }

    /// A local file the remote path leaves alone: one whose upload did not verify waits for attention; in copy
    /// mode one with a verified copy is done.
    pub(crate) fn done_remotely(&self, ctx: &Ctx, meta: &Value, kind: &str) -> bool {
        if index::entry(meta, kind)["error"] == "remote_mismatch" {
            return true;
        }
        index::remote(meta, kind).is_some() && ctx.settings.effective_mode() == Mode::Copy
    }

    pub(crate) fn remote_ref(&self, ctx: &Ctx, block: &Value) -> Option<RemoteRef> {
        let file_id = block["file_id"].as_str().filter(|s| !s.is_empty())?;
        Some(RemoteRef::new(
            ctx.backend.caps().kind,
            file_id,
            index::bytes_of(block),
            block["sha256"].as_str().map(str::to_owned),
            block["md5"].as_str().map(str::to_owned),
        ))
    }

    /// ('ok' | 'mismatch', verified_by). A fresh upload's completion answer can tell; else Drive is asked, and
    /// without a checksum from it the copy is read back and hashed.
    pub(crate) async fn verify(
        &self,
        ctx: &Ctx,
        r: &RemoteRef,
        sha256: &str,
        size: u64,
        md5: Option<&str>,
    ) -> super::Result<Option<&'static str>> {
        let by = if r.sha256.is_some() { "sha256" } else { "md5" };
        if completion_matches(r, Some(sha256), Some(size), md5) {
            return Ok(Some(by));
        }
        match ctx.backend.verify(r, Some(sha256), Some(size), md5).await? {
            Verdict::Ok => Ok(Some(by)),
            Verdict::Mismatch => Ok(None),
            Verdict::Unverifiable => {
                use sha2::Digest;
                let mut reader = ctx.backend.open(r, 0, None).await?;
                let mut h = sha2::Sha256::new();
                while let Some(chunk) = reader.next_chunk().await? {
                    h.update(&chunk);
                }
                Ok((hex::encode(h.finalize()) == sha256).then_some("download"))
            }
        }
    }

    /// Caller holds the record lock. The current index when the recording is still the one archived from.
    pub(crate) async fn unchanged(
        &self,
        id: &str,
        kind: &str,
        revision: &Value,
        file: Option<(u64, u64)>,
    ) -> Option<Value> {
        let current = self.host.meta(id).await?;
        if current["status"] != "ready"
            || index::revision(&current) != *revision
            || index::location(&current, kind) != "local"
        {
            return None;
        }
        if let Some(file) = file
            && stat(&self.host.folder(id).join(kind)).ok() != Some(file)
        {
            return None;
        }
        Some(current)
    }

    /// Python `archive_remote`, steps 1-6. `adopt_only` (recovery): only a copy find() returns is verified and
    /// committed; nothing is uploaded. `guard` (capacity): the move happens only while the recording stays eligible.
    pub(crate) async fn archive_remote(
        &self,
        ctx: &Ctx,
        id: &str,
        meta: &Value,
        kind: &str,
        adopt_only: bool,
        guard: Option<&Reclaim>,
    ) -> Outcome {
        let kind = kind_static(kind);
        let source = self.host.folder(id).join(kind);
        let revision = index::revision(meta);
        let mut stage = Stage::Start;
        loop {
            stage = match stage {
                Stage::Start => {
                    let Ok((size, mtime)) = stat(&source) else {
                        return Outcome::Failed("os_error");
                    };
                    // The recorded sha256 when the file is unchanged since (md5 then comes from the verified copy).
                    let (sha256, md5) = match index::fresh_hash(meta, kind, size, mtime) {
                        Some(h) => (h, None),
                        None => match file_digests(&source).await {
                            Ok((s, m)) => (s, Some(m)),
                            Err(_) => return Outcome::Failed("os_error"),
                        },
                    };
                    Stage::Hashed {
                        size,
                        mtime,
                        sha256,
                        md5,
                    }
                }
                Stage::Hashed {
                    size,
                    mtime,
                    sha256,
                    md5,
                } => {
                    // A copy whose completion answer or index commit was lost is adopted instead of uploaded again.
                    let found = match ctx.backend.find(id, kind).await {
                        Ok(f) => f,
                        Err(e) => return Outcome::Failed(self.remote_fail(ctx, &e)),
                    };
                    let found = found
                        .into_iter()
                        .find(|r| r.sha256.as_deref() == Some(sha256.as_str()) && r.bytes == size);
                    match found {
                        Some(remote) => Stage::Located {
                            size,
                            mtime,
                            sha256,
                            md5,
                            remote,
                        },
                        None if adopt_only => return Outcome::Changed,
                        None => match self
                            .upload(
                                ctx,
                                id,
                                kind,
                                meta,
                                &revision,
                                size,
                                &sha256,
                                md5.as_deref(),
                            )
                            .await
                        {
                            Ok(remote) => Stage::Located {
                                size,
                                mtime,
                                sha256,
                                md5,
                                remote,
                            },
                            Err(outcome) => return outcome,
                        },
                    }
                }
                Stage::Located {
                    size,
                    mtime,
                    sha256,
                    md5,
                    remote,
                } => {
                    match self
                        .verify(ctx, &remote, &sha256, size, md5.as_deref())
                        .await
                    {
                        Ok(Some(verified_by)) => Stage::Verified {
                            size,
                            mtime,
                            sha256,
                            md5,
                            remote,
                            verified_by,
                        },
                        Ok(None) => {
                            // The copy stays on Drive (never deleted automatically) and the local file stays.
                            tracing::warn!(kind, "drive_copy_differs_from_local");
                            self.drop_pending(id, kind, &revision, true).await;
                            return Outcome::Failed("hash_mismatch");
                        }
                        Err(e) => {
                            if matches!(e.code, ErrorCode::NotFound | ErrorCode::Mismatch) {
                                self.drop_pending(
                                    id,
                                    kind,
                                    &revision,
                                    e.code == ErrorCode::Mismatch,
                                )
                                .await;
                            }
                            return Outcome::Failed(self.remote_fail(ctx, &e));
                        }
                    }
                }
                Stage::Verified {
                    size,
                    mtime,
                    sha256,
                    md5,
                    remote,
                    verified_by,
                } => {
                    match self
                        .commit(
                            ctx,
                            id,
                            kind,
                            &revision,
                            (size, mtime),
                            &sha256,
                            md5,
                            &remote,
                            verified_by,
                            guard,
                        )
                        .await
                    {
                        Some(outcome) => return outcome,
                        None => Stage::Done,
                    }
                }
                Stage::Done => return Outcome::Archived,
            };
        }
    }

    /// Step 3: the resumable upload; the session is recorded in the index as soon as it exists, its progress at
    /// most every PROGRESS_BYTES or PROGRESS_SECONDS, by a task beside the upload (the callback cannot wait).
    #[allow(clippy::too_many_arguments)]
    async fn upload(
        &self,
        ctx: &Ctx,
        id: &str,
        kind: &'static str,
        meta: &Value,
        revision: &Value,
        size: u64,
        sha256: &str,
        md5: Option<&str>,
    ) -> Result<RemoteRef, Outcome> {
        let recorded = index::remote_pending(meta, kind);
        let backend_kind = ctx.backend.caps().kind.as_str();
        let pending = recorded.filter(|p| p["backend"] == backend_kind);
        let mut resume = pending.as_ref().map(resume_of).unwrap_or_default();
        let started_at = pending
            .as_ref()
            .and_then(|p| p["started_at"].as_f64())
            .unwrap_or_else(now);
        let shared = Arc::new(Mutex::new(Progress::default()));
        let wake = Arc::new(tokio::sync::Notify::new());
        let transfer = self
            .manual_active()
            .then(|| lock(&self.transfer).clone())
            .flatten();
        let key = (id.to_owned(), kind.to_owned());
        if let Some(t) = &transfer {
            t.begin(
                if resume.session.is_some() {
                    resume.bytes_sent
                } else {
                    0
                },
                &key,
            );
        }
        let mut last = (resume.session.clone(), resume.bytes_sent, Instant::now());
        let (cb_shared, cb_wake, cb_transfer, cb_key) =
            (shared.clone(), wake.clone(), transfer.clone(), key.clone());
        let mut on_progress = move |sent: u64, r: &UploadResume| -> super::Result<()> {
            if let Some(t) = &cb_transfer {
                t.progress(sent, &cb_key);
            }
            let mut p = lock(&cb_shared);
            if p.changed {
                return Err(BackendError::new(ErrorCode::Mismatch, "local_changed"));
            }
            let Some(session) = r.session.clone() else {
                return Ok(());
            };
            if r.size.is_some_and(|s| s > 0 && sent >= s) {
                return Ok(()); // Complete: the commit follows.
            }
            if last.0.as_ref() == Some(&session)
                && sent.saturating_sub(last.1) < PROGRESS_BYTES
                && last.2.elapsed().as_secs_f64() < PROGRESS_SECONDS
            {
                return Ok(());
            }
            last = (Some(session), sent, Instant::now());
            let mut snapshot = r.clone();
            snapshot.bytes_sent = sent;
            p.latest = Some(snapshot);
            cb_wake.notify_one();
            Ok(())
        };
        let first_session = resume.session.clone();
        let record = async {
            let mut started = (first_session, started_at);
            loop {
                wake.notified().await;
                let (latest, done) = {
                    let mut p = lock(&shared);
                    (p.latest.take(), p.done)
                };
                if let Some(r) = latest {
                    if r.session != started.0 {
                        started = (r.session.clone(), now());
                    }
                    let fields = json!({"backend": backend_kind, "session": r.session.as_ref().map(|s| s.expose()),
                        "started_at": started.1, "bytes_sent": r.bytes_sent, "sha256": r.sha256, "size": r.size});
                    let l = self.host.store.record_lock(id).await;
                    let _g = l.lock().await;
                    match self.unchanged(id, kind, revision, None).await {
                        None => lock(&shared).changed = true,
                        Some(mut current) => {
                            index::set_remote_pending(&mut current, kind, fields);
                            let _ = self.host.save(current).await;
                        }
                    }
                }
                if done {
                    return;
                }
            }
        };
        let send = async {
            let out = ctx
                .backend
                .upload(
                    &self.host.folder(id).join(kind),
                    id,
                    kind,
                    Some(sha256),
                    md5,
                    size,
                    &mut resume,
                    &mut on_progress,
                )
                .await;
            lock(&shared).done = true;
            wake.notify_one();
            out
        };
        let (out, ()) = tokio::join!(send, record);
        if let Some(t) = &transfer
            && out.is_err()
        {
            t.end(0, &key);
        }
        let changed = lock(&shared).changed;
        match out {
            Ok(r) if !changed => Ok(r),
            Ok(_) => Err(Outcome::Changed),
            Err(e) if changed || e.reason() == Some("local_changed") => Err(Outcome::Changed),
            Err(e) => {
                // A completed upload whose file is gone or is not the local content: the session is dropped, or
                // every later cycle would stop at the same file. Only a wrong content marks the entry.
                if matches!(e.code, ErrorCode::NotFound | ErrorCode::Mismatch) {
                    self.drop_pending(id, kind, revision, e.code == ErrorCode::Mismatch)
                        .await;
                }
                Err(Outcome::Failed(self.remote_fail(ctx, &e)))
            }
        }
    }

    /// Steps 5-6 under the record lock. None when committed (and, when allowed, the local file removed).
    #[allow(clippy::too_many_arguments)]
    async fn commit(
        &self,
        ctx: &Ctx,
        id: &str,
        kind: &'static str,
        revision: &Value,
        file: (u64, u64),
        sha256: &str,
        md5: Option<String>,
        remote: &RemoteRef,
        verified_by: &str,
        guard: Option<&Reclaim>,
    ) -> Option<Outcome> {
        let l = self.host.store.record_lock(id).await;
        let _g = l.lock().await;
        // The copy on Drive is adopted by find() if the content comes back.
        let Some(mut current) = self.unchanged(id, kind, revision, Some(file)).await else {
            return Some(Outcome::Changed);
        };
        let allowed = match guard {
            Some(r) => self.eligible(&current, r).await,
            None => true,
        };
        let s = &ctx.settings;
        let movable = s.effective_mode() == Mode::Move
            && s.allow_local_delete
            && current["status"] == "ready"
            && !self.host.busy(id).await
            && allowed;
        let md5 = remote.md5.clone().or(md5);
        let file_id = remote.file_id.clone().unwrap_or_default();
        if movable {
            index::set_location(
                &mut current,
                kind,
                json!({"location": "gdrive", "file_id": file_id, "bytes": file.0, "sha256": sha256, "md5": md5, "archived_at": now()}),
            );
        } else {
            index::set_remote(
                &mut current,
                kind,
                json!({"backend": ctx.backend.caps().kind.as_str(), "file_id": file_id, "bytes": file.0, "sha256": sha256,
                       "md5": md5, "uploaded_at": now(), "verified_by": verified_by}),
            );
            index::clear_remote_pending(&mut current, kind);
        }
        // The hash just verified, so a later start knows this file without reading it again.
        index::record_hash(&mut current, kind, file.0, file.1, sha256);
        // Flushed to disk before the local file goes: after a power loss the index must not still say local.
        if self.host.save(current).await.is_err() {
            return Some(Outcome::Failed("os_error"));
        }
        if !movable {
            lock(&self.st).remote_copies_total += 1;
            return None;
        }
        if std::fs::remove_file(self.host.folder(id).join(kind)).is_err() {
            tracing::warn!(kind, "archived_file_stays_local_until_recovery");
            lock(&self.st).recovered = false;
        }
        None
    }

    /// Clears the upload in progress; with `mismatch` the entry is marked remote_mismatch (kept until attended).
    async fn drop_pending(&self, id: &str, kind: &str, revision: &Value, mismatch: bool) {
        let l = self.host.store.record_lock(id).await;
        let _g = l.lock().await;
        let Some(mut current) = self.unchanged(id, kind, revision, None).await else {
            return;
        };
        if mismatch {
            index::set_error(&mut current, kind, "remote_mismatch");
        } else if index::remote_pending(&current, kind).is_none() {
            return;
        }
        index::clear_remote_pending(&mut current, kind);
        let _ = self.host.save(current).await;
    }

    /// Python `_archive_remote_files`: `_archive` for Drive with up to UPLOAD_WORKERS files in flight. This task
    /// picks the files in order and settles every result itself; a pass never returns while a worker runs.
    pub(crate) async fn archive_remote_files(
        &self,
        ctx: &Ctx,
        result: &mut CycleResult,
    ) -> Option<&'static str> {
        let manual = self.manual_active();
        let rows = self.candidates(ctx).await;
        let mut set = tokio::task::JoinSet::new();
        let mut flight: BTreeMap<(String, String), u64> = BTreeMap::new();
        let mut held: BTreeSet<String> = BTreeSet::new();
        let mut stop: Option<&'static str> = None;
        let me = self.arc();
        'rows: for (id, meta) in rows {
            for kind in ARCHIVE_ORDER {
                if stop.is_some() || held.contains(&id) {
                    break;
                }
                if index::location(&meta, kind) != "local" || self.done_remotely(ctx, &meta, kind) {
                    continue;
                }
                while !manual
                    && !flight.is_empty()
                    && result.archived + flight.len() as u64 >= ctx.settings.batch as u64
                {
                    self.settle(
                        &mut set,
                        &mut flight,
                        &mut held,
                        &mut stop,
                        result,
                        manual,
                        ctx,
                    )
                    .await;
                }
                if stop.is_some() || held.contains(&id) {
                    break;
                }
                if !manual && result.archived >= ctx.settings.batch as u64 {
                    break 'rows;
                }
                let size = match std::fs::metadata(self.host.folder(&id).join(kind)) {
                    Ok(m) => m.len(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => {
                        stop = Some("os_error");
                        break;
                    }
                };
                if size == 0 {
                    // Drive takes an empty upload as a status query and never completes it: the file stays local.
                    if lock(&self.st).empty.insert((id.clone(), kind.to_owned())) {
                        tracing::warn!(kind, "empty_file_not_archived_to_drive");
                    }
                    continue;
                }
                if !flight.is_empty() {
                    // Uploads come first and shutdown waits for nothing new: no further file starts.
                    if self.host.upload_receiving().await {
                        stop = Some("uploading");
                        break;
                    }
                    if self.stopping() {
                        stop = Some("shutdown");
                        break;
                    }
                }
                let key = (id.clone(), kind.to_owned());
                flight.insert(key.clone(), size);
                let (me, ctx2, meta2) = (me.clone(), ctx.clone(), meta.clone());
                set.spawn(async move {
                    let out = me
                        .archive_remote(&ctx2, &key.0, &meta2, &key.1, false, None)
                        .await;
                    (key, out)
                });
                while flight.len() >= UPLOAD_WORKERS {
                    self.settle(
                        &mut set,
                        &mut flight,
                        &mut held,
                        &mut stop,
                        result,
                        manual,
                        ctx,
                    )
                    .await;
                }
            }
            if stop.is_some() {
                break;
            }
        }
        while !flight.is_empty() {
            self.settle(
                &mut set,
                &mut flight,
                &mut held,
                &mut stop,
                result,
                manual,
                ctx,
            )
            .await;
        }
        stop
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle(
        &self,
        set: &mut tokio::task::JoinSet<((String, String), Outcome)>,
        flight: &mut BTreeMap<(String, String), u64>,
        held: &mut BTreeSet<String>,
        stop: &mut Option<&'static str>,
        result: &mut CycleResult,
        manual: bool,
        ctx: &Ctx,
    ) {
        let Some(joined) = set.join_next().await else {
            flight.clear();
            return;
        };
        let (key, outcome) = match joined {
            Ok(v) => v,
            Err(_) => {
                // A worker that panicked: nothing of its file was committed past what its own steps fsync'd.
                stop.get_or_insert("os_error");
                return;
            }
        };
        let size = flight.remove(&key).unwrap_or(0);
        let transfer = lock(&self.transfer).clone();
        match outcome {
            Outcome::Archived => {
                result.archived += 1;
                result.bytes += size;
                lock(&self.st).archived_files_total += 1;
                if manual {
                    self.job_file(ctx, &key.0, &key.1, size, true, None).await;
                }
                if stop.is_none() && self.host.upload_receiving().await {
                    *stop = Some("uploading");
                }
                if stop.is_none() && self.stopping() {
                    *stop = Some("shutdown");
                }
            }
            Outcome::Changed => {
                if let Some(t) = transfer {
                    t.end(0, &key);
                }
                held.insert(key.0);
            }
            Outcome::Failed(reason) => {
                if stop.is_some() && !(manual && reason == "hash_mismatch") {
                    if let Some(t) = transfer {
                        t.end(0, &key);
                    }
                    return;
                }
                if manual {
                    self.job_file(ctx, &key.0, &key.1, size, false, Some(reason))
                        .await;
                }
                if reason == "hash_mismatch" && manual {
                    held.insert(key.0);
                    return;
                }
                *stop = Some(reason);
            }
        }
    }
}
