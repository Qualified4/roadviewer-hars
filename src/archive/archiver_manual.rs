//! 'Archive everything eligible' (Python `manual_plan`, `start_manual`, `_manual_cycle`, `.archive-manual.json`):
//! the same cycle lock and per-file engine as the scheduled cycle, repeated until nothing eligible is left, without
//! the batch or the local threshold. The minimum age stays; pinned recordings are archived like the others.
use super::{
    archiver::{Archive, Ctx, CycleResult, FAILURES, lock},
    fsutil,
    index::{self, ARCHIVE_ORDER},
    settings::Mode,
};
use crate::store::now;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const MANUAL_JOB: &str = ".archive-manual.json";
/// A manual run waits this long for an upload or conversion, then goes on.
pub const MANUAL_WAIT: Duration = Duration::from_secs(5);
/// A run's speed is measured over about this many seconds of the latest transfer, once it spans RATE_MIN.
pub const RATE_WINDOW: f64 = 30.0;
pub const RATE_MIN: f64 = 5.0;

/// Python `_Transfer`: bytes a manual run has actually sent, for its speed and remaining time (memory only).
pub struct Transfer {
    start: Instant,
    inner: Mutex<TransferState>,
}
#[derive(Default)]
struct TransferState {
    sent: u64,
    files: BTreeMap<(String, String), u64>,
    samples: VecDeque<(f64, u64)>,
}
impl Default for Transfer {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            inner: Mutex::default(),
        }
    }
}
impl Transfer {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
    fn sample(s: &mut TransferState, now: f64) {
        if s.samples.back().is_none_or(|(t, _)| now - t >= 1.0) {
            s.samples.push_back((now, s.sent));
        }
        while s.samples.len() > 1 && s.samples[0].0 < now - RATE_WINDOW {
            s.samples.pop_front();
        }
    }
    /// A file starts; a resumed upload starts at its offset (those bytes were sent by an earlier attempt).
    pub fn begin(&self, offset: u64, key: &(String, String)) {
        let now = self.now();
        let mut s = lock(&self.inner);
        s.files.insert(key.clone(), offset);
        Self::sample(&mut s, now);
    }
    pub fn progress(&self, sent: u64, key: &(String, String)) {
        let now = self.now();
        let mut s = lock(&self.inner);
        let before = s.files.get(key).copied().unwrap_or(0);
        if sent > before {
            s.sent += sent - before;
        }
        s.files.insert(key.clone(), sent);
        Self::sample(&mut s, now);
    }
    /// The file is done (or failed); `size` adds a file copied in one piece (the NAS).
    pub fn end(&self, size: u64, key: &(String, String)) {
        let now = self.now();
        let mut s = lock(&self.inner);
        s.sent += size;
        s.files.remove(key);
        if size > 0 {
            Self::sample(&mut s, now);
        }
    }
    /// (bytes of the files in progress, bytes/s or None while too little has been measured).
    pub fn snapshot(&self) -> (u64, Option<f64>) {
        let now = self.now();
        let mut s = lock(&self.inner);
        let current = s.files.values().sum();
        if s.samples.is_empty() {
            return (current, None);
        }
        Self::sample(&mut s, now);
        let (t, sent) = s.samples[0];
        (
            current,
            (now - t >= RATE_MIN).then(|| (s.sent - sent) as f64 / (now - t)),
        )
    }
}

impl Archive {
    fn job_path(&self) -> std::path::PathBuf {
        self.root.join(MANUAL_JOB)
    }

    /// Stopped mid-run (restart, crash): each finished file is committed, the run itself did not complete.
    pub(crate) fn load_job(&self) -> Option<Value> {
        let mut job = fsutil::read_object(&self.job_path()).map(Value::Object)?;
        if job["state"] == "running" {
            job["state"] = json!("interrupted");
            job["waiting"] = Value::Null;
            let _ = fsutil::atomic_json(&self.job_path(), &job);
        }
        Some(job)
    }

    pub(crate) fn save_job(&self) {
        if let Some(job) = lock(&self.job).clone()
            && fsutil::atomic_json(&self.job_path(), &job).is_err()
        {
            tracing::warn!("manual_archive_state_not_saved");
        }
    }

