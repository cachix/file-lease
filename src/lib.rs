#![doc = include_str!("../README.md")]

use std::{
    ffi::OsStr,
    fmt, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
mod command;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod helper;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_registry;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "unix.rs"]
mod platform;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod profiling;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod protocol;
#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
mod volume_pool;

/// A cloneable lease on memory-backed files.
///
/// Clones share the same storage. By default, dropping the final clone
/// releases every file and waits for the helper to finish cleanup. Call
/// [`Self::handoff`] before exporting paths or calling `exec` to retain them
/// until the owner process exits.
///
/// The owner is identified by PID. Choose a process that is alive and stays
/// alive while files are needed: if the PID exits and is reused before the
/// first allocation, the helper watches the unrelated process instead.
#[derive(Clone)]
pub struct FileLease {
    owner: u32,
    executable: Option<PathBuf>,
    keeper: Arc<Mutex<Option<platform::Keeper>>>,
}

impl fmt::Debug for FileLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileLease")
            .field("owner", &self.owner)
            .field("executable", &self.executable)
            .finish_non_exhaustive()
    }
}

/// A file created by a [`FileLease`].
///
/// This is a name, not a lifetime: the file stays readable until it is
/// released or its lease ends, whether or not this value is kept.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LeasedFile {
    path: PathBuf,
}

impl LeasedFile {
    /// The path consumers open.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Convert into the path consumers open.
    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

impl AsRef<Path> for LeasedFile {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for LeasedFile {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

/// Options for [`FileLease::materialize_with`].
#[derive(Clone, Debug, Default)]
pub struct FileOptions {
    suffix: Option<String>,
    filename: Option<String>,
}

impl FileOptions {
    /// Default options: no suffix.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request a file name suffix such as `.pem`. macOS preserves it and
    /// Linux omits it. See [`ErrorKind::InvalidSuffix`] for accepted values.
    pub fn suffix(mut self, suffix: impl Into<String>) -> Self {
        self.suffix = Some(suffix.into());
        self
    }

    /// Expose the file through a symlink with this exact filename in a new
    /// private temporary directory. Works on Linux and macOS. The contents
    /// remain memory-backed; only the link and directory use the filesystem.
    ///
    /// Names accept 1 to 255 ASCII letters, digits, dots, underscores, or
    /// hyphens, except `.` and `..`. Cannot be combined with [`Self::suffix`].
    /// The helper removes the directory on release or lease cleanup, including
    /// after handoff. Killing the helper may leave a dangling link behind.
    pub fn filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }
}

impl FileLease {
    /// Create a lease owned by `owner`. Resources are allocated lazily by
    /// the first [`Self::materialize`] call, which also validates the owner.
    pub fn new(owner: u32) -> Self {
        Self {
            owner,
            executable: None,
            keeper: Arc::new(Mutex::new(None)),
        }
    }

    /// Select a matching helper executable in an embedding application's
    /// deployment. Set this before cloning the lease.
    pub fn with_helper_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    /// Store `bytes` in a new read-only, memory-backed file.
    pub fn materialize(&self, bytes: &[u8]) -> Result<LeasedFile> {
        self.materialize_with(bytes, &FileOptions::default())
    }

    /// Store `bytes` in a new read-only, memory-backed file with `options`.
    pub fn materialize_with(&self, bytes: &[u8], options: &FileOptions) -> Result<LeasedFile> {
        if let Some(filename) = options.filename.as_deref() {
            if options.suffix.is_some() {
                return Err(Error::from_io(
                    tagged(
                        ErrorKind::InvalidFilename,
                        "Filename and suffix cannot be combined",
                    ),
                    ErrorKind::InvalidFilename,
                ));
            }
            validate_filename(filename)
                .map_err(|error| Error::from_io(error, ErrorKind::InvalidFilename))?;
        }
        let mut keeper = self.lock()?;
        if keeper.is_none() {
            *keeper = Some(
                platform::Keeper::spawn(self.owner, self.executable.as_deref())
                    .map_err(|error| Error::from_io(error, ErrorKind::Helper))?,
            );
        }
        let path = keeper
            .as_mut()
            .unwrap()
            .materialize(
                bytes,
                options.suffix.as_deref().unwrap_or(""),
                options.filename.as_deref(),
            )
            .map_err(|error| Error::from_io(error, ErrorKind::Io))?;
        Ok(LeasedFile { path })
    }

