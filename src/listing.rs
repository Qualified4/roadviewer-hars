//! `/api/logs` rows with the derived fields of Python `build_list_row`, `route_starts` and
//! `recording_order`, computed from the recording folder rather than stale metadata copies.
use crate::parity::{RecordingOrder, SOURCES, STORED_KINDS, number};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

const SKIPPED_EVENTS_WARNING: &str = "지원하지 않는 새 로그 이벤트";
const LIST_FIELDS: [&str; 7] = [
    "drive_summary",
    "started_at",
    "start_source",
    "first_mono",
    "gps_offset",
    "clock_offset",
    "has_track",
];
const ROUTE_START_FIELDS: [&str; 3] = ["first_mono", "gps_offset", "clock_offset"];
/// Summary values HARS also copies into meta.json; Python rows take them from summary.json only.
const SUMMARY_COPIES: [&str; 10] = [
    "drive_summary",
    "started_at",
    "start_source",
    "first_mono",
    "gps_offset",
    "clock_offset",
    "has_track",
    "videos",
    "replay_v",
    "start_v",
];
/// HARS bookkeeping that Python metadata does not have.
const INTERNAL: [&str; 4] = [
    "queued_at",
    "conversion_id",
    "cancel_requested",
    "video_downloads",
];
const EARLIEST: f64 = 1420070400.;
const RACY_NS: u128 = 2_000_000_000;

type Signature = Vec<Option<(u128, u64, u64)>>;
type Rows = HashMap<PathBuf, (Signature, Map<String, Value>)>;
static CACHE: LazyLock<Mutex<Rows>> = LazyLock::new(Default::default);

fn stamp(m: &std::fs::Metadata) -> u128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
fn signature(folder: &Path) -> Signature {
    ["", "meta.json", "prepared", "prepared/summary.json"]
        .iter()
        .map(|part| {
            let path = if part.is_empty() {
                folder.to_path_buf()
            } else {
                folder.join(part)
            };
            std::fs::metadata(path).ok().map(|m| {
                #[cfg(unix)]
                let ino = std::os::unix::fs::MetadataExt::ino(&m);
                #[cfg(not(unix))]
                let ino = 0;
                (stamp(&m), m.len(), ino)
            })
        })
        .collect()
}

pub fn file_size(path: &Path) -> Option<u64> {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
}
fn tree_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => tree_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Where Python's index says a kind is (`media_store.location`): local unless recorded otherwise.
pub fn location<'a>(meta: &'a Value, kind: &str) -> &'a str {
    meta["storage"][kind]["location"]
        .as_str()
        .unwrap_or("local")
}
fn archived(meta: &Value, kind: &str) -> bool {
    matches!(location(meta, kind), "nas" | "gdrive")
}
/// `server.stored`: registered for this recording (on the archive, or present locally).
pub fn stored(folder: &Path, meta: &Value, kind: &str) -> bool {
    archived(meta, kind)
        || (location(meta, kind) == "local"
            && (folder.join(kind).is_file() || meta["storage"][kind]["remote"].is_object()))
}
/// `media_store.media_size`.
pub fn media_size(folder: &Path, meta: &Value, kind: &str) -> u64 {
    if archived(meta, kind) {
        return meta["storage"][kind]["bytes"].as_u64().unwrap_or(0);
    }
    file_size(&folder.join(kind))
        .or_else(|| meta["storage"][kind]["remote"]["bytes"].as_u64())
        .unwrap_or(0)
}
pub fn original_bytes(folder: &Path, meta: &Value) -> u64 {
    STORED_KINDS
        .iter()
        .filter(|k| stored(folder, meta, k))
        .map(|k| media_size(folder, meta, k))
        .sum()
}
pub fn has_video(folder: &Path, meta: &Value) -> bool {
    STORED_KINDS[1..].iter().any(|k| stored(folder, meta, k))
}
fn source_kinds(source: &str) -> [&'static str; 2] {
    let (_, raw, mp4) = SOURCES.iter().find(|s| s.0 == source).unwrap();
    [raw, mp4]
}
pub fn video_download_kind(folder: &Path, meta: &Value) -> Option<&'static str> {
    ["front", "qcamera", "wide"]
        .iter()
        .flat_map(|s| source_kinds(s))
        .find(|k| stored(folder, meta, k))
}
pub fn video_downloads(folder: &Path, meta: &Value) -> Vec<Value> {
    ["front", "wide", "qcamera"]
        .iter()
        .filter_map(|source| {
            let kind = source_kinds(source)
                .into_iter()
                .find(|k| stored(folder, meta, k))?;
            Some(json!({"source":source,"kind":kind,"bytes":media_size(folder,meta,kind)}))
        })
        .collect()
}
pub fn download_options(folder: &Path, meta: &Value) -> Vec<Value> {
    let mut rows = vec![];
    for (source, original, mp4) in SOURCES {
        let prepared = folder.join("prepared").join(mp4);
        let video = stored(folder, meta, mp4) || prepared.is_file();
        let raw = stored(folder, meta, original);
        if !raw && !video {
            continue;
        }
        let codec = meta["video_codecs"][source].as_str();
        let reversible = video
            && if source == "qcamera" {
                codec.is_none_or(|c| c == "h264")
            } else {
                codec == Some("hevc")
            };
        let size = if stored(folder, meta, mp4) {
            media_size(folder, meta, mp4)
        } else if video {
            file_size(&prepared).unwrap_or(0)
        } else {
            media_size(folder, meta, original)
        };
        rows.push(json!({"source":source,"mp4":video,"original":raw||reversible,"bytes":size,"original_kind":original}));
    }
    rows
}

