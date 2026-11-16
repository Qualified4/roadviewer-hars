//! Python `ArchiveStore` + `SmbBackend` (a mounted share), split by write scope: [`NasBackend`] writes only under
//! a root whose sentinel names HARS as its owner; [`NasLegacy`] reads a Python share and cannot write.
use super::{
    Availability, CACHE_SECONDS, CHUNK, Clock, MEDIA_KINDS, NAS_RESERVE, OWNER_VALUE,
    PROBE_WAIT_SECONDS, PYTHON_NAS_OWNER, SENTINEL, backend::*, fsutil,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// The data root's `.archive.json` key for the share's storage id.
pub const NAS_LOCAL_KEY: &str = "storage_id";

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Python `valid_path`: a folder inside a share (`/share/<name>/...`); `allow_any_path` (Python's
/// `RV_ARCHIVE_ALLOW_ANY_PATH=1`, tests only) accepts any absolute path.
pub fn valid_path(value: &str, allow_any_path: bool) -> bool {
    if value.is_empty() || value.split('/').any(|p| p == "." || p == "..") {
        return false;
    }
    if allow_any_path {
        return Path::new(value).is_absolute();
    }
    let Some(rest) = value.strip_prefix("/share/") else {
        return false;
    };
    let mut parts = rest.split('/');
    let name_ok = parts.next().is_some_and(|n| {
        !n.is_empty() && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    });
    let tail: Vec<&str> = parts.collect();
    name_ok
        && !tail.is_empty()
        && tail.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
        })
}

#[derive(Clone, Debug, Default)]
pub struct NasSettings {
    pub enabled: bool,
    pub error: Option<String>,
    /// For [`NasBackend`] the HARS write root; for [`NasLegacy`] the Python share folder.
    pub path: Option<PathBuf>,
    pub allow_any_path: bool,
}

fn kind_ok(kind: &str) -> bool {
    MEDIA_KINDS.contains(&kind)
}

/// Python `_probe`, plus the owner mark when `owner` is given.
fn probe_sync(path: Option<&Path>, local: Option<String>, owner: Option<&str>) -> Probe {
    let Some(path) = path else {
        return Probe::unavailable("path_missing");
    };
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Probe::unavailable("path_missing"),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Probe::unavailable("path_missing");
        }
        Err(_) => return Probe::unavailable("os_error"),
    }
    let sentinel: Value = match std::fs::read(path.join(SENTINEL)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return Probe::unavailable("sentinel_invalid"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Probe::unavailable("sentinel_missing");
        }
        Err(_) => return Probe::unavailable("os_error"),
    };
    if let Some(owner) = owner
        && sentinel.get("owner").and_then(Value::as_str) != Some(owner)
    {
        return Probe::unavailable("not_owned");
    }
    if local.is_none() || sentinel.get("storage_id").and_then(Value::as_str) != local.as_deref() {
        return Probe::unavailable("storage_id_mismatch");
    }
    Probe::connected()
}

fn parse_ref(r: &RemoteRef) -> Result<(String, String)> {
    let file_id = r.file_id.as_deref().unwrap_or_default();
    let (id, kind) = file_id.split_once('/').unwrap_or((file_id, ""));
    if matches!(id, "" | "." | "..") || !kind_ok(kind) {
        return Err(BackendError::new(ErrorCode::Io, "bad_ref"));
    }
    Ok((id.to_owned(), kind.to_owned()))
}

fn unavailable(e: &std::io::Error) -> BackendError {
    BackendError::new(ErrorCode::Unavailable, io_reason(e))
}

