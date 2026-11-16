//! The share side of the archiver (Python `archive_file`, `cleanup_nas`, `_recover` and the missing-file rules).
//! Every write goes through the HARS write root: a share is only touched while its probe answers connected, and
//! the probe requires the sentinel's `owner: "roadviewer-hars"`.
use super::{
    ErrorCode, NasBackend,
    archiver::{
        Archive, Ctx, CycleResult, Outcome, file_sha256, is_file, kind_static, lock, share_remove,
        share_stat, stat,
    },
    index::{self, ARCHIVE_ORDER, VIDEO_KINDS},
    nas::Copied,
};
use crate::store::now;
use serde_json::{Value, json};

/// One file's way to the share. Until `Committed` the local file is the only trusted copy and is never touched.
enum Step {
    Start,
    Hashed {
        size: u64,
        mtime: u64,
        sha256: String,
    },
    Copied {
        size: u64,
        mtime: u64,
        sha256: String,
        written: bool,
    },
    Committed,
}

fn over(ctx: &Ctx, used: u64, free: u64) -> Option<&'static str> {
    let s = &ctx.settings;
    if s.nas_max_bytes > 0 && used > s.nas_max_bytes {
        Some("nas_max")
    } else if free < s.nas_margin_bytes {
        Some("nas_margin")
    } else {
        None
    }
}

impl Archive {
    /// Python `_archive` for the share: one file at a time, oldest recording first, videos before the log.
    pub(crate) async fn archive_nas_files(
        &self,
        ctx: &Ctx,
        result: &mut CycleResult,
    ) -> Option<&'static str> {
        let manual = self.manual_active();
        for (id, meta) in self.candidates(ctx).await {
            for kind in ARCHIVE_ORDER {
                if !manual && result.archived >= ctx.settings.batch as u64 {
                    return None;
                }
                if index::location(&meta, kind) != "local" {
                    continue;
                }
                let size = match std::fs::metadata(self.host.folder(&id).join(kind)) {
                    Ok(m) => m.len(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => return Some("os_error"),
                };
                match self.archive_file(ctx, &id, &meta, kind).await {
                    Outcome::Archived => {
                        result.archived += 1;
                        result.bytes += size;
                        lock(&self.st).archived_files_total += 1;
                        if manual {
                            self.job_file(ctx, &id, kind, size, true, None).await;
                        }
                        // Uploads come first: the file in progress is finished, then the cycle ends.
                        if self.host.upload_receiving().await {
                            return Some("uploading");
                        }
                        if self.stopping() {
                            return Some("shutdown");
                        }
                    }
                    // The recording changed meanwhile: its other files wait for the next cycle.
                    Outcome::Changed => break,
                    Outcome::Failed(reason) => {
                        if manual {
                            self.job_file(ctx, &id, kind, size, false, Some(reason))
                                .await;
                            if reason == "hash_mismatch" {
                                break;
                            }
                        }
                        return Some(reason);
                    }
                }
            }
        }
        None
    }

    fn fail_nas(&self, nas: &NasBackend, reason: &'static str) -> Outcome {
        if matches!(reason, "storage_unavailable" | "os_error") {
            super::ArchiveRead::invalidate(nas);
        }
        Outcome::Failed(reason)
    }