/// The parts of a row read from files, cached while the folder's four stats are unchanged.
fn file_fields(folder: &Path, meta: &Value) -> Map<String, Value> {
    let sig = signature(folder);
    if let Some((cached, fields)) = CACHE.lock().unwrap().get(folder)
        && *cached == sig
    {
        return fields.clone();
    }
    let mut row = Map::new();
    row.insert("bytes".into(), json!(original_bytes(folder, meta)));
    row.insert("video".into(), json!(has_video(folder, meta)));
    row.insert(
        "video_download".into(),
        json!(video_download_kind(folder, meta)),
    );
    row.insert(
        "video_downloads".into(),
        json!(video_downloads(folder, meta)),
    );
    row.insert(
        "download_options".into(),
        json!(download_options(folder, meta)),
    );
    let log = if stored(folder, meta, "rlog.zst") {
        media_size(folder, meta, "rlog.zst")
    } else {
        0
    };
    row.insert("log_bytes".into(), json!(log));
    let prepared = folder.join("prepared");
    row.insert("prepared_bytes".into(), json!(tree_size(&prepared)));
    if prepared.join("thumbnail.jpg").is_file() {
        row.insert("thumbnail".into(), json!(true));
        let info = std::fs::read(prepared.join("thumbnail.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        if let Some(v) = info.get("version") {
            row.insert("thumbnail_version".into(), v.clone());
        }
        if info["score"].is_number() {
            row.insert("thumbnail_score".into(), info["score"].clone());
        }
    }
    if meta["status"] == "ready"
        && let Ok(bytes) = std::fs::read(prepared.join("summary.json"))
        && let Ok(summary) = serde_json::from_slice::<Value>(&bytes)
    {
        for key in LIST_FIELDS {
            if let Some(v) = summary.get(key).filter(|v| !v.is_null()) {
                row.insert(key.into(), v.clone());
            }
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let newest = sig.iter().flatten().map(|s| s.0).max().unwrap_or(0);
    let mut cache = CACHE.lock().unwrap();
    if !matches!(meta["status"].as_str(), Some("queued" | "processing")) && newest + RACY_NS < now {
        cache.insert(folder.to_path_buf(), (sig, row.clone()));
    } else {
        cache.remove(folder);
    }
    row
}

pub fn row(folder: &Path, meta: &Value) -> Map<String, Value> {
    let mut row = meta.as_object().cloned().unwrap_or_default();
    for key in ["bookmarks", "storage", "content_hashes"]
        .iter()
        .chain(&SUMMARY_COPIES)
        .chain(&INTERNAL)
    {
        row.remove(*key);
    }
    let warnings = meta["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| {
            !w.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| w.to_string())
                .starts_with(SKIPPED_EVENTS_WARNING)
        })
        .cloned()
        .collect::<Vec<_>>();
    row.insert("warnings".into(), json!(warnings));
    row.extend(file_fields(folder, meta));
    row
}

fn round3(v: f64) -> f64 {
    format!("{v:.3}").parse().unwrap_or(v)
}
/// `server.route_starts`: dates every segment of a route from one offset.
pub fn route_starts(items: &mut [Map<String, Value>], now: f64) {
    let mut routes: BTreeMap<String, Vec<(u64, usize)>> = BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        let order = RecordingOrder::of(&Value::Object(item.clone()));
        if let (Some(key), Some(n)) = (order.route_key(), order.segment()) {
            routes.entry(key).or_default().push((n, i));
        }
    }
    for segments in routes.values_mut() {
        segments.sort();
        let gps = segments
            .iter()
            .filter(|(_, i)| items[*i].get("start_source").is_some_and(|v| v == "gps"))
            .filter_map(|(n, i)| number(items[*i].get("gps_offset")?).map(|v| (*n, v)))
            .collect::<Vec<_>>();
        let clocks = segments
            .iter()
            .filter_map(|(n, i)| number(&items[*i].get("clock_offset").cloned()?).map(|v| (*n, v)))
            .collect::<Vec<_>>();
        if gps.is_empty() && clocks.is_empty() {
            continue;
        }
        let latest = clocks
            .iter()
            .copied()
            .max_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        for (number, i) in segments.iter() {
            let item = &mut items[*i];
            let Some(mono) = item.get("first_mono").filter(|v| v.is_i64() || v.is_u64()) else {
                continue;
            };
            if item.get("start_source").is_some_and(|v| v == "gps") {
                continue;
            }
            let mono = mono.as_f64().unwrap();
            let (offset, source) =
                if let Some(best) = gps.iter().min_by_key(|(n, _)| n.abs_diff(*number)) {
                    (best.1, "gps")
                } else {
                    (latest.unwrap().1, "clock")
                };
            let start = round3(mono / 1e9 + offset);
            if (EARLIEST..=now + 86400.).contains(&start) {
                item.insert("started_at".into(), json!(start));
                item.insert("start_source".into(), json!(source));
            }
        }
    }
    for item in items.iter_mut() {
        for key in ROUTE_START_FIELDS {
            item.remove(key);
        }
    }
}