struct NasCore {
    settings: NasSettings,
    root: PathBuf,
    avail: Availability,
    clock: Clock,
    usage_cached: Mutex<Option<(f64, (u64, u64))>>,
    /// The owner mark a write root must carry; None for a read-only Python share.
    owner: Option<&'static str>,
    local: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl NasCore {
    async fn probe(&self, wait: bool) -> Probe {
        if !self.settings.enabled {
            return Probe::disabled(self.settings.error.clone());
        }
        // A hung mount blocks only the probing thread: other callers get 'checking'.
        self.avail
            .probe(wait, || {
                let (path, local, owner) = (self.settings.path.clone(), (self.local)(), self.owner);
                async move {
                    tokio::task::spawn_blocking(move || probe_sync(path.as_deref(), local, owner))
                        .await
                        .unwrap_or_else(|_| Probe::unavailable("os_error"))
                }
            })
            .await
    }
    async fn available(&self) -> bool {
        self.probe(false).await.state == State::Connected
    }
    fn last_state(&self) -> Option<State> {
        if !self.settings.enabled {
            return Some(State::Disabled);
        }
        self.avail.last_state()
    }
    fn invalidate(&self) {
        self.avail.invalidate();
        *lock(&self.usage_cached) = None;
    }

    pub fn path_for(&self, id: &str, kind: &str) -> Result<PathBuf> {
        let root = self
            .settings
            .path
            .as_ref()
            .ok_or_else(|| BackendError::new(ErrorCode::Unavailable, "share"))?;
        if matches!(id, "" | "." | "..") || id.contains('/') || !kind_ok(kind) {
            return Err(BackendError::new(ErrorCode::Io, "bad_ref"));
        }
        Ok(root.join(id).join(kind))
    }

    async fn path(&self, r: &RemoteRef) -> Result<PathBuf> {
        if !self.available().await {
            return Err(BackendError::new(ErrorCode::Unavailable, "share"));
        }
        let (id, kind) = parse_ref(r)?;
        self.path_for(&id, &kind)
    }

    async fn stat(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
        let path = self.path(r).await?;
        let meta = tokio::task::spawn_blocking(move || share_meta(&path))
            .await
            .map_err(|_| BackendError::new(ErrorCode::Io, "join"))?;
        match meta {
            Ok(m) if !m.is_file() => Ok(None),
            Ok(m) => Ok(Some(RemoteStat {
                exists: true,
                bytes: m.len(),
                sha256: None,
                md5: None,
                trashed: false,
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(unavailable(&e)),
        }
    }

    async fn find(&self, id: &str, kind: &str) -> Result<Vec<RemoteRef>> {
        let mut r = RemoteRef::new(BackendKind::Smb, format!("{id}/{kind}"), 0, None, None);
        let Some(stat) = self.stat(&r).await? else {
            return Ok(Vec::new());
        };
        r.bytes = stat.bytes;
        Ok(vec![r])
    }

    async fn open(&self, r: &RemoteRef, start: u64, end: Option<u64>) -> Result<ChunkReader> {
        let path = self.path(r).await?;
        let opened = tokio::task::spawn_blocking(move || {
            let mut file = open_share_file(&path)?;
            file.seek(SeekFrom::Start(start))?;
            Ok::<_, std::io::Error>(file)
        })
        .await
        .map_err(|_| BackendError::new(ErrorCode::Io, "join"))?;
        let file = match opened {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BackendError::bare(ErrorCode::NotFound));
            }
            Err(e) => return Err(unavailable(&e)),
        };
        Ok(ChunkReader(Reader::File {
            file: tokio::fs::File::from_std(file),
            left: end.map(|e| (e + 1).saturating_sub(start)),
        }))
    }

    async fn verify(
        &self,
        r: &RemoteRef,
        sha256: Option<&str>,
        size: Option<u64>,
    ) -> Result<Verdict> {
        let Some(stat) = self.stat(r).await? else {
            return Err(BackendError::bare(ErrorCode::NotFound));
        };
        if size.is_some_and(|s| s != stat.bytes) {
            return Ok(Verdict::Mismatch);
        }
        let Some(sha256) = sha256 else {
            return Ok(Verdict::Unverifiable);
        };
        let mut reader = self.open(r, 0, None).await?;
        let mut h = Sha256::new();
        while let Some(chunk) = reader.next_chunk().await? {
            h.update(&chunk);
        }
        Ok(if hex::encode(h.finalize()) == sha256 {
            Verdict::Ok
        } else {
            Verdict::Mismatch
        })
    }

    async fn download_to(&self, r: &RemoteRef, dest: &Path, sha256: Option<&str>) -> Result<()> {
        let reader = self.open(r, 0, None).await?;
        stage(reader, dest, sha256.or(r.sha256.as_deref()), Some(r.bytes)).await
    }

    /// Walks the share's tree; reused for CACHE_SECONDS. A read error gives (0, 0) and is not cached.
    async fn usage(&self) -> Usage {
        let connected = self.available().await;
        let (used, free) = if !connected {
            (0, 0)
        } else if let Some((at, u)) = *lock(&self.usage_cached)
            && (self.clock)() - at < CACHE_SECONDS
        {
            u
        } else {
            let path = self.settings.path.clone().unwrap_or_default();
            let measured = tokio::task::spawn_blocking(move || -> std::io::Result<(u64, u64)> {
                Ok((tree_size(&path)?, fs2::available_space(&path)?))
            })
            .await
            .ok()
            .and_then(|r| r.ok());
            match measured {
                Some(u) => {
                    *lock(&self.usage_cached) = Some(((self.clock)(), u));
                    u
                }
                None => (0, 0),
            }
        };
        Usage {
            used,
            free: Some(free),
            limit: None,
            reliable: self.last_state() == Some(State::Connected),
        }
    }
}

fn tree_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let meta = std::fs::symlink_metadata(entry.path())?;
        total += if meta.is_dir() {
            tree_size(&entry.path())?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

fn not_found(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, what)
}

fn linked_folder(path: &Path) -> bool {
    path.parent()
        .and_then(|d| std::fs::symlink_metadata(d).ok())
        .is_some_and(|m| m.file_type().is_symlink())
}

/// A share entry's own metadata, never a link's target. A recording folder that is a link reads as missing: a link
/// on the share (whoever can write there can plant one) must not reach a file outside it.
pub(crate) fn share_meta(path: &Path) -> std::io::Result<std::fs::Metadata> {
    if linked_folder(path) {
        return Err(not_found("share_link"));
    }
    std::fs::symlink_metadata(path)
}

/// A regular file on the share, opened without following a link. It is checked before the open as well: a FIFO
/// there would hang the open.
fn open_share_file(path: &Path) -> std::io::Result<std::fs::File> {
    if !share_meta(path)?.is_file() {
        return Err(not_found("not_a_file"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|e| {
        #[cfg(unix)]
        if e.raw_os_error() == Some(libc::ELOOP) {
            return not_found("share_link");
        }
        e
    })?;
    if !file.metadata()?.is_file() {
        return Err(not_found("not_a_file"));
    }
    Ok(file)
}

fn mounted(path: &Path, allow_any_path: bool) -> bool {
    if allow_any_path {
        return true;
    }
    let mut parts = path.components().skip(1);
    let (Some(share), Some(name)) = (parts.next(), parts.next()) else {
        return false;
    };
    if share.as_os_str() != "share" || parts.next().is_none() {
        return false;
    }
    is_mount(&Path::new("/share").join(name.as_os_str()))
}

#[cfg(unix)]
fn is_mount(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(me), Some(parent)) = (std::fs::symlink_metadata(path), path.parent()) else {
        return false;
    };
    if me.file_type().is_symlink() {
        return false;
    }
    std::fs::metadata(parent).is_ok_and(|p| p.dev() != me.dev() || p.ino() == me.ino())
}
#[cfg(not(unix))]
fn is_mount(_: &Path) -> bool {
    false
}

/// One writer per share target in this process, whichever backend instance or cycle it comes from. A blocking copy
/// outlives the future that started it (an abort only drops the future), so without this a later cycle or a
/// recovery could create, sweep or rename the same `<kind>.part` while the earlier copy still writes it. The guard
/// is taken and released on the blocking thread that does the writing.
static WRITERS: LazyLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = LazyLock::new(Mutex::default);

fn writer(target: &Path) -> Arc<Mutex<()>> {
    let mut all = lock(&WRITERS);
    if let Some(w) = all.get(target).and_then(Weak::upgrade) {
        return w;
    }
    all.retain(|_, w| w.strong_count() > 0);
    let w = Arc::new(Mutex::new(()));
    all.insert(target.to_owned(), Arc::downgrade(&w));
    w
}

/// Blocking: removes a share target with no copy of it in progress, unless `cancel` is set. A removal runs on a
/// blocking thread that outlives its caller; once the caller has gone (a timeout, an aborted cycle) a later cycle may
/// have written and indexed this very file, so the flag is read under the target's writer guard, right before the
/// unlink. A recording folder that is a link is refused rather than removed through.
pub(crate) fn remove_target(target: &Path, cancel: &AtomicBool) -> std::io::Result<()> {
    let w = writer(target);
    let _g = lock(&w);
    if cancel.load(Ordering::SeqCst) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "cancelled",
        ));
    }
    if linked_folder(target) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "share_link",
        ));
    }
    std::fs::remove_file(target)
}

