//! Archive preferences (Python `media_store.ArchiveSettings` and `archive_settings.py`): `.archive-settings.json`
//! with strict validation and field error codes, the OAuth client from the add-on options, and the one-time
//! migration of archive options found in `options.json`.
use super::{DEFAULT_DRIVE_FOLDER, Secret, fsutil, nas::valid_path};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const FILENAME: &str = ".archive-settings.json";
pub const VERSION: i64 = 1;
pub const MB: u64 = 1 << 20;
const MARKERS: [&str; 4] = ["options.json", "defaults", "web", "error"];
/// Python `RANGES`: (lo, hi) per integer key.
const RANGES: [(&str, i64, Option<i64>); 6] = [
    ("archive_local_threshold_mb", 0, None),
    ("archive_min_age_hours", 0, Some(8760)),
    ("archive_interval_minutes", 1, Some(1440)),
    ("archive_batch", 1, Some(64)),
    ("archive_nas_max_mb", 0, None),
    ("archive_nas_margin_mb", 0, None),
];
const BOOLS: [&str; 2] = ["archive_enabled", "archive_allow_local_delete"];
/// Python `KEYS` (the fields of `values()`).
pub const KEYS: [&str; 12] = [
    "archive_enabled",
    "archive_backend",
    "archive_mode",
    "archive_allow_local_delete",
    "archive_path",
    "archive_local_threshold_mb",
    "archive_min_age_hours",
    "archive_interval_minutes",
    "archive_batch",
    "archive_nas_max_mb",
    "archive_nas_margin_mb",
    "archive_gdrive_folder_name",
];
pub const CREDENTIAL_KEYS: [&str; 2] = ["archive_gdrive_client_id", "archive_gdrive_client_secret"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Smb,
    Gdrive,
}
impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smb => "smb",
            Self::Gdrive => "gdrive",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Move,
    Copy,
}
impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Copy => "copy",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArchiveSettings {
    pub enabled: bool,
    pub path: Option<PathBuf>,
    pub local_threshold_bytes: u64,
    pub min_age_seconds: u64,
    pub interval_seconds: u64,
    pub batch: usize,
    pub nas_max_bytes: u64,
    pub nas_margin_bytes: u64,
    pub error: Option<String>,
    pub backend: Backend,
    pub mode: Mode,
    pub allow_local_delete: bool,
    pub gdrive_client_id: String,
    pub gdrive_client_secret: Secret,
    pub gdrive_folder_name: String,
    /// Python `RV_ARCHIVE_ALLOW_ANY_PATH=1` (tests point the archive at temporary folders).
    pub allow_any_path: bool,
}

impl Default for ArchiveSettings {
    fn default() -> Self {
        Self::from_options(&Map::new(), false)
    }
}

/// Python `_int`: Supervisor validates the schema, but a hand-edited options.json must not crash startup.
fn int(options: &Map<String, Value>, key: &str, default: i64, lo: i64, hi: Option<i64>) -> u64 {
    let value = match options.get(key) {
        None => default,
        Some(Value::Bool(b)) => i64::from(*b),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.is_finite())
                    .map(|f| f.trunc() as i64)
            })
            .unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(default),
        Some(_) => default,
    };
    let value = hi.map_or(value, |hi| value.min(hi));
    value.max(lo) as u64
}
fn boolean(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "true" | "1"),
        _ => false,
    }
}
fn text<'a>(options: &'a Map<String, Value>, key: &str) -> &'a str {
    options.get(key).and_then(Value::as_str).unwrap_or("")
}

