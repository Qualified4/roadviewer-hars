//! Reading a recording's media wherever the archive index says it is (Python `server.py`: `media_file`,
//! `media_path`, `abort_remote`, `stage_remote`, `remote_chunks` and `remote_response`). A NAS file is a path like a
//! local one; a file only Drive has is streamed from Drive, or staged (hash-verified) where a path is needed.
use crate::archive::{
    Archive, ArchiveRead, BackendError, ErrorCode, index,
    resolve::{RemoteFile, Resolved},
};
use axum::{
    Json,
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header as h},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Python `STAGING_MARGIN`.
pub const STAGING_MARGIN: u64 = 100 * 1024 * 1024;
/// Python `PLAYBACK_RANGE`: an open playback range is answered with at most this much.
pub const PLAYBACK_RANGE: u64 = 16 << 20;
pub const STAGING: &str = ".staging";
const REMOTE_UNAVAILABLE: &str =
    "영상 저장소(Google Drive)에 연결할 수 없습니다. 인터넷과 Google Drive 연결을 확인하세요.";

/// A file the caller can read: a path (local folder or NAS), or a file only the remote backend has.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Path(PathBuf),
    Remote(RemoteFile),
}

/// Python `MEDIA_ERRORS` (and a backend failure while reading, `abort_remote`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaError {
    pub code: &'static str,
    /// The answer names Google Drive instead of the NAS (Python `request_state.remote_unavailable`).
    pub remote: bool,
}
impl MediaError {
    pub fn new(code: &'static str, remote: bool) -> Self {
        Self { code, remote }
    }
    pub fn not_found() -> Self {
        Self::new("not_found", false)
    }
    /// Python `abort_remote`.
    pub fn from_backend(e: &BackendError) -> Self {
        match e.code {
            ErrorCode::NotFound => Self::not_found(),
            ErrorCode::Mismatch => Self::new("media_corrupt", true),
            _ => Self::new("storage_unavailable", true),
        }
    }
    fn status(self) -> StatusCode {
        match self.code {
            "storage_unavailable" | "storage_checking" => StatusCode::SERVICE_UNAVAILABLE,
            "media_corrupt" => StatusCode::CONFLICT,
            "insufficient_disk_space" => StatusCode::INSUFFICIENT_STORAGE,
            _ => StatusCode::NOT_FOUND,
        }
    }
    fn message(self) -> Option<&'static str> {
        Some(match self.code {
            "storage_unavailable" if self.remote => REMOTE_UNAVAILABLE,
            "storage_unavailable" => "영상 저장소에 연결할 수 없습니다. NAS 연결을 확인하세요.",
            "storage_checking" => "영상 저장소 연결을 확인하는 중입니다. 잠시 후 다시 시도하세요.",
            "media_corrupt" => "영상 파일을 읽을 수 없습니다. 손상되었을 수 있습니다.",
            "insufficient_disk_space" => "보관된 파일을 내려받을 디스크 공간이 부족합니다.",
            _ => return None,
        })
    }
}
impl IntoResponse for MediaError {
    fn into_response(self) -> Response {
        let body = match self.message() {
            Some(message) => json!({"error": self.code, "message": message}),
            None => json!({"error": self.code}),
        };
        let mut response = (self.status(), Json(body)).into_response();
        if self.code == "storage_checking" {
            response
                .headers_mut()
                .insert(h::RETRY_AFTER, HeaderValue::from_static("5"));
        }
        response
    }
}
impl From<MediaError> for crate::error::ApiError {
    fn from(e: MediaError) -> Self {
        crate::error::ApiError(e.status(), e.code)
    }
}

/// Python `remote_entry`: the file is (or would be read) on a remote backend rather than the NAS or local disk.
fn remote_entry(meta: &Value, kind: &str) -> bool {
    let where_ = index::location(meta, kind);
    index::REMOTE_BACKENDS.contains(&where_.as_str())
        || where_ == "local" && index::remote(meta, kind).is_some()
}

/// Python `media_file`: the readable file, or None if it does not exist. An unreachable archive or a recorded read
/// error is an error instead: it must never look like a missing file to the caller.
pub async fn media_file(
    archive: &Archive,
    id: &str,
    meta: &Value,
    kind: &str,
) -> Result<Option<Source>, MediaError> {
    let (found, state) = archive.resolve(id, meta, kind).await;
    let code = match state {
        "available" | "missing" => {
            return Ok(match found {
                Resolved::Local(p) => Some(Source::Path(p)),
                Resolved::Remote(r) => Some(Source::Remote(r)),
                Resolved::Nothing => None,
            });
        }
        "checking" => "storage_checking",
        "corrupt" => "media_corrupt",
        _ => "storage_unavailable",
    };
    Err(MediaError::new(code, remote_entry(meta, kind)))
}