/// [`remove_target`] off the runtime's workers, cancelled when the caller goes.
pub(crate) async fn remove_share_file(path: &Path) -> std::io::Result<()> {
    let (path, flag) = (path.to_owned(), Arc::new(AtomicBool::new(false)));
    let _cancel = CancelOnDrop(flag.clone());
    tokio::task::spawn_blocking(move || remove_target(&path, &flag))
        .await
        .unwrap_or_else(|_| Err(std::io::Error::other("join")))
}

/// Blocking: a `<kind>.part` left behind is removed unless a copy of this process still owns it (that copy removes
/// or renames it itself). True when it was removed.
pub(crate) fn remove_stale_part(part: &Path) -> std::io::Result<bool> {
    let name = part.file_name().unwrap_or_default().to_string_lossy();
    let target = part.with_file_name(name.strip_suffix(".part").unwrap_or(&name));
    let w = writer(&target);
    let Ok(_g) = w.try_lock() else {
        return Ok(false);
    };
    match std::fs::remove_file(part) {
        // Renamed or removed by its copy since it was listed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        r => r.map(|()| true),
    }
}

/// Set when the future waiting for a blocking share operation is gone: a copy stops before its next block and
/// before the rename, a removal before its unlink.
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// How [`NasBackend::copy_file`] put the file on the share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Copied {
    /// An identical copy was already there (left by a crash before its index was written).
    Reused,
    Written,
}

