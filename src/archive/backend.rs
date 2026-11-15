//! The contract shared by the Drive and NAS backends (Python `archive_backends`: `BackendError`, `Capabilities`,
//! `RemoteRef`, `RemoteStat`, `Usage`, `redact`, `completion_matches`, `_stage`).
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, future::Future, path::Path};

pub type Result<T> = std::result::Result<T, BackendError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Unavailable,
    AuthExpired,
    RateLimited,
    QuotaExceeded,
    NotFound,
    Mismatch,
    Io,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::AuthExpired => "auth_expired",
            Self::RateLimited => "rate_limited",
            Self::QuotaExceeded => "quota_exceeded",
            Self::NotFound => "not_found",
            Self::Mismatch => "mismatch",
            Self::Io => "io",
        }
    }
}

/// Python `REASON`: Google's fixed tokens only, so free text (a URL, a header, a body) never reaches a message.
pub fn valid_reason(reason: &str) -> bool {
    (1..=64).contains(&reason.len())
        && reason
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

#[derive(Clone, PartialEq, Eq)]
pub struct BackendError {
    pub code: ErrorCode,
    pub retryable: bool,
    pub reason: Option<String>,
}

/// The reason Python uses for a file this instance must never delete; HARS also gives it to every target outside
/// its own write root (a Python-owned folder or share).
pub const FOREIGN_FILE: &str = "foreign_file";

impl BackendError {
    pub fn new(code: ErrorCode, reason: impl AsRef<str>) -> Self {
        let reason = reason.as_ref();
        Self {
            code,
            retryable: false,
            reason: valid_reason(reason).then(|| reason.to_owned()),
        }
    }
    pub fn bare(code: ErrorCode) -> Self {
        Self {
            code,
            retryable: false,
            reason: None,
        }
    }
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
    pub fn is_foreign(&self) -> bool {
        self.reason() == Some(FOREIGN_FILE)
    }
    pub fn foreign() -> Self {
        Self::new(ErrorCode::Io, FOREIGN_FILE)
    }
    pub(crate) fn from_io(e: &std::io::Error) -> Self {
        Self::new(ErrorCode::Io, io_reason(e))
    }
}

/// An OS error as a token (its message can carry a path).
pub(crate) fn io_reason(e: &std::io::Error) -> String {
    format!("{:?}", e.kind())
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            Some(reason) => write!(f, "{}: {reason}", self.code.as_str()),
            None => f.write_str(self.code.as_str()),
        }
    }
}
impl fmt::Debug for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BackendError({self}, retryable={})", self.retryable)
    }
}
impl std::error::Error for BackendError {}

pub fn redact(text: &str, secrets: &[Option<&str>]) -> String {
    let mut secrets: Vec<&str> = secrets
        .iter()
        .flatten()
        .copied()
        .filter(|s| !s.is_empty())
        .collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut text = text.to_owned();
    for secret in secrets {
        text = text.replace(secret, "<secret>");
    }
    text
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Smb,
    Gdrive,
}
impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smb => "smb",
            Self::Gdrive => "gdrive",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub kind: BackendKind,
    pub local_paths: bool,
    pub checksum: &'static [&'static str],
    pub range_reads: bool,
    pub atomic_rename: bool,
    pub remote_cleanup: bool,
}

pub const DRIVE_CAPS: Capabilities = Capabilities {
    kind: BackendKind::Gdrive,
    local_paths: false,
    checksum: &["sha256", "md5"],
    range_reads: true,
    atomic_rename: false,
    remote_cleanup: false,
};
pub const SMB_CAPS: Capabilities = Capabilities {
    kind: BackendKind::Smb,
    local_paths: true,
    checksum: &[],
    range_reads: true,
    atomic_rename: true,
    remote_cleanup: true,
};