    /// Python `archive_file`: hash, copy to `.part` + fsync, read back and compare, rename, commit the index, unlink.
    pub(crate) async fn archive_file(
        &self,
        ctx: &Ctx,
        id: &str,
        meta: &Value,
        kind: &str,
    ) -> Outcome {
        let Some(nas) = ctx.nas() else {
            return Outcome::Failed("storage_unavailable");
        };
        let kind = kind_static(kind);
        let source = self.host.folder(id).join(kind);
        let revision = index::revision(meta);
        let mut step = Step::Start;
        loop {
            step = match step {
                Step::Start => {
                    if !self.nas_available(nas).await {
                        return self.fail_nas(nas, "storage_unavailable");
                    }
                    let Ok((size, mtime)) = stat(&source) else {
                        return self.fail_nas(nas, "os_error");
                    };
                    let sha256 = match index::fresh_hash(meta, kind, size, mtime) {
                        Some(h) => h,
                        None => match file_sha256(&source).await {
                            Ok(h) => h,
                            Err(_) => return self.fail_nas(nas, "os_error"),
                        },
                    };
                    Step::Hashed {
                        size,
                        mtime,
                        sha256,
                    }
                }
                Step::Hashed {
                    size,
                    mtime,
                    sha256,
                } => {
                    // A copy left by a crash before its index was written is reused when it is identical.
                    let copied = nas
                        .copy_file(&source, id, kind, Some(&sha256), size, &mut |_, _| Ok(()))
                        .await;
                    match copied {
                        Ok(how) => Step::Copied {
                            size,
                            mtime,
                            sha256,
                            written: how == Copied::Written,
                        },
                        Err(e) => {
                            let reason = match e.code {
                                ErrorCode::QuotaExceeded => "nas_full",
                                ErrorCode::Mismatch => "hash_mismatch",
                                ErrorCode::Unavailable => "storage_unavailable",
                                _ if e.is_foreign() => "storage_unavailable",
                                _ => "os_error",
                            };
                            tracing::warn!(kind, error = %e, "archive_copy_failed");
                            return self.fail_nas(nas, reason);
                        }
                    }
                }
                Step::Copied {
                    size,
                    mtime,
                    sha256,
                    written,
                } => {
                    if !nas.confirm(id, kind, size, written).await {
                        return self.fail_nas(nas, "storage_unavailable");
                    }
                    let l = self.host.store.record_lock(id).await;
                    let _g = l.lock().await;
                    let current = self.host.meta(id).await;
                    let unchanged = current.as_ref().is_some_and(|c| {
                        c["status"] == "ready"
                            && index::revision(c) == revision
                            && index::location(c, kind) == "local"
                            && stat(&source).ok() == Some((size, mtime))
                    });
                    if !unchanged {
                        drop(_g);
                        // Converted again, deleted or replaced meanwhile: no index refers to the copy.
                        if current.is_none_or(|c| index::location(&c, kind) != "nas") {
                            self.remove_nas_copy(nas, id, kind).await;
                        }
                        return Outcome::Changed;
                    }
                    let before = current.expect("checked");
                    let mut current = before.clone();
                    index::set_location(
                        &mut current,
                        kind,
                        json!({"location": "nas", "bytes": size, "sha256": sha256, "archived_at": now()}),
                    );
                    // Flushed to disk before the local file goes: after a power loss the index must not still say local.
                    if self.host.save(current).await.is_err() {
                        // The copy on the share is an orphan, reused next time.
                        return self.fail_nas(nas, "os_error");
                    }
                    // The only other copy must still be on the share when the local one goes.
                    if !nas.confirm(id, kind, size, false).await {
                        if self.host.save(before).await.is_err() {
                            tracing::warn!(kind, "archive_index_not_restored");
                            lock(&self.st).recovered = false;
                        }
                        return self.fail_nas(nas, "storage_unavailable");
                    }
                    if std::fs::remove_file(&source).is_err() {
                        tracing::warn!(kind, "archived_file_stays_local_until_recovery");
                        lock(&self.st).recovered = false;
                    }
                    Step::Committed
                }
                Step::Committed => return Outcome::Archived,
            };
        }
    }

    /// A copy no index refers to; only on a connected (HARS-owned) root.
    async fn remove_nas_copy(&self, nas: &NasBackend, id: &str, kind: &str) {
        if self.nas_available(nas).await
            && let Ok(path) = nas.path_for(id, kind)
        {
            let _ = share_remove(&path).await;
        }
    }