pub fn sort(items: &mut [Map<String, Value>]) {
    items.sort_by_cached_key(|m| std::cmp::Reverse(RecordingOrder::of(&Value::Object(m.clone()))));
}

pub fn forget_missing(root: &Path, ids: &[String]) {
    let keep = ids.iter().map(|i| root.join(i)).collect::<Vec<_>>();
    CACHE
        .lock()
        .unwrap()
        .retain(|k, _| !k.starts_with(root) || keep.contains(k));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn route_starts_use_nearest_gps_then_latest_clock() {
        let row = |id: &str, n: u32, extra: Value| {
            let mut m = json!({"id":id,"name":format!("1a--2b--{n}")});
            m.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            m.as_object().unwrap().clone()
        };
        let mut items = vec![
            row(
                "a",
                0,
                json!({"first_mono":1_000_000_000u64,"clock_offset":1.6e9}),
            ),
            row(
                "b",
                1,
                json!({"first_mono":61_000_000_000u64,"start_source":"gps","gps_offset":1.7e9,"started_at":1.7e9}),
            ),
            row("c", 2, json!({"first_mono":121_000_000_000u64})),
        ];
        route_starts(&mut items, 2e9);
        assert_eq!(items[0]["started_at"], json!(1.7e9 + 1.));
        assert_eq!(items[2]["start_source"], "gps");
        assert!(items.iter().all(|i| !i.contains_key("first_mono")));
        let mut clocks = vec![
            row("a", 0, json!({"first_mono":0u64,"clock_offset":1.5e9})),
            row("b", 3, json!({"first_mono":1u64,"clock_offset":1.6e9})),
        ];
        route_starts(&mut clocks, 2e9);
        assert_eq!(clocks[0]["started_at"], json!(1.6e9));
        assert_eq!(clocks[0]["start_source"], "clock");
    }
}