fn cancelled() -> BackendError {
    BackendError::new(ErrorCode::Io, "cancelled")
}

/// Python `archive_file`'s copy, whole on one blocking thread holding the target's writer guard: reuse an identical
/// target, else `<kind>.part`, fsync, read back and compare, rename. Any failure or cancellation removes the part.
/// `ours` reads the root's sentinel now: the caller's probe may be cached, and the root must still be this HARS root
/// (not a Python share, not an empty mount point) when the copy starts and when it replaces the target.
fn copy_to_share(
    local: &Path,
    target: &Path,
    sha256: Option<&str>,
    size: u64,
    ours: &dyn Fn() -> Probe,
    cancel: &AtomicBool,
    progress: &tokio::sync::mpsc::UnboundedSender<u64>,
) -> Result<Copied> {
    let w = writer(target);
    let _g = lock(&w);
    if cancel.load(Ordering::SeqCst) {
        return Err(cancelled());
    }
    let still_ours = || {
        let probe = ours();
        match probe.reason.as_deref() {
            _ if probe.state == State::Connected => Ok(()),
            Some("not_owned") => Err(BackendError::foreign()),
            _ => Err(BackendError::new(ErrorCode::Unavailable, "share")),
        }
    };
    still_ours()?;
    let io = |e: std::io::Error| BackendError::from_io(&e);
    // One level only (Python): a share folder missing because the share went away must not be recreated on the
    // local disk below its mount point.
    let parent = target.parent().unwrap_or(Path::new("/"));
    match std::fs::create_dir(parent) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(io(e)),
        _ => {}
    }
    if !std::fs::symlink_metadata(parent).map_err(io)?.is_dir() {
        return Err(BackendError::new(ErrorCode::Io, "share_link"));
    }
    // A copy left by a crash before its index was written is reused when it is identical.
    let same_size = std::fs::symlink_metadata(target).is_ok_and(|m| m.is_file() && m.len() == size);
    if same_size && sha256.is_some_and(|w| file_sha256(target).is_ok_and(|h| h == w)) {
        return Ok(Copied::Reused);
    }
    if fs2::available_space(parent).map_err(io)? < size + NAS_RESERVE {
        return Err(BackendError::new(ErrorCode::QuotaExceeded, "nas_full"));
    }
    let part = fsutil::with_suffix(target, ".part");
    // A part left behind, or planted as a link, is replaced rather than written through.
    match std::fs::remove_file(&part) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(e)),
        _ => {}
    }
    let written = (|| {
        let mut src = std::fs::File::open(local).map_err(io)?;
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .map_err(io)?;
        let (mut h, mut count, mut buf) = (Sha256::new(), 0u64, vec![0; CHUNK]);
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(cancelled());
            }
            let n = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io(e)),
            };
            h.update(&buf[..n]);
            dst.write_all(&buf[..n]).map_err(io)?;
            count += n as u64;
            let _ = progress.send(count);
        }
        if count != size {
            return Err(BackendError::new(ErrorCode::Mismatch, "size"));
        }
        let digest = hex::encode(h.finalize());
        if sha256.is_some_and(|s| s != digest) {
            return Err(BackendError::new(ErrorCode::Mismatch, "sha256"));
        }
        dst.sync_all().map_err(io)?;
        drop_cache(&dst);
        drop(dst);
        // The bytes on the share must be the bytes read: compared from the share, not this machine's cache.
        if file_sha256(&part).map_err(io)? != digest {
            return Err(BackendError::new(ErrorCode::Mismatch, "sha256"));
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(cancelled());
        }
        still_ours()?;
        std::fs::rename(&part, target).map_err(io)?;
        fsutil::sync_dir(target).map_err(io)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    written.map(|()| Copied::Written)
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// The HARS-owned NAS write root: its sentinel `.roadviewer-storage.json` carries `owner: "roadviewer-hars"` and this
/// data root's storage id. A share whose sentinel names another owner is never claimed or written.
pub struct NasBackend {
    core: NasCore,
}
impl fmt::Debug for NasBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NasBackend()")
    }
}