    async fn nas_usage(&self, nas: &NasBackend) -> (u64, u64) {
        super::ArchiveRead::invalidate(nas);
        match super::ArchiveRead::usage(nas).await {
            Ok(u) => (u.used, u.free.unwrap_or(0)),
            Err(_) => (0, 0),
        }
    }

    /// Python `cleanup_nas`: over its limit, archived videos of unpinned recordings go first, oldest first; the
    /// original logs only after every video. Local files and the rest of the index are never touched.
    pub(crate) async fn cleanup_nas(&self, ctx: &Ctx) -> u64 {
        {
            let mut st = lock(&self.st);
            st.cleanup_last_reason = None;
            st.cleanup_error = None;
        }
        let Some(nas) = ctx.nas() else { return 0 };
        if !ctx.settings.enabled {
            return 0;
        }
        let (mut used, mut free) = self.nas_usage(nas).await;
        if used == 0 && free == 0 {
            // (0, 0) from a reachable share is a read error (a real share holds at least the sentinel).
            if self.nas_available(nas).await {
                lock(&self.st).cleanup_error = Some("os_error");
            }
            return 0;
        }
        if used == 0 {
            return 0;
        }
        let reason = over(ctx, used, free);
        lock(&self.st).cleanup_last_reason = reason;
        if reason.is_none() {
            return 0;
        }
        if self.manual_active() {
            // A manual run never deletes archived copies: over its limit, the share takes nothing more.
            lock(&self.st).cleanup_error = Some("nas_full");
            return 0;
        }
        let rows = self.cleanup_candidates().await;
        let (mut removed, mut left, mut measured) = (0u64, false, true);
        let groups: [&[&str]; 2] = [&VIDEO_KINDS, &["rlog.zst"]];
        for kinds in groups {
            if !measured {
                // The estimate may be off (sizes from the index): the irreplaceable logs are only considered
                // against a fresh measurement.
                (used, free) = self.nas_usage(nas).await;
                measured = true;
                if used == 0 && free == 0 {
                    let e = if self.nas_available(nas).await {
                        "os_error"
                    } else {
                        "storage_unavailable"
                    };
                    lock(&self.st).cleanup_error = Some(e);
                    return removed;
                }
            }
            for (id, m) in &rows {
                if over(ctx, used, free).is_none() {
                    left = true;
                    break;
                }
                for kind in kinds {
                    let e = index::entry(m, kind);
                    if e["location"] != "nas" {
                        continue;
                    }
                    // An index without a size would leave the estimate unchanged: measure the file instead.
                    let size = match (index::bytes_of(&e), nas.path_for(id, kind)) {
                        (0, Ok(p)) => share_stat(&p).await.map_or(0, |m| m.len()),
                        (n, _) => n,
                    };
                    match self.remove_archived(nas, id, kind, &e).await {
                        None => return removed,
                        Some(true) => {
                            removed += 1;
                            lock(&self.st).removed_files_total += 1;
                            used = used.saturating_sub(size);
                            free += size;
                            measured = false;
                        }
                        Some(false) => {}
                    }
                }
            }
            if left {
                break;
            }
        }
        if !measured {
            (used, free) = self.nas_usage(nas).await;
        }
        if used > 0 && over(ctx, used, free).is_some() {
            lock(&self.st).cleanup_error = Some(if left {
                "nas_full"
            } else {
                "nas_full_no_candidates"
            });
        }
        removed
    }