/// Python `media_path`: [`media_file`], answering 404 when there is none.
pub async fn media_path(
    archive: &Archive,
    id: &str,
    meta: &Value,
    kind: &str,
) -> Result<Source, MediaError> {
    media_file(archive, id, meta, kind)
        .await?
        .ok_or_else(MediaError::not_found)
}

/// A staged copy or an export, removed when this is dropped. A request whose client goes away is dropped at its next
/// await, and what it staged or exported must go with it instead of filling the data root until a restart (Python's
/// `except BaseException: unstage(dest)`); the removal itself runs on a blocking thread, so it is not cut short.
#[derive(Debug)]
pub struct Temporary {
    path: Option<PathBuf>,
    staged: bool,
}
impl Temporary {
    pub fn file(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            staged: false,
        }
    }
    pub fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or(Path::new(""))
    }
    /// The path, for a caller that removes it itself.
    pub fn keep(mut self) -> PathBuf {
        self.path.take().unwrap_or_default()
    }
    /// Removed now, the removal awaited.
    pub async fn remove(mut self) {
        if let Some(path) = self.path.take() {
            let staged = self.staged;
            let _ = tokio::task::spawn_blocking(move || remove_temporary(&path, staged)).await;
        }
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let staged = self.staged;
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(move || remove_temporary(&path, staged));
            }
            Err(_) => remove_temporary(&path, staged),
        }
    }
}
fn remove_temporary(path: &Path, staged: bool) {
    if !staged {
        let _ = std::fs::remove_file(path);
    } else if let Some(dir) = path.parent() {
        // `.staging` itself only once empty: other requests' copies stay.
        let _ = std::fs::remove_dir_all(dir);
        if let Some(staging) = dir.parent() {
            let _ = std::fs::remove_dir(staging);
        }
    }
}

/// Python `stage_remote`: a hash-verified local copy of a remote-only file under `<folder>/.staging`, for where a
/// path is needed (original-format export, the converter's inputs). The caller removes it with [`unstage`].
pub async fn stage_remote(
    archive: &Archive,
    folder: &Path,
    file: &RemoteFile,
) -> Result<PathBuf, MediaError> {
    stage(archive, folder, file).await.map(Temporary::keep)
}

/// [`stage_remote`], removed when the result (or this future, mid-download) is dropped.
pub async fn stage(
    archive: &Archive,
    folder: &Path,
    file: &RemoteFile,
) -> Result<Temporary, MediaError> {
    let free = fs2::available_space(&archive.root)
        .map_err(|_| MediaError::new("storage_unavailable", false))?;
    if free < file.size + STAGING_MARGIN {
        return Err(MediaError::new("insufficient_disk_space", false));
    }
    let dest = folder
        .join(STAGING)
        .join(uuid::Uuid::new_v4().simple().to_string())
        .join(&file.kind);
    let staged = Temporary {
        path: Some(dest.clone()),
        staged: true,
    };
    if tokio::fs::create_dir_all(dest.parent().unwrap())
        .await
        .is_err()
    {
        staged.remove().await;
        return Err(MediaError::new("storage_unavailable", false));
    }
    let backend = archive.backend();
    let sha = file.reference.sha256.clone();
    if let Err(e) = backend
        .download_to(&file.reference, &dest, sha.as_deref())
        .await
    {
        staged.remove().await;
        return Err(MediaError::from_backend(&e));
    }
    Ok(staged)
}

/// Python `unstage`: the staged copy's folder, and `.staging` once nothing else uses it.
pub async fn unstage(path: &Path) {
    let path = path.to_owned();
    let _ = tokio::task::spawn_blocking(move || remove_temporary(&path, true)).await;
}

/// Python's original-format export (`media.reverse`) of `input` into `output`, run as a task of its own: a request
/// dropped meanwhile leaves the task to finish, and the task then drops the staged input and the output it made.
pub async fn export_original(
    input: PathBuf,
    staged: Option<Temporary>,
    kind: String,
    output: PathBuf,
    offset: f64,
) -> anyhow::Result<Temporary> {
    tokio::spawn(async move {
        let out = Temporary::file(output);
        let made = crate::media::reverse(&input, &kind, out.path(), offset).await;
        if let Some(staged) = staged {
            staged.remove().await;
        }
        made.map(|()| out)
    })
    .await?
}

