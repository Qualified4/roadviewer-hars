//! Startup checks carried over a clean stop. The startup checks (crash recovery: interrupted uploads, moves stopped
//! before their local unlink, stale copies) read every recording, and a restart (an add-on update) used to run them
//! again from the start. When the process stops with no cycle running and the checks already done, nothing can have
//! been left half done, so the next start is the same as if the process had kept running: the checks stay done.
//!
//! The note is written only while holding the cycle lock (no cycle runs, and none starts again before the process
//! ends), and it is removed as it is read, before it is trusted: a start that later crashes leaves no note, and the
//! following start runs the full checks. A note made for another archive target (backend, share path, Drive folder)
//! is not used.
use super::{
    archiver::{Archive, lock},
    fsutil,
    settings::ArchiveSettings,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const CLEAN_STOP: &str = ".archive-clean-stop.json";

/// The archive the checks were made against.
fn target(s: &ArchiveSettings) -> Value {
    json!({
        "backend": s.backend.as_str(),
        "path": s.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "gdrive_folder_name": s.gdrive_folder_name,
    })
}

fn pairs(set: &BTreeSet<(String, String)>) -> Value {
    Value::Array(set.iter().map(|(id, kind)| json!([id, kind])).collect())
}

fn set(v: &Value) -> Option<BTreeSet<(String, String)>> {
    v.as_array()?
        .iter()
        .map(|p| Some((p[0].as_str()?.to_owned(), p[1].as_str()?.to_owned())))
        .collect()
}

impl Archive {
    fn clean_stop_path(&self) -> std::path::PathBuf {
        self.root.join(CLEAN_STOP)
    }

    /// At a clean stop (after `shutdown`): notes that the startup checks are done, when no cycle is running. The
    /// cycle lock is kept until the process ends, so no cycle starts after the note. Returns whether it was written.
    pub fn record_clean_stop(&self) -> bool {
        let Ok(guard) = self.cycle.clone().try_lock_owned() else {
            return false; // A cycle is under way and will be cut short: the next start checks everything.
        };
        let settings = self.settings();
        let note = {
            let st = lock(&self.st);
            if !settings.enabled || !st.recovered || st.startup_failure.is_some() {
                return false;
            }
            json!({
                "target": target(&settings),
                "held_checked": st.held_checked,
                "missing": pairs(&st.missing),
                "empty": pairs(&st.empty),
            })
        };
        if fsutil::atomic_json(&self.clean_stop_path(), &note).is_err() {
            tracing::warn!("archive_clean_stop_not_saved");
            return false;
        }
        std::mem::forget(guard);
        tracing::info!("archive_clean_stop_saved");
        true
    }

    /// At open: takes the note of the last clean stop. It is removed first; it counts only when that worked and it
    /// was made for the archive configured now.
    pub(crate) fn adopt_clean_stop(&self) {
        let path = self.clean_stop_path();
        let Some(note) = fsutil::read_object(&path) else {
            let _ = std::fs::remove_file(&path); // Unreadable: not trusted, and not read again.
            return;
        };
        if std::fs::remove_file(&path).is_err() {
            return;
        }
        let settings = self.settings();
        let (Some(missing), Some(empty)) = (set(&note["missing"]), set(&note["empty"])) else {
            return;
        };
        if !settings.enabled || note.get("target") != Some(&target(&settings)) {
            return;
        }
        let mut st = lock(&self.st);
        st.recovered = true;
        st.held_checked = note["held_checked"].as_bool().unwrap_or(false);
        st.missing = missing;
        st.empty = empty;
        tracing::info!("archive_startup_checks_carried_over");
    }
}