    async fn cleanup_candidates(&self) -> Vec<(String, Value)> {
        let pins = self.host.pins().await;
        let mut rows = Vec::new();
        for id in self.host.ids().await {
            let Some(m) = self.host.meta(&id).await else {
                continue;
            };
            // Pinned recordings are protected; queued or converting ones still need their inputs.
            if ARCHIVE_ORDER
                .iter()
                .any(|k| index::location(&m, k) == "nas")
                && !pins.contains(&id)
                && !matches!(m["status"].as_str(), Some("queued" | "processing"))
            {
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

    /// Caller holds the record lock. The indexed copy is still the one chosen; with `guarded` also nothing protects
    /// (pinned) or reads it (conversion running or queued).
    async fn removable(&self, id: &str, kind: &str, e: &Value, guarded: bool) -> Option<Value> {
        let current = self.host.meta(id).await?;
        if guarded
            && (self.host.pins().await.contains(id)
                || self.host.busy(id).await
                || matches!(current["status"].as_str(), Some("queued" | "processing")))
        {
            return None;
        }
        let now_ = index::entry(&current, kind);
        (now_["location"] == "nas"
            && now_.get("sha256") == e.get("sha256")
            && now_.get("bytes") == e.get("bytes"))
        .then_some(current)
    }

    /// Some(true) when removed and indexed as removed, Some(false) when skipped, None when cleanup must stop. The
    /// intent is flushed to the index before the unlink: recovery confirms a removal only for an entry carrying it.
    async fn remove_archived(
        &self,
        nas: &NasBackend,
        id: &str,
        kind: &str,
        e: &Value,
    ) -> Option<bool> {
        {
            let l = self.host.store.record_lock(id).await;
            let _g = l.lock().await;
            let Some(mut current) = self.removable(id, kind, e, true).await else {
                return Some(false);
            };
            let mut marked = index::entry(&current, kind);
            marked["removing"] = json!(true);
            index::set_location(&mut current, kind, marked);
            if self.host.save(current).await.is_err() {
                lock(&self.st).cleanup_error = Some("os_error");
                return None;
            }
        }
        let path = nas.path_for(id, kind).ok()?;
        let unlinked = if self.nas_available(nas).await {
            share_remove(&path).await
        } else {
            Err(std::io::Error::other("share_unavailable"))
        };
        match unlinked {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Already gone counts as removed, but an unmounted share looks the same: confirm it first.
                super::ArchiveRead::invalidate(nas);
                if !self.nas_available(nas).await {
                    self.withdraw(id, kind, e).await;
                    lock(&self.st).cleanup_error = Some("storage_unavailable");
                    return None;
                }
            }
            Err(_) => {
                super::ArchiveRead::invalidate(nas);
                self.withdraw(id, kind, e).await;
                lock(&self.st).cleanup_error = Some("os_error");
                return None;
            }
        }
        let l = self.host.store.record_lock(id).await;
        let _g = l.lock().await;
        // Changed meanwhile: its new entry is not recorded as removed.
        let Some(mut current) = self.removable(id, kind, e, false).await else {
            return Some(false);
        };
        let mut fields = index::kept(e);
        fields.insert("location".into(), json!("removed"));
        fields.insert("removed_at".into(), json!(now()));
        fields.insert("reason".into(), json!("nas_full"));
        index::set_location(&mut current, kind, Value::Object(fields));
        // Committed together: a video flag left true would make the restart check see a stale result.
        current["video"] = json!(self.host.has_video(id, &current));
        if self.host.save(current).await.is_err() {
            lock(&self.st).cleanup_error = Some("os_error");
            return None;
        }
        Some(true)
    }

    /// A cleanup's intent is withdrawn: the file stays. Returns the entry without it, or None when it changed.
    async fn withdraw(&self, id: &str, kind: &str, e: &Value) -> Option<Value> {
        let r = self
            .with_meta(id, |current| {
                let now_ = index::entry(current, kind);
                if now_["location"] != "nas" || !index::same(&now_, e) {
                    return None;
                }
                let mut now_ = now_;
                now_.as_object_mut()?.remove("removing");
                index::set_location(current, kind, now_.clone());
                Some(now_)
            })
            .await?;
        r.ok()
    }

    /// Python `_recover` (caller holds the cycle lock): half-written copies go; a local file whose verified copy is
    /// indexed goes; a copy without its index is adopted. Sets `recovered` only when every recording was checked
    /// with the share reachable.
    pub(crate) async fn recover_nas(&self, ctx: &Ctx) {
        let Some(nas) = ctx.nas() else { return };
        if !self.nas_available(nas).await {
            return;
        }
        let Some(root) = nas.settings().path.clone() else {
            return;
        };
        let parts = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            for top in std::fs::read_dir(&root)? {
                let top = top?;
                let name = top.file_name().to_string_lossy().into_owned();
                if !(crate::store::valid_id(&name) && top.file_type()?.is_dir()) {
                    continue;
                }
                for f in std::fs::read_dir(top.path())? {
                    let f = f?;
                    if f.file_name().to_string_lossy().ends_with(".part")
                        && f.file_type()?.is_file()
                    {
                        super::nas::remove_stale_part(&f.path())?;
                    }
                }
            }
            Ok(())
        })
        .await;
        if !matches!(parts, Ok(Ok(()))) {
            tracing::warn!("archive_recovery_partial_copies_failed");
            super::ArchiveRead::invalidate(nas);
            return;
        }
        for id in self.host.ids().await {
            self.checked();
            let Some(mut m) = self.host.meta(&id).await else {
                continue;
            };
            if !self.adopt(nas, &id, &mut m).await {
                return;
            }
            for kind in super::MEDIA_KINDS {
                if !self.recover_nas_file(nas, &id, &m, kind).await {
                    return;
                }
            }
        }
        lock(&self.st).recovered = true;
    }

