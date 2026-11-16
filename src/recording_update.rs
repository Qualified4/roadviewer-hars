//! A camera file uploaded for a recording that is already registered (Python `register_files`:
//! `additional_videos`, `clear_prepared` and the update branch). The upload journal names the
//! recording and the added files; [`apply`] turns the moved files into the recording's index and
//! can run again after a crash with the same result.
use crate::{
    error::{ApiError, Result},
    listing::{location, original_bytes, stored},
    store::{Store, now},
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Python `video_sources.SOURCES`: (camera, original, MP4 kept or made from it).
const SOURCES: [(&str, &str, &str); 3] = [
    ("qcamera", "qcamera.ts", "camera.mp4"),
    ("front", "fcamera.hevc", "fcamera.mp4"),
    ("wide", "ecamera.hevc", "ecamera.mp4"),
];
pub const VIDEO_KINDS: [&str; 6] = [
    "qcamera.ts",
    "fcamera.hevc",
    "ecamera.hevc",
    "camera.mp4",
    "fcamera.mp4",
    "ecamera.mp4",
];

fn derived(original: &str) -> Option<(&'static str, &'static str)> {
    SOURCES.iter().find(|s| s.1 == original).map(|s| (s.0, s.2))
}
fn original_of(mp4: &str) -> Option<&'static str> {
    SOURCES.iter().find(|s| s.2 == mp4).map(|s| s.1)
}

/// Python `retained_original`: a matching re-upload must not recreate a second permanent copy
/// of a camera whose MP4 was kept instead of its original.
fn retained_original(
    folder: &Path,
    meta: &Value,
    kind: &str,
    digest: &str,
    known: &BTreeMap<String, String>,
) -> bool {
    let (source, mp4) = derived(kind).unwrap();
    let codec = if source == "qcamera" { "h264" } else { "hevc" };
    stored(folder, meta, mp4)
        && known.get(kind).map(String::as_str) == Some(digest)
        && meta["video_codecs"][source] == codec
}

/// Python `additional_videos`: the incoming camera kinds this recording does not have yet.
/// `planned` holds kinds an earlier group of the same upload already adds.
pub fn additional_videos(
    folder: &Path,
    meta: &Value,
    incoming: &BTreeMap<String, String>,
    known: &BTreeMap<String, String>,
    planned: &BTreeSet<String>,
) -> Vec<String> {
    let mut added = vec![];
    for kind in VIDEO_KINDS {
        let Some(digest) = incoming.get(kind) else {
            continue;
        };
        if stored(folder, meta, kind) || planned.contains(kind) {
            continue;
        }
        if let Some((_, mp4)) = derived(kind) {
            if retained_original(folder, meta, kind, digest, known) {
                continue;
            }
            if stored(folder, meta, mp4) && known.get(kind) != Some(digest) {
                continue;
            }
        } else {
            let original = original_of(kind).unwrap();
            if stored(folder, meta, original) || folder.join("prepared").join(kind).is_file() {
                continue;
            }
        }
        added.push(kind.to_owned());
    }
    added
}

/// Why a new camera cannot be added now (Python `register_files`). A reconversion needs every
/// archived file, so an archived recording waits while the archive's last state is not
/// `connected`; it also needs a readable log, judged for an archived one by `states` resolved
/// before the locks (None: not resolved, which waits for the archive too).
pub fn refusal(
    meta: &Value,
    reachable: bool,
    states: Option<&BTreeMap<String, String>>,
) -> Option<ApiError> {
    let archived = crate::storage::archived(meta);
    if archived && !reachable {
        return Some(ApiError(StatusCode::CONFLICT, "storage_unavailable"));
    }
    let log = location(meta, "rlog.zst");
    let state = states.and_then(|s| s.get("rlog.zst")).map(String::as_str);
    let reconvertible =
        log == "local" || matches!(log, "nas" | "gdrive") && state == Some("available");
    if !reconvertible {
        if matches!(log, "nas" | "gdrive")
            && matches!(state, None | Some("storage_unavailable" | "checking"))
        {
            return Some(ApiError(StatusCode::CONFLICT, "storage_unavailable"));
        }
        return Some(ApiError(StatusCode::CONFLICT, "log_unavailable"));
    }
    None
}