    /// Revoke one file created by this lease without releasing its other
    /// files. Existing readers may retain the contents until they close them.
    pub fn release(&self, file: &LeasedFile) -> Result<()> {
        self.lock()?
            .as_mut()
            .ok_or_else(|| tagged(ErrorKind::Release, "Path does not belong to this lease"))
            .and_then(|keeper| keeper.release(&file.path))
            .map_err(|error| Error::from_io(error, ErrorKind::Io))
    }

    /// Keep existing files until the owner exits. After handoff, no clone can
    /// allocate more files. Handing off an empty lease has no effect.
    pub fn handoff(&self) -> Result<()> {
        self.handoff_inner(None)
    }

    /// Transfer existing files to another live process, which then owns them.
    pub fn handoff_to(&self, owner: u32) -> Result<()> {
        self.handoff_inner(Some(owner))
    }

    fn handoff_inner(&self, owner: Option<u32>) -> Result<()> {
        if let Some(keeper) = self.lock()?.as_mut() {
            keeper
                .handoff_to(owner)
                .map_err(|error| Error::from_io(error, ErrorKind::Io))?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<platform::Keeper>>> {
        self.keeper.lock().map_err(|_| {
            Error::from_io(io::Error::other("File lease lock poisoned"), ErrorKind::Io)
        })
    }
}

fn validate_filename(filename: &str) -> io::Result<()> {
    if filename.is_empty()
        || filename.len() > 255
        || matches!(filename, "." | "..")
        || !filename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(tagged(
            ErrorKind::InvalidFilename,
            "Invalid file lease filename",
        ));
    }
    Ok(())
}

/// Run the internal helper if this process was started as one.
///
/// Returns `None` for ordinary invocations. Call this before application
/// argument parsing when hosting the helper in an existing executable, and
/// exit with the returned status. Otherwise install the matching `file-lease`
/// binary alongside the application.
pub fn maybe_run_helper() -> Option<i32> {
    platform::maybe_run_keeper()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;
    pub(super) struct Keeper;
    impl Keeper {
        pub(super) fn spawn(_: u32, _: Option<&Path>) -> io::Result<Self> {
            Err(tagged(
                ErrorKind::Unsupported,
                "Process-owned file delivery requires Linux or macOS",
            ))
        }
        pub(super) fn materialize(
            &mut self,
            _: &[u8],
            _: &str,
            _: Option<&str>,
        ) -> io::Result<PathBuf> {
            unreachable!()
        }
        pub(super) fn handoff_to(&mut self, _: Option<u32>) -> io::Result<()> {
            unreachable!()
        }
        pub(super) fn release(&mut self, _: &Path) -> io::Result<()> {
            unreachable!()
        }
    }
    pub(super) fn maybe_run_keeper() -> Option<i32> {
        None
    }
}

mod error;
mod process;
use error::tagged;
pub use error::{Error, ErrorKind, Result};
pub use process::ProcessInfo;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    #[test]
    fn keeper_entry() {
        if std::env::var_os("_FILE_LEASE_TEST_OLD_PROTOCOL").is_some() {
            use std::{io::Write, os::fd::FromRawFd, os::unix::net::UnixStream};
            let fd = std::env::var("_FILE_LEASE_KEEPER_SOCKET")
                .unwrap()
                .parse()
                .unwrap();
            let mut stream = unsafe { UnixStream::from_raw_fd(fd) };
            stream.write_all(b"SFL1").unwrap();
            loop {
                std::thread::park();
            }
        }
        if let Some(code) = maybe_run_helper() {
            std::process::exit(code);
        }
    }

