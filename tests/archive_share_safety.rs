//! What the share copy's confirmation may remove, and what a link on the share may reach. A confirmation removes
//! only the copy its own call wrote onto an empty mount point; never a file on a root another owner marks, and never
//! once its caller has given up (a later cycle may have written and indexed the same file since).
#![cfg(unix)]
use roadviewer_hars::archive::{
    ArchiveRead, ArchiveWrite, BackendKind, ErrorCode, NasBackend, NasSettings, RemoteRef,
    SENTINEL, UploadResume, monotonic_clock,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KIND: &str = "rlog.zst";

fn share(root: &Path, share: &Path) -> NasBackend {
    NasBackend::new(
        NasSettings {
            enabled: true,
            error: None,
            path: Some(share.to_path_buf()),
            allow_any_path: true,
        },
        root,
        monotonic_clock(),
    )
}

fn content(size: usize, seed: u8) -> Vec<u8> {
    (0..size)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[1] ^ seed)
        .collect()
}
fn sha(d: &[u8]) -> String {
    hex::encode(Sha256::digest(d))
}

struct Share {
    tmp: tempfile::TempDir,
    root: PathBuf,
    nas: PathBuf,
}
impl Share {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let (root, nas) = (tmp.path().join("data"), tmp.path().join("share"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&nas).unwrap();
        Self { tmp, root, nas }
    }
    fn target(&self) -> PathBuf {
        self.nas.join(ID).join(KIND)
    }
    fn storage_id(&self) -> String {
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.nas.join(SENTINEL)).unwrap()).unwrap();
        v["storage_id"].as_str().unwrap().to_owned()
    }
}

async fn upload(
    b: &NasBackend,
    local: &Path,
    data: &[u8],
) -> roadviewer_hars::archive::Result<RemoteRef> {
    b.upload(
        local,
        ID,
        KIND,
        Some(&sha(data)),
        None,
        data.len() as u64,
        &mut UploadResume::default(),
        &mut |_, _| Ok(()),
    )
    .await
}

fn mkfifo(path: &Path) {
    assert!(
        std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

/// The writing end of `fifo`, once a reader is blocked opening it (bounded).
fn writer_when_read(fifo: &Path) -> std::fs::File {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
        {
            Ok(f) => {
                use std::os::fd::AsRawFd;
                // Blocking writes from here on: the reader takes the bytes at its own pace.
                unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFL, 0) };
                return f;
            }
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline => {
                std::thread::yield_now();
            }
            Err(e) => panic!("no reader on the source: {e}"),
        }
    }
}

/// Cycle A's confirmation found the share looking unmounted with a copy of the right size, and its caller gave up
/// while it waited. Cycle B then wrote the same file again (and would index it and remove the local file): the
/// confirmation of A, finishing late, must leave B's copy alone.
#[test]
fn a_confirmation_its_caller_gave_up_on_never_removes_a_later_copy() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let s = Share::new();
    let data = content(1 << 20, 7);
    let size = data.len() as u64;
    let source = s.tmp.path().join("source");
    mkfifo(&source);
    let b = rt.block_on(async {
        let b = Arc::new(share(&s.root, &s.nas));
        b.activate().await;
        b
    });
    // Cycle B's copy holds the target's writer while it waits for its source.
    let later = rt.spawn({
        let (b, source, data) = (b.clone(), source.clone(), data.clone());
        async move { upload(&b, &source, &data).await }
    });
    let mut feed = writer_when_read(&source);
    std::fs::create_dir_all(s.target().parent().unwrap()).unwrap();
    std::fs::write(s.target(), &data).unwrap();
    let away = s.tmp.path().join("sentinel-away");
    std::fs::rename(s.nas.join(SENTINEL), &away).unwrap();
    let gave_up = rt.block_on(async {
        tokio::time::timeout(Duration::from_millis(300), b.confirm(ID, KIND, size, true))
            .await
            .is_err()
    });
    assert!(gave_up, "the confirmation waits for the copy in progress");
    std::fs::rename(&away, s.nas.join(SENTINEL)).unwrap();
    feed.write_all(&data).unwrap();
    drop(feed);
    rt.block_on(later)
        .unwrap()
        .expect("the later copy succeeds");
    // Every blocking thread, the abandoned confirmation's included, runs to its end.
    rt.shutdown_timeout(Duration::from_secs(30));
    let held = std::fs::read(s.target()).expect("the later copy stays on the share");
    assert_eq!(sha(&held), sha(&data));
}

