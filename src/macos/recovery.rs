//! Durable journals, mount validation, and bounded recovery of dead keepers.
#[cfg(test)]
use super::tests;
use super::{TOOL_TIMEOUT, c_string, command_until, process_info, unlink_payload};
use crate::{macos_registry as registry, profiling::Span};
use rustix::fs::fstatfs;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(super) fn device_identity(metadata: &fs::Metadata) -> [u64; 4] {
    [
        metadata.ino(),
        metadata.rdev(),
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ]
}

#[derive(Serialize, Deserialize)]
pub(super) struct Journal {
    pub(super) keeper: u32,
    pub(super) birth: [u64; 2],
    #[serde(default)]
    pub(super) lease_id: Option<uuid::Uuid>,
    pub(super) device: String,
    pub(super) mount: PathBuf,
    pub(super) filesystem: Option<u64>,
    pub(super) device_identity: Option<[u64; 4]>,
    // None means an older journal with unknown attachment state. Only an
    // explicit false permits cleanup without identifying an attached device.
    #[serde(default)]
    pub(super) attachment_started: Option<bool>,
}

impl Journal {
    pub(super) fn identify_device(&mut self, directory: &Path) -> io::Result<()> {
        if self.device.is_empty() {
            let device = self
                .lease_id
                .map(registry::device)
                .transpose()?
                .flatten()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "RAM-disk attachment has not been identified yet",
                    )
                })?;
            self.device_identity = Some(device_identity(&fs::metadata(&device)?));
            self.device = device;
            self.save(directory)?;
        }
        Ok(())
    }

    pub(super) fn read(directory: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("lease.json"))?;
        if file.metadata()?.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other("File lease journal is not user-owned"));
        }
        serde_json::from_reader(file.take(16384)).map_err(io::Error::other)
    }

    pub(super) fn save(&self, directory: &Path) -> io::Result<()> {
        let _span = Span::new("journal.save");
        // Keep the previous complete record if a writer dies during an update.
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut file, self).map_err(io::Error::other)?;
        {
            let _span = Span::new("journal.file_sync");
            file.as_file().sync_all()?;
        }
        file.persist(directory.join("lease.json"))
            .map_err(|error| error.error)?;
        let _span = Span::new("journal.directory_sync");
        fs::File::open(directory)?.sync_all()
    }

    pub(super) fn validate_mount(&self) -> io::Result<bool> {
        Ok(self.mounted_root()?.is_some())
    }

    pub(super) fn mounted_root(&self) -> io::Result<Option<(fs::File, rustix::fs::StatFs)>> {
        let _span = Span::new("mount.inspect");
        if !self.device.strip_prefix("/dev/disk").is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            return Err(io::Error::other(
                "Invalid RAM-disk device in file lease journal",
            ));
        }
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.mount)?;
        let info = fstatfs(&root)?;
        if c_string(&info.f_mntonname) != self.mount.as_os_str().as_bytes() {
            return Ok(None);
        }
        if c_string(&info.f_fstypename) != b"hfs"
            || c_string(&info.f_mntfromname) != self.device.as_bytes()
            || self.filesystem.is_some_and(|device| {
                root.metadata()
                    .map_or(true, |metadata| metadata.dev() != device)
            })
            || info.f_owner != unsafe { libc::geteuid() }
        {
            return Err(io::Error::other(
                "RAM-disk identity changed; refusing file lease cleanup",
            ));
        }
        Ok(Some((root, info)))
    }

    pub(super) fn validate_device(&self) -> io::Result<bool> {
        let _span = Span::new("device.validate");
        if self.device.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Cannot discard an unfinished RAM-disk attachment",
            ));
        }
        let metadata = match fs::metadata(&self.device) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if let Some(id) = self.lease_id {
            match registry::device(id)? {
                // Disk numbers are reused eagerly. With no device carrying our
                // identifier, this node belongs to another attachment and ours
                // is already gone.
                None => return Ok(false),
                Some(device) if device != self.device => {
                    return Err(io::Error::other("RAM-disk lease identifier changed"));
                }
                Some(_) => {}
            }
        }
        if Some(device_identity(&metadata)) != self.device_identity {
            return Err(io::Error::other("RAM-disk device identity changed"));
        }
        Ok(true)
    }
}

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
