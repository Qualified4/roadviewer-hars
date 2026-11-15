//! Offline copy import only. Never edits a running Python installation.
use crate::{
    auth::{DeviceState, validate_state},
    store::{Store, atomic_json, read_json, sha},
};
use std::path::Path;

pub async fn import_snapshot(snapshot: &Path, target: &Path) -> anyhow::Result<usize> {
    anyhow::ensure!(
        snapshot != Path::new("/data/roadviewer"),
        "use an offline snapshot, not the Python live directory"
    );
    let snapshot = tokio::fs::canonicalize(snapshot).await?;
    let store = Store::open(target.to_owned()).await?;
    let target = tokio::fs::canonicalize(&store.root).await?;
    anyhow::ensure!(
        !target.starts_with(&snapshot) && !snapshot.starts_with(&target),
        "snapshot and target must be isolated"
    );
    anyhow::ensure!(
        store.records.read().await.is_empty(),
        "import requires a fresh HARS data directory"
    );
    let destination = target.join(".devices.json");
    anyhow::ensure!(
        !destination.exists(),
        "authentication state already exists; refusing overwrite"
    );
    let uploads = snapshot.join(".uploads");
    if uploads.exists() {
        anyhow::ensure!(
            tokio::fs::read_dir(uploads)
                .await?
                .next_entry()
                .await?
                .is_none(),
            "snapshot contains upload state; drain uploads before taking snapshot"
        );
    }
    let path = snapshot.join(".devices.json");
    let meta = tokio::fs::symlink_metadata(&path).await?;
    anyhow::ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 8 * 1024 * 1024,
        "invalid authentication snapshot file"
    );
    let before = tokio::fs::read(&path).await?;
    let state: DeviceState = read_json(&path).await?;
    validate_state(&state)?;
    let after = tokio::fs::read(&path).await?;
    anyhow::ensure!(
        sha(&before) == sha(&after),
        "snapshot changed during import"
    );
    let count = state.devices.len();
    atomic_json(&destination, &state).await?;
    // Do not return hashes, nonces, credential identifiers, or receipt content.
    Ok(count)
}
