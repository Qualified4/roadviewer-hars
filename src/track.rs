//! GPS and wall-clock calculations from the preserved Python conversion contract,
//! plus in-memory track caching and lightweight ETag generation for `/api/tracks` and `/api/logs/{id}/track`.
use crate::{logs::Decoded, replay::r};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::UNIX_EPOCH,
};

fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|v| v.is_finite())
}
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
fn plausible(t: f64, now: f64) -> bool {
    (1420070400.0..=now + 86400.).contains(&t)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Starts {
    first: Option<u64>,
    init_mono: Option<u64>,
    init: Option<(u64, f64)>,
    clock: Option<(u64, f64)>,
    gps: Option<(u64, f64)>,
}
impl Starts {
    pub fn note(&mut self, kind: &str, mono: u64) {
        if kind == "initData" {
            self.init_mono.get_or_insert(mono);
        } else {
            self.first = Some(self.first.map_or(mono, |first| first.min(mono)));
        }
    }
    pub fn add(&mut self, kind: &str, mono: u64, valid: bool, value: &Value) {
        if kind == "initData" && self.init.is_none() {
            if let Some(wall) = number(&value["wallTimeNanos"]).filter(|v| *v != 0.) {
                self.init = Some((mono, wall / 1e9));
            }
        } else if kind == "clocks" {
            if let Some(wall) = number(&value["wallTimeNanos"])
                .map(|v| v / 1e9)
                .filter(|v| plausible(*v, now()))
            {
                self.clock = Some((mono, wall));
            }
        } else if matches!(kind, "gpsLocation" | "gpsLocationExternal")
            && self.gps.is_none()
            && valid
            && value["hasFix"] == true
            && let Some(wall) = number(&value["unixTimestampMillis"])
                .map(|v| v / 1e3)
                .filter(|v| plausible(*v, now()))
        {
            self.gps = Some((mono, wall));
        }
    }
    pub fn fields(&self, now: f64) -> Value {
        let first = self.first.or(self.init_mono);
        let chosen = first.and_then(|first| {
            [
                (self.gps, "gps"),
                (self.clock, "clock"),
                (self.init, "device"),
            ]
            .into_iter()
            .find_map(|(candidate, name)| {
                let (mono, wall) = candidate?;
                // Subtraction in floating point must permit an initData earlier than this segment.
                let start = wall - (mono as i128 - first as i128) as f64 / 1e9;
                (plausible(wall, now) && plausible(start, now)).then_some((r(start, 3), name))
            })
        });
        json!({"started_at":chosen.map(|v|v.0),"start_source":chosen.map(|v|v.1),"first_mono":first,
            "gps_offset":if chosen.is_some_and(|v|v.1=="gps") {self.gps.map(|(m,w)|r(w-m as f64/1e9,6))} else {None},
            "clock_offset":self.clock.map(|(m,w)|r(w-m as f64/1e9,6)),"start_v":3})
    }
    pub fn summary(&self) -> Value {
        self.fields(now())
    }
}

pub fn distance(a: &[f64; 2], b: &[f64; 2]) -> f64 {
    let (la1, lo1, la2, lo2) = (
        a[0].to_radians(),
        a[1].to_radians(),
        b[0].to_radians(),
        b[1].to_radians(),
    );
    let h =
        ((la2 - la1) / 2.).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.).sin().powi(2);
    2. * 6371008.8 * h.sqrt().min(1.).asin()
}
pub fn collect(decoded: &Decoded, origin: f64) -> Value {
    let _profile = crate::profile::Scope::new("gps_build");
    let mut chosen = vec![];
    let mut source = None;
    for topic in [
        "gpsLocationExternal",
        "gpsLocation",
        "vehicleGps",
        "carrotMan",
    ] {
        let mut fixes = vec![];
        let mut previous = None;
        let rows = decoded
            .streams
            .get(if topic == "vehicleGps" {
                "carState"
            } else {
                topic
            })
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for row in rows {
            let v = &row.value;
            let (stamp, lat, lon, speed, bearing) = match topic {
                "vehicleGps" => {
                    let g = &v["vehicleGps"];
                    if g["available"] != true || g["valid"] != true {
                        continue;
                    }
                    let sample = g["sampleMonoTime"].as_u64().filter(|v| *v != 0);
                    if sample.is_some() && sample == previous {
                        continue;
                    }
                    (
                        sample.unwrap_or(row.stamp),
                        number(&g["latitude"]),
                        number(&g["longitude"]),
                        number(&v["vEgo"]),
                        if g["headingValid"] == true {
                            number(&g["headingDeg"])
                        } else {
                            None
                        },
                    )
                }
                "carrotMan" => (
                    row.stamp,
                    number(&v["xPosLat"]),
                    number(&v["xPosLon"]),
                    None,
                    number(&v["xPosAngle"]),
                ),
                _ => {
                    if !row.valid
                        || v["hasFix"] != true
                        || !number(&v["horizontalAccuracy"])
                            .is_some_and(|v| (0.0..=100.).contains(&v))
                    {
                        continue;
                    }
                    (
                        row.stamp,
                        number(&v["latitude"]),
                        number(&v["longitude"]),
                        number(&v["speed"]),
                        number(&v["bearingDeg"]),
                    )
                }
            };
            let (Some(lat), Some(lon)) = (lat, lon) else {
                continue;
            };
            if !(-90.0..=90.).contains(&lat)
                || !(-180.0..=180.).contains(&lon)
                || (lat.abs() < 1e-7 && lon.abs() < 1e-7)
            {
                continue;
            }
            if topic == "vehicleGps" {
                previous = v["vehicleGps"]["sampleMonoTime"]
                    .as_u64()
                    .filter(|v| *v != 0);
            }
            fixes.push((
                stamp,
                lat,
                lon,
                speed.filter(|v| *v >= 0.),
                bearing.map(|v| v.rem_euclid(360.)),
            ));
        }
        if !fixes.is_empty() {
            chosen = fixes;
            source = Some(topic);
            break;
        }
    }
    chosen.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then(a.2.total_cmp(&b.2))
    });
    let mut points: Vec<Value> = vec![];
    let mut second = None;
    for (stamp, lat, lon, speed, bearing) in chosen {
        let t = stamp as f64 / 1e9 - origin;
        let bucket = t.floor() as i64;
        if second == Some(bucket) {
            continue;
        }
        if let Some(last) = points.last() {
            let gap = t - last[0].as_f64().unwrap();
            if gap > 0.
                && distance(
                    &[last[1].as_f64().unwrap(), last[2].as_f64().unwrap()],
                    &[lat, lon],
                ) / gap
                    > 90.
            {
                continue;
            }
        }
        second = Some(bucket);
        points.push(json!([
            r(t, 2),
            r(lat, 6),
            r(lon, 6),
            speed.map(|v| r(v, 1)),
            bearing.map(|v| r(v, 0))
        ]));
    }
    json!({"v":2,"source":source,"points":points})
}
pub fn summary(track: &Value) -> Value {
    let points = track["points"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let coord = |p: &Value| [p[1].as_f64().unwrap(), p[2].as_f64().unwrap()];
    let distance = points
        .windows(2)
        .map(|p| distance(&coord(&p[0]), &coord(&p[1])))
        .sum::<f64>();
    let bbox = if points.is_empty() {
        None
    } else {
        let mut b = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for p in points {
            let [lat, lon] = coord(p);
            b[0] = b[0].min(lat);
            b[1] = b[1].min(lon);
            b[2] = b[2].max(lat);
            b[3] = b[3].max(lon);
        }
        Some(b)
    };
    json!({"track_v":2,"has_track":!points.is_empty(),"track_distance_m":if points.is_empty(){None}else{Some(distance.round_ties_even() as u64)},"track_bbox":bbox,"track_read":true})
}

pub fn simplify(coords: &[[f64; 2]], limit: usize) -> Vec<[f64; 2]> {
    if coords.len() <= limit {
        return coords.to_vec();
    }
    assert!(limit >= 2);
    let scale = (coords.iter().map(|p| p[0]).sum::<f64>() / coords.len() as f64)
        .to_radians()
        .cos();
    let xy = coords
        .iter()
        .map(|p| {
            [
                p[1].to_radians() * scale * 6371008.8,
                p[0].to_radians() * 6371008.8,
            ]
        })
        .collect::<Vec<_>>();
    let mut tolerance = 2.;
    loop {
        let mut marked = vec![false; xy.len()];
        marked[0] = true;
        marked[xy.len() - 1] = true;
        let mut stack = vec![(0, xy.len() - 1)];
        while let Some((a, b)) = stack.pop() {
            let ([ax, ay], [bx, by]) = (xy[a], xy[b]);
            let (dx, dy) = (bx - ax, by - ay);
            let length = dx.hypot(dy);
            let mut best = -1.;
            let mut index = None;
            for (i, [px, py]) in xy.iter().copied().enumerate().take(b).skip(a + 1) {
                let distance = if length != 0. {
                    (dy * (px - ax) - dx * (py - ay)).abs() / length
                } else {
                    (px - ax).hypot(py - ay)
                };
                if distance > best {
                    best = distance;
                    index = Some(i);
                }
            }
            if best > tolerance
                && let Some(index) = index
            {
                marked[index] = true;
                stack.push((a, index));
                stack.push((index, b));
            }
        }
        let out = coords
            .iter()
            .zip(marked)
            .filter_map(|(p, keep)| keep.then_some(*p))
            .collect::<Vec<_>>();
        if out.len() <= limit {
            return out;
        }
        tolerance *= 2.;
    }
}

type FileStamp = (u128, u64, u64);
type SegmentSig = (Option<FileStamp>, Option<FileStamp>);
type RouteGroupEntry = (Vec<String>, Vec<(Option<u64>, String)>);

#[derive(Clone, Debug)]
struct CachedSegment {
    has_track: bool,
    duration: Value,
    track_distance_m: Value,
    simplified_150: Vec<[f64; 2]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RouteInputSig {
    all_ids: Vec<String>,
    ready_segments: Vec<(String, Option<u64>, SegmentSig)>,
}

#[derive(Clone, Debug)]
struct CachedRoute {
    sig: RouteInputSig,
    route: Option<Value>,
}

#[derive(Clone, Debug)]
struct CachedTracksResponse {
    etag: String,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct TrackCache {
    segments: HashMap<PathBuf, (SegmentSig, CachedSegment)>,
    routes: HashMap<(PathBuf, String), CachedRoute>,
    responses: HashMap<PathBuf, CachedTracksResponse>,
}

static TRACK_CACHE: LazyLock<Mutex<TrackCache>> = LazyLock::new(Default::default);

pub enum TracksAnswer {
    NotModified { etag: String },
    Ok { etag: String, bytes: Vec<u8> },
}

pub fn route_order(meta: &Value) -> (String, Option<u64>) {
    let order = crate::parity::RecordingOrder::of(meta);
    match order.route_key() {
        Some(key) => (key, order.segment()),
        None => (meta["id"].as_str().unwrap_or("").into(), None),
    }
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let ino = 0;
    Some((mtime, m.len(), ino))
}

fn segment_sig(folder: &Path) -> SegmentSig {
    let prepared = folder.join("prepared");
    (
        file_stamp(&prepared.join("summary.json")),
        file_stamp(&prepared.join("track.json.gz")),
    )
}

fn hash_stamp(hasher: &mut Sha256, stamp: Option<FileStamp>) {
    match stamp {
        None => hasher.update([0u8]),
        Some((mtime, len, ino)) => {
            hasher.update([1u8]);
            hasher.update(mtime.to_le_bytes());
            hasher.update(len.to_le_bytes());
            hasher.update(ino.to_le_bytes());
        }
    }
}

fn hash_segment_sig(hasher: &mut Sha256, sig: SegmentSig) {
    hash_stamp(hasher, sig.0);
    hash_stamp(hasher, sig.1);
}

fn empty_segment() -> CachedSegment {
    CachedSegment {
        has_track: false,
        duration: Value::Null,
        track_distance_m: Value::Null,
        simplified_150: vec![],
    }
}

fn load_segment(folder: &Path, sig: SegmentSig) -> CachedSegment {
    {
        let cache = TRACK_CACHE.lock().unwrap();
        if let Some((cached_sig, seg)) = cache.segments.get(folder)
            && *cached_sig == sig
        {
            return seg.clone();
        }
    }
    let seg = if sig.0.is_none() || sig.1.is_none() {
        empty_segment()
    } else {
        let prepared = folder.join("prepared");
        let parsed = std::fs::read(prepared.join("summary.json"))
            .ok()
            .and_then(|v| serde_json::from_slice::<Value>(&v).ok());
        match parsed {
            Some(summary) if summary["has_track"] == true => {
                match crate::logs::read_gzip(&prepared.join("track.json.gz")) {
                    Ok(track) if track["points"].as_array().is_some_and(|p| !p.is_empty()) => {
                        let coords = track["points"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter_map(|p| Some([p[1].as_f64()?, p[2].as_f64()?]))
                            .collect::<Vec<_>>();
                        CachedSegment {
                            has_track: true,
                            duration: summary["duration"].clone(),
                            track_distance_m: summary["track_distance_m"].clone(),
                            simplified_150: simplify(&coords, 150),
                        }
                    }
                    _ => empty_segment(),
                }
            }
            _ => empty_segment(),
        }
    };
    TRACK_CACHE
        .lock()
        .unwrap()
        .segments
        .insert(folder.to_path_buf(), (sig, seg.clone()));
    seg
}

/// Invalidates cached track data for one recording folder and its store's aggregated caches.
pub fn invalidate_recording(folder: &Path) {
    let mut cache = TRACK_CACHE.lock().unwrap();
    cache.segments.remove(folder);
    if let Some(root) = folder.parent() {
        cache.responses.remove(root);
        cache.routes.retain(|(r, _), _| r != root);
    }
}

/// Invalidates all cached track data belonging to a store root.
pub fn invalidate_root(root: &Path) {
    let mut cache = TRACK_CACHE.lock().unwrap();
    cache.responses.remove(root);
    cache.routes.retain(|(r, _), _| r != root);
    cache.segments.retain(|p, _| !p.starts_with(root));
}

/// Returns `(cached_segments, cached_routes, has_cached_tracks_response)` for a store root.
pub fn cache_counts(root: &Path) -> (usize, usize, bool) {
    let cache = TRACK_CACHE.lock().unwrap();
    let segments = cache
        .segments
        .keys()
        .filter(|p| p.starts_with(root))
        .count();
    let routes = cache.routes.keys().filter(|(r, _)| r == root).count();
    let response = cache.responses.contains_key(root);
    (segments, routes, response)
}

/// Builds the `/api/logs/{id}/track` response for a single route (`key`) only,
/// skipping disk reads completely on a lightweight ETag match (`304 Not Modified`).
pub fn build_single_track(
    root: &Path,
    id: &str,
    key: &str,
    ready_segments: &[(Option<u64>, String)],
    if_none_match: &str,
) -> TracksAnswer {
    let stamped = ready_segments
        .iter()
        .map(|(segment, seg_id)| (*segment, seg_id.clone(), segment_sig(&root.join(seg_id))))
        .collect::<Vec<_>>();

    let mut hasher = Sha256::new();
    hasher.update(b"rv-track-v1\0");
    hasher.update(id.as_bytes());
    hasher.update([0u8]);
    hasher.update(key.as_bytes());
    hasher.update([0u8]);
    for (segment, seg_id, sig) in &stamped {
        hasher.update(seg_id.as_bytes());
        hasher.update([0u8]);
        match segment {
            None => hasher.update([0u8]),
            Some(n) => {
                hasher.update([1u8]);
                hasher.update(n.to_le_bytes());
            }
        }
        hash_segment_sig(&mut hasher, *sig);
    }
    let etag = format!("\"{}\"", &hex::encode(hasher.finalize())[..32]);
    if if_none_match == etag {
        return TracksAnswer::NotModified { etag };
    }

    let mut segments = vec![];
    for (segment, seg_id, sig) in stamped {
        if sig.0.is_none() || sig.1.is_none() {
            continue;
        }
        let folder = root.join(&seg_id);
        let cached_meta = {
            let cache = TRACK_CACHE.lock().unwrap();
            cache
                .segments
                .get(&folder)
                .filter(|(s, _)| *s == sig)
                .map(|(_, c)| (c.has_track, c.duration.clone(), c.track_distance_m.clone()))
        };
        let (duration, distance_m) = match cached_meta {
            Some((false, _, _)) => continue,
            Some((true, dur, dist)) => (dur, dist),
            None => {
                let prepared = folder.join("prepared");
                let Ok(summary) = std::fs::read(prepared.join("summary.json")).and_then(|v| {
                    serde_json::from_slice::<Value>(&v).map_err(std::io::Error::other)
                }) else {
                    continue;
                };
                if summary["has_track"] != true {
                    TRACK_CACHE
                        .lock()
                        .unwrap()
                        .segments
                        .insert(folder.clone(), (sig, empty_segment()));
                    continue;
                }
                (
                    summary["duration"].clone(),
                    summary["track_distance_m"].clone(),
                )
            }
        };
        let Ok(track) = crate::logs::read_gzip(&folder.join("prepared").join("track.json.gz"))
        else {
            continue;
        };
        if track["points"].as_array().is_none_or(|p| p.is_empty()) {
            continue;
        }
        segments.push(json!({
            "id": seg_id,
            "segment": segment,
            "duration": duration,
            "distance_m": distance_m,
            "points": track["points"],
        }));
    }
    let bytes = serde_json::to_vec(&json!({"id": id, "segments": segments})).unwrap();
    TracksAnswer::Ok { etag, bytes }
}

/// Builds the `/api/tracks` response using the three-tier in-memory cache
/// (per-segment `simplified_150`, per-route `simplified_300`, and serialized response + lightweight ETag).
pub fn build_tracks(
    root: &Path,
    metas: &[Value],
    wanted: Option<&BTreeSet<String>>,
    if_none_match: &str,
) -> TracksAnswer {
    // Group all recordings by route key, preserving `metas` ordering (sorted by recording id).
    let mut groups: BTreeMap<String, RouteGroupEntry> = BTreeMap::new();
    for m in metas {
        let Some(rec_id) = m["id"].as_str() else {
            continue;
        };
        let (key, segment) = route_order(m);
        let entry = groups.entry(key).or_default();
        entry.0.push(rec_id.to_owned());
        if m["status"] == "ready" {
            entry.1.push((segment, rec_id.to_owned()));
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(b"rv-tracks-v1\0");
    if let Some(wanted) = wanted {
        hasher.update(b"filter\0");
        for id in wanted {
            hasher.update(id.as_bytes());
            hasher.update([0u8]);
        }
    }

    let mut stamped_groups: Vec<(String, RouteInputSig)> = Vec::with_capacity(groups.len());
    for (key, (all_ids, mut ready)) in groups {
        if wanted.is_some_and(|w| !all_ids.iter().any(|id| w.contains(id))) {
            continue;
        }
        ready.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let ready_segments = ready
            .into_iter()
            .map(|(segment, seg_id)| {
                let sig = segment_sig(&root.join(&seg_id));
                (seg_id, segment, sig)
            })
            .collect::<Vec<_>>();

        hasher.update(key.as_bytes());
        hasher.update([0u8]);
        for id in &all_ids {
            hasher.update(id.as_bytes());
            hasher.update([0u8]);
        }
        hasher.update([1u8]);
        for (seg_id, segment, sig) in &ready_segments {
            hasher.update(seg_id.as_bytes());
            hasher.update([0u8]);
            match segment {
                None => hasher.update([0u8]),
                Some(n) => {
                    hasher.update([1u8]);
                    hasher.update(n.to_le_bytes());
                }
            }
            hash_segment_sig(&mut hasher, *sig);
        }
        stamped_groups.push((
            key,
            RouteInputSig {
                all_ids,
                ready_segments,
            },
        ));
    }

    let etag = format!("\"{}\"", &hex::encode(hasher.finalize())[..32]);
    if if_none_match == etag {
        return TracksAnswer::NotModified { etag };
    }

    if wanted.is_none() {
        let cache = TRACK_CACHE.lock().unwrap();
        if let Some(cached) = cache.responses.get(root)
            && cached.etag == etag
        {
            return TracksAnswer::Ok {
                etag,
                bytes: cached.bytes.clone(),
            };
        }
    }

    if wanted.is_none() {
        let keep_folders: BTreeSet<PathBuf> = metas
            .iter()
            .filter_map(|m| m["id"].as_str().map(|id| root.join(id)))
            .collect();
        let keep_keys: BTreeSet<&str> = stamped_groups.iter().map(|(k, _)| k.as_str()).collect();
        let mut cache = TRACK_CACHE.lock().unwrap();
        cache
            .segments
            .retain(|p, _| !p.starts_with(root) || keep_folders.contains(p));
        cache
            .routes
            .retain(|(r, k), _| r != root || keep_keys.contains(k.as_str()));
    }

    let mut routes = vec![];
    for (key, sig) in stamped_groups {
        let cached = {
            let cache = TRACK_CACHE.lock().unwrap();
            cache
                .routes
                .get(&(root.to_path_buf(), key.clone()))
                .filter(|c| c.sig == sig)
                .map(|c| c.route.clone())
        };
        let route = match cached {
            Some(route) => route,
            None => {
                let mut segments = vec![];
                let mut points = vec![];
                let mut distance_m = 0.0f64;
                for (seg_id, _, seg_sig) in &sig.ready_segments {
                    let seg = load_segment(&root.join(seg_id), *seg_sig);
                    if !seg.has_track {
                        continue;
                    }
                    segments.push(json!(seg_id));
                    points.extend_from_slice(&seg.simplified_150);
                    distance_m += seg.track_distance_m.as_f64().unwrap_or(0.);
                }
                let built = if segments.is_empty() {
                    None
                } else {
                    Some(json!({
                        "key": key,
                        "ids": sig.all_ids,
                        "segments": segments,
                        "points": simplify(&points, 300),
                        "distance_m": distance_m,
                    }))
                };
                TRACK_CACHE.lock().unwrap().routes.insert(
                    (root.to_path_buf(), key),
                    CachedRoute {
                        sig,
                        route: built.clone(),
                    },
                );
                built
            }
        };
        if let Some(route) = route {
            routes.push(route);
        }
    }

    let bytes = serde_json::to_vec(&json!({"routes": routes})).unwrap();
    if wanted.is_none() {
        TRACK_CACHE.lock().unwrap().responses.insert(
            root.to_path_buf(),
            CachedTracksResponse {
                etag: etag.clone(),
                bytes: bytes.clone(),
            },
        );
    }
    TracksAnswer::Ok { etag, bytes }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn route_init_does_not_set_segments_first_clock_and_gps_wins() {
        let mut clock = Starts::default();
        clock.note("initData", 1_000_000_000);
        clock.add(
            "initData",
            1_000_000_000,
            true,
            &json!({"wallTimeNanos":1600000000000000000u64}),
        );
        clock.note("unselectedEvent", 11_000_000_000);
        clock.add(
            "clocks",
            12_000_000_000,
            true,
            &json!({"wallTimeNanos":1700000001000000000u64}),
        );
        clock.add(
            "gpsLocation",
            13_000_000_000,
            true,
            &json!({"hasFix":true,"unixTimestampMillis":1700000003000u64}),
        );
        let fields = clock.fields(1800000000.);
        assert_eq!(fields["first_mono"], 11_000_000_000u64);
        assert_eq!(fields["started_at"], 1700000001.);
        assert_eq!(fields["start_source"], "gps");
        assert_eq!(fields["clock_offset"], 1699999989.);
    }
}