impl ArchiveSettings {
    /// Python `ArchiveSettings.from_options`.
    pub fn from_options(options: &Map<String, Value>, allow_any_path: bool) -> Self {
        let mut enabled = boolean(options.get("archive_enabled"));
        let raw = options.get("archive_path").and_then(Value::as_str);
        let backend = options
            .get("archive_backend")
            .map_or(Some("smb"), Value::as_str);
        let ok = raw.is_some_and(|r| valid_path(r, allow_any_path));
        let present = options
            .get("archive_path")
            .is_some_and(|v| !matches!(v, Value::Null) && v.as_str() != Some(""));
        let mut error = None;
        // The share path belongs to SMB; Drive does not need one.
        if backend != Some("gdrive") && (present && !ok || enabled && !ok) {
            enabled = false;
            error = Some("invalid_archive_path".to_owned());
        }
        let backend = match backend {
            Some("smb") => Backend::Smb,
            Some("gdrive") => Backend::Gdrive,
            _ => {
                enabled = false;
                error = Some("invalid_backend".to_owned());
                Backend::Smb
            }
        };
        let folder = text(options, "archive_gdrive_folder_name").trim();
        Self {
            enabled,
            path: raw.filter(|_| ok).map(PathBuf::from),
            error,
            local_threshold_bytes: int(options, "archive_local_threshold_mb", 0, 0, None) * MB,
            min_age_seconds: int(options, "archive_min_age_hours", 24, 0, Some(8760)) * 3600,
            interval_seconds: int(options, "archive_interval_minutes", 1440, 1, Some(1440)) * 60,
            batch: int(options, "archive_batch", 4, 1, Some(64)) as usize,
            nas_max_bytes: int(options, "archive_nas_max_mb", 0, 0, None) * MB,
            nas_margin_bytes: int(options, "archive_nas_margin_mb", 1024, 0, None) * MB,
            backend,
            mode: if options.get("archive_mode").and_then(Value::as_str) == Some("copy") {
                Mode::Copy
            } else {
                Mode::Move
            },
            allow_local_delete: boolean(options.get("archive_allow_local_delete")),
            gdrive_client_id: text(options, "archive_gdrive_client_id").trim().to_owned(),
            gdrive_client_secret: Secret(
                text(options, "archive_gdrive_client_secret")
                    .trim()
                    .to_owned(),
            ),
            gdrive_folder_name: if folder.is_empty() {
                DEFAULT_DRIVE_FOLDER.to_owned()
            } else {
                folder.to_owned()
            },
            allow_any_path,
        }
    }

    /// Drive keeps the local original unless deleting it was explicitly allowed as well.
    pub fn effective_mode(&self) -> Mode {
        match self.backend {
            Backend::Smb => self.mode,
            Backend::Gdrive if self.mode == Mode::Move && self.allow_local_delete => Mode::Move,
            Backend::Gdrive => Mode::Copy,
        }
    }
    pub fn requires_client(&self) -> bool {
        self.backend == Backend::Gdrive
    }
    pub fn remote(&self) -> bool {
        self.backend == Backend::Gdrive
    }
}

