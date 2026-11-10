//! Native macOS storage, guardian, and crash-recovery regressions.
use super::{
    REQUIRED_MOUNT_FLAGS, command, command_until,
    recovery::{
        Journal, cleanup_directory, cleanup_with_retry, device_identity, recover_orphans,
        recover_orphans_until, retry_cleanup, runtime_root,
    },
    setup::{Volume, record_attachment},
    storage::Storage,
    validate_mount_flags,
};
use crate::macos_registry as registry;
use rustix::fs::statfs as mount_info;
use std::os::unix::ffi::OsStrExt;
use std::{
    cell::RefCell,
    process::Child,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use std::{
    fs::{self, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CleanupStage {
    BeforeUnmount,
    BeforeDetach,
    MetadataSweep,
}

type CleanupHook = dyn FnMut(CleanupStage, &Path, Instant) -> io::Result<()>;

thread_local! {
    // Recovery runs on the caller's thread. A scoped thread-local hook
    // avoids changing environment or behavior in other tests and helpers.
    static CLEANUP_HOOK: RefCell<Option<Box<CleanupHook>>> = const { RefCell::new(None) };
}

pub(super) fn cleanup_checkpoint(
    stage: CleanupStage,
    mount: &Path,
    deadline: Instant,
) -> io::Result<()> {
    CLEANUP_HOOK.with_borrow_mut(|hook| match hook {
        Some(hook) => hook(stage, mount, deadline),
        None => Ok(()),
    })
}

struct CleanupHookGuard;
impl CleanupHookGuard {
    fn install(hook: impl FnMut(CleanupStage, &Path, Instant) -> io::Result<()> + 'static) -> Self {
        CLEANUP_HOOK.with_borrow_mut(|slot| {
            assert!(slot.is_none());
            *slot = Some(Box::new(hook));
        });
        Self
    }
}
impl Drop for CleanupHookGuard {
    fn drop(&mut self) {
        CLEANUP_HOOK.with_borrow_mut(|slot| *slot = None);
    }
}

struct OrphanStorage {
    _storage: Storage,
    path: PathBuf,
    directory: PathBuf,
    id: uuid::Uuid,
}

fn orphan_storage(root: &Path) -> OrphanStorage {
    let mut storage = Storage {
        volume: Some(Volume::create_in(root).unwrap()),
        counter: 0,
        files: std::collections::HashSet::new(),
    };
    let path = storage.materialize(b"orphan-fixture", "").unwrap();
    let volume = storage.volume.as_mut().unwrap();
    // Setup has completed with a real keeper birth time. Mark only its
    // journal as orphaned, leaving the guardian dormant while testing.
    volume.journal.birth = [0, 0];
    volume.journal.save(&volume.directory).unwrap();
    let directory = volume.directory.clone();
    let id = volume.journal.lease_id.unwrap();
    OrphanStorage {
        _storage: storage,
        path,
        directory,
        id,
    }
}

#[test]
fn recovery_deadline_interrupts_cleanup_and_later_attempt_finishes_all_orphans() {
    for interrupted_stage in [
        CleanupStage::BeforeUnmount,
        CleanupStage::BeforeDetach,
        CleanupStage::MetadataSweep,
    ] {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let orphans = [orphan_storage(&root_path), orphan_storage(&root_path)];
        let visited = Arc::new(Mutex::new(Vec::new()));
        let visits = visited.clone();
        let hook = CleanupHookGuard::install(move |stage, mount, deadline| {
            if stage != interrupted_stage {
                return Ok(());
            }
            visits.lock().unwrap().push(mount.to_path_buf());
            // Exercise the production volume-command wrapper and its
            // termination path with a child that outlives the budget.
            command_until("/bin/sleep", &["60".as_ref()], deadline)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        recover_orphans_until(&root_path, deadline).unwrap();
        drop(hook);
        assert!(Instant::now() < deadline + Duration::from_secs(3));
        let visited = visited.lock().unwrap();
        assert_eq!(
            visited.len(),
            1,
            "recovery started another orphan after expiry"
        );
        let interrupted = orphans
            .iter()
            .find(|orphan| orphan.path.parent() == Some(visited[0].as_path()))
            .unwrap();
        for orphan in &orphans {
            Journal::read(&orphan.directory).unwrap();
            assert!(orphan.directory.join("setup.lock").exists());
        }
        assert_eq!(
            orphans.iter().filter(|orphan| orphan.path.exists()).count(),
            1
        );
        for orphan in orphans.iter().filter(|orphan| orphan.path.exists()) {
            assert_eq!(fs::read(&orphan.path).unwrap(), b"orphan-fixture");
        }
        let journal = Journal::read(&interrupted.directory).unwrap();
        match interrupted_stage {
            CleanupStage::BeforeUnmount => assert!(journal.validate_mount().unwrap()),
            CleanupStage::BeforeDetach => {
                assert!(!journal.validate_mount().unwrap());
                assert!(journal.validate_device().unwrap());
            }
            CleanupStage::MetadataSweep => {
                assert!(!journal.mount.exists());
                assert!(registry::device(interrupted.id).unwrap().is_none());
            }
        }
        assert!(
            fs::read_to_string(interrupted.directory.join("cleanup.error"))
                .unwrap()
                .contains("timed out")
        );

        recover_orphans(&root_path).unwrap();
        for orphan in &orphans {
            assert!(!orphan.directory.exists());
            assert!(!orphan.path.exists());
            assert!(registry::device(orphan.id).unwrap().is_none());
        }
    }
}

#[test]
fn volume_full_allocation_removes_partial_file_and_preserves_existing_secrets() {
    let mut storage = Storage::default();
    let existing = storage.materialize(b"existing-fixture", "").unwrap();
    let mount = existing.parent().unwrap();
    let entries = || {
        let mut names: Vec<_> = fs::read_dir(mount)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.as_bytes().starts_with(b"secret-"))
            .collect();
        names.sort();
        names
    };
    let original_entries = entries();
    // The RAM volume is 64 MiB including filesystem overhead. A 64 MiB
    // payload therefore starts writing and fails with ENOSPC.
    let error = storage
        .materialize(&vec![42; 64 * 1024 * 1024], "")
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
    assert_eq!(entries(), original_entries);
    assert_eq!(fs::read(&existing).unwrap(), b"existing-fixture");
    // Reclaimed space remains usable after the failed partial allocation.
    let next = storage.materialize(b"next-fixture", "").unwrap();
    assert_eq!(fs::read(&next).unwrap(), b"next-fixture");
    assert_eq!(fs::read(&existing).unwrap(), b"existing-fixture");
    storage.cleanup().unwrap();
    assert!(!existing.exists());
    assert!(!next.exists());
}

#[test]
fn payloads_deleted_by_consumers_count_as_released() {
    let mut storage = Storage::default();
    let released = storage.materialize(b"released-fixture", "").unwrap();
    let kept = storage.materialize(b"kept-fixture", "").unwrap();
    let swept = storage.materialize(b"swept-fixture", "").unwrap();
    fs::remove_file(&released).unwrap();
    fs::remove_file(&swept).unwrap();
    storage.release(&released).unwrap();
    assert!(storage.release(&released).is_err());
    assert_eq!(fs::read(&kept).unwrap(), b"kept-fixture");
    let mount = kept.parent().unwrap().to_owned();
    storage.cleanup().unwrap();
    assert!(!mount.exists());
}

#[test]
fn numeric_suffixes_cannot_collide_with_later_file_counters() {
    let mut storage = Storage::default();
    let first = storage.materialize(b"original", "2").unwrap();
    storage.release(&first).unwrap();
    let mut paths = Vec::new();
    // The released first name used to collide at counter 12; a live
    // second name with suffix "2" used to collide at counter 22.
    for counter in 2..=22 {
        let suffix = if counter == 2 { "2" } else { "" };
        let path = storage.materialize(b"later", suffix).unwrap();
        assert_ne!(path, first);
        assert!(!first.exists());
        paths.push(path);
    }
    for path in paths {
        assert_eq!(fs::read(path).unwrap(), b"later");
    }
    storage.cleanup().unwrap();
}

fn unattached_journal(root: &Path) -> (tempfile::TempDir, Journal) {
    let directory = tempfile::tempdir_in(root).unwrap();
    let mount = directory.path().join("memory");
    fs::DirBuilder::new().mode(0o700).create(&mount).unwrap();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.path().join("setup.lock"))
        .unwrap();
    let journal = Journal {
        keeper: std::process::id(),
        // A mismatched birth time represents a dead keeper with a reused PID.
        birth: [0, 0],
        lease_id: Some(uuid::Uuid::new_v4()),
        device: String::new(),
        mount,
        filesystem: None,
        device_identity: None,
        attachment_started: Some(false),
    };
    journal.save(directory.path()).unwrap();
    (directory, journal)
}

#[test]
fn recovery_removes_metadata_when_attachment_never_started() {
    let root = tempfile::tempdir().unwrap();
    let (directory, _) = unattached_journal(root.path());
    recover_orphans(root.path()).unwrap();
    assert!(!directory.path().exists());
}

#[test]
fn exited_attachments_without_a_registered_device_are_recoverable() {
    use std::os::unix::process::ExitStatusExt;
    let root = tempfile::tempdir().unwrap();
    // Exit code 1, and a success that never registered our identifier.
    for status in [1 << 8, 0] {
        let (directory, mut journal) = unattached_journal(root.path());
        journal.attachment_started = Some(true);
        journal.save(directory.path()).unwrap();
        let status = std::process::ExitStatus::from_raw(status);
        assert!(record_attachment(&mut journal, directory.path(), status).is_err());
        let saved = Journal::read(directory.path()).unwrap();
        assert_eq!(saved.attachment_started, Some(false));
        assert!(saved.device.is_empty());
        cleanup_directory(directory.path()).unwrap();
        assert!(!directory.path().exists());
    }
}

#[test]
fn reused_disk_number_does_not_block_recovery() {
    let root = tempfile::tempdir().unwrap();
    let (directory, mut journal) = unattached_journal(root.path());
    // The boot disk stands in for another attachment that reused our
    // disk number. No device carries this journal's fresh identifier.
    journal.device = "/dev/disk0".into();
    journal.device_identity = Some(device_identity(&fs::metadata("/dev/disk0").unwrap()));
    journal.attachment_started = Some(true);
    journal.save(directory.path()).unwrap();
    assert!(!journal.validate_device().unwrap());
    cleanup_directory(directory.path()).unwrap();
    assert!(!directory.path().exists());
}

#[test]
fn recovery_keeps_unknown_and_inflight_attachments() {
    let root = tempfile::tempdir().unwrap();
    for state in [None, Some(true)] {
        let (directory, mut journal) = unattached_journal(root.path());
        journal.attachment_started = state;
        let mut value = serde_json::to_value(&journal).unwrap();
        if state.is_none() {
            // Existing journals predate the attachment_started field.
            value.as_object_mut().unwrap().remove("attachment_started");
        }
        fs::write(
            directory.path().join("lease.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        let error = cleanup_directory(directory.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(directory.path().join("lease.json").exists());
        assert!(journal.mount.exists());
    }
}

#[test]
fn expired_recovery_budget_leaves_journals_for_another_attempt() {
    let root = tempfile::tempdir().unwrap();
    let (directory, _) = unattached_journal(root.path());
    recover_orphans_until(root.path(), Instant::now()).unwrap();
    assert!(directory.path().join("lease.json").exists());
    recover_orphans(root.path()).unwrap();
    assert!(!directory.path().exists());
}

#[test]
fn preattachment_cleanup_preserves_unexpected_mount_contents() {
    let root = tempfile::tempdir().unwrap();
    let (directory, journal) = unattached_journal(root.path());
    let sentinel = journal.mount.join("unrelated");
    fs::write(&sentinel, b"keep").unwrap();
    assert!(cleanup_directory(directory.path()).is_err());
    assert_eq!(fs::read(sentinel).unwrap(), b"keep");
    assert!(directory.path().join("lease.json").exists());
}

#[test]
fn persistent_cleanup_failure_is_recorded_and_removed_after_recovery() {
    let root = tempfile::tempdir().unwrap();
    let (directory, journal) = unattached_journal(root.path());
    let sentinel = journal.mount.join("unrelated");
    fs::write(&sentinel, b"private-fixture").unwrap();
    assert!(cleanup_with_retry(directory.path()).is_err());
    let diagnostic = directory.path().join("cleanup.error");
    assert!(!fs::read_to_string(&diagnostic).unwrap().is_empty());
    assert_eq!(fs::metadata(&diagnostic).unwrap().mode() & 0o777, 0o600);
    fs::remove_file(sentinel).unwrap();
    recover_orphans(root.path()).unwrap();
    assert!(!directory.path().exists());
}

#[test]
fn all_security_mount_flags_are_required() {
    validate_mount_flags(REQUIRED_MOUNT_FLAGS).unwrap();
    for flag in [
        libc::MNT_NOEXEC,
        libc::MNT_NOSUID,
        libc::MNT_NODEV,
        libc::MNT_DONTBROWSE,
    ] {
        assert!(validate_mount_flags(REQUIRED_MOUNT_FLAGS & !(flag as u32)).is_err());
    }
    assert!(
        validate_mount_flags(REQUIRED_MOUNT_FLAGS | libc::MNT_IGNORE_OWNERSHIP as u32).is_err()
    );
}

#[test]
fn unmounted_storage_never_writes_or_releases_on_the_backing_disk() {
    let lease = crate::FileLease::new(std::process::id());
    let file = lease.materialize(b"ram-only-fixture").unwrap();
    let path = file.path().to_owned();
    let mount = path.parent().unwrap();
    validate_mount_flags(mount_info(mount).unwrap().f_flags).unwrap();
    command("/sbin/umount", &["-f".as_ref(), mount.as_os_str()]).unwrap();
    // This is now the ordinary disk directory hidden by the RAM mount.
    // A release must not delete its unrelated file, and another allocation
    // must not put any payload bytes here.
    fs::write(&path, b"unrelated-disk-fixture").unwrap();
    let allocation = lease.materialize(b"must-not-reach-disk");
    let release = lease.release(&file);
    let disk_contents = fs::read(&path).unwrap();
    let disk_entries = fs::read_dir(mount).unwrap().count();
    fs::remove_file(&path).unwrap();
    let replacement = tempfile::tempdir().unwrap();
    fs::remove_dir(mount).unwrap();
    std::os::unix::fs::symlink(replacement.path(), mount).unwrap();
    let symlink_allocation = lease.materialize(b"must-not-follow-symlink");
    let replacement_entries = fs::read_dir(replacement.path()).unwrap().count();
    fs::remove_file(mount).unwrap();
    fs::DirBuilder::new().mode(0o700).create(mount).unwrap();
    drop(lease);
    assert!(allocation.is_err());
    assert!(release.is_err());
    assert!(symlink_allocation.is_err());
    assert_eq!(disk_contents, b"unrelated-disk-fixture");
    assert_eq!(disk_entries, 1);
    assert_eq!(replacement_entries, 0);
}

#[test]
fn storage_rejects_weakened_root_permissions() {
    let lease = crate::FileLease::new(std::process::id());
    let path = lease.materialize(b"private-fixture").unwrap().into_path();
    let mount = path.parent().unwrap();
    fs::set_permissions(mount, fs::Permissions::from_mode(0o755)).unwrap();
    let allocation = lease.materialize(b"must-stay-private");
    fs::set_permissions(mount, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(allocation.is_err());
    assert_eq!(fs::read(&path).unwrap(), b"private-fixture");
}

#[test]
fn storage_rejects_replaced_device_identity() {
    let lease = crate::FileLease::new(std::process::id());
    let path = lease
        .materialize(b"device-identity-fixture")
        .unwrap()
        .into_path();
    let directory = path.parent().unwrap().parent().unwrap().to_path_buf();
    let mut journal = Journal::read(&directory).unwrap();
    journal.device_identity = Some([0; 4]);
    let stale_volume = Volume { directory, journal };
    assert!(stale_volume.open_root().is_err());
    assert_eq!(fs::read(path).unwrap(), b"device-identity-fixture");
}

struct PausedSetup {
    test: tempfile::TempDir,
    keeper: Child,
    directory: PathBuf,
    worker: u32,
    id: uuid::Uuid,
}
impl PausedSetup {
    fn start(stage: &str) -> Self {
        let test = tempfile::tempdir().unwrap();
        fs::write(test.path().join("stage"), stage).unwrap();
        let mut keeper = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "macos::tests::create_entry", "--nocapture"])
            .env("_FILE_LEASE_CREATE_TEST", "1")
            .env("_FILE_LEASE_SETUP_TEST", test.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let (directory, _device, worker): (PathBuf, String, u32) = loop {
            if let Ok(bytes) = fs::read(test.path().join("ready"))
                && let Ok(ready) = serde_json::from_slice(&bytes)
            {
                break ready;
            }
            if Instant::now() >= deadline || keeper.try_wait().unwrap().is_some() {
                let _ = keeper.kill();
                let _ = keeper.wait();
                panic!("did not reach {stage} setup checkpoint");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let id = Journal::read(&directory).unwrap().lease_id.unwrap();
        Self {
            test,
            keeper,
            directory,
            worker,
            id,
        }
    }
    fn resume(&self) {
        unsafe {
            libc::kill(self.worker as _, libc::SIGCONT);
        }
        fs::write(self.test.path().join("resume"), "").unwrap();
    }
    fn wait_for_cleanup(&self, recover: bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while (self.directory.exists() || registry::device(self.id).unwrap().is_some())
            && Instant::now() < deadline
        {
            if recover {
                recover_orphans(&runtime_root().unwrap()).unwrap();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!self.directory.exists(), "journal survived recovery");
        // A concurrently starting lease may reuse the old disk number.
        // Its unique drive identifier must disappear, not every disk node
        // that happens to have the same name later.
        assert!(registry::device(self.id).unwrap().is_none());
    }
}
impl Drop for PausedSetup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.worker as _, libc::SIGCONT);
        }
        let _ = fs::write(self.test.path().join("resume"), "");
        let _ = self.keeper.kill();
        let _ = self.keeper.wait();
    }
}

#[test]
fn create_entry() {
    if std::env::var_os("_FILE_LEASE_CREATE_TEST").is_some() {
        let _volume = Volume::create().unwrap();
        panic!("setup checkpoint was not interrupted");
    }
}

#[test]
fn guardian_cleans_keeper_death_during_setup_without_another_lease() {
    for stage in ["prepared", "attaching", "attached", "formatted", "mounted"] {
        let mut paused = PausedSetup::start(stage);
        paused.keeper.kill().unwrap();
        paused.keeper.wait().unwrap();
        paused.resume();
        paused.wait_for_cleanup(false);
    }
}

#[test]
fn worker_death_during_setup_is_recoverable() {
    for stage in ["prepared", "attaching", "attached", "formatted", "mounted"] {
        let mut paused = PausedSetup::start(stage);
        assert_eq!(unsafe { libc::kill(paused.worker as _, libc::SIGKILL) }, 0);
        assert!(!paused.keeper.wait().unwrap().success());
        paused.wait_for_cleanup(true);
    }
}

#[test]
fn stopped_setup_worker_does_not_block_other_leases() {
    let mut paused = PausedSetup::start("attached");
    assert_eq!(unsafe { libc::kill(paused.worker as _, libc::SIGSTOP) }, 0);
    paused.keeper.kill().unwrap();
    paused.keeper.wait().unwrap();
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let lease = crate::FileLease::new(std::process::id());
        let result = lease
            .materialize(b"independent-lease")
            .map(|path| (lease, path));
        let _ = sender.send(result);
    });
    let result = receiver.recv_timeout(Duration::from_secs(8));
    // Release the old worker even when a regression blocked the new lease.
    paused.resume();
    thread.join().unwrap();
    paused.wait_for_cleanup(true);
    let (_lease, path) = result
        .expect("a busy orphan blocked an unrelated lease")
        .unwrap();
    assert_eq!(fs::read(path).unwrap(), b"independent-lease");
}

fn lease_state(lease: &crate::FileLease) -> (PathBuf, PathBuf, uuid::Uuid, u32) {
    let path = lease.materialize(b"guardian-test").unwrap().into_path();
    let directory = path.parent().unwrap().parent().unwrap().to_path_buf();
    let journal = Journal::read(&directory).unwrap();
    let guardian = fs::read_to_string(directory.join("setup.ready"))
        .unwrap()
        .parse()
        .unwrap();
    (path, directory, journal.lease_id.unwrap(), guardian)
}

fn removed(directory: &Path, id: uuid::Uuid) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while (directory.exists() || registry::device(id).unwrap().is_some())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!directory.exists(), "guardian did not remove the journal");
    assert!(registry::device(id).unwrap().is_none());
}