    #[test]
    fn old_helper_child() {
        if std::env::var_os("_FILE_LEASE_TEST_OLD_PROTOCOL").is_none() {
            return;
        }
        let error = FileLease::new(std::process::id())
            .materialize(b"must-not-reach-old-helper")
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Helper);
        assert!(
            error.to_string().contains("Incompatible file lease helper"),
            "{error}"
        );
    }

    #[test]
    fn old_helpers_are_rejected_before_payload_delivery() {
        assert!(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::old_helper_child", "--nocapture"])
                .env("_FILE_LEASE_TEST_OLD_PROTOCOL", "1")
                .status()
                .unwrap()
                .success()
        );
    }

    fn revoked(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !path.exists(),
            "secret survived its lease: {}",
            path.display()
        );
    }

    #[test]
    fn named_files_preserve_names_and_have_independent_private_directories() {
        use std::os::unix::fs::PermissionsExt;

        let lease = FileLease::new(std::process::id());
        let options = FileOptions::new().filename("credentials.json");
        let first = lease.materialize_with(b"first", &options).unwrap();
        let second = lease.materialize_with(b"second", &options).unwrap();
        assert_eq!(first.path().file_name().unwrap(), "credentials.json");
        assert_ne!(first.path().parent(), second.path().parent());
        assert!(first.path().is_absolute());
        assert!(first.path().is_symlink());
        assert_eq!(
            std::fs::metadata(first.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let target = std::fs::read_link(&first).unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        let clone = lease.clone();
        drop(lease);
        clone.release(&first).unwrap();
        assert!(!first.path().parent().unwrap().exists());
        assert!(!target.exists());
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        let third = clone.materialize_with(b"third", &options).unwrap();
        assert_ne!(first, third);
        drop(clone);
        assert!(!second.path().parent().unwrap().exists());
        assert!(!third.path().parent().unwrap().exists());
    }

    #[test]
    fn named_files_follow_handoff_and_remove_links_on_owner_exit() {
        let mut owner = Command::new("sleep").arg("60").spawn().unwrap();
        let lease = FileLease::new(std::process::id());
        let file = lease
            .materialize_with(b"handed off", &FileOptions::new().filename("config.toml"))
            .unwrap();
        lease.handoff_to(owner.id()).unwrap();
        drop(lease);
        assert_eq!(std::fs::read(&file).unwrap(), b"handed off");
        owner.kill().unwrap();
        owner.wait().unwrap();
        let directory = file.path().parent().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while directory.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!directory.exists());
    }

    #[test]
    fn invalid_filenames_fail_before_starting_a_helper() {
        let lease = FileLease::new(std::process::id());
        for filename in [
            "",
            ".",
            "..",
            "../outside",
            "/absolute",
            "a/b",
            "a\\b",
            "a\0b",
            "é",
            &"a".repeat(256),
        ] {
            let error = lease
                .materialize_with(b"payload", &FileOptions::new().filename(filename))
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidFilename, "{filename:?}");
            assert!(lease.keeper.lock().unwrap().is_none());
            assert_eq!(io::Error::from(error).kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(
            lease
                .materialize_with(
                    b"payload",
                    &FileOptions::new().filename("key.pem").suffix(".pem")
                )
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidFilename
        );
        let file = lease
            .materialize_with(b"valid", &FileOptions::new().filename("a".repeat(255)))
            .unwrap();
        assert_eq!(std::fs::read(file).unwrap(), b"valid");
    }

    #[test]
    fn helper_crash_revokes_named_file_contents() {
        let lease = FileLease::new(std::process::id());
        let file = lease
            .materialize_with(b"crash fixture", &FileOptions::new().filename("secret.pem"))
            .unwrap();
        let pid = lease.keeper.lock().unwrap().as_ref().unwrap().pid();
        assert_eq!(unsafe { libc::kill(pid as _, libc::SIGKILL) }, 0);
        drop(lease);
        revoked(file.path());
        // SIGKILL bypasses the helper's directory cleanup. Only metadata remains.
        assert!(file.path().is_symlink());
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(file.path().parent().unwrap()).unwrap();
    }

    #[test]
    fn files_are_read_only_and_released_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let lease = FileLease::new(std::process::id());
        let path = lease.materialize(b"public-test-key").unwrap().into_path();
        assert_eq!(std::fs::read(&path).unwrap(), b"public-test-key");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o400
        );
        #[cfg(target_os = "linux")]
        {
            use std::io::Write;
            assert!(
                std::fs::read_link(&path)
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("/memfd:file-lease")
            );
            if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(&path) {
                assert!(file.write_all(b"overwrite").is_err());
            }
        }
        drop(lease);
        revoked(&path);
    }