impl NasBackend {
    pub fn new(settings: NasSettings, root: impl Into<PathBuf>, clock: Clock) -> Self {
        let root: PathBuf = root.into();
        let local_root = root.clone();
        Self {
            core: NasCore {
                settings,
                root,
                avail: Availability::new(clock.clone()),
                clock,
                usage_cached: Mutex::default(),
                owner: Some(OWNER_VALUE),
                local: Arc::new(move || fsutil::local_id(&local_root, NAS_LOCAL_KEY)),
            },
        }
    }
    pub fn settings(&self) -> &NasSettings {
        &self.core.settings
    }
    pub fn path_for(&self, id: &str, kind: &str) -> Result<PathBuf> {
        self.core.path_for(id, kind)
    }
    pub fn mounted(&self) -> bool {
        self.core
            .settings
            .path
            .as_deref()
            .is_some_and(|p| mounted(p, self.core.settings.allow_any_path))
    }

    /// The share is still this HARS root and holds the copy just written, read now rather than from the probe
    /// cache: a share unmounted between files leaves an empty mount point on the local disk, where a copy and its
    /// read-back also succeed. With `written` (this call's copy was [`Copied::Written`], not reused) that copy is
    /// removed when the root has lost its sentinel or its folder, i.e. it landed on the local mount point. A root
    /// another owner marks (`not_owned`, `storage_id_mismatch`) is never cleaned: its `<id>/<kind>` may be theirs.
    /// Nothing is removed once the caller has stopped waiting.
    pub async fn confirm(&self, id: &str, kind: &str, size: u64, written: bool) -> bool {
        self.core.invalidate();
        let (Some(root), Ok(target)) = (
            self.core.settings.path.clone(),
            self.core.path_for(id, kind),
        ) else {
            return false;
        };
        let (local, owner) = ((self.core.local)(), self.core.owner);
        let flag = Arc::new(AtomicBool::new(false));
        let _cancel = CancelOnDrop(flag.clone());
        let check = tokio::task::spawn_blocking(move || {
            let probe = probe_sync(Some(&root), local, owner);
            let held = share_meta(&target).is_ok_and(|m| m.is_file() && m.len() == size);
            let unmounted = matches!(
                probe.reason.as_deref(),
                Some("sentinel_missing" | "path_missing")
            );
            if written && held && unmounted && remove_target(&target, &flag).is_ok() {
                let _ = target.parent().map(std::fs::remove_dir);
            }
            probe.state == State::Connected && held
        });
        let ok = tokio::time::timeout(Duration::from_secs_f64(PROBE_WAIT_SECONDS), check)
            .await
            .is_ok_and(|r| r.unwrap_or(false));
        if !ok {
            self.core.invalidate();
        }
        ok
    }