#[test]
fn guardian_survives_keeper_process_group_sigkill() {
    let lease = crate::FileLease::new(std::process::id());
    let (path, directory, id, _) = lease_state(&lease);
    let keeper = lease.keeper.lock().unwrap().as_ref().unwrap().pid();
    assert_eq!(unsafe { libc::kill(-(keeper as i32), libc::SIGKILL) }, 0);
    drop(lease);
    removed(&directory, id);
    assert!(!path.exists());
}

#[test]
fn keeper_still_cleans_up_if_guardian_is_killed() {
    let lease = crate::FileLease::new(std::process::id());
    let (path, directory, id, guardian) = lease_state(&lease);
    assert_eq!(unsafe { libc::kill(guardian as i32, libc::SIGKILL) }, 0);
    assert_eq!(fs::read(&path).unwrap(), b"guardian-test");
    drop(lease);
    removed(&directory, id);
}

#[test]
fn killing_both_processes_leaves_recoverable_metadata() {
    let lease = crate::FileLease::new(std::process::id());
    let (_, directory, id, guardian) = lease_state(&lease);
    let keeper = lease.keeper.lock().unwrap().as_ref().unwrap().pid();
    assert_eq!(unsafe { libc::kill(guardian as i32, libc::SIGKILL) }, 0);
    assert_eq!(unsafe { libc::kill(keeper as i32, libc::SIGKILL) }, 0);
    drop(lease);
    recover_orphans(&runtime_root().unwrap()).unwrap();
    removed(&directory, id);
}

