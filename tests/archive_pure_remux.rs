//! Converting again a recording whose raw originals the archiver moved (share
//! read in place, Drive staged copy) runs the default pure engine on those
//! inputs through the real executable and its worker process, with no fallback.
//! Media and log are synthetic.
mod common;
use capnp::{message::Builder, serialize};
use common::archive_env::*;
use roadviewer_hars::{convert, log_capnp, logs, media_read};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Stdio},
};

const FRAMES: u32 = 20;
const BASE: u64 = 10_000_000_000;
const STEP: u64 = 50_000_000;
const NEXT_GAP: u64 = 47_000_000;

fn encode(args: &[&str], out: &Path) -> Vec<u8> {
    assert!(
        Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=160x96:rate=20")
            .args(["-frames:v", &FRAMES.to_string(), "-an", "-threads", "1"])
            .args(args)
            .arg(out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    std::fs::read(out).unwrap()
}

fn rlog() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut zst = zstd::stream::write::Encoder::new(&mut out, 1).unwrap();
        let mut emit = |stamp: u64, fill: &dyn Fn(log_capnp::event::Builder<'_>)| {
            let mut msg = Builder::new_default();
            let mut event = msg.init_root::<log_capnp::event::Builder>();
            event.set_log_mono_time(stamp);
            event.set_valid(true);
            fill(event);
            serialize::write_message(&mut zst, &msg).unwrap();
        };
        for i in 0..=FRAMES {
            let (frame, eof) = (100 + i, BASE + u64::from(i) * STEP);
            // The camera frame after the last encoded one is logged, not encoded.
            let eof = if i == FRAMES {
                eof - STEP + NEXT_GAP
            } else {
                eof
            };
            emit(eof, &|e| {
                let mut c = e.init_road_camera_state();
                c.set_frame_id(frame);
                c.set_timestamp_eof(eof);
            });
            if i == FRAMES {
                break;
            }
            for topic in 0..2 {
                emit(eof + 1, &|e| {
                    let mut x = if topic == 0 {
                        e.init_q_road_encode_idx()
                    } else {
                        e.init_road_encode_idx()
                    };
                    x.set_frame_id(frame);
                    x.set_segment_id(i);
                    x.set_timestamp_eof(eof);
                });
            }
            emit(eof + 20_000_000, &|e| {
                let mut m = e.init_model_v2();
                m.set_frame_id(frame);
                m.set_timestamp_eof(eof);
                let line = |mut l: log_capnp::x_y_z_t_data::Builder<'_>, y: f32| {
                    let mut ys = l.reborrow().init_y(3);
                    for i in 0..3 {
                        ys.set(i, y + i as f32 * 0.1);
                    }
                    let mut xs = l.reborrow().init_x(3);
                    for (i, x) in [0f32, 10., 30.].into_iter().enumerate() {
                        xs.set(i as u32, x);
                    }
                    l.init_z(3);
                };
                line(m.reborrow().init_position(), 0.);
                let mut lanes = m.reborrow().init_lane_lines(4);
                for (i, y) in [-5f32, -1.5, 1.5, 5.].into_iter().enumerate() {
                    line(lanes.reborrow().get(i as u32), y);
                }
                let mut edges = m.reborrow().init_road_edges(2);
                for (i, y) in [-6f32, 6.].into_iter().enumerate() {
                    line(edges.reborrow().get(i as u32), y);
                }
                let mut probs = m.reborrow().init_lane_line_probs(4);
                for (i, p) in [0.1f32, 0.9, 0.9, 0.1].into_iter().enumerate() {
                    probs.set(i as u32, p);
                }
                let mut stds = m.init_road_edge_stds(2);
                stds.set(0, 0.2);
                stds.set(1, 0.2);
            });
        }
        zst.finish().unwrap();
    }
    out
}

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