/// Python `archived_states`: the state of each archived file of a recording, resolved outside
/// every lock (the share may be slow).
pub async fn archived_states(
    archive: &crate::archive::Archive,
    meta: &Value,
) -> BTreeMap<String, String> {
    let id = meta["id"].as_str().unwrap_or_default();
    let mut states = BTreeMap::new();
    for kind in crate::parity::STORED_KINDS {
        if matches!(location(meta, kind), "nas" | "gdrive") {
            states.insert(
                kind.to_owned(),
                archive.resolve(id, meta, kind).await.1.to_owned(),
            );
        }
    }
    states
}

/// The archived originals a resolved `available` lets [`clear_prepared`] replace by a new conversion.
pub fn available_originals(states: Option<&BTreeMap<String, String>>) -> BTreeSet<String> {
    SOURCES
        .iter()
        .filter(|(_, original, _)| {
            states.and_then(|s| s.get(*original)).map(String::as_str) == Some("available")
        })
        .map(|(_, original, _)| (*original).to_owned())
        .collect()
}

/// Python's `restored`: every added kind is an original whose MP4 is kept and whose content is
/// the one recorded, so the replay data stays valid.
pub fn restored(
    folder: &Path,
    added: &[String],
    incoming: &BTreeMap<String, String>,
    known: &BTreeMap<String, String>,
) -> bool {
    added.iter().all(|kind| {
        derived(kind).is_some_and(|(_, mp4)| folder.join(mp4).is_file())
            && incoming.get(kind) == known.get(kind)
    })
}

/// Python `media_store.drop_remote`.
fn drop_remote(meta: &mut Value, kind: &str, sha256: Option<&str>) {
    let Some(storage) = meta.get_mut("storage").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(e) = storage.get_mut(kind).and_then(Value::as_object_mut) else {
        return;
    };
    if e.get("location").and_then(Value::as_str) != Some("local") {
        return;
    }
    if let (Some(block), Some(sha)) = (e.get("remote").filter(|b| b.is_object()), sha256)
        && block["sha256"] == sha
    {
        return;
    }
    let mismatch = e.get("error").and_then(Value::as_str) == Some("remote_mismatch");
    if !(e.contains_key("remote") || e.contains_key("remote_pending") || mismatch) {
        return;
    }
    e.remove("remote");
    e.remove("remote_pending");
    if mismatch {
        e.remove("error");
    }
    if e.len() == 1 {
        storage.remove(kind);
    }
}

/// Python `media_store.public_storage`.
fn public(meta: &Value) -> Value {
    let mut meta = meta.clone();
    if let Some(storage) = meta.get_mut("storage").and_then(Value::as_object_mut) {
        for e in storage.values_mut().filter_map(Value::as_object_mut) {
            e.remove("remote_pending");
            e.remove("file_id");
            if let Some(remote) = e.get_mut("remote").and_then(Value::as_object_mut) {
                remote.remove("remote_pending");
                remote.remove("file_id");
            }
        }
    }
    meta
}

/// Python `clear_prepared`. Only a readable original lets its MP4 go: a local one that is present,
/// or an archived one whose state was resolved `available` beforehand (`archived`). The only
/// retained copy of a camera is never removed.
pub async fn clear_prepared(
    folder: &Path,
    meta: &mut Value,
    revision: u64,
    archived: &BTreeSet<String>,
) -> std::io::Result<()> {
    match tokio::fs::remove_dir_all(folder.join("prepared")).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    for (_, original, mp4) in SOURCES {
        let readable = match location(meta, original) {
            "local" => folder.join(original).is_file(),
            "nas" | "gdrive" => archived.contains(original),
            _ => false,
        };
        if readable {
            match tokio::fs::remove_file(folder.join(mp4)).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            drop_remote(meta, mp4, None);
        }
    }
    meta["bytes"] = json!(original_bytes(folder, meta));
    meta["duration"] = Value::Null;
    meta["model_frames"] = Value::Null;
    meta["warnings"] = json!([]);
    meta["error"] = Value::Null;
    meta["conversion_revision"] = json!(revision);
    Ok(())
}