    /// The run as persisted, with the startup checks and, while it runs, its transfer.
    pub(crate) fn job_status(&self) -> Value {
        let Some(mut job) = lock(&self.job).clone() else {
            return Value::Null;
        };
        let (checking, active) = {
            let st = lock(&self.st);
            (st.checking, st.manual_active)
        };
        job["checking"] = checking.map_or(Value::Null, |(d, t)| json!({"done": d, "total": t}));
        let transfer = lock(&self.transfer).clone();
        if active
            && job["state"] == "running"
            && let Some(t) = transfer
        {
            let (current, rate) = t.snapshot();
            let total = job["total_bytes"].as_u64().unwrap_or(0);
            let done = job["processed_bytes"].as_u64().unwrap_or(0);
            let left = total.saturating_sub(done).saturating_sub(current);
            job["current_bytes"] = json!(current);
            job["rate"] = json!(rate);
            job["eta_seconds"] = match rate {
                Some(r) if r > 0.0 && job["waiting"].is_null() => json!(left as f64 / r),
                _ => Value::Null,
            };
        }
        job
    }

    /// (kind, size) of the local files a manual run would archive.
    fn manual_files(&self, ctx: &Ctx, id: &str, m: &Value) -> Vec<(&'static str, u64)> {
        let folder = self.host.folder(id);
        ARCHIVE_ORDER
            .iter()
            .filter(|k| {
                index::location(m, k) == "local" && !(ctx.remote() && self.done_remotely(ctx, m, k))
            })
            .filter_map(|k| {
                let size = std::fs::metadata(folder.join(k)).ok()?.len();
                (!ctx.remote() || size > 0).then_some((*k, size))
            })
            .collect()
    }

    /// What a manual run would process now, for the confirmation; reads only the local index and files.
    pub async fn manual_plan(&self) -> Value {
        let ctx = self.ctx();
        let moving = ctx.moves_local();
        let now_ = now();
        let (mut files, mut recordings, mut size, mut min_age, mut not_ready) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        for id in self.host.ids().await {
            let Some(m) = self.host.meta(&id).await else {
                continue;
            };
            let found = self.manual_files(&ctx, &id, &m);
            if found.is_empty() {
                continue;
            }
            if m["status"] != "ready" || self.host.busy(&id).await {
                not_ready += 1;
                continue;
            }
            if m["uploaded"]
                .as_f64()
                .is_none_or(|u| now_ - u < ctx.settings.min_age_seconds as f64)
            {
                min_age += 1;
                continue;
            }
            recordings += 1;
            files += found.len() as u64;
            size += found.iter().map(|(_, n)| n).sum::<u64>();
        }
        let s = &ctx.settings;
        json!({
            "backend": s.backend.as_str(), "mode": if moving { "move" } else { "copy" },
            "configured_mode": if ctx.remote() { s.mode.as_str() } else { "move" },
            "deletes_local": moving,
            "local_delete_blocked": ctx.remote() && s.mode == Mode::Move && !s.allow_local_delete,
            "files": files, "recordings": recordings, "bytes": size, "frees_bytes": if moving { size } else { 0 },
            "min_age_hours": s.min_age_seconds / 3600, "excluded": {"min_age": min_age, "not_ready": not_ready},
        })
    }

    /// Starts the run in the background; None while a cycle (scheduled, manual or capacity) holds the lock.
    pub async fn start_manual(&self) -> Option<Value> {
        let guard = self.cycle.clone().try_lock_owned().ok()?;
        let plan = self.manual_plan().await;
        let job = json!({
            "id": crate::store::id(), "state": "running", "started_at": now(), "finished_at": null,
            "mode": plan["mode"], "deletes_local": plan["deletes_local"], "total_files": plan["files"],
            "total_bytes": plan["bytes"], "processed": 0, "succeeded": 0, "failed": 0, "bytes_done": 0,
            "processed_bytes": 0, "freed_bytes": 0, "error": null, "last_failure": null, "waiting": null,
        });
        *lock(&self.job) = Some(job.clone());
        *lock(&self.transfer) = Some(Arc::new(Transfer::default()));
        self.save_job();
        {
            let mut st = lock(&self.st);
            st.manual_active = true;
            st.running = true;
        }
        let me = self.arc();
        tokio::spawn(async move {
            me.run_locked(guard).await;
        });
        Some(job)
    }

