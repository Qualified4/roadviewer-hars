//! One isolated conversion worker. All outputs go to a caller-owned staging directory.
use crate::{logs, media, pure_remux::FailureClass, replay, store, telemetry, track};
use anyhow::Context;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const CAMERAS: [(&str, &str, &str, &str); 3] = [
    ("qcamera", "qcamera.ts", "camera.mp4", "qRoadEncodeIdx"),
    ("front", "fcamera.hevc", "fcamera.mp4", "roadEncodeIdx"),
    ("wide", "ecamera.hevc", "ecamera.mp4", "wideRoadEncodeIdx"),
];

/// Python `RV_MEDIA_PATHS` / `RV_RECORDING_DIR`: where the server found each video (the archive, a staged copy) and
/// the recording folder, when the log is not read from it.
pub const MEDIA_PATHS: &str = "HARS_MEDIA_PATHS";
pub const RECORDING_DIR: &str = "HARS_RECORDING_DIR";
pub const VIDEO_INPUTS: [&str; 6] = [
    "qcamera.ts",
    "fcamera.hevc",
    "ecamera.hevc",
    "camera.mp4",
    "fcamera.mp4",
    "ecamera.mp4",
];
fn input(src: &Path, kind: &str) -> PathBuf {
    if let Some(paths) = std::env::var(MEDIA_PATHS)
        .ok()
        .and_then(|v| serde_json::from_str::<BTreeMap<String, PathBuf>>(&v).ok())
    {
        return paths.get(kind).cloned().unwrap_or_default();
    }
    let filename = src.file_name().unwrap().to_string_lossy();
    if let Some(prefix) = filename.strip_suffix("--rlog.zst") {
        src.with_file_name(format!(
            "{prefix}--{}",
            if kind == "camera.mp4" {
                "qcamera.mp4"
            } else {
                kind
            }
        ))
    } else {
        src.with_file_name(kind)
    }
}
/// Left in the staging directory by a conversion that stopped to keep the published result:
/// `{"warning": ...}` for the record, which `jobs` returns to `ready`.
pub const KEPT_PREVIOUS: &str = "kept-previous.json";

/// Whether the result published now plays `camera`: its summary lists the camera and the MP4 it
/// plays is still there (in `prepared/`, stored next to the originals, or archived).
async fn previous_plays(recording: &Path, meta: &Value, camera: &str, mp4: &str) -> bool {
    let Ok(summary) = store::read_json::<Value>(&recording.join("prepared/summary.json")).await
    else {
        return false;
    };
    summary["videos"][camera].is_object()
        && (recording.join("prepared").join(mp4).is_file()
            || recording.join(mp4).is_file()
            || crate::archive::index::is_archived_location(&crate::archive::index::location(
                meta, mp4,
            )))
}
fn seconds(v: &Value) -> Option<f64> {
    v.as_str()
        .and_then(|v| v.parse().ok())
        .or_else(|| v.as_f64())
        .filter(|v: &f64| v.is_finite())
}
fn stamp(v: &Value) -> anyhow::Result<u64> {
    v["timestampEof"]
        .as_u64()
        .context("missing camera frame time")
}

fn timestamp(v: &Value, units: &Value) -> Option<f64> {
    let (n, d) = units.as_str()?.split_once('/')?;
    let (n, d) = (n.parse::<i64>().ok()?, d.parse::<i64>().ok()?);
    let value = v.as_i64()?;
    (d != 0).then(|| (value as i128 * n as i128) as f64 / d as f64)
}

/// End of exposure of the camera frame right after the last encoded one. The
/// encoder rotates segments between two frames, so the rlog still logs that
/// next frame's camera state; it bounds how long the last frame was shown.
fn next_camera_frame(decoded: &logs::Decoded, camera: &str, last: Option<&Value>) -> Option<u64> {
    let topic = if camera == "wide" {
        "wideRoadCameraState"
    } else {
        "roadCameraState"
    };
    let next = last?["frameId"].as_u64()?.checked_add(1)?;
    let mut found = decoded
        .streams
        .get(topic)?
        .iter()
        .filter(|row| row.value["frameId"].as_u64() == Some(next))
        .map(|row| row.value["timestampEof"].as_u64());
    let eof = found.next()??;
    // Two different records for one frame leave its time unknown.
    found.all(|other| other == Some(eof)).then_some(eof)
}

