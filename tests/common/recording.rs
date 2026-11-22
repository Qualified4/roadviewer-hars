//! A synthetic recording (rlog with camera frame indexes and model lines) and
//! its encoded media, shared by the tests that run real conversions.
#![allow(dead_code)]
use capnp::{message::Builder, serialize};
use roadviewer_hars::log_capnp;
use std::{
    path::Path,
    process::{Command, Stdio},
};

pub const FRAMES: u32 = 20;
pub const BASE: u64 = 10_000_000_000;
pub const STEP: u64 = 50_000_000;
pub const NEXT_GAP: u64 = 47_000_000;

pub fn encode(args: &[&str], out: &Path) -> Vec<u8> {
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

pub fn rlog() -> Vec<u8> {
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
            emit(eof, &|e| {
                let mut c = e.init_wide_road_camera_state();
                c.set_frame_id(frame);
                c.set_timestamp_eof(eof);
            });
            if i == FRAMES {
                break;
            }
            for topic in 0..3 {
                emit(eof + 1, &|e| {
                    let mut x = match topic {
                        0 => e.init_q_road_encode_idx(),
                        1 => e.init_road_encode_idx(),
                        _ => e.init_wide_road_encode_idx(),
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
