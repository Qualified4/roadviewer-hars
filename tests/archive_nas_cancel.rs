//! A share copy whose caller goes away (an aborted cycle, a stopped run): the copy stops on its blocking thread,
//! leaves no part behind and never writes the part of the copy that comes after it.
use roadviewer_hars::archive::{
    ArchiveWrite, BackendError, ErrorCode, NasBackend, NasSettings, UploadResume, monotonic_clock,
};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KIND: &str = "fcamera.hevc";
const SIZE: usize = 24 << 20;

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

fn content(seed: u8) -> Vec<u8> {
    (0..SIZE)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[0] ^ seed)
        .collect()
}
fn sha(d: &[u8]) -> String {
    hex::encode(Sha256::digest(d))
}

fn parts(share: &Path) -> Vec<String> {
    std::fs::read_dir(share.join(ID))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".part"))
                .collect()
        })
        .unwrap_or_default()
}

async fn setup() -> (tempfile::TempDir, Arc<NasBackend>, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let (root, nas) = (tmp.path().join("data"), tmp.path().join("share"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&nas).unwrap();
    let b = Arc::new(share(&root, &nas));
    b.activate().await;
    (tmp, b, nas)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aborted_copy_never_touches_the_next_copy_of_the_same_file() {
    let (tmp, b, nas) = setup().await;
    let (old, new) = (content(1), content(2));
    let (old_path, new_path) = (tmp.path().join("old"), tmp.path().join("new"));
    std::fs::write(&old_path, &old).unwrap();
    std::fs::write(&new_path, &new).unwrap();
    for delay in [0u64, 1, 2, 5, 10, 20, 50, 100] {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut tx = Some(tx);
        let (b1, p1, h1) = (b.clone(), old_path.clone(), sha(&old));
        let first = tokio::spawn(async move {
            // Stopped at its last block: what is left is the fsync, the read-back and the rename.
            let mut progress = move |done: u64, _: &UploadResume| {
                if done == SIZE as u64
                    && let Some(tx) = tx.take()
                {
                    let _ = tx.send(());
                }
                Ok(())
            };
            b1.upload(
                &p1,
                ID,
                KIND,
                Some(&h1),
                None,
                SIZE as u64,
                &mut UploadResume::default(),
                &mut progress,
            )
            .await
        });
        rx.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        first.abort();
        // Aborted, or finished before the abort arrived: either way the next copy must be undisturbed.
        let _ = first.await;
        // Recovery sweeps leftover parts before the next cycle copies the file again.
        let _ = std::fs::remove_file(nas.join(ID).join(format!("{KIND}.part")));
        // A new backend instance, as after a settings change or a restart of the archiver.
        let again = share(&tmp.path().join("data"), &nas);
        again
            .upload(
                &new_path,
                ID,
                KIND,
                Some(&sha(&new)),
                None,
                SIZE as u64,
                &mut UploadResume::default(),
                &mut |_, _| Ok(()),
            )
            .await
            .unwrap_or_else(|e| panic!("delay={delay}: the later copy failed: {e}"));
        let held = std::fs::read(nas.join(ID).join(KIND)).unwrap();
        assert_eq!(sha(&held), sha(&new), "delay={delay}");
        assert!(parts(&nas).is_empty(), "{:?}", parts(&nas));
        std::fs::remove_file(nas.join(ID).join(KIND)).unwrap();
    }
}

#[tokio::test]
async fn a_copy_stopped_by_its_caller_is_gone_before_the_caller_hears() {
    let (tmp, b, nas) = setup().await;
    let data = content(3);
    let path = tmp.path().join("local");
    std::fs::write(&path, &data).unwrap();
    let err = b
        .upload(
            &path,
            ID,
            KIND,
            Some(&sha(&data)),
            None,
            SIZE as u64,
            &mut UploadResume::default(),
            &mut |_, _| Err(BackendError::new(ErrorCode::Io, "stopped")),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Io);
    assert!(parts(&nas).is_empty(), "{:?}", parts(&nas));
    assert!(!nas.join(ID).join(KIND).exists());
}