/// A remote file (or one range of it) as a response body, one chunk at a time. Python `remote_chunks`: the request is
/// made here, so an unreachable archive is answered before any byte is sent.
pub async fn remote_body(
    archive: &Archive,
    file: &RemoteFile,
    range: Option<(u64, u64)>,
) -> Result<Body, MediaError> {
    let backend = archive.backend();
    let reader = match range {
        Some((start, end)) => backend.open(&file.reference, start, Some(end)).await,
        None => backend.open(&file.reference, 0, None).await,
    }
    .map_err(|e| MediaError::from_backend(&e))?;
    let stream = futures_util::stream::unfold(reader, |mut reader| async move {
        match reader.next_chunk().await {
            Ok(Some(chunk)) => Some((Ok::<_, std::io::Error>(chunk), reader)),
            Ok(None) => None,
            Err(e) => Some((Err(std::io::Error::other(e.code.as_str())), reader)),
        }
    });
    Ok(Body::from_stream(stream))
}

/// A conversion's inputs (Python `run_job` before its lock): every stored kind resolved, Drive-only files staged
/// as verified local copies, and whether the job must wait for the archive instead of running without them.
pub struct Inputs {
    /// Where the converter reads each kind (its local path when it is not available anywhere).
    pub paths: std::collections::BTreeMap<&'static str, PathBuf>,
    pub states: std::collections::BTreeMap<&'static str, &'static str>,
    /// Staged copies the caller removes with [`unstage`] when the job ends.
    pub staged: Vec<PathBuf>,
    /// Python's deferral message (`deferred: "storage"`); None when the job can run.
    pub deferred: Option<&'static str>,
}

pub async fn conversion_inputs(archive: &Archive, folder: &Path, id: &str, meta: &Value) -> Inputs {
    let mut found = std::collections::BTreeMap::new();
    for kind in crate::parity::STORED_KINDS {
        found.insert(kind, archive.resolve(id, meta, kind).await);
    }
    let remote: Vec<&'static str> = found
        .iter()
        .filter(|(_, (r, s))| matches!(r, Resolved::Remote(_)) && *s == "available")
        .map(|(k, _)| *k)
        .collect();
    let size = |k: &str| match &found[k].0 {
        Resolved::Remote(r) => r.size,
        _ => 0,
    };
    let mut short = false;
    let mut staged = Vec::new();
    // Room for every copy is checked before the first download: a job deferred for space must not download part of
    // its inputs again at each retry.
    let needed = remote.iter().map(|k| size(k)).sum::<u64>() + STAGING_MARGIN;
    if !remote.is_empty() && fs2::available_space(&archive.root).unwrap_or(0) < needed {
        short = true;
        for kind in &remote {
            found.insert(kind, (Resolved::Nothing, "storage_unavailable"));
        }
    } else {
        for kind in &remote {
            let Resolved::Remote(file) = found[kind].0.clone() else {
                continue;
            };
            let result = match stage_remote(archive, folder, &file).await {
                Ok(path) => {
                    staged.push(path.clone());
                    (Resolved::Local(path), "available")
                }
                Err(e) if e.code == "insufficient_disk_space" => {
                    short = true;
                    (Resolved::Nothing, "storage_unavailable")
                }
                Err(e) => (
                    Resolved::Nothing,
                    match e.code {
                        "media_corrupt" => "corrupt",
                        "not_found" => "missing",
                        _ => "storage_unavailable",
                    },
                ),
            };
            found.insert(kind, result);
        }
    }
    let wait = found.values().any(|(r, s)| {
        matches!(*s, "storage_unavailable" | "checking") || matches!(r, Resolved::Remote(_))
    });
    let deferred = wait.then_some(if short {
        "디스크 공간이 부족해 변환을 미뤘습니다."
    } else {
        "저장소에 연결할 수 없어 변환을 미뤘습니다."
    });
    let paths = found
        .iter()
        .map(|(k, (r, s))| {
            let path = match r {
                Resolved::Local(p) if *s == "available" => p.clone(),
                _ => folder.join(k),
            };
            (*k, path)
        })
        .collect();
    let states = found.iter().map(|(k, (_, s))| (*k, *s)).collect();
    Inputs {
        paths,
        states,
        staged,
        deferred,
    }
}

/// Python `cleanup_uploads(startup=True)`: copies of remote files left by an interrupted download or conversion.
pub fn cleanup_staging(root: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let staging = entry.path().join(STAGING);
        if crate::store::valid_id(&name) && staging.is_dir() {
            std::fs::remove_dir_all(staging)?;
        }
    }
    Ok(())
}

/// A `Range` header Python answers with 416.
#[derive(Debug, PartialEq, Eq)]
pub struct Unsatisfiable;

