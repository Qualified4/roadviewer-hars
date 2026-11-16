//! A camera whose remux fails, for the machine (here the worker memory limit)
//! or for its own input, ends as Python's `prepare_video` failure does: a
//! warning on a ready recording without that video, the other camera intact.
//! A resource failure is not retried by FFmpeg. The original stays: retention
//! off deletes only originals whose MP4 the conversion produced (`jobs`).
//! Real executable, synthetic media and log.
#[path = "common/recording.rs"]
mod recording;
use recording::{FRAMES, encode, rlog};
use roadviewer_hars::{convert, logs};
use serde_json::json;
use std::{path::Path, process::Output};

fn media(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    encode(
        &[
            "-c:v", "libx264", "-bf", "0", "-g", "20", "-pix_fmt", "yuv420p", "-f", "mpegts",
        ],
        &dir.join("qcamera.ts"),
    );
    encode(
        &[
            "-c:v",
            "libx265",
            "-x265-params",
            "pools=1:frame-threads=1:bframes=0:keyint=20:log-level=error",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "hevc",
        ],
        &dir.join("fcamera.hevc"),
    );
    std::fs::write(dir.join("rlog.zst"), rlog()).unwrap();
}

fn convert(dir: &Path, out: &Path, memory_mb: Option<&str>) -> Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_roadviewer-hars"));
    command
        .arg("convert-log")
        .arg(dir.join("rlog.zst"))
        .arg(out)
        .arg("resource")
        .env(
            convert::MEDIA_PATHS,
            json!({"qcamera.ts": dir.join("qcamera.ts"), "fcamera.hevc": dir.join("fcamera.hevc")})
                .to_string(),
        )
        .env_remove("HARS_REMUX_ENGINE")
        .env("RUST_LOG", "roadviewer_hars=info");
    if let Some(mb) = memory_mb {
        command.env("HARS_PURE_WORKER_MEMORY_MB", mb);
    }
    command.output().unwrap()
}

fn log(run: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    )
}

fn warned(data: &serde_json::Value) -> bool {
    data["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w.as_str().unwrap().contains("영상 준비 실패"))
}

#[test]
fn a_camera_failing_for_memory_is_a_warning_without_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("media");
    media(&dir);
    let complete = convert(&dir, &temp.path().join("complete"), None);
    assert!(complete.status.success(), "{}", log(&complete));
    let data = logs::read_gzip(&temp.path().join("complete/data.json.gz")).unwrap();
    assert_eq!(data["videos"]["front"]["frames"], FRAMES);
    assert_eq!(data["videos"]["qcamera"]["frames"], FRAMES);
    assert!(!warned(&data));

    let before = std::fs::read(dir.join("fcamera.hevc")).unwrap();
    let limited = temp.path().join("limited");
    let run = convert(&dir, &limited, Some("1"));
    let text = log(&run);
    assert!(run.status.success(), "{text}");
    let data = logs::read_gzip(&limited.join("data.json.gz")).unwrap();
    assert!(data["videos"].as_object().unwrap().is_empty(), "{data}");
    assert!(warned(&data), "{}", data["warnings"]);
    assert!(!limited.join("fcamera.mp4").exists() && !limited.join("camera.mp4").exists());
    assert_eq!(
        text.matches("\"failure_class\":\"resource\"").count(),
        2,
        "{text}"
    );
    assert!(text.contains("out_of_memory"), "{text}");
    assert!(!text.contains("remux_fallback"), "{text}");
    assert_eq!(std::fs::read(dir.join("fcamera.hevc")).unwrap(), before);
}

#[test]
fn a_camera_failing_for_its_input_is_a_warning() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("media");
    media(&dir);
    let raw = std::fs::read(dir.join("fcamera.hevc")).unwrap();
    std::fs::write(dir.join("fcamera.hevc"), &raw[raw.len() / 2..]).unwrap();
    let out = temp.path().join("out");
    let run = convert(&dir, &out, None);
    assert!(run.status.success(), "{}", log(&run));
    let data = logs::read_gzip(&out.join("data.json.gz")).unwrap();
    assert!(data["videos"].get("front").is_none(), "{data}");
    assert_eq!(data["videos"]["qcamera"]["frames"], FRAMES);
    assert!(warned(&data), "{}", data["warnings"]);
}
