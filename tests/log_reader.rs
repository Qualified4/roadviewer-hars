use capnp::{message::Builder, serialize};
use roadviewer_hars::{log_capnp, logs};
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
    let value = serde_json::json!({"route":"한글","frames":[{"t":0.0}]});
    logs::write_gzip(&path, &value).unwrap();
    assert_eq!(logs::read_gzip(&path).unwrap(), value);
}