/// The byte range a `Range` header asks of a file of `size` bytes (Python `remote_response`'s parse): Ok(None)
/// without a header, Err for an unsatisfiable one. Playback's open ranges are shortened to [`PLAYBACK_RANGE`].
pub fn requested_range(
    header: Option<&str>,
    size: u64,
    attachment: bool,
) -> Result<Option<(u64, u64)>, Unsatisfiable> {
    let Some(header) = header else {
        return Ok(None);
    };
    let (first, last) = header
        .trim()
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .filter(|(a, b)| {
            a.bytes().all(|c| c.is_ascii_digit()) && b.bytes().all(|c| c.is_ascii_digit())
        })
        .unwrap_or(("", ""));
    let number = |s: &str| s.parse::<u64>().unwrap_or(u64::MAX);
    let (mut start, mut end) = (0, size.saturating_sub(1));
    if !first.is_empty() {
        start = number(first);
        end = if !last.is_empty() {
            number(last).min(size.saturating_sub(1))
        } else if attachment {
            size.saturating_sub(1)
        } else {
            size.saturating_sub(1)
                .min(start.saturating_add(PLAYBACK_RANGE - 1))
        };
    } else if !last.is_empty() {
        start = size.saturating_sub(number(last));
    }
    let unsatisfiable = (first.is_empty() && last.is_empty())
        || (first.is_empty() && number(last) == 0)
        || (!first.is_empty() && !last.is_empty() && number(last) < number(first))
        || start >= size;
    if unsatisfiable {
        return Err(Unsatisfiable);
    }
    Ok(Some((start, end)))
}

/// Python `remote_response`: a file only the remote backend has, served like a local one with one byte range at most.
/// `attachment` names the download; playback has none.
pub async fn remote_response(
    archive: &Archive,
    file: &RemoteFile,
    mime: &str,
    attachment: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    let size = file.size;
    let header = headers.get(h::RANGE).map(|v| v.to_str().unwrap_or(""));
    let range = match requested_range(header, size, attachment.is_some()) {
        Ok(r) => r,
        Err(Unsatisfiable) => {
            let mut response = (
                StatusCode::RANGE_NOT_SATISFIABLE,
                Json(json!({"error": "range_not_satisfiable"})),
            )
                .into_response();
            let hs = response.headers_mut();
            hs.insert(
                h::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}")).unwrap(),
            );
            hs.insert(h::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return response;
        }
    };
    let body = match remote_body(archive, file, range).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let (start, end) = range.unwrap_or((0, size.saturating_sub(1)));
    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut response = (status, body).into_response();
    let hs = response.headers_mut();
    hs.insert(h::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap());
    hs.insert(h::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // Playback has no Content-Length (Content-Range carries the size): Ingress buffers a response it knows is small.
    if attachment.is_some() || range.is_none() {
        // An empty file has no last byte: Python's end - start + 1 is 0 there.
        let length = if range.is_some() {
            end + 1 - start
        } else {
            size
        };
        hs.insert(h::CONTENT_LENGTH, HeaderValue::from(length));
    }
    if range.is_some() {
        hs.insert(
            h::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")).unwrap(),
        );
    }
    let etag = hex::encode(Sha256::digest(file.etag.as_bytes()));
    hs.insert(
        h::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", &etag[..32])).unwrap(),
    );
    hs.insert(
        h::CACHE_CONTROL,
        HeaderValue::from_static(if attachment.is_some() {
            "no-store"
        } else {
            "private, max-age=3600"
        }),
    );
    if let Some(name) = attachment
        && let Ok(v) = HeaderValue::from_str(&crate::parity::message_disposition(name))
    {
        hs.insert(h::CONTENT_DISPOSITION, v);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_follow_python_remote_response() {
        let size = 100;
        assert_eq!(requested_range(None, size, false), Ok(None));
        assert_eq!(
            requested_range(Some("bytes=10-19"), size, false),
            Ok(Some((10, 19)))
        );
        assert_eq!(
            requested_range(Some("bytes=10-"), size, true),
            Ok(Some((10, 99)))
        );
        assert_eq!(
            requested_range(Some("bytes=-30"), size, false),
            Ok(Some((70, 99)))
        );
        assert_eq!(
            requested_range(Some("bytes=-300"), size, false),
            Ok(Some((0, 99)))
        );
        assert_eq!(
            requested_range(Some("bytes=90-500"), size, false),
            Ok(Some((90, 99)))
        );
        for bad in [
            "bytes=-0",
            "bytes=-",
            "bytes=20-10",
            "bytes=100-",
            "items=1-2",
            "bytes=1-2,4-5",
        ] {
            assert_eq!(
                requested_range(Some(bad), size, false),
                Err(Unsatisfiable),
                "{bad}"
            );
        }
        let big = 64 << 20;
        assert_eq!(
            requested_range(Some("bytes=0-"), big, false),
            Ok(Some((0, PLAYBACK_RANGE - 1)))
        );
        assert_eq!(
            requested_range(Some("bytes=0-"), big, true),
            Ok(Some((0, big - 1)))
        );
    }
}