    #[test]
    fn concurrent_clones_share_files_until_the_last_clone_drops() {
        use std::{sync::Barrier, thread};

        let lease = FileLease::new(std::process::id());
        let barrier = Arc::new(Barrier::new(4));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let lease = lease.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    let file = lease.materialize(b"clone-payload").unwrap();
                    (lease, file)
                })
            })
            .collect();
        let mut live: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        let (last, last_file) = live.pop().unwrap();
        let files: Vec<_> = live.into_iter().map(|(_, file)| file).collect();
        drop(lease);
        for file in files.iter().chain([&last_file]) {
            assert_eq!(std::fs::read(file).unwrap(), b"clone-payload");
        }
        last.release(&files[0]).unwrap();
        assert!(!files[0].path().exists());
        assert_eq!(std::fs::read(&last_file).unwrap(), b"clone-payload");
        drop(last);
        for file in files.iter().chain([&last_file]) {
            revoked(file.path());
        }
    }

    #[test]
    fn owner_sigkill_releases_handed_off_files_with_open_readers() {
        let mut owner = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let lease = FileLease::new(owner.id());
        let path = lease.materialize(b"leased-key").unwrap().into_path();
        let reader = std::fs::File::open(&path).unwrap();
        lease.handoff().unwrap();
        lease.handoff().unwrap();
        assert_eq!(
            lease.materialize(b"too-late").unwrap_err().kind(),
            ErrorKind::HandedOff
        );
        drop(lease);
        assert_eq!(std::fs::read(&path).unwrap(), b"leased-key");
        owner.kill().unwrap();
        owner.wait().unwrap();
        revoked(&path);
        drop(reader);
    }

    #[test]
    fn daemon_handoff_survives_launcher_exit() {
        let mut launcher = Command::new("sleep").arg("60").spawn().unwrap();
        let mut daemon = Command::new("sleep").arg("60").spawn().unwrap();
        let lease = FileLease::new(launcher.id());
        let path = lease.materialize(b"daemon-key").unwrap().into_path();
        lease.handoff().unwrap();
        lease.handoff_to(daemon.id()).unwrap();
        drop(lease);
        launcher.kill().unwrap();
        launcher.wait().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"daemon-key");
        daemon.kill().unwrap();
        daemon.wait().unwrap();
        revoked(&path);
    }

    #[test]
    fn separate_leases_do_not_overwrite_or_revoke_each_other() {
        let first = FileLease::new(std::process::id());
        let second = FileLease::new(std::process::id());
        let one = first.materialize(b"one").unwrap().into_path();
        let two = second.materialize(b"two").unwrap().into_path();
        assert_ne!(one, two);
        drop(second);
        revoked(&two);
        assert_eq!(std::fs::read(one).unwrap(), b"one");
    }

    #[test]
    fn failed_handoff_preserves_files_allocation_and_original_owner_watch() {
        let mut owner = Command::new("sleep").arg("60").spawn().unwrap();
        let mut missing = Command::new("true").spawn().unwrap();
        let missing_pid = missing.id();
        missing.wait().unwrap();
        let lease = FileLease::new(owner.id());
        let first = lease.materialize(b"keep").unwrap().into_path();
        assert_eq!(
            lease.handoff_to(missing_pid).unwrap_err().kind(),
            ErrorKind::Handoff
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"keep");
        let second = lease.materialize(b"still allocating").unwrap().into_path();
        lease.handoff().unwrap();
        // A failed transfer must also preserve a previously handed-off lease.
        assert!(lease.handoff_to(missing_pid).is_err());
        drop(lease);
        assert_eq!(std::fs::read(&first).unwrap(), b"keep");
        assert_eq!(std::fs::read(&second).unwrap(), b"still allocating");
        owner.kill().unwrap();
        owner.wait().unwrap();
        revoked(&first);
        revoked(&second);
    }

    #[test]
    fn killed_keeper_releases_storage_without_another_lease() {
        let lease = FileLease::new(std::process::id());
        let path = lease.materialize(b"crash-key").unwrap().into_path();
        let pid = lease.keeper.lock().unwrap().as_ref().unwrap().pid();
        assert_eq!(unsafe { libc::kill(pid as _, libc::SIGKILL) }, 0);
        drop(lease);
        revoked(&path);
    }

    #[test]
    fn invalid_requests_fail_closed_with_typed_errors() {
        let kind = |result: Result<LeasedFile>| result.unwrap_err().kind();
        assert_eq!(
            kind(FileLease::new(0).materialize(b"key")),
            ErrorKind::InvalidOwner
        );
        let mut exited = Command::new("true").spawn().unwrap();
        let exited_pid = exited.id();
        exited.wait().unwrap();
        assert_eq!(
            kind(FileLease::new(exited_pid).materialize(b"key")),
            ErrorKind::InvalidOwner
        );
        let lease = FileLease::new(std::process::id());
        assert_eq!(
            kind(lease.materialize_with(b"key", &FileOptions::new().suffix("/outside"))),
            ErrorKind::InvalidSuffix
        );
        let oversized = vec![0; protocol::MAX_SECRET_BYTES as usize + 1];
        assert_eq!(kind(lease.materialize(&oversized)), ErrorKind::TooLarge);
        assert_eq!(
            kind(
                FileLease::new(std::process::id())
                    .with_helper_executable("/nonexistent/file-lease")
                    .materialize(b"key")
            ),
            ErrorKind::Helper
        );
        // A rejected request leaves the helper usable.
        lease.materialize(b"key").unwrap();
    }

    #[test]
    fn errors_convert_into_io_errors() {
        fn propagate() -> io::Result<LeasedFile> {
            Ok(FileLease::new(0).materialize(b"key")?)
        }
        assert_eq!(propagate().unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn releasing_one_file_preserves_others_and_never_reuses_its_path() {
        let lease = FileLease::new(std::process::id());
        let first = lease
            .materialize_with(b"first", &FileOptions::new().suffix(".pem"))
            .unwrap();
        let second = lease
            .materialize_with(b"second", &FileOptions::new().suffix(".der"))
            .unwrap();
        #[cfg(target_os = "linux")]
        assert!(first.path().extension().is_none());
        #[cfg(target_os = "macos")]
        assert_eq!(first.path().extension().unwrap(), "pem");
        // A file from another lease is not this lease's to revoke.
        let other = FileLease::new(std::process::id());
        let foreign = other.materialize(b"foreign").unwrap();
        assert_eq!(
            lease.release(&foreign).unwrap_err().kind(),
            ErrorKind::Release
        );
        assert_eq!(
            FileLease::new(std::process::id())
                .release(&foreign)
                .unwrap_err()
                .kind(),
            ErrorKind::Release
        );
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
        lease.release(&first).unwrap();
        assert!(!first.path().exists());
        assert_eq!(
            lease.release(&first).unwrap_err().kind(),
            ErrorKind::Release
        );
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        let third = lease.materialize(b"third").unwrap();
        assert_ne!(first, third);
        assert!(!first.path().exists());
        drop(lease);
        revoked(second.path());
        revoked(third.path());
    }

    #[test]
    fn process_metadata_identifies_the_current_executable() {
        let info = ProcessInfo::read(std::process::id()).unwrap();
        assert!(info.executed);
        assert_eq!(info.executable, std::env::current_exe().unwrap());
        assert!(!info.arguments.is_empty());
    }
}
