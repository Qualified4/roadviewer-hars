use serde_json::{Map, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Python `storage_policy.atomic_json`: a 0600 temporary file, fsync'd, renamed over the target; the directory
/// is fsync'd too so the rename survives a power cut.
pub(crate) fn atomic_json(path: &Path, value: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let temp = with_suffix(path, &format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let outcome = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        sync_dir(path)
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    outcome
}

pub(crate) fn sync_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn read_object(path: &Path) -> Option<Map<String, Value>> {
    match serde_json::from_slice(&std::fs::read(path).ok()?).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// One key of the data root's `.archive.json` (Python `LOCAL_COPY`), a non-empty string or None.
pub(crate) fn local_id(root: &Path, key: &str) -> Option<String> {
    read_object(&root.join(super::LOCAL_COPY))?
        .get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Merged into what is there, so another backend's key in the file survives.
pub(crate) fn write_local_id(root: &Path, key: &str, value: &str) -> std::io::Result<()> {
    let path = root.join(super::LOCAL_COPY);
    let mut map = read_object(&path).unwrap_or_default();
    map.insert(key.to_owned(), Value::String(value.to_owned()));
    atomic_json(&path, &Value::Object(map))
}