/// 1:1 with the index's `remote` block. SMB: `file_id` is `<id>/<kind>`.
#[derive(Clone, Debug)]
pub struct RemoteRef {
    pub backend: BackendKind,
    pub file_id: Option<String>,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub md5: Option<String>,
    /// Set by a Drive upload whose last chunk this attempt sent: the checksums above are the completion answer's.
    /// Not part of the index or of equality (Python `field(compare=False)`).
    pub completed: bool,
    pub sent_md5: Option<String>,
}
impl PartialEq for RemoteRef {
    fn eq(&self, o: &Self) -> bool {
        (
            self.backend,
            &self.file_id,
            self.bytes,
            &self.sha256,
            &self.md5,
        ) == (o.backend, &o.file_id, o.bytes, &o.sha256, &o.md5)
    }
}
impl RemoteRef {
    pub fn new(
        backend: BackendKind,
        file_id: impl Into<String>,
        bytes: u64,
        sha256: Option<String>,
        md5: Option<String>,
    ) -> Self {
        Self {
            backend,
            file_id: Some(file_id.into()),
            bytes,
            sha256,
            md5,
            completed: false,
            sent_md5: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteStat {
    pub exists: bool,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub md5: Option<String>,
    pub trashed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub used: u64,
    pub free: Option<u64>,
    pub limit: Option<u64>,
    pub reliable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Mismatch,
    Unverifiable,
}
impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Mismatch => "mismatch",
            Self::Unverifiable => "unverifiable",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Connected,
    Unavailable,
    Checking,
    Disabled,
}
impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Unavailable => "unavailable",
            Self::Checking => "checking",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub state: State,
    pub reason: Option<String>,
}
impl Probe {
    pub fn connected() -> Self {
        Self {
            state: State::Connected,
            reason: None,
        }
    }
    pub fn unavailable(reason: &str) -> Self {
        Self {
            state: State::Unavailable,
            reason: Some(reason.to_owned()),
        }
    }
    pub fn checking() -> Self {
        Self {
            state: State::Checking,
            reason: Some("probe_pending".into()),
        }
    }
    pub fn disabled(reason: Option<String>) -> Self {
        Self {
            state: State::Disabled,
            reason,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Done,
    Incomplete,
    Expired,
}
impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Incomplete => "incomplete",
            Self::Expired => "expired",
        }
    }
}