/// Python `values()`: the editable preferences (never the OAuth client).
pub fn values(s: &ArchiveSettings) -> Map<String, Value> {
    let path = s
        .path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let v = json!({
        "archive_enabled": s.enabled,
        "archive_backend": s.backend.as_str(),
        "archive_mode": s.mode.as_str(),
        "archive_allow_local_delete": s.allow_local_delete,
        "archive_path": path,
        "archive_local_threshold_mb": s.local_threshold_bytes / MB,
        "archive_min_age_hours": s.min_age_seconds / 3600,
        "archive_interval_minutes": s.interval_seconds / 60,
        "archive_batch": s.batch,
        "archive_nas_max_mb": s.nas_max_bytes / MB,
        "archive_nas_margin_mb": s.nas_margin_bytes / MB,
        "archive_gdrive_folder_name": s.gdrive_folder_name,
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

/// Python `validate`: types and ranges before `from_options` can coerce or clamp invalid input.
pub fn validate(body: &Value, allow_any_path: bool) -> BTreeMap<String, &'static str> {
    let mut errors = BTreeMap::new();
    let Some(body) = body.as_object() else {
        errors.insert("_body".to_owned(), "expected_object");
        return errors;
    };
    for (key, value) in body {
        if !KEYS.contains(&key.as_str()) {
            errors.insert(key.clone(), "unknown_field");
            continue;
        }
        if let Some((_, lo, hi)) = RANGES.iter().find(|(k, _, _)| k == key) {
            match value.as_i64() {
                None if value.as_u64().is_some() => {
                    errors.insert(key.clone(), "out_of_range");
                }
                None => {
                    errors.insert(key.clone(), "expected_integer");
                }
                Some(v) if v < *lo || hi.is_some_and(|hi| v > hi) => {
                    errors.insert(key.clone(), "out_of_range");
                }
                Some(_) => {}
            }
        } else if BOOLS.contains(&key.as_str()) {
            if !value.is_boolean() {
                errors.insert(key.clone(), "expected_boolean");
            }
        } else if let Some(v) = value.as_str() {
            let bad_enum = match key.as_str() {
                "archive_backend" => !matches!(v, "smb" | "gdrive"),
                "archive_mode" => !matches!(v, "move" | "copy"),
                _ => false,
            };
            if bad_enum {
                errors.insert(key.clone(), "invalid_enum");
            } else if key == "archive_gdrive_folder_name"
                && !(1..=255).contains(&v.trim().chars().count())
            {
                errors.insert(key.clone(), "invalid_length");
            }
        } else {
            errors.insert(key.clone(), "expected_string");
        }
    }
    if errors.is_empty()
        && let Some(e) = ArchiveSettings::from_options(body, allow_any_path).error
    {
        errors.insert(
            "archive_path".to_owned(),
            if e == "invalid_backend" {
                "invalid_backend"
            } else {
                "invalid_archive_path"
            },
        );
    }
    errors
}

pub fn document(s: &ArchiveSettings, migrated_from: &str) -> Value {
    let mut m = values(s);
    m.insert("version".into(), json!(VERSION));
    m.insert("migrated_from".into(), json!(migrated_from));
    Value::Object(m)
}

/// Python `parse`: the saved preferences plus the OAuth client from the add-on options.
pub fn parse(
    data: &Map<String, Value>,
    options: &Map<String, Value>,
    allow_any_path: bool,
) -> ArchiveSettings {
    let mut merged = data.clone();
    for key in CREDENTIAL_KEYS {
        merged.insert(
            key.to_owned(),
            options.get(key).cloned().unwrap_or(json!("")),
        );
    }
    ArchiveSettings::from_options(&merged, allow_any_path)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Loaded {
    pub settings: ArchiveSettings,
    pub migrated_from: String,
    pub error: Option<&'static str>,
}

/// Python `load`: an existing file is authoritative, including when it cannot be read safely.
pub fn load(root: &Path, options: &Map<String, Value>, allow_any_path: bool) -> Loaded {
    let path = root.join(FILENAME);
    let attempt = || -> Result<Loaded, ()> {
        match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let data: Map<String, Value> = KEYS
                    .iter()
                    .filter_map(|k| options.get(*k).map(|v| ((*k).to_owned(), v.clone())))
                    .collect();
                let marker = if data.is_empty() {
                    "defaults"
                } else {
                    "options.json"
                };
                if !validate(&Value::Object(data.clone()), allow_any_path).is_empty() {
                    return Err(());
                }
                let settings = parse(&data, options, allow_any_path);
                // Only migrated values are written; with nothing to migrate the defaults apply until the user saves.
                if marker == "options.json" {
                    fsutil::atomic_json(&path, &document(&settings, marker)).map_err(|_| ())?;
                }
                Ok(Loaded {
                    settings,
                    migrated_from: marker.to_owned(),
                    error: None,
                })
            }
            Err(_) => Err(()),
            Ok(bytes) => {
                let Ok(Value::Object(mut data)) = serde_json::from_slice::<Value>(&bytes) else {
                    return Err(());
                };
                match data.get("version") {
                    None => {}
                    Some(v) if v.as_i64() == Some(VERSION) && !v.is_f64() => {}
                    Some(_) => return Err(()),
                }
                let marker = match data.get("migrated_from") {
                    None => "web".to_owned(),
                    Some(Value::String(s)) if MARKERS.contains(&s.as_str()) => s.clone(),
                    Some(_) => return Err(()),
                };
                data.remove("version");
                data.remove("migrated_from");
                // Ignore the retired preference when loading files saved by older versions.
                data.remove("archive_gdrive_fill_tracks");
                if !validate(&Value::Object(data.clone()), allow_any_path).is_empty() {
                    return Err(());
                }
                Ok(Loaded {
                    settings: parse(&data, options, allow_any_path),
                    migrated_from: marker,
                    error: None,
                })
            }
        }
    };
    attempt().unwrap_or_else(|()| {
        // Never recover a bad preference file from stale options, or write over it.
        let mut settings = parse(&Map::new(), options, allow_any_path);
        settings.enabled = false;
        settings.mode = Mode::Copy;
        Loaded {
            settings,
            migrated_from: "error".to_owned(),
            error: Some("settings_error"),
        }
    })
}

/// The add-on options (`/data/options.json`) merged with Python's test hook `RV_ARCHIVE_OPTIONS_JSON`.
pub fn options() -> Map<String, Value> {
    let mut options = fsutil::read_object(Path::new("/data/options.json")).unwrap_or_default();
    if let Ok(extra) = std::env::var("RV_ARCHIVE_OPTIONS_JSON")
        && let Ok(Value::Object(extra)) = serde_json::from_str::<Value>(&extra)
    {
        options.extend(extra);
    }
    options
}

pub fn allow_any_path() -> bool {
    std::env::var("RV_ARCHIVE_ALLOW_ANY_PATH").as_deref() == Ok("1")
}