async fn camera(
    source: &Path,
    kind: &str,
    out: &Path,
    rows: &[&Value],
    origin: f64,
    source_pts: f64,
    next_eof: Option<u64>,
) -> anyhow::Result<Value> {
    let qcamera = matches!(kind, "qcamera.ts" | "camera.mp4");
    anyhow::ensure!(
        !rows.is_empty()
            && rows
                .iter()
                .enumerate()
                .all(|(i, v)| v["segmentId"].as_u64() == Some(i as u64)),
        "카메라 프레임 인덱스가 없거나 연속되지 않습니다."
    );
    let stamps = rows
        .iter()
        .map(|v| stamp(v))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let first = stamps[0];
    anyhow::ensure!(
        first > 0 && stamps.windows(2).all(|v| v[1] > v[0]),
        "카메라 프레임 시간이 올바르지 않습니다."
    );
    let mut times = stamps
        .iter()
        .map(|v| (v - first) as f64 / 1e9)
        .collect::<Vec<_>>();
    let mut source_pts = source_pts;
    let mut spread = None;
    let mut pts0 = 0.;
    let mut scanned = None;
    if qcamera {
        let inspection = media::inspect(source, kind).await?;
        let info = &inspection.info;
        let mut pts = info["packets"]
            .as_array()
            .context("missing packets")?
            .iter()
            .filter(|p| p["codec_type"] == "video")
            .map(|p| timestamp(&p["pts"], &info["streams"][0]["time_base"]).context("missing PTS"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        pts.sort_by(f64::total_cmp);
        anyhow::ensure!(
            pts.len() == rows.len(),
            "영상과 로그의 프레임 수가 일치하지 않아 영상 동기화를 중단했습니다."
        );
        let offsets = pts
            .iter()
            .zip(&stamps)
            .map(|(p, s)| *s as f64 / 1e9 - p)
            .collect::<Vec<_>>();
        let delta = offsets.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - offsets.iter().copied().fold(f64::INFINITY, f64::min);
        anyhow::ensure!(
            delta < 0.005,
            "영상과 로그의 프레임 시간이 일치하지 않아 영상 동기화를 중단했습니다."
        );
        spread = Some(delta * 1000.);
        pts0 = pts[0];
        times = if kind.ends_with(".ts") {
            source_pts = pts0;
            pts.iter().map(|p| p - pts0).collect()
        } else {
            pts
        };
        scanned = Some(inspection);
    }
    let next_frame_ns = next_eof
        .and_then(|next| next.checked_sub(stamps[stamps.len() - 1]))
        .filter(|gap| *gap > 0);
    let info = media::remux_probed(source, kind, out, &times, next_frame_ns, scanned).await?;
    let stream = &info["streams"][0];
    let duration = timestamp(&stream["duration_ts"], &stream["time_base"])
        .or_else(|| seconds(&stream["duration"]))
        .unwrap_or(times[times.len() - 1] + 0.05);
    let mut result = json!({"duration":duration,"frames":times.len(),"width":stream["width"],"height":stream["height"],"codec":stream["codec_name"],"start":first as f64/1e9-origin-if qcamera && kind.ends_with(".mp4") {pts0}else{0.}});
    if qcamera {
        result["timestampSpreadMs"] = json!(spread);
        result["sourcePts"] = json!(source_pts);
    }
    Ok(result)
}

pub async fn prepare(
    source: PathBuf,
    dest: PathBuf,
    label: String,
    mut progress: impl FnMut(&str, f64),
) -> anyhow::Result<Value> {
    let _profile = crate::profile::Scope::new("convert_total");
    anyhow::ensure!(source.is_file(), "missing rlog source");
    anyhow::ensure!(!dest.exists(), "conversion destination must be new");
    tokio::fs::create_dir_all(&dest).await?;
    // Reserve a margin, and let every write surface ENOSPC. This does not delete
    // or move any input, nor claim that an estimate guarantees a complete job.
    anyhow::ensure!(
        fs2::available_space(&dest)? > media::SPACE_MARGIN,
        "insufficient conversion disk space"
    );
    progress("log_read", 0.);
    let src = source.clone();
    let (decoded, origin) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let decoded = logs::decode(&src)?;
        let origin = replay::origin(&decoded)?;
        Ok((decoded, origin))
    })
    .await??;
    progress("log_read", 100.);
    let recording = std::env::var_os(RECORDING_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|| source.parent().map(Path::to_path_buf).unwrap_or_default());
    let meta: Value = store::read_json(&recording.join("meta.json"))
        .await
        .unwrap_or(json!({}));
    let mut videos = BTreeMap::<String, Value>::new();
    let mut cache_seed = format!(
        "{}:{:?}:{:?}:hars-replay-2",
        source.display(),
        source.metadata()?.modified()?,
        meta.get("video_origins")
    );
    let mut warnings = vec![];
    let mut playable = BTreeMap::new();
    for (index, (name, raw, mp4, topic)) in CAMERAS.iter().enumerate() {
        let a = input(&source, raw);
        let b = input(&source, mp4);
        let (path, kind) = if a.is_file() {
            (a, *raw)
        } else if b.is_file() {
            (b, *mp4)
        } else {
            continue;
        };
        cache_seed.push_str(&format!(
            "{}:{:?}",
            path.display(),
            path.metadata()?.modified()?
        ));
        let mut rows = decoded
            .streams
            .get(*topic)
            .map(|r| r.iter().map(|r| &r.value).collect::<Vec<_>>())
            .unwrap_or_default();
        rows.sort_by_key(|r| r["segmentId"].as_u64().unwrap_or(u64::MAX));
        let next_eof = next_camera_frame(&decoded, name, rows.last().copied());
        progress("video_read", index as f64 * 100. / 3.);
        match camera(
            &path,
            kind,
            &dest.join(mp4),
            &rows,
            origin,
            meta["video_origins"][name].as_f64().unwrap_or(0.),
            next_eof,
        )
        .await
        {
            Ok(info) => {
                playable.insert(
                    *name,
                    (
                        if kind.ends_with(".mp4") {
                            path
                        } else {
                            dest.join(mp4)
                        },
                        *mp4,
                    ),
                );
                videos.insert(name.to_string(), info);
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(dest.join(mp4)).await;
                let failure = media::failure(&error);
                let label = if *name == "qcamera" { "qcamera" } else { raw };
                if failure.class != FailureClass::Resource {
                    warnings.push(format!("{label} 영상 준비 실패: {error}"));
                    continue;
                }
                let warning = format!(
                    "{label} 영상 준비 실패: {} ({}): {}",
                    failure.class.name(),
                    failure.code,
                    failure.reason
                );
                // The same disk, memory or deadline may not fail the next attempt, unlike an input or
                // engine failure: a camera the published result plays is not dropped for it. Only the
                // files on disk say so: `meta.videos` outlives `clear_prepared`, as in Python.
                if previous_plays(&recording, &meta, name, mp4).await {
                    tracing::error!(
                        event = "conversion_kept_previous",
                        kind = *raw,
                        failure_class = failure.class.name(),
                        reason_code = failure.code,
                        reason = failure.reason,
                    );
                    store::atomic_json(
                        &dest.join(KEPT_PREVIOUS),
                        &json!({"warning": format!("{warning} (이전 변환 결과 유지)")}),
                    )
                    .await?;
                    return Err(anyhow::Error::new(failure)
                        .context(format!("{label}: previous conversion result kept")));
                }
                warnings.push(warning);
            }
        }
    }
    if videos.is_empty() {
        warnings.push("재생 가능한 영상이 없어 도로 형태만 표시합니다.".to_owned());
    }
    progress("video_verify", 100.);
    // Inspect cameras before allocating the frame tree. Native processes no
    // longer overlap the largest render allocation in the process-tree RSS.
    progress("log_analysis", 0.);
    let (mut decoded, mut frames) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mut decoded = decoded;
        let _profile = crate::profile::Scope::new("frame_analysis");
        let (frames, _) = replay::frames(&decoded, |_, _| {})?;
        decoded.streams.retain(|kind, _| {
            matches!(
                kind.as_str(),
                "carState"
                    | "carControl"
                    | "carOutput"
                    | "controlsState"
                    | "gpsLocationExternal"
                    | "gpsLocation"
                    | "carrotMan"
            )
        });
        Ok((decoded, frames))
    })
    .await??;
    progress("log_analysis", 100.);

    let default_video = ["front", "qcamera", "wide"]
        .into_iter()
        .find(|v| videos.contains_key(*v));
    let start = videos
        .values()
        .filter_map(|v| v["start"].as_f64())
        .fold(frames[0]["t"].as_f64().unwrap(), f64::min);
    for f in &mut frames {
        f["t"] = json!(replay::r(f["t"].as_f64().unwrap() - start, 6));
    }
    for v in videos.values_mut() {
        v["start"] = json!(replay::r(v["start"].as_f64().unwrap() - start, 6));
    }
    let log_start = frames[0]["t"].as_f64().unwrap();
    let log_end = frames.last().unwrap()["t"].as_f64().unwrap();
    let duration = videos
        .values()
        .map(|v| v["start"].as_f64().unwrap() + v["duration"].as_f64().unwrap())
        .fold(log_end, f64::max);
    let telemetry_origin = origin - log_start;
    progress("telemetry_build", 0.);
    let target = dest.clone();
    let label = label.clone();
    let summary=tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let telemetry_profile=crate::profile::Scope::new("telemetry_build");
        let telem=telemetry::extract(&decoded.streams,telemetry_origin,duration,&frames);
        drop(telemetry_profile);
        let drive=telemetry::drive_summary(&telem);
        {let _p=crate::profile::Scope::new("telemetry_save");logs::write_gzip(&target.join("telemetry.json.gz"),&telem)?;}
        drop(telem);
        let track=track::collect(&decoded,telemetry_origin);let track_fields=track::summary(&track);
        if track["points"].as_array().is_some_and(|p|!p.is_empty()){logs::write_gzip(&target.join("track.json.gz"),&track)?;}
        let video=default_video.and_then(|s|videos.get(s)).cloned().unwrap_or(Value::Null);
        let mut summary=json!({"duration":duration,"warnings":warnings,"model_frames":frames.len(),"video":video,"videos":videos,"drive_summary":drive,"replay_v":2});
        summary.as_object_mut().unwrap().extend(decoded.starts.summary().as_object().unwrap().clone());
        summary.as_object_mut().unwrap().extend(track_fields.as_object().unwrap().clone());
        let counts=std::mem::take(&mut decoded.counts);
        drop(decoded);
        let mut data=json!({"route":label,"key":&store::sha(cache_seed)[..20],"duration":duration,"logStart":log_start,"logEnd":log_end,"video":video,"videos":videos,"defaultVideo":default_video,"warnings":warnings,"counts":counts});
        // json!(frames) serializes a borrowed value and clones the entire replay.
        // Transfer ownership instead; compact the sole frame tree in place.
        data["frames"]=Value::Array(frames);
        replay::compact(&mut data);
        {let _p=crate::profile::Scope::new("replay_save");logs::write_gzip(&target.join("data.json.gz"),&data)?;}
        Ok(summary)
    }).await??;
    progress("telemetry_compress", 100.);
    // Optional pixel decoding is bounded separately from log/bitstream analysis.
    let mut thumbnail = false;
    for name in ["qcamera", "front", "wide"] {
        if let Some((path, kind)) = playable.get(name)
            && media::thumbnail(path, kind, &dest.join("thumbnail.jpg"))
                .await
                .is_ok()
        {
            thumbnail = true;
            break;
        }
    }
    if !thumbnail && !playable.is_empty() {
        tokio::fs::write(dest.join("thumbnail.failed"), []).await?;
    }
    store::atomic_json(&dest.join("summary.json"), &summary).await?;
    progress("summary_save", 100.);
    Ok(summary)
}