/// A resumable session URI accepts data without a token: a credential that never reaches a log or message.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionUri(pub String);
impl fmt::Debug for SessionUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionUri(<secret>)")
    }
}
impl SessionUri {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// The index's `remote_pending` upload state (Python's `resume` dict: session, bytes_sent, sha256, size).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadResume {
    pub session: Option<SessionUri>,
    #[serde(default)]
    pub bytes_sent: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// Called with the bytes the remote holds; an error stops the upload (Python's on_progress raising) and is returned.
pub type Progress<'a> = &'a mut (dyn FnMut(u64, &UploadResume) -> Result<()> + Send);

/// A streamed read of a remote file (Python's chunk iterators).
pub struct ChunkReader(pub(crate) Reader);
pub(crate) enum Reader {
    Http(reqwest::Response),
    File {
        file: tokio::fs::File,
        left: Option<u64>,
    },
}
impl ChunkReader {
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        match &mut self.0 {
            Reader::Http(r) => r
                .chunk()
                .await
                .map_err(|e| crate::archive::http_client::transport_error(&e)),
            Reader::File { file, left } => {
                use tokio::io::AsyncReadExt;
                if *left == Some(0) {
                    return Ok(None);
                }
                let want = left.map_or(super::CHUNK, |l| l.min(super::CHUNK as u64) as usize);
                let mut buf = vec![0; want];
                let n = file
                    .read(&mut buf)
                    .await
                    .map_err(|e| BackendError::new(ErrorCode::Unavailable, io_reason(&e)))?;
                if n == 0 {
                    return Ok(None);
                }
                buf.truncate(n);
                if let Some(l) = left {
                    *l -= n as u64;
                }
                Ok(Some(Bytes::from(buf)))
            }
        }
    }
    pub async fn read_all(mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

/// DriveBackend.verify()'s rules applied to a fresh upload's completion answer: true only for what verify() would
/// call ok; anything else is left to verify(), which asks the backend.
pub fn completion_matches(
    r: &RemoteRef,
    sha256: Option<&str>,
    size: Option<u64>,
    md5: Option<&str>,
) -> bool {
    if !r.completed || r.file_id.as_deref().is_none_or(str::is_empty) || size != Some(r.bytes) {
        return false;
    }
    if let (Some(a), Some(b)) = (r.sha256.as_deref(), sha256) {
        return a == b;
    }
    let md5 = md5.or(r.sent_md5.as_deref());
    matches!((r.md5.as_deref(), md5), (Some(a), Some(b)) if a == b)
}

/// Writes to `<dest>.part`, hashing as it goes; only a verified copy is renamed into place. Without a sha256 the
/// byte count is the only check, and a copy that fails it never reaches its final name.
pub(crate) async fn stage(
    mut chunks: ChunkReader,
    dest: &Path,
    sha256: Option<&str>,
    size: Option<u64>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let part = super::fsutil::with_suffix(dest, ".part");
    let outcome = async {
        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(|e| BackendError::from_io(&e))?;
        let (mut h, mut count) = (Sha256::new(), 0u64);
        while let Some(chunk) = chunks.next_chunk().await? {
            h.update(&chunk);
            count += chunk.len() as u64;
            file.write_all(&chunk)
                .await
                .map_err(|e| BackendError::from_io(&e))?;
        }
        if let Some(sha256) = sha256 {
            if hex::encode(h.finalize()) != sha256 {
                return Err(BackendError::new(ErrorCode::Mismatch, "sha256"));
            }
        } else if size != Some(count) {
            return Err(BackendError::new(ErrorCode::Mismatch, "no_checksum"));
        }
        file.sync_all()
            .await
            .map_err(|e| BackendError::from_io(&e))?;
        drop(file);
        tokio::fs::rename(&part, dest)
            .await
            .map_err(|e| BackendError::from_io(&e))
    }
    .await;
    if outcome.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    outcome
}

/// Reading an archive: what both a HARS-owned write root and a read-only (Python-owned) remote answer.
pub trait ArchiveRead: Send + Sync {
    fn caps(&self) -> &'static Capabilities;
    /// `wait`: a file read waits for a probe already running instead of answering `checking`.
    fn probe(&self, wait: bool) -> impl Future<Output = Probe> + Send;
    /// The cached state only (None when stale); never touches the remote.
    fn last_state(&self) -> Option<State>;
    fn invalidate(&self);
    fn storage_id(&self) -> Option<String>;
    fn stat(&self, r: &RemoteRef) -> impl Future<Output = Result<Option<RemoteStat>>> + Send;
    fn find(&self, id: &str, kind: &str) -> impl Future<Output = Result<Vec<RemoteRef>>> + Send;
    fn verify(
        &self,
        r: &RemoteRef,
        sha256: Option<&str>,
        size: Option<u64>,
        md5: Option<&str>,
    ) -> impl Future<Output = Result<Verdict>> + Send;
    /// `start..=end` (inclusive, Python's `open(ref, start, end)`); the request is made before this returns.
    fn open(
        &self,
        r: &RemoteRef,
        start: u64,
        end: Option<u64>,
    ) -> impl Future<Output = Result<ChunkReader>> + Send;
    fn download_to(
        &self,
        r: &RemoteRef,
        dest: &Path,
        sha256: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send;
    fn usage(&self) -> impl Future<Output = Result<Usage>> + Send;
}

/// Writing: only a backend bound to a HARS-owned write root implements this, so a read-only remote has no
/// upload or delete to call. Every write still checks the HARS owner mark on the target before it runs.
pub trait ArchiveWrite: ArchiveRead {
    /// Claims a fresh write root (never one that already carries another owner's mark), else only verifies.
    fn activate(&self) -> impl Future<Output = ()> + Send;
    #[allow(clippy::too_many_arguments)]
    fn upload(
        &self,
        local: &Path,
        id: &str,
        kind: &str,
        sha256: Option<&str>,
        md5: Option<&str>,
        size: u64,
        resume: &mut UploadResume,
        on_progress: Progress<'_>,
    ) -> impl Future<Output = Result<RemoteRef>> + Send;
    fn session_status(
        &self,
        resume: &UploadResume,
    ) -> impl Future<Output = Result<SessionState>> + Send;
    /// Ok(true): deleted or already gone (confirmed). Ok(false): unsure, try again later. Err(foreign_file):
    /// not a media file of this HARS write root; never deleted.
    fn delete(&self, r: &RemoteRef) -> impl Future<Output = Result<bool>> + Send;
}