    /// [`ArchiveWrite::upload`]'s copy, telling a copy found identical on the share from one written now.
    pub async fn copy_file(
        &self,
        local: &Path,
        id: &str,
        kind: &str,
        sha256: Option<&str>,
        size: u64,
        on_progress: Progress<'_>,
    ) -> Result<Copied> {
        self.writable().await?;
        let target = self.core.path_for(id, kind)?;
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = CancelOnDrop(flag.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (local, want) = (local.to_owned(), sha256.map(str::to_owned));
        let (root, storage_id) = (self.core.settings.path.clone(), (self.core.local)());
        let copy = tokio::task::spawn_blocking(move || {
            let ours = || probe_sync(root.as_deref(), storage_id.clone(), Some(OWNER_VALUE));
            copy_to_share(&local, &target, want.as_deref(), size, &ours, &flag, &tx)
        });
        while let Some(count) = rx.recv().await {
            if let Err(e) = on_progress(count, &UploadResume::default()) {
                // The copy is stopped and its part gone before the caller hears of it.
                drop(cancel);
                let _ = copy.await;
                return Err(e);
            }
        }
        let copied = copy
            .await
            .map_err(|_| BackendError::new(ErrorCode::Io, "join"))??;
        drop(cancel);
        if matches!(copied, Copied::Reused) {
            on_progress(size, &UploadResume::default())?;
        }
        Ok(copied)
    }

    /// Writes need the root connected and HARS-owned; a root marked by anyone else is refused as foreign.
    async fn writable(&self) -> Result<PathBuf> {
        let probe = self.core.probe(false).await;
        if probe.reason.as_deref() == Some("not_owned") {
            return Err(BackendError::foreign());
        }
        if probe.state != State::Connected {
            return Err(BackendError::new(ErrorCode::Unavailable, "share"));
        }
        self.core
            .settings
            .path
            .clone()
            .ok_or_else(|| BackendError::new(ErrorCode::Unavailable, "share"))
    }
}

impl ArchiveRead for NasBackend {
    fn caps(&self) -> &'static Capabilities {
        &SMB_CAPS
    }
    async fn probe(&self, wait: bool) -> Probe {
        self.core.probe(wait).await
    }
    fn last_state(&self) -> Option<State> {
        self.core.last_state()
    }
    fn invalidate(&self) {
        self.core.invalidate();
    }
    fn storage_id(&self) -> Option<String> {
        (self.core.local)()
    }
    async fn stat(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
        self.core.stat(r).await
    }
    async fn find(&self, id: &str, kind: &str) -> Result<Vec<RemoteRef>> {
        self.core.find(id, kind).await
    }
    async fn verify(
        &self,
        r: &RemoteRef,
        sha256: Option<&str>,
        size: Option<u64>,
        _md5: Option<&str>,
    ) -> Result<Verdict> {
        self.core.verify(r, sha256, size).await
    }
    async fn open(&self, r: &RemoteRef, start: u64, end: Option<u64>) -> Result<ChunkReader> {
        self.core.open(r, start, end).await
    }
    async fn download_to(&self, r: &RemoteRef, dest: &Path, sha256: Option<&str>) -> Result<()> {
        self.core.download_to(r, dest, sha256).await
    }
    async fn usage(&self) -> Result<Usage> {
        Ok(self.core.usage().await)
    }
}

