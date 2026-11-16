//! Remote archive backends (Python `archive_backends.py`): Google Drive and a mounted NAS share behind one
//! contract. Independent of the store and the HTTP router; the archiver builds on it.
//!
//! Write scope: a backend that can write ([`ArchiveWrite`]) is bound to a HARS-owned root, marked
//! `hars_owner=roadviewer-hars` (Drive appProperties) or `"owner": "roadviewer-hars"` (NAS sentinel). Data written by
//! the Python Road Viewer is reached through [`DriveLegacy`] / [`NasLegacy`], which only implement [`ArchiveRead`].
pub mod archiver;
mod archiver_manual;
mod archiver_nas;
mod archiver_recover;
mod archiver_remote;
pub mod backend;
pub mod drive;
pub mod drive_auth;
mod fsutil;
pub mod http_client;
pub mod index;
pub mod nas;
pub mod pending;
pub mod resolve;
pub mod settings;
pub mod track_fill;

pub use archiver::{Archive, Factory, Host};
pub use archiver_manual::Transfer;
pub use archiver_remote::{UPLOAD_WORKERS, remote_reason};
pub use backend::*;
pub use drive::{
    DriveBackend, DriveClient, DriveConnection, DriveLegacy, DriveSettings, PendingCode,
};
pub use drive_auth::{DeviceFlow, DriveAuth, Poll, Secret};
pub use http_client::{Http, HttpConfig};
pub use nas::{NasBackend, NasLegacy, NasSettings};

use std::{
    future::Future,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
pub const CHUNK: usize = 1 << 20;
pub const CACHE_SECONDS: f64 = 10.0;
pub const STAT_SECONDS: f64 = 600.0;
pub const READ_TRUST_SECONDS: f64 = 300.0;
pub const PROBE_WAIT_SECONDS: f64 = 20.0;
pub const USAGE_REFRESH_SECONDS: f64 = 300.0;
pub const NAS_RESERVE: u64 = 100 << 20;
/// Python `LOCAL_COPY` and `SENTINEL`.
pub const LOCAL_COPY: &str = ".archive.json";
pub const SENTINEL: &str = ".roadviewer-storage.json";
/// Python `STORED_KINDS`.
pub const MEDIA_KINDS: [&str; 7] = [
    "rlog.zst",
    "qcamera.ts",
    "fcamera.hevc",
    "ecamera.hevc",
    "camera.mp4",
    "fcamera.mp4",
    "ecamera.mp4",
];

/// The owner mark of a HARS write root and of every file HARS writes there.
pub const OWNER_KEY: &str = "hars_owner";
pub const OWNER_VALUE: &str = "roadviewer-hars";
/// Python's own folder name default (`media_store.DEFAULT_FOLDER`) and NAS sentinel owner.
pub const PYTHON_DEFAULT_FOLDER: &str = "Road Viewer";
pub const PYTHON_NAS_OWNER: &str = "roadviewer";
pub const DEFAULT_DRIVE_FOLDER: &str = "Road Viewer HARS";
/// A suggested NAS write root next to (never inside) Python's archive folder.
pub const DEFAULT_NAS_DIR: &str = "roadviewer-hars";

/// Seconds; DriveAuth takes the wall clock (token expiry is stored), backends a monotonic one.
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

pub fn wall_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
    })
}
pub fn monotonic_clock() -> Clock {
    let start = std::time::Instant::now();
    Arc::new(move || start.elapsed().as_secs_f64())
}

/// Python `_Availability`: a probe result is reused for CACHE_SECONDS; while a probe is in flight other callers
/// get 'checking' instead of waiting or trusting a stale answer.
pub(crate) struct Availability {
    clock: Clock,
    cached: Mutex<Option<(f64, Probe)>>,
    gate: tokio::sync::Mutex<()>,
}