/// The journal entry for one recording: what [`apply`] needs without the session's manifest.
pub fn entry(
    meta: &Value,
    restored: bool,
    added: &[(usize, String, String, String)],
    archived: &BTreeSet<String>,
) -> Value {
    let revision = meta["conversion_revision"].as_u64().unwrap_or(0) + u64::from(!restored);
    json!({"id":meta["id"],"restored":restored,"conversion_revision":revision,"archived_available":archived,
        "kinds":added.iter().map(|(index,kind,name,sha)| json!({"index":index,"kind":kind,"name":name,"sha256":sha})).collect::<Vec<_>>()})
}

/// Registers the moved camera files with their recording. Run under the registration lock after
/// the journal's moves; once applied (the recorded hashes and revision match) it only answers.
/// Returns the recording as the finish response lists it under `updated`.
pub async fn apply(store: &Store, update: &Value) -> Result<Value> {
    let id = update["id"].as_str().unwrap_or_default();
    let lock = store.record_lock(id).await;
    let _guard = lock.lock().await;
    let folder = store.folder(id).await?;
    let mut meta = store.meta(id).await?;
    let restored = update["restored"] == true;
    let revision = update["conversion_revision"].as_u64().unwrap_or(0);
    let kinds = update["kinds"].as_array().cloned().unwrap_or_default();
    let applied = kinds.iter().all(|k| {
        let kind = k["kind"].as_str().unwrap_or_default();
        meta["files"].get(kind).is_some() && meta["content_hashes"][kind]["sha256"] == k["sha256"]
    }) && (restored || meta["conversion_revision"].as_u64().unwrap_or(0) >= revision);
    if !applied {
        for k in &kinds {
            let kind = k["kind"].as_str().unwrap_or_default();
            let sha = k["sha256"].as_str().unwrap_or_default();
            let stat = tokio::fs::metadata(folder.join(kind)).await?;
            #[cfg(unix)]
            let mtime_ns = {
                use std::os::unix::fs::MetadataExt;
                stat.mtime() as u64 * 1_000_000_000 + stat.mtime_nsec() as u64
            };
            #[cfg(not(unix))]
            let mtime_ns = stat
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            if !meta["files"].is_object() {
                meta["files"] = json!({});
            }
            meta["files"][kind] = k["name"].clone();
            // A camera removed by archive cleanup is local again once re-uploaded.
            if location(&meta, kind) == "removed"
                && let Some(storage) = meta["storage"].as_object_mut()
            {
                storage.remove(kind);
            }
            drop_remote(&mut meta, kind, Some(sha));
            if !meta["content_hashes"].is_object() {
                meta["content_hashes"] = json!({});
            }
            meta["content_hashes"][kind] =
                json!({"size":stat.len(),"mtime_ns":mtime_ns,"sha256":sha});
        }
        if !restored {
            let archived = update["archived_available"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect();
            clear_prepared(&folder, &mut meta, revision, &archived).await?;
            let settings = store.processing.read().await.clone();
            let queued = meta["manual_conversion"] == true
                || (settings["auto_convert"] == true && meta["auto_excluded"] != true);
            meta["status"] = json!(if queued { "queued" } else { "unconverted" });
            if queued {
                meta["queued_at"] = json!(now());
            }
            // A conversion still running for the old files sees another generation and is
            // discarded instead of publishing replay data without the new camera.
            if let Some(m) = meta.as_object_mut() {
                m.remove("conversion_id");
            }
        }
        meta["video"] = json!(true);
        meta["bytes"] = json!(original_bytes(&folder, &meta));
        // HARS's download list (`/download/video`) must name the camera files now on disk.
        meta["video_downloads"] = crate::jobs::download_entries(&folder).await?;
        crate::fault::point("update_before_metadata");
        store.save_meta(meta.clone()).await?;
    }
    let mut row = public(&meta);
    if restored {
        row["original_restored"] = json!(true);
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn map(rows: &[(&str, &str)]) -> BTreeMap<String, String> {
        rows.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }
    #[test]
    fn additional_videos_follows_python_rules() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        let none = BTreeSet::new();
        let added = |meta: &Value, incoming: &[(&str, &str)], known: &[(&str, &str)]| {
            additional_videos(folder, meta, &map(incoming), &map(known), &none)
        };
        let meta = json!({});
        // A kind it has, or the log, is never added.
        std::fs::write(folder.join("qcamera.ts"), b"q").unwrap();
        assert!(added(&meta, &[("rlog.zst", "l"), ("qcamera.ts", "x")], &[]).is_empty());
        assert_eq!(
            added(&meta, &[("fcamera.hevc", "f")], &[]),
            ["fcamera.hevc"]
        );
        // An MP4 is not added beside its stored original or a prepared copy.
        assert!(added(&meta, &[("camera.mp4", "m")], &[]).is_empty());
        std::fs::create_dir(folder.join("prepared")).unwrap();
        std::fs::write(folder.join("prepared/ecamera.mp4"), b"e").unwrap();
        assert!(added(&meta, &[("ecamera.mp4", "m")], &[]).is_empty());
        // An original whose MP4 was kept instead: other content is refused, the same content
        // only when the kept MP4's codec does not show it was made from it.
        std::fs::write(folder.join("fcamera.mp4"), b"kept").unwrap();
        assert!(
            added(
                &meta,
                &[("fcamera.hevc", "new")],
                &[("fcamera.hevc", "old")]
            )
            .is_empty()
        );
        let hevc = json!({"video_codecs":{"front":"hevc"}});
        assert!(
            added(
                &hevc,
                &[("fcamera.hevc", "old")],
                &[("fcamera.hevc", "old")]
            )
            .is_empty()
        );
        let unknown = added(
            &meta,
            &[("fcamera.hevc", "old")],
            &[("fcamera.hevc", "old")],
        );
        assert_eq!(unknown, ["fcamera.hevc"]);
        assert!(restored(
            folder,
            &unknown,
            &map(&[("fcamera.hevc", "old")]),
            &map(&[("fcamera.hevc", "old")])
        ));
        // An archived kind counts as stored.
        let archived = json!({"storage":{"ecamera.hevc":{"location":"nas"}}});
        assert!(added(&archived, &[("ecamera.hevc", "e")], &[]).is_empty());
        // Python `register_files`: an archived recording waits for a connected archive, then
        // needs a readable log (an archived one by its state resolved before the locks).
        assert!(refusal(&archived, false, None).is_some_and(|e| e.1 == "storage_unavailable"));
        assert!(refusal(&archived, true, None).is_none());
        let log = json!({"storage":{"rlog.zst":{"location":"gdrive"}}});
        let states = |s: &str| BTreeMap::from([("rlog.zst".to_owned(), s.to_owned())]);
        assert!(refusal(&log, true, None).is_some_and(|e| e.1 == "storage_unavailable"));
        assert!(
            refusal(&log, true, Some(&states("checking")))
                .is_some_and(|e| e.1 == "storage_unavailable")
        );
        assert!(
            refusal(&log, true, Some(&states("corrupt"))).is_some_and(|e| e.1 == "log_unavailable")
        );
        assert!(refusal(&log, true, Some(&states("available"))).is_none());
        let removed = json!({"storage":{"rlog.zst":{"location":"removed"}}});
        assert!(refusal(&removed, true, None).is_some_and(|e| e.1 == "log_unavailable"));
        assert!(refusal(&meta, false, None).is_none());
    }
    #[test]
    fn drop_remote_keeps_a_copy_of_the_same_content() {
        let mut meta = json!({"storage":{"camera.mp4":{"location":"local","remote":{"sha256":"a","file_id":"x"}}}});
        drop_remote(&mut meta, "camera.mp4", Some("a"));
        assert!(meta["storage"]["camera.mp4"]["remote"].is_object());
        assert_eq!(
            public(&meta)["storage"]["camera.mp4"]["remote"],
            json!({"sha256":"a"})
        );
        drop_remote(&mut meta, "camera.mp4", Some("b"));
        assert_eq!(meta["storage"], json!({}));
    }
}