impl ArchiveWrite for NasBackend {
    /// Creates the sentinel only on a fresh setup (no local id, no sentinel) with the share mounted: an empty
    /// unmounted directory or another instance's share is never claimed.
    async fn activate(&self) {
        let s = &self.core.settings;
        if !s.enabled {
            return;
        }
        self.core.invalidate();
        let Some(path) = s.path.clone() else {
            return;
        };
        let (root, local, allow) = (
            self.core.root.clone(),
            (self.core.local)(),
            s.allow_any_path,
        );
        let _ = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if local.is_some()
                || probe_sync(Some(&path), None, Some(OWNER_VALUE))
                    .reason
                    .as_deref()
                    != Some("sentinel_missing")
                || !mounted(&path, allow)
            {
                return Ok(());
            }
            let storage_id = uuid::Uuid::new_v4().simple().to_string();
            let created_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            fsutil::atomic_json(
                &path.join(SENTINEL),
                &json!({"storage_id": storage_id, "created_at": created_at, "owner": OWNER_VALUE}),
            )?;
            fsutil::write_local_id(&root, NAS_LOCAL_KEY, &storage_id)
        })
        .await;
        self.core.invalidate();
    }

    /// Python `Archiver.archive_file`'s copy: `<kind>.part`, fsync, read back from the share and compared, then
    /// renamed over the target. Until the rename the existing target is untouched; a failure removes the part.
    async fn upload(
        &self,
        local: &Path,
        id: &str,
        kind: &str,
        sha256: Option<&str>,
        md5: Option<&str>,
        size: u64,
        _resume: &mut UploadResume,
        on_progress: Progress<'_>,
    ) -> Result<RemoteRef> {
        self.copy_file(local, id, kind, sha256, size, on_progress)
            .await?;
        Ok(RemoteRef::new(
            BackendKind::Smb,
            format!("{id}/{kind}"),
            size,
            sha256.map(str::to_owned),
            md5.map(str::to_owned),
        ))
    }

    async fn session_status(&self, _resume: &UploadResume) -> Result<SessionState> {
        Ok(SessionState::Expired)
    }

    async fn delete(&self, r: &RemoteRef) -> Result<bool> {
        let (id, kind) = parse_ref(r)?;
        let root = match self.writable().await {
            Ok(root) => root,
            Err(e) if e.is_foreign() => return Err(e),
            Err(_) => return Ok(false),
        };
        let path = self.core.path_for(&id, &kind)?;
        let flag = Arc::new(AtomicBool::new(false));
        let _cancel = CancelOnDrop(flag.clone());
        let removed = tokio::task::spawn_blocking(move || {
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
                && remove_target(&path, &flag).is_err()
            {
                return false;
            }
            // The recording's folder, once empty; anything else left there is kept rather than guessed at.
            let _ = std::fs::remove_dir(root.join(&id));
            true
        })
        .await
        .unwrap_or(false);
        self.core.invalidate();
        Ok(removed)
    }
}

#[cfg(target_os = "linux")]
fn drop_cache(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    // Advisory: a refusal changes nothing.
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}
#[cfg(not(target_os = "linux"))]
fn drop_cache(_: &std::fs::File) {}