/// A root whose sentinel names another owner, or another HARS instance's storage id, is never cleaned: the file
/// there at `<id>/<kind>` may be that owner's (Python uses the same layout).
#[tokio::test]
async fn a_confirmation_never_removes_from_a_root_another_owner_marks() {
    let s = Share::new();
    let b = share(&s.root, &s.nas);
    b.activate().await;
    let mine = s.storage_id();
    let theirs = content(4096, 3);
    std::fs::create_dir_all(s.target().parent().unwrap()).unwrap();
    for sentinel in [
        json!({"storage_id": mine, "owner": "roadviewer"}),
        json!({"storage_id": "0".repeat(32), "owner": "roadviewer-hars"}),
    ] {
        std::fs::write(s.target(), &theirs).unwrap();
        std::fs::write(s.nas.join(SENTINEL), sentinel.to_string()).unwrap();
        assert!(!b.confirm(ID, KIND, theirs.len() as u64, true).await);
        assert_eq!(
            std::fs::read(s.target()).ok().map(|d| sha(&d)),
            Some(sha(&theirs)),
            "{sentinel}: the other owner's file stays"
        );
    }
}

/// A copy written while the share was mounted, whose share then looks unmounted at the confirmation, is the one
/// thing a confirmation removes: it is this call's own write on what is now a local mount point.
#[tokio::test]
async fn a_confirmation_removes_its_own_copy_from_an_empty_mount_point() {
    let s = Share::new();
    let b = share(&s.root, &s.nas);
    b.activate().await;
    let data = content(4096, 4);
    let local = s.tmp.path().join("local");
    std::fs::write(&local, &data).unwrap();
    upload(&b, &local, &data).await.unwrap();
    std::fs::remove_file(s.nas.join(SENTINEL)).unwrap();
    assert!(!b.confirm(ID, KIND, data.len() as u64, true).await);
    assert!(!s.target().exists());
    assert!(!s.nas.join(ID).exists());
}

fn secret(s: &Share) -> PathBuf {
    let outside = s.tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join(KIND), b"container file").unwrap();
    outside
}

fn reference() -> RemoteRef {
    RemoteRef::new(BackendKind::Smb, format!("{ID}/{KIND}"), 0, None, None)
}

/// `<id>/<kind>` or `<id>` as a link on the share never reads a file outside it.
#[tokio::test]
async fn a_link_on_the_share_is_not_followed_when_reading() {
    let s = Share::new();
    let b = share(&s.root, &s.nas);
    b.activate().await;
    let outside = secret(&s);
    std::fs::create_dir_all(s.nas.join(ID)).unwrap();
    std::os::unix::fs::symlink(outside.join(KIND), s.target()).unwrap();
    for round in ["file", "folder"] {
        if round == "folder" {
            std::fs::remove_dir_all(s.nas.join(ID)).unwrap();
            std::os::unix::fs::symlink(&outside, s.nas.join(ID)).unwrap();
        }
        assert!(b.stat(&reference()).await.unwrap().is_none(), "{round}");
        assert!(b.find(ID, KIND).await.unwrap().is_empty(), "{round}");
        let opened = b.open(&reference(), 0, None).await;
        assert_eq!(
            opened.err().map(|e| e.code),
            Some(ErrorCode::NotFound),
            "{round}"
        );
        let dest = s.tmp.path().join(format!("download-{round}"));
        assert!(
            b.download_to(&reference(), &dest, None).await.is_err(),
            "{round}"
        );
        assert!(!dest.exists(), "{round}");
    }
}

/// A link planted on the HARS root is never written through: the copy refuses a linked recording folder and
/// replaces a linked `.part` instead of writing into what it points at.
#[tokio::test]
async fn a_link_on_the_share_is_not_followed_when_writing() {
    let s = Share::new();
    let b = share(&s.root, &s.nas);
    b.activate().await;
    let outside = secret(&s);
    let data = content(4096, 5);
    let local = s.tmp.path().join("local");
    std::fs::write(&local, &data).unwrap();
    std::os::unix::fs::symlink(&outside, s.nas.join(ID)).unwrap();
    assert!(upload(&b, &local, &data).await.is_err());
    assert_eq!(
        std::fs::read(outside.join(KIND)).unwrap(),
        b"container file"
    );
    assert!(!b.delete(&reference()).await.unwrap_or(false));
    assert_eq!(
        std::fs::read(outside.join(KIND)).unwrap(),
        b"container file"
    );
    std::fs::remove_file(s.nas.join(ID)).unwrap();
    std::fs::create_dir(s.nas.join(ID)).unwrap();
    std::os::unix::fs::symlink(
        outside.join(KIND),
        s.nas.join(ID).join(format!("{KIND}.part")),
    )
    .unwrap();
    upload(&b, &local, &data).await.unwrap();
    assert_eq!(
        std::fs::read(outside.join(KIND)).unwrap(),
        b"container file"
    );
    assert_eq!(sha(&std::fs::read(s.target()).unwrap()), sha(&data));
}
