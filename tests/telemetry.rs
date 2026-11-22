use roadviewer_hars::{logs::Sample, telemetry};
use serde_json::{Value, json};
use std::collections::BTreeMap;

// Synthetic regression values captured from the 1.3.18 telemetry contract.
fn compare(input: Value, expected: &str) {
    let streams: BTreeMap<String, Vec<Sample>> = input["streams"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|r| Sample {
                        stamp: r[0].as_u64().unwrap(),
                        valid: r[1].as_bool().unwrap(),
                        value: r[2].clone(),
                    })
                    .collect(),
            )
        })
        .collect();
    let ours = telemetry::extract(&streams, 1., 2., input["frames"].as_array().unwrap());
    let expected: Value = serde_json::from_str(expected).unwrap();
    equivalent(&ours, &expected["telemetry"], "telemetry");
    equivalent(
        &telemetry::drive_summary(&ours),
        &expected["summary"],
        "summary",
    );
}
fn equivalent(a: &Value, b: &Value, path: &str) {
    if let (Some(a), Some(b)) = (a.as_f64(), b.as_f64()) {
        assert!((a - b).abs() < 1e-6, "numeric mismatch at {path}");
        return;
    }
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            assert!(a.keys().eq(b.keys()), "field mismatch at {path}");
            for (key, value) in a {
                equivalent(value, &b[key], &format!("{path}.{key}"));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "array length at {path}");
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                equivalent(a, b, &format!("{path}[{i}]"));
            }
        }
        _ => assert!(a == b, "value mismatch at {path}"),
    }
}
#[test]
fn telemetry_handles_invalid_samples_gating_enums_ccnc_and_takeovers() {
    compare(
        json!({"streams":{
        "carState":[[1000000000u64,true,{"vEgo":10,"gas":0.3,"brake":0.2,"steeringPressed":false,"gasPressed":false,"brakePressed":false}],[1050000000u64,false,{"vEgo":50}],[1100000000u64,true,{"vEgo":12,"steeringPressed":true}]],
        "carControl":[[1000000000u64,true,{"enabled":true,"longActive":true,"latActive":false,"actuators":{"accel":1.5,"aTarget":1.2,"longControlState":"pid","steeringAngleDeg":2}}],[1100000000u64,true,{"enabled":false,"longActive":false,"latActive":true,"actuators":{"accel":1.5,"torque":0.2,"longControlState":"stopping"}}],[2500000000u64,true,{"enabled":true,"longActive":true,"latActive":true}]],
        "controlsState":[[1000000000u64,true,{"curvature":0.02,"longControlState":"off","lateralControlState":{"angleState":{"active":true,"steeringAngleDesiredDeg":3.5}}}],[1050000000u64,false,{"curvature":1}]]},
        "frames":[{"t":0,"valid":true,"ccncTargets":[]},{"t":0.05,"valid":true,"ccncTargets":[{"slot":"FF","x":12,"yRel":-1}]},{"t":0.1,"valid":false,"ccncTargets":null}]}),
        include_str!("fixtures/telemetry-gating.json"),
    );
}
#[test]
fn telemetry_handles_legacy_output_fallback_and_empty_streams() {
    compare(
        json!({"streams":{"carControl":[[1000000000u64,true,{"actuatorsOutputDEPRECATED":{"torque":0.5,"steeringAngleDeg":-2}}]]},"frames":[]}),
        include_str!("fixtures/telemetry-legacy.json"),
    );
    compare(
        json!({"streams":{},"frames":[]}),
        include_str!("fixtures/telemetry-empty.json"),
    );
}