    /// One indexed file of `_recover`; false when the share failed (recovery stops).
    async fn recover_nas_file(
        &self,
        nas: &NasBackend,
        id: &str,
        m: &Value,
        kind: &'static str,
    ) -> bool {
        let mut e = index::entry(m, kind);
        if e["location"] == "removed" {
            return e["reason"] != "missing" || self.restore_removed(nas, id, m, kind, &e).await;
        }
        let Some(sha) = e["sha256"]
            .as_str()
            .map(str::to_owned)
            .filter(|_| e["location"] == "nas")
        else {
            return true;
        };
        let Ok(target) = nas.path_for(id, kind) else {
            return true;
        };
        let local = self.host.folder(id).join(kind);
        let present = match share_stat(&target).await {
            Ok(meta) => meta.is_file(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => {
                super::ArchiveRead::invalidate(nas);
                return false;
            }
        };
        if present {
            lock(&self.st)
                .missing
                .remove(&(id.to_owned(), kind.to_owned()));
            if e.get("removing") == Some(&json!(true)) {
                // A cleanup stopped before its unlink: the file stays and so does its entry.
                match self.withdraw(id, kind, &e).await {
                    Some(w) => e = w,
                    None => return true,
                }
            }
        }
        if !present && (e.get("error").is_some_and(|v| !v.is_null()) || !local.exists()) {
            return self.settle_missing(nas, id, kind, &e).await;
        }
        if e.get("error").is_some_and(|v| !v.is_null()) {
            return self.recheck_error(nas, id, kind, &e, &target).await;
        }
        if !is_file(&local) {
            return true;
        }
        let same = if present {
            match file_sha256(&target).await {
                Ok(h) => h == sha,
                Err(_) => {
                    super::ArchiveRead::invalidate(nas);
                    return false;
                }
            }
        } else {
            false
        };
        if same {
            let l = self.host.store.record_lock(id).await;
            let _g = l.lock().await;
            if self
                .host
                .meta(id)
                .await
                .is_some_and(|c| index::entry(&c, kind) == e)
            {
                let _ = std::fs::remove_file(&local);
            }
            return true;
        }
        // The share copy is missing or differs. A local file identical to the indexed content is the good copy:
        // the index goes back to local and the next cycle replaces the share copy.
        let Ok((size, mtime)) = stat(&local) else {
            return true;
        };
        let Ok(digest) = file_sha256(&local).await else {
            return true;
        };
        let recorded = index::fresh_hash(m, kind, size, mtime).as_deref() == Some(digest.as_str());
        let restore = digest == sha || recorded;
        // A missing file on an unmounted share looks the same: confirm the share before changing the index.
        super::ArchiveRead::invalidate(nas);
        if !self.nas_available(nas).await {
            return false;
        }
        if !present && !restore {
            // Neither copy is known good, but a missing lookup proves nothing wrong: the index stays.
            self.note_missing(id, kind);
            return true;
        }
        tracing::warn!(kind, restore, "archived_copy_missing_or_differs");
        let _ = self
            .with_meta(id, |current| {
                if index::entry(current, kind) != e {
                    return None;
                }
                let storage = current["storage"].as_object_mut()?;
                if restore {
                    storage.remove(kind);
                    if storage.is_empty() {
                        current.as_object_mut()?.remove("storage");
                    }
                } else {
                    storage.get_mut(kind)?["error"] = json!("recovery_mismatch");
                }
                Some(())
            })
            .await;
        true
    }

    /// Python `_adopt`: a copy committed to the share whose index write was lost; the index points at it.
    async fn adopt(&self, nas: &NasBackend, id: &str, m: &mut Value) -> bool {
        for kind in super::MEDIA_KINDS {
            let Some((known_size, sha)) = index::known_hash(m, kind) else {
                continue;
            };
            if index::location(m, kind) != "local" {
                continue;
            }
            // An original replaced by its MP4 keeps its hash on purpose; it is not looked for.
            if let Some(mp4) = derived(kind)
                && self.host.stored(id, m, mp4)
            {
                continue;
            }
            if self.host.folder(id).join(kind).exists() {
                continue;
            }
            let Ok(target) = nas.path_for(id, kind) else {
                continue;
            };
            let size = match share_stat(&target).await {
                Ok(meta) if meta.is_file() => meta.len(),
                Ok(_) => continue,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    super::ArchiveRead::invalidate(nas);
                    return false;
                }
            };
            if known_size.is_some_and(|s| s != size) {
                continue;
            }
            match file_sha256(&target).await {
                Ok(h) if h == sha => {}
                Ok(_) => continue,
                Err(_) => {
                    super::ArchiveRead::invalidate(nas);
                    return false;
                }
            }
            let folder = self.host.folder(id);
            let adopted = self
                .with_meta(id, |current| {
                    if index::location(current, kind) != "local" || folder.join(kind).exists() {
                        return None;
                    }
                    index::set_location(
                        current,
                        kind,
                        json!({"location": "nas", "bytes": size, "sha256": sha, "archived_at": now()}),
                    );
                    Some(current.clone())
                })
                .await;
            if let Some(Ok(current)) = adopted {
                *m = current;
                tracing::warn!(kind, "archived_copy_adopted_into_index");
            }
        }
        true
    }

