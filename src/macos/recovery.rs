//! Bounded revocation, journal cleanup, and recovery of dead keepers.
#[cfg(test)]
use super::tests;
use super::{
    filesystem::unlink_payload,
    journal::Journal,
    process::process_info,
    tools::{TOOL_TIMEOUT, command_until},
};
use crate::profiling::Span;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn runtime_root() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::other("Cannot locate file-lease metadata"))?;
    let root = PathBuf::from(home).join("Library/Application Support/file-lease");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    let metadata = fs::symlink_metadata(&root)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "file-lease metadata directory must be private and user-owned",
        ));
    }
    root.canonicalize()
}

impl Journal {
    fn cleanup(&self, deadline: Instant) -> io::Result<()> {
        if !self.validate_device()? {
            return Ok(());
        }
        let Some((root, _)) = self.mounted_root()? else {
            command_until(
                "/usr/bin/hdiutil",
                &["detach".as_ref(), "-force".as_ref(), self.device.as_ref()],
                deadline,
            )?;
            return Ok(());
        };
        // Revalidate after pinning the root: a recycled disk number must not
        // turn a previous device check into permission to clean another mount.
        if !self.validate_device()? {
            return Err(io::Error::other(
                "RAM-disk device disappeared during cleanup",
            ));
        }
        // Remove all readable names first, even if a later unmount fails.
        for entry in fs::read_dir(&self.mount)? {
            check_deadline(deadline)?;
            let entry = entry?;
            if entry.file_type()?.is_file() {
                // A consumer may delete the name after it was listed.
                unlink_payload(&root, entry.file_name())?;
            }
        }
        drop(root);
        #[cfg(test)]
        tests::cleanup_checkpoint(tests::CleanupStage::BeforeUnmount, &self.mount, deadline)?;
        command_until(
            "/sbin/umount",
            &["-f".as_ref(), self.mount.as_os_str()],
            deadline,
        )?;
        #[cfg(test)]
        tests::cleanup_checkpoint(tests::CleanupStage::BeforeDetach, &self.mount, deadline)?;
        command_until(
            "/usr/bin/hdiutil",
            &["detach".as_ref(), "-force".as_ref(), self.device.as_ref()],
            deadline,
        )?;
        Ok(())
    }
}

/// Recover only journals belonging to dead keepers, comparing process birth
/// times and the mounted filesystem identity before removing any resource.
pub(super) fn recover_orphans(root: &Path) -> io::Result<()> {
    let _span = Span::new("recovery.scan");
    recover_orphans_until(root, Instant::now() + RECOVERY_TIMEOUT)
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Cleanup deadline expired",
        ))
    } else {
        Ok(())
    }
}

fn report_cleanup_failure(directory: &Path, error: &io::Error) {
    // Detached helpers must not retain the launcher's captured output pipes.
    // Preserve a metadata-only diagnostic for inspection after the caller exits.
    // Serialize with cleanup so a report cannot recreate files after its sweep.
    let _ = (|| -> io::Result<()> {
        let Some(_lock) = try_setup_lock(directory)? else {
            return Ok(());
        };
        Journal::read(directory)?;
        let mut report = tempfile::NamedTempFile::new_in(directory)?;
        writeln!(report, "{error}")?;
        report.as_file().sync_all()?;
        report
            .persist(directory.join("cleanup.error"))
            .map_err(|error| error.error)?;
        Ok(())
    })();
    eprintln!(
        "file-lease: cleanup deferred for {}: {error}; a later lease will retry recovery",
        directory.display()
    );
}

pub(super) fn recover_orphans_until(root: &Path, deadline: Instant) -> io::Result<()> {
    for directory in fs::read_dir(root)? {
        if check_deadline(deadline).is_err() {
            eprintln!(
                "file-lease: orphan recovery budget exhausted; remaining journals will be retried by a later lease"
            );
            break;
        }
        let directory = directory?;
        if !directory.file_type()?.is_dir() {
            continue;
        }
        let journal = match Journal::read(&directory.path()) {
            Ok(journal) => journal,
            Err(_) => continue,
        };
        if journal.mount != directory.path().join("memory") {
            continue;
        }
        match process_info(journal.keeper) {
            Ok(info) if [info.pbi_start_tvsec, info.pbi_start_tvusec] == journal.birth => continue,
            Err(error) if !matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) => {
                continue;
            }
            _ => {}
        }
        // Recovery skips busy setup or cleanup without delaying other leases.
        if let Err(error) = cleanup_directory_until(&directory.path(), deadline)
            && error.kind() != io::ErrorKind::WouldBlock
        {
            report_cleanup_failure(&directory.path(), &error);
        }
    }
    Ok(())
}

// All cleanup paths share this lock and re-read metadata after acquiring it.
// Never recursively remove the directory: it may still contain a mounted disk.
pub(super) fn cleanup_directory(directory: &Path) -> io::Result<()> {
    cleanup_directory_until(directory, Instant::now() + 2 * TOOL_TIMEOUT)
}

fn cleanup_directory_until(directory: &Path, deadline: Instant) -> io::Result<()> {
    check_deadline(deadline)?;
    if !directory.try_exists()? {
        return Ok(());
    }
    let Some(_lock) = try_setup_lock(directory)? else {
        return if !directory.try_exists()? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "File lease is busy",
            ))
        };
    };
    let mut journal = match Journal::read(directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound && !directory.try_exists()? => {
            return Ok(());
        }
        result => result?,
    };
    if journal.mount != directory.join("memory") {
        return Err(io::Error::other("Invalid file lease mount path"));
    }
    // The worker records this transition while holding the setup lock, before
    // starting hdiutil. A pre-attachment failure has no volume to recover. Old
    // journals and interrupted attachments remain conservative.
    if journal.attachment_started != Some(false) {
        journal.identify_device(directory)?;
        journal.cleanup(deadline)?;
    } else if !journal.device.is_empty() {
        return Err(io::Error::other("Inconsistent RAM-disk attachment state"));
    }
    check_deadline(deadline)?;
    match fs::remove_dir(&journal.mount) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for entry in fs::read_dir(directory)? {
        check_deadline(deadline)?;
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.file_name() != "setup.lock"
            && entry.file_name() != "lease.json"
        {
            fs::remove_file(entry.path())?;
            #[cfg(test)]
            tests::cleanup_checkpoint(
                tests::CleanupStage::MetadataSweep,
                &journal.mount,
                deadline,
            )?;
        }
    }
    // Keep the journal until the sweep finishes, including if its deadline
    // expires after the mount directory has already been removed.
    fs::remove_file(directory.join("lease.json"))?;
    // Remove the lock last. Cleanup opens existing locks only, so contenders
    // cannot create a new lock inode while this one is being removed.
    fs::remove_file(directory.join("setup.lock"))?;
    fs::remove_dir(directory)
}

pub(super) fn retry_cleanup(mut cleanup: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    for attempt in 0..3 {
        match cleanup() {
            Ok(()) => return Ok(()),
            Err(error) if attempt == 2 => return Err(error),
            Err(_) => std::thread::sleep(Duration::from_millis(100 << attempt)),
        }
    }
    unreachable!()
}

pub(super) fn cleanup_with_retry(directory: &Path) -> io::Result<()> {
    let result = retry_cleanup(|| cleanup_directory(directory));
    if let Err(error) = &result {
        report_cleanup_failure(directory, error);
    }
    result
}

fn try_setup_lock(directory: &Path) -> io::Result<Option<fs::File>> {
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("setup.lock"))
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if lock.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Ok(None);
    }
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(Some(lock)),
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}