impl Availability {
    pub(crate) fn new(clock: Clock) -> Self {
        Self {
            clock,
            cached: Mutex::default(),
            gate: tokio::sync::Mutex::new(()),
        }
    }
    fn cached(&self) -> Option<(f64, Probe)> {
        self.cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn fresh(&self) -> Option<Probe> {
        self.cached()
            .filter(|(at, _)| (self.clock)() - at < CACHE_SECONDS)
            .map(|(_, p)| p)
    }
    pub(crate) fn known(&self) -> Option<Probe> {
        self.cached().map(|(_, p)| p)
    }
    pub(crate) fn last_state(&self) -> Option<State> {
        self.fresh().map(|p| p.state)
    }
    pub(crate) fn connected_within(&self, seconds: f64) -> bool {
        self.cached()
            .is_some_and(|(at, p)| p.state == State::Connected && (self.clock)() - at < seconds)
    }
    pub(crate) fn invalidate(&self) {
        *self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
    pub(crate) async fn probe<F, Fut>(&self, wait: bool, run: F) -> Probe
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Probe>,
    {
        if let Some(p) = self.fresh() {
            return p;
        }
        let guard = if wait {
            tokio::time::timeout(
                Duration::from_secs_f64(PROBE_WAIT_SECONDS),
                self.gate.lock(),
            )
            .await
            .ok()
        } else {
            self.gate.try_lock().ok()
        };
        let Some(_guard) = guard else {
            return Probe::checking();
        };
        if let Some(p) = self.fresh() {
            return p;
        }
        let result = run().await;
        *self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(((self.clock)(), result.clone()));
        result
    }
}

/// The active write backend chosen by settings (Python `make_backend`).
pub enum WriteBackend {
    Drive(Arc<DriveBackend>),
    Nas(Arc<NasBackend>),
}
/// A read-only Python-owned remote.
pub enum LegacyBackend {
    Drive(Arc<DriveLegacy>),
    Nas(Arc<NasLegacy>),
}

macro_rules! dispatch {
    ($self:ident, $b:ident => $e:expr) => {
        match $self {
            Self::Drive($b) => $e,
            Self::Nas($b) => $e,
        }
    };
}

macro_rules! impl_read {
    ($t:ty) => {
        impl ArchiveRead for $t {
            fn caps(&self) -> &'static Capabilities {
                dispatch!(self, b => b.caps())
            }
            async fn probe(&self, wait: bool) -> Probe {
                dispatch!(self, b => b.probe(wait).await)
            }
            fn last_state(&self) -> Option<State> {
                dispatch!(self, b => b.last_state())
            }
            fn invalidate(&self) {
                dispatch!(self, b => b.invalidate())
            }
            fn storage_id(&self) -> Option<String> {
                dispatch!(self, b => b.storage_id())
            }
            async fn stat(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
                dispatch!(self, b => b.stat(r).await)
            }
            async fn find(&self, id: &str, kind: &str) -> Result<Vec<RemoteRef>> {
                dispatch!(self, b => b.find(id, kind).await)
            }
            async fn verify(&self, r: &RemoteRef, sha256: Option<&str>, size: Option<u64>, md5: Option<&str>) -> Result<Verdict> {
                dispatch!(self, b => b.verify(r, sha256, size, md5).await)
            }
            async fn open(&self, r: &RemoteRef, start: u64, end: Option<u64>) -> Result<ChunkReader> {
                dispatch!(self, b => b.open(r, start, end).await)
            }
            async fn download_to(&self, r: &RemoteRef, dest: &Path, sha256: Option<&str>) -> Result<()> {
                dispatch!(self, b => b.download_to(r, dest, sha256).await)
            }
            async fn usage(&self) -> Result<Usage> {
                dispatch!(self, b => b.usage().await)
            }
        }
    };
}
impl_read!(WriteBackend);
impl_read!(LegacyBackend);

impl LegacyBackend {
    pub async fn list_media(&self) -> Result<Vec<LegacyFile>> {
        dispatch!(self, b => b.list_media().await)
    }
}

impl ArchiveWrite for WriteBackend {
    async fn activate(&self) {
        dispatch!(self, b => b.activate().await)
    }
    async fn upload(
        &self,
        local: &Path,
        id: &str,
        kind: &str,
        sha256: Option<&str>,
        md5: Option<&str>,
        size: u64,
        resume: &mut UploadResume,
        on_progress: Progress<'_>,
    ) -> Result<RemoteRef> {
        dispatch!(self, b => b.upload(local, id, kind, sha256, md5, size, resume, on_progress).await)
    }
    async fn session_status(&self, resume: &UploadResume) -> Result<SessionState> {
        dispatch!(self, b => b.session_status(resume).await)
    }
    async fn delete(&self, r: &RemoteRef) -> Result<bool> {
        dispatch!(self, b => b.delete(r).await)
    }
}

/// Python `make_backend` for the Drive side: `<root>/.gdrive-auth.json` with the configured OAuth client.
pub fn make_drive(
    settings: DriveSettings,
    root: &Path,
    client_id: &str,
    client_secret: Secret,
    http: Arc<Http>,
) -> Arc<DriveBackend> {
    let auth = Arc::new(DriveAuth::new(
        root.join(drive_auth::TOKEN_FILE),
        client_id,
        client_secret,
        http.clone(),
        None,
        wall_clock(),
    ));
    let client = DriveClient::new(http, auth.clone(), None, None);
    Arc::new(DriveBackend::new(
        settings,
        root,
        auth,
        client,
        monotonic_clock(),
    ))
}