#[test]
fn dead_shared_manager_and_guardians_leave_all_volumes_recoverable() {
    let pool = crate::volume_pool::Pool::get(&std::env::current_exe().unwrap()).unwrap();
    let first = crate::FileLease::new(std::process::id());
    let second = crate::FileLease::new(std::process::id());
    let payload = vec![54; 40 * 1024 * 1024];
    let one = first.materialize(&payload).unwrap().into_path();
    let two = second.materialize(&payload).unwrap().into_path();
    assert_ne!(one.parent(), two.parent());
    let states: Vec<_> = [&one, &two]
        .into_iter()
        .map(|path| {
            let directory = path.parent().unwrap().parent().unwrap().to_path_buf();
            let id = Journal::read(&directory).unwrap().lease_id.unwrap();
            let guardian: u32 = fs::read_to_string(directory.join("setup.ready"))
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(guardian as _, libc::SIGKILL) }, 0);
            (directory, id)
        })
        .collect();
    assert_eq!(unsafe { libc::kill(pool.pid() as _, libc::SIGKILL) }, 0);
    drop(first);
    drop(second);
    drop(pool);
    recover_orphans(&runtime_root().unwrap()).unwrap();
    for (directory, id) in states {
        removed(&directory, id);
    }
    assert!(!one.exists());
    assert!(!two.exists());
}

#[test]
fn transient_cleanup_failures_are_retried() {
    let mut attempts = 0;
    retry_cleanup(|| {
        attempts += 1;
        if attempts == 1 {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Ok(())
        }
    })
    .unwrap();
    assert_eq!(attempts, 2);
}

#[test]
fn persistent_cleanup_failures_return_after_bounded_retries() {
    let mut attempts = 0;
    assert!(
        retry_cleanup(|| {
            attempts += 1;
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        })
        .is_err()
    );
    assert_eq!(attempts, 3);
}