/// A share folder written by the Python Road Viewer (`<path>/<id>/<kind>`, sentinel owner `roadviewer`), read with
/// Python's storage id. It has no upload, delete or activate.
pub struct NasLegacy {
    core: NasCore,
}
impl fmt::Debug for NasLegacy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NasLegacy()")
    }
}
impl NasLegacy {
    /// `storage_id`: Python's `.archive.json` `storage_id` (see python_storage_id()).
    pub fn new(path: impl Into<PathBuf>, storage_id: &str, clock: Clock) -> Self {
        let storage_id = storage_id.to_owned();
        Self {
            core: NasCore {
                settings: NasSettings {
                    enabled: true,
                    error: None,
                    path: Some(path.into()),
                    allow_any_path: false,
                },
                root: PathBuf::new(),
                avail: Availability::new(clock.clone()),
                clock,
                usage_cached: Mutex::default(),
                owner: None,
                local: Arc::new(move || Some(storage_id.clone())),
            },
        }
    }
    pub fn python_storage_id(python_root: &Path) -> Option<String> {
        fsutil::local_id(python_root, NAS_LOCAL_KEY)
    }
    /// The Python sentinel's owner, for telling a Python share from a HARS root.
    pub fn python_owner() -> &'static str {
        PYTHON_NAS_OWNER
    }
    pub fn path_for(&self, id: &str, kind: &str) -> Result<PathBuf> {
        self.core.path_for(id, kind)
    }
    /// The share's sentinel as written (owner, storage id), or the probe reason it cannot be read.
    pub fn sentinel(path: &Path) -> std::result::Result<Value, &'static str> {
        match std::fs::metadata(path) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err("path_missing"),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Err("path_missing");
            }
            Err(_) => return Err("os_error"),
        }
        match std::fs::read(path.join(SENTINEL)) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                .ok()
                .filter(Value::is_object)
                .ok_or("sentinel_invalid"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("sentinel_missing"),
            Err(_) => Err("os_error"),
        }
    }
    /// Every `<id>/<kind>` regular file of the share (symlinks are not followed).
    pub async fn list_media(&self) -> Result<Vec<LegacyFile>> {
        if !self.core.available().await {
            return Err(BackendError::new(ErrorCode::Unavailable, "share"));
        }
        let root = self.core.settings.path.clone().unwrap_or_default();
        tokio::task::spawn_blocking(move || -> std::io::Result<Vec<LegacyFile>> {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(&root)? {
                let entry = entry?;
                let id = entry.file_name().to_string_lossy().into_owned();
                if id.starts_with('.') || !entry.file_type()?.is_dir() {
                    continue;
                }
                for kind in MEDIA_KINDS {
                    match std::fs::symlink_metadata(entry.path().join(kind)) {
                        Ok(m) if m.is_file() => out.push(LegacyFile {
                            id: id.clone(),
                            kind: kind.to_owned(),
                            marked_sha256: None,
                            remote: RemoteRef::new(
                                BackendKind::Smb,
                                format!("{id}/{kind}"),
                                m.len(),
                                None,
                                None,
                            ),
                        }),
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            Ok(out)
        })
        .await
        .map_err(|_| BackendError::new(ErrorCode::Io, "join"))?
        .map_err(|e| unavailable(&e))
    }
}

impl ArchiveRead for NasLegacy {
    fn caps(&self) -> &'static Capabilities {
        &SMB_CAPS
    }
    async fn probe(&self, wait: bool) -> Probe {
        self.core.probe(wait).await
    }
    fn last_state(&self) -> Option<State> {
        self.core.last_state()
    }
    fn invalidate(&self) {
        self.core.invalidate();
    }
    fn storage_id(&self) -> Option<String> {
        (self.core.local)()
    }
    async fn stat(&self, r: &RemoteRef) -> Result<Option<RemoteStat>> {
        self.core.stat(r).await
    }
    async fn find(&self, id: &str, kind: &str) -> Result<Vec<RemoteRef>> {
        self.core.find(id, kind).await
    }
    async fn verify(
        &self,
        r: &RemoteRef,
        sha256: Option<&str>,
        size: Option<u64>,
        _md5: Option<&str>,
    ) -> Result<Verdict> {
        self.core.verify(r, sha256, size).await
    }
    async fn open(&self, r: &RemoteRef, start: u64, end: Option<u64>) -> Result<ChunkReader> {
        self.core.open(r, start, end).await
    }
    async fn download_to(&self, r: &RemoteRef, dest: &Path, sha256: Option<&str>) -> Result<()> {
        self.core.download_to(r, dest, sha256).await
    }
    async fn usage(&self) -> Result<Usage> {
        Ok(self.core.usage().await)
    }
}
