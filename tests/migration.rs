use roadviewer_hars::{
    auth::{Auth, derive_secret},
    migration::import_snapshot,
    store::{atomic_json, read_json},
};
use serde_json::{Value, json};

#[tokio::test]
async fn offline_import_preserves_existing_device_authentication_without_pairing() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let auth = Auth::open(source.path()).await.unwrap();
    let p = auth.pairing("POST").await;
    let credentials = auth
        .pair(json!({"code":p["code"],"metadata":{"name":"Existing device"}}))
        .await
        .unwrap();
    drop(auth);
    assert_eq!(
        import_snapshot(source.path(), target.path()).await.unwrap(),
        1
    );
    let copied: Value = read_json(&target.path().join(".devices.json"))
        .await
        .unwrap();
    let expected = derive_secret(
        copied["master"].as_str().unwrap(),
        credentials["device_id"].as_str().unwrap(),
    )
    .unwrap();
    assert!(expected == credentials["device_secret"].as_str().unwrap());
    assert!(import_snapshot(source.path(), target.path()).await.is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(target.path().join(".devices.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[tokio::test]
async fn active_sessions_invalid_state_and_live_targets_are_refused() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let _auth = Auth::open(source.path()).await.unwrap();
    std::fs::create_dir_all(source.path().join(".uploads/session")).unwrap();
    assert!(import_snapshot(source.path(), target.path()).await.is_err());
    assert!(!target.path().join(".devices.json").exists());
    std::fs::remove_dir(source.path().join(".uploads/session")).unwrap();
    atomic_json(
        &source.path().join(".devices.json"),
        &json!({"master":"bad","devices":{},"nonces":{},"receipts":{}}),
    )
    .await
    .unwrap();
    assert!(import_snapshot(source.path(), target.path()).await.is_err());
    assert!(!target.path().join(".devices.json").exists());
}