    /// Python `_settle_missing`: only a cleanup that recorded its intent is confirmed as removed; anything else may
    /// be a lookup that missed, so the index is kept and the file counted.
    async fn settle_missing(&self, nas: &NasBackend, id: &str, kind: &str, e: &Value) -> bool {
        super::ArchiveRead::invalidate(nas);
        if !self.nas_available(nas).await {
            return false;
        }
        if e.get("removing") != Some(&json!(true)) {
            self.note_missing(id, kind);
            return true;
        }
        let folder = self.host.folder(id);
        let host = self.host.clone();
        let _ = self
            .with_meta(id, |current| {
                if index::entry(current, kind) != *e || folder.join(kind).exists() {
                    return None;
                }
                let mut fields = index::kept(e);
                fields.insert("location".into(), json!("removed"));
                fields.insert("removed_at".into(), json!(now()));
                fields.insert("reason".into(), json!("nas_full"));
                index::set_location(current, kind, Value::Object(fields));
                current["video"] = json!(host.has_video(id, current));
                Some(())
            })
            .await;
        true
    }

    /// Python `_recheck_missing`: files recovery did not find are looked for again; the index never changes here.
    pub(crate) async fn recheck_missing_nas(&self, ctx: &Ctx) {
        let Some(nas) = ctx.nas() else { return };
        let missing: Vec<(String, String)> = lock(&self.st).missing.iter().cloned().collect();
        for (id, kind) in missing {
            let e = self.host.meta(&id).await.map(|m| index::entry(&m, &kind));
            if e.is_none_or(|e| e["location"] != "nas") {
                lock(&self.st).missing.remove(&(id, kind));
                continue;
            }
            let Ok(path) = nas.path_for(&id, &kind) else {
                continue;
            };
            match share_stat(&path).await {
                Ok(m) if m.is_file() => {
                    lock(&self.st).missing.remove(&(id, kind));
                }
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    super::ArchiveRead::invalidate(nas);
                    return;
                }
            }
        }
    }

    /// Python `_recheck_error`: an entry marked corrupt whose copy matches the index again is cleared.
    async fn recheck_error(
        &self,
        nas: &NasBackend,
        id: &str,
        kind: &str,
        e: &Value,
        target: &std::path::Path,
    ) -> bool {
        match file_sha256(target).await {
            Ok(h) if Some(h.as_str()) == e["sha256"].as_str() => {}
            Ok(_) => return true,
            Err(_) => {
                super::ArchiveRead::invalidate(nas);
                return false;
            }
        }
        let _ = self
            .with_meta(id, |current| {
                if index::entry(current, kind) != *e {
                    return None;
                }
                let mut cleared = e.clone();
                cleared.as_object_mut()?.remove("error");
                index::set_location(current, kind, cleared);
                Some(())
            })
            .await;
        true
    }

    /// Python `_restore_removed`: earlier versions recorded a file they did not find as removed (reason missing);
    /// when the share holds the recorded content, the index points at it again.
    async fn restore_removed(
        &self,
        nas: &NasBackend,
        id: &str,
        m: &Value,
        kind: &str,
        e: &Value,
    ) -> bool {
        let Some((known_size, sha)) = index::known_hash(m, kind) else {
            return true;
        };
        if self.host.folder(id).join(kind).exists() {
            return true;
        }
        let Ok(target) = nas.path_for(id, kind) else {
            return true;
        };
        let size = match share_stat(&target).await {
            Ok(meta) if meta.is_file() => meta.len(),
            Ok(_) => return true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => {
                super::ArchiveRead::invalidate(nas);
                return false;
            }
        };
        if known_size.is_some_and(|s| s != size) {
            return true;
        }
        match file_sha256(&target).await {
            Ok(h) if h == sha => {}
            Ok(_) => return true,
            Err(_) => {
                super::ArchiveRead::invalidate(nas);
                return false;
            }
        }
        let folder = self.host.folder(id);
        let host = self.host.clone();
        let _ = self
            .with_meta(id, |current| {
                if index::entry(current, kind) != *e || folder.join(kind).exists() {
                    return None;
                }
                let t = now();
                index::set_location(
                    current,
                    kind,
                    json!({"location": "nas", "bytes": size, "sha256": sha, "archived_at": t, "restored_at": t}),
                );
                current["video"] = json!(host.has_video(id, current));
                Some(())
            })
            .await;
        true
    }
}

/// Python `DERIVED`: an original's MP4.
pub(crate) fn derived(kind: &str) -> Option<&'static str> {
    match kind {
        "qcamera.ts" => Some("camera.mp4"),
        "fcamera.hevc" => Some("fcamera.mp4"),
        "ecamera.hevc" => Some("ecamera.mp4"),
        _ => None,
    }
}

#[cfg(test)]
#[path = "archiver_nas_tests.rs"]
mod tests;