    pub(crate) async fn manual_cycle(&self, ctx: &Ctx, result: &mut CycleResult) -> &'static str {
        let reason = loop {
            let reason = self.cycle_once(ctx, result).await.unwrap_or("done");
            // Uploads and conversions come first, as for the scheduled cycle; the run waits and goes on.
            if !matches!(reason, "uploading" | "processing") {
                break reason;
            }
            self.update_job(|j| j["waiting"] = json!(reason));
            tokio::select! {
                _ = tokio::time::sleep(MANUAL_WAIT) => {}
                _ = self.wake.notified() => {}
            }
            if self.stopping() {
                break "shutdown";
            }
            self.update_job(|j| j["waiting"] = Value::Null);
            self.replan_job().await;
        };
        self.finish_job(reason);
        reason
    }

    fn update_job(&self, f: impl FnOnce(&mut Value)) {
        if let Some(job) = lock(&self.job).as_mut() {
            f(job);
        }
        self.save_job();
    }

    /// While the run waited more became eligible: counted again as what is done plus what is left now.
    async fn replan_job(&self) {
        let plan = self.manual_plan().await;
        self.update_job(|j| {
            let total = j["total_files"].as_u64().unwrap_or(0);
            let done = j["processed"].as_u64().unwrap_or(0);
            j["total_files"] = json!(total.max(done + plan["files"].as_u64().unwrap_or(0)));
            let total_b = j["total_bytes"].as_u64().unwrap_or(0);
            let done_b = j["processed_bytes"].as_u64().unwrap_or(0);
            j["total_bytes"] = json!(total_b.max(done_b + plan["bytes"].as_u64().unwrap_or(0)));
        });
    }

    pub(crate) fn finish_job(&self, reason: &str) {
        let mut st = lock(&self.st);
        if !st.manual_active {
            return;
        }
        st.manual_active = false;
        drop(st);
        self.update_job(|j| {
            if FAILURES.contains(&reason) || matches!(reason, "disabled" | "internal_error") {
                j["state"] = json!("failed");
                j["error"] = json!(reason);
            } else if reason == "shutdown" {
                j["state"] = json!("interrupted");
            } else {
                j["state"] = json!(if j["failed"].as_u64().unwrap_or(0) > 0 {
                    "partial"
                } else {
                    "done"
                });
            }
            j["finished_at"] = json!(now());
            j["waiting"] = Value::Null;
        });
    }

    /// Counted by the task running the pass, after the file settled.
    pub(crate) async fn job_file(
        &self,
        ctx: &Ctx,
        id: &str,
        kind: &str,
        size: u64,
        ok: bool,
        reason: Option<&str>,
    ) {
        if let Some(t) = lock(&self.transfer).clone() {
            t.end(
                if ok && !ctx.remote() { size } else { 0 },
                &(id.to_owned(), kind.to_owned()),
            );
        }
        let moved = ok
            && self
                .host
                .meta(id)
                .await
                .is_some_and(|m| index::location(&m, kind) != "local")
            && !self.host.folder(id).join(kind).exists();
        self.update_job(|j| {
            let add = |j: &mut Value, k: &str, n: u64| j[k] = json!(j[k].as_u64().unwrap_or(0) + n);
            add(j, "processed", 1);
            add(j, "processed_bytes", size);
            // A file that became eligible during a pass still fits the total.
            let (p, pb) = (
                j["processed"].as_u64().unwrap_or(0),
                j["processed_bytes"].as_u64().unwrap_or(0),
            );
            j["total_files"] = json!(j["total_files"].as_u64().unwrap_or(0).max(p));
            j["total_bytes"] = json!(j["total_bytes"].as_u64().unwrap_or(0).max(pb));
            if ok {
                add(j, "succeeded", 1);
                add(j, "bytes_done", size);
                if moved {
                    add(j, "freed_bytes", size);
                }
            } else {
                add(j, "failed", 1);
                j["last_failure"] = json!(reason);
            }
        });
    }
}
