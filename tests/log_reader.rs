use capnp::{message::Builder, serialize};
use roadviewer_hars::{car_capnp::car_state::GearShifter, log_capnp, logs, replay};
use serde_json::json;
use std::io::Write;

#[test]
fn streaming_zstd_reads_framed_events_and_scalar_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rlog.zst");
    let mut compressed =
        zstd::stream::write::Encoder::new(std::fs::File::create(&path).unwrap(), 1).unwrap();
    for (stamp, speed) in [(1000000000, 0.0), (1010000000, 12.5)] {
        let mut msg = Builder::new_default();
        let mut event = msg.init_root::<log_capnp::event::Builder>();
        event.set_log_mono_time(stamp);
        event.set_valid(true);
        let mut state = event.init_car_state();
        state.set_v_ego(speed);
        state.set_a_ego(-1.);
        serialize::write_message(&mut compressed, &msg).unwrap();
    }
    compressed.finish().unwrap();
    let decoded = logs::decode(&path).unwrap();
    assert_eq!(decoded.counts["carState"], 2);
    assert_eq!(decoded.streams["carState"][0].value["vEgo"], 0.0);
    assert_eq!(decoded.streams["carState"][1].value["vEgo"], 12.5);
    assert_eq!(decoded.streams["carState"][0].value["gasPressed"], false);
}
#[test]
fn replay_records_drive_hud_changes_from_carrot_events() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rlog.zst");
    let mut compressed =
        zstd::stream::write::Encoder::new(std::fs::File::create(&path).unwrap(), 1).unwrap();
    let mut emit = |stamp, valid, init: &dyn Fn(log_capnp::event::Builder<'_>)| {
        let mut msg = Builder::new_default();
        let mut event = msg.init_root::<log_capnp::event::Builder>();
        event.set_log_mono_time(stamp);
        event.set_valid(valid);
        init(event);
        serialize::write_message(&mut compressed, &msg).unwrap();
    };
    let stamp = |i: u64| 1_000_000_000 + i * 50_000_000;
    for i in 0..9 {
        emit(stamp(i), true, &|event| {
            let mut model = event.init_model_v2();
            model.set_frame_id(i as u32);
            let mut position = model.reborrow().init_position();
            position.reborrow().init_x(0);
            position.init_y(0);
            model.reborrow().init_lane_lines(0);
            model.reborrow().init_lane_line_probs(0);
            model.reborrow().init_road_edges(0);
            model.init_road_edge_stds(0);
        });
    }
    for (i, valid, mode, gear, step, ev_active, ev_valid) in [
        (1, true, 1, GearShifter::Drive, 5, true, true),
        (2, true, 1, GearShifter::Drive, 5, true, true),
        (3, true, 2, GearShifter::Reverse, 0, false, true),
        (4, false, 3, GearShifter::Park, 1, true, true),
    ] {
        // The HUD uses the latest sample before each camera frame.
        emit(stamp(i) - 1_000_000, valid, &|event| {
            let mut plan = event.init_longitudinal_plan();
            plan.set_my_driving_mode(mode);
            plan.set_desired_distance(18.5);
        });
        emit(stamp(i) - 1_000_000, valid, &|event| {
            let mut state = event.init_car_state();
            state.set_gear_shifter(gear);
            state.set_gear_step(step);
            state.set_ev_mode_active(ev_active);
            state.set_ev_mode_valid(ev_valid);
        });
    }
    emit(stamp(5) - 1_000_000, true, &|event| {
        event.init_longitudinal_plan().set_my_driving_mode(4);
    });
    compressed.finish().unwrap();

    let decoded = logs::decode(&path).unwrap();
    assert_eq!(
        decoded.streams["longitudinalPlan"][0].value["myDrivingMode"],
        1
    );
    let (frames, _) = replay::frames(&decoded, |_, _| {}).unwrap();
    let hud_fields = [
        "drivingMode",
        "gearShifter",
        "gearStep",
        "evModeActive",
        "evModeValid",
    ];
    let changes = frames
        .iter()
        .map(|frame| {
            hud_fields
                .iter()
                .filter_map(|&key| Some((key.to_owned(), frame.get(key)?.clone())))
                .collect::<serde_json::Map<_, _>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        json!(changes),
        json!([
            {},
            {"drivingMode":1,"gearShifter":"drive","gearStep":5,"evModeActive":true,"evModeValid":true},
            {},
            {"drivingMode":2,"gearShifter":"reverse","gearStep":0,"evModeActive":false},
            {"drivingMode":0,"gearShifter":"unknown","evModeValid":false},
            {"drivingMode":4},
            {},
            {},
            {"drivingMode":0}
        ])
    );
    assert_eq!(frames[3]["gapTarget"], 18.5);
    assert!(frames[4]["gapTarget"].is_null());
}

#[test]
fn corrupt_and_truncated_logs_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rlog.zst");
    std::fs::write(&path, b"not zstandard").unwrap();
    assert!(logs::decode(&path).is_err());
    let mut compressed =
        zstd::stream::write::Encoder::new(std::fs::File::create(&path).unwrap(), 1).unwrap();
    compressed.write_all(&[0u8; 7]).unwrap();
    compressed.finish().unwrap();
    assert!(logs::decode(&path).is_err());
}
#[test]
fn gzip_prepared_json_roundtrip() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("data.json.gz");
    let value = serde_json::json!({"route":"한글","frames":[{"t":0.0}],"buffer_boundary":"한글\"\\".repeat(20000)});
    logs::write_gzip(&path, &value).unwrap();
    assert_eq!(logs::read_gzip(&path).unwrap(), value);
    assert_eq!(&std::fs::read(path).unwrap()[4..8], &[0; 4]);
}
