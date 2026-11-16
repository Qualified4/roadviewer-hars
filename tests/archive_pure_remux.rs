//! Converting again a recording whose raw originals the archiver moved (share
//! read in place, Drive staged copy) runs the default pure engine on those
//! inputs through the real executable and its worker process, with no fallback.
//! Media and log are synthetic.
mod common;
#[path = "common/recording.rs"]
mod recording;
use common::archive_env::*;
use recording::{FRAMES, encode, rlog};
use roadviewer_hars::{convert, logs, media_read};
use serde_json::{Value, json};
use std::process::Command;

async fn convert_moved(e: &Env, id: &str) -> (String, Value) {
    let folder = e.root.join(id);
    let inputs = media_read::conversion_inputs(e.a(), &folder, id, &e.meta(id).await).await;
    assert_eq!(inputs.deferred, None);
    for kind in ["rlog.zst", "qcamera.ts", "fcamera.hevc"] {
        assert!(
            !inputs.paths[kind].starts_with(&folder)
                || inputs.paths[kind].starts_with(folder.join(".staging")),
            "{kind} must come from the archive"
        );
    }
    let media = convert::VIDEO_INPUTS
        .iter()
        .map(|k| {
            (
                (*k).to_owned(),
                json!(
                    inputs
                        .paths
                        .get(*k)
                        .cloned()
                        .unwrap_or_else(|| folder.join(k))
                ),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let out = e.tmp.path().join(format!("prepared-{id}"));
    let run = Command::new(env!("CARGO_BIN_EXE_roadviewer-hars"))
        .arg("convert-log")
        .arg(&inputs.paths["rlog.zst"])
        .arg(&out)
        .arg("archived")
        .env(convert::MEDIA_PATHS, Value::Object(media).to_string())
        .env(convert::RECORDING_DIR, &folder)
        .env_remove("HARS_REMUX_ENGINE")
        .env("RUST_LOG", "roadviewer_hars=info")
        .output()
        .unwrap();
    for path in &inputs.staged {
        media_read::unstage(path).await;
    }
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    assert!(
        run.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    (stdout, logs::read_gzip(&out.join("data.json.gz")).unwrap())
}

fn assert_pure(stdout: &str, data: &Value) {
    let events = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["fields"]["event"]
                .as_str()
                .is_some_and(|e| e.starts_with("remux"))
        })
        .map(|v| v["fields"].clone())
        .collect::<Vec<_>>();
    let engines = events
        .iter()
        .map(|f| {
            (
                f["event"].as_str().unwrap(),
                f["engine"].as_str().unwrap_or(""),
                f["kind"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        engines,
        [
            ("remux_engine", "pure", "qcamera.ts"),
            ("remux_engine", "pure", "fcamera.hevc")
        ],
        "{stdout}"
    );
    assert!(events.iter().all(|f| f["terminal_basis"] == "NextFrame"));
    let last = f64::from(FRAMES - 1) * 0.05;
    assert!((data["videos"]["front"]["duration"].as_f64().unwrap() - (last + 0.047)).abs() < 1e-9);
    assert!(
        (data["videos"]["qcamera"]["duration"].as_f64().unwrap() - (last + 0.047)).abs() < 1e-9
    );
    assert_eq!(data["videos"]["qcamera"]["frames"], FRAMES);
}

async fn recording(e: &Env) -> String {
    let dir = e.tmp.path().join("media");
    std::fs::create_dir_all(&dir).unwrap();
    let ts = encode(
        &[
            "-c:v", "libx264", "-bf", "0", "-g", "20", "-pix_fmt", "yuv420p", "-f", "mpegts",
        ],
        &dir.join("q.ts"),
    );
    let hevc = encode(
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
        &dir.join("f.hevc"),
    );
    let id = e
        .record(
            'a',
            &[
                ("qcamera.ts", ts),
                ("fcamera.hevc", hevc),
                ("rlog.zst", rlog()),
            ],
            OLD,
        )
        .await;
    e.cycle().await;
    for kind in ["qcamera.ts", "fcamera.hevc", "rlog.zst"] {
        assert!(!e.local(&id, kind), "{kind} was archived");
    }
    id
}

#[tokio::test]
async fn share_originals_convert_with_the_pure_engine() {
    let e = Env::new().await;
    e.use_nas(json!({"archive_nas_margin_mb": 0})).await;
    let id = recording(&e).await;
    let (stdout, data) = convert_moved(&e, &id).await;
    assert_pure(&stdout, &data);
}

#[tokio::test]
async fn drive_originals_convert_with_the_pure_engine() {
    let e = Env::new().await;
    e.use_drive(json!({"archive_mode": "move", "archive_allow_local_delete": true}))
        .await;
    let id = recording(&e).await;
    let (stdout, data) = convert_moved(&e, &id).await;
    assert_pure(&stdout, &data);
    assert!(!e.root.join(&id).join(".staging").exists());
}
