//! RAM-volume provisioning and the independent setup guardian.
use super::{
    TOOL_TIMEOUT, command, process_info,
    recovery::{Journal, cleanup_with_retry, device_identity, recover_orphans, runtime_root},
    validate_mount_flags, watch_owner,
};
use crate::{command as tool_command, helper, macos_registry as registry, profiling::Span};
use rustix::fs::statfs as mount_info;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const SETUP_TIMEOUT: Duration = Duration::from_secs(120);

const SETUP_ENV: &str = "_FILE_LEASE_SETUP_DIRECTORY";

pub(crate) fn maybe_run_setup() -> Option<io::Result<()>> {
    let directory = std::env::var_os(SETUP_ENV)?;
    Some(setup_and_guard(Path::new(&directory)))
}

fn setup_and_guard(directory: &Path) -> io::Result<()> {
    let setup = (|| {
        let journal = Journal::read(directory)?;
        // Register before starting attachment, then check birth time to close
        // the PID reuse race between reading the journal and registration.
        let watch = watch_owner(journal.keeper)?;
        let info = process_info(journal.keeper)?;
        if [info.pbi_start_tvsec, info.pbi_start_tvusec] != journal.birth {
            return Err(io::Error::other("File lease keeper identity changed"));
        }
        checkpoint("prepared", directory, "")?;
        setup_device(directory)?;
        Ok(watch)
    })();
    // Publish readiness while setup is still locked. After unlocking, orphan
    // recovery may remove the directory, so the guardian must not create any
    // further metadata files there.
    let signal = (|| -> io::Result<()> {
        let _span = Span::new("guardian.ready_publish");
        let mut ready = tempfile::NamedTempFile::new_in(directory)?;
        write!(
            ready,
            "{}",
            if setup.is_ok() { std::process::id() } else { 0 }
        )?;
        ready
            .persist(directory.join("setup.ready"))
            .map_err(|error| error.error)?;
        Ok(())
    })();
    // stdout was the inherited setup lock. The guardian must release that
    // reference before competing with the keeper or orphan recovery for cleanup.
    let null = fs::File::open("/dev/null")?;
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if let Ok(watch) = &setup
        && signal.is_ok()
    {
        let mut event = libc::pollfd {
            fd: watch.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // Normal cleanup also ends the keeper. Block on its exit instead
            // of periodically waking every idle lease to inspect metadata.
            let result = unsafe { libc::poll(&mut event, 1, -1) };
            if result > 0 && event.revents != 0 {
                break;
            }
            if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break; // Revoke storage if monitoring fails.
            }
        }
    }
    let cleanup = cleanup_with_retry(directory);
    setup.map(|_| ()).and(signal).and(cleanup)
}

fn setup_device(directory: &Path) -> io::Result<()> {
    let _span = Span::new("setup.device");
    let mut journal = Journal::read(directory)?;
    // stdout holds the inherited lock throughout setup. The guardian closes
    // it after setup, before monitoring the keeper and competing for cleanup.
    let id = journal
        .lease_id
        .ok_or_else(|| io::Error::other("Missing file lease identifier"))?;
    // hdiutil inherits the setup lock through stdout. If this worker dies
    // while attachment is in flight, recovery skips it until hdiutil exits.
    // The identifier was journaled before launching either process and is
    // attached to the IOKit device independently of this worker's output.
    let mut attach = Command::new("/usr/bin/hdiutil");
    attach
        .args([
            "attach",
            "-nomount",
            "-drivekey",
            &format!("{}={id}", registry::LEASE_KEY),
            "ram://131072",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::null());
    // The checkpoint is before the bounded wait, so tests can still kill a
    // worker while attachment is running. The tool retains the setup lock.
    journal.attachment_started = Some(true);
    journal.save(directory)?;
    #[cfg(feature = "profiling")]
    let attach_span = Span::new("tool.attach");
    let attach = match attach.process_group(0).spawn() {
        Ok(attach) => attach,
        Err(error) => {
            journal.attachment_started = Some(false);
            journal.save(directory)?;
            return Err(error);
        }
    };
    if let Err(error) = checkpoint("attaching", directory, "") {
        tool_command::terminate(attach);
        return Err(error);
    }
    // A terminated attachment may still be in flight, so its journal stays
    // conservative. Only a tool that exited by itself has a final outcome.
    let status = tool_command::wait_status(attach, TOOL_TIMEOUT)
        .map_err(|error| io::Error::new(error.kind(), format!("hdiutil attach: {error}")))?;
    #[cfg(feature = "profiling")]
    drop(attach_span);
    record_attachment(&mut journal, directory, status)?;
    let mount = journal.mount.clone();
    let uid = unsafe { libc::geteuid() }.to_string();
    let gid = unsafe { libc::getegid() }.to_string();
    checkpoint("attached", directory, &journal.device)?;
    journal.save(directory)?;
    let device = &journal.device;
    command(
        "/sbin/newfs_hfs",
        &[
            "-U".as_ref(),
            uid.as_ref(),
            "-G".as_ref(),
            gid.as_ref(),
            "-M".as_ref(),
            "700".as_ref(),
            "-v".as_ref(),
            "file-lease".as_ref(),
            device.as_ref(),
        ],
    )?;
    checkpoint("formatted", directory, device)?;
    command(
        "/sbin/mount",
        &[
            "-t".as_ref(),
            "hfs".as_ref(),
            "-o".as_ref(),
            "owners,nobrowse,nodev,nosuid,noexec".as_ref(),
            device.as_ref(),
            mount.as_os_str(),
        ],
    )?;
    let info = mount_info(&mount)?;
    validate_mount_flags(info.f_flags)?;
    checkpoint("mounted", directory, device)?;
    journal.filesystem = Some(fs::metadata(&mount)?.dev());
    if !journal.validate_mount()? {
        return Err(io::Error::other("RAM-disk mount did not appear"));
    }
    let metadata = fs::metadata(&mount)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::other(
            "RAM-disk root permissions are not private",
        ));
    }
    journal.save(directory)?;
    Ok(())
}

/// Record the outcome of an attachment command that has exited.
pub(super) fn record_attachment(
    journal: &mut Journal,
    directory: &Path,
    status: std::process::ExitStatus,
) -> io::Result<()> {
    let id = journal
        .lease_id
        .ok_or_else(|| io::Error::other("Missing file lease identifier"))?;
    if let Some(device) = registry::device(id)? {
        journal.device_identity = Some(device_identity(&fs::metadata(&device)?));
        journal.device = device;
        if status.success() {
            return Ok(());
        }
        // A failed command that still attached our device leaves it
        // identified, so cleanup can detach it.
        journal.save(directory)?;
        return Err(tool_command::exit_error(status));
    }
    // Fail closed on systems without drive-key support, but detach the
    // device reported directly by our completed attachment command. If that
    // detach fails, the journal keeps the attachment unresolved.
    let output = fs::read_to_string(directory.join("setup.lock"))?;
    if let Some(device) = output.split_whitespace().find(|value| {
        value.strip_prefix("/dev/disk").is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
    }) {
        command(
            "/usr/bin/hdiutil",
            &["detach".as_ref(), "-force".as_ref(), device.as_ref()],
        )?;
    }
    // No device carries our identifier, so nothing remains to detach.
    // Without this, recovery could never identify the attachment and would
    // keep the lease directory forever.
    journal.attachment_started = Some(false);
    journal.save(directory)?;
    Err(if status.success() {
        io::Error::other("RAM-disk lease identifier was not registered")
    } else {
        tool_command::exit_error(status)
    })
}

fn checkpoint(stage: &str, directory: &Path, device: &str) -> io::Result<()> {
    #[cfg(test)]
    if let Some(test) = std::env::var_os("_FILE_LEASE_SETUP_TEST") {
        let test = PathBuf::from(test);
        if fs::read_to_string(test.join("stage"))? == stage {
            fs::write(
                test.join("ready"),
                serde_json::to_vec(&(directory, device, std::process::id()))?,
            )?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !test.join("resume").exists() {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::other("Setup checkpoint timed out"));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    let _ = (stage, directory, device);
    Ok(())
}

pub(super) struct Volume {
    pub(super) directory: PathBuf,
    pub(super) journal: Journal,
}
impl Volume {
    /// Pin and verify the mounted root before accessing payloads. A path-based
    /// check followed by a path-based write could reach the disk-backed mount
    /// directory if the RAM volume were unmounted between those operations.
    pub(super) fn open_root(&self) -> io::Result<fs::File> {
        let _span = Span::new("root.validate");
        let (root, info) = self
            .journal
            .mounted_root()?
            .ok_or_else(|| io::Error::other("RAM-disk storage is no longer mounted"))?;
        if !self.journal.validate_device()? {
            return Err(io::Error::other("RAM-disk storage device disappeared"));
        }
        let metadata = root.metadata()?;
        if Some(metadata.dev()) != self.journal.filesystem
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o700
        {
            return Err(io::Error::other(
                "RAM-disk storage identity or permissions changed",
            ));
        }
        validate_mount_flags(info.f_flags)?;
        Ok(root)
    }

    pub(super) fn create() -> io::Result<Self> {
        let _span = Span::new("volume.create");
        let root = runtime_root()?;
        recover_orphans(&root)?;
        Self::create_in(&root)
    }

    pub(super) fn create_in(root: &Path) -> io::Result<Self> {
        let directory = tempfile::Builder::new().prefix("lease-").tempdir_in(root)?;
        let mount = directory.path().join("memory");
        fs::DirBuilder::new().mode(0o700).create(&mount)?;
        let process = process_info(std::process::id())?;
        let journal = Journal {
            keeper: std::process::id(),
            birth: [process.pbi_start_tvsec, process.pbi_start_tvusec],
            lease_id: Some(uuid::Uuid::new_v4()),
            device: String::new(),
            mount: mount.clone(),
            filesystem: None,
            device_identity: None,
            attachment_started: Some(false),
        };
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.path().join("setup.lock"))?;
        loop {
            match lock.lock() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        }
        journal.save(directory.path())?;
        // After attachment can start, metadata must survive every error path.
        let directory = directory.keep();
        let setup = (|| -> io::Result<Journal> {
            let _span = Span::new("guardian.wait_ready");
            let mut worker = Command::new(std::env::current_exe()?);
            worker
                .env(SETUP_ENV, &directory)
                .stdin(Stdio::null())
                .stdout(Stdio::from(lock.try_clone()?))
                .stderr(Stdio::null())
                .process_group(0);
            #[cfg(test)]
            worker.args(["--exact", "tests::keeper_entry", "--nocapture"]);
            let mut worker = worker.spawn()?;
            let deadline = Instant::now() + SETUP_TIMEOUT;
            loop {
                if let Some(status) = worker.try_wait()? {
                    return Err(io::Error::other(format!(
                        "RAM-disk guardian exited during setup: {status}"
                    )));
                }
                if let Ok(ready) = fs::read_to_string(directory.join("setup.ready"))
                    && let Ok(pid) = ready.parse::<u32>()
                {
                    helper::reap_when_exited(worker);
                    if pid == 0 {
                        return Err(io::Error::other("RAM-disk setup failed"));
                    }
                    return Journal::read(&directory);
                }
                if Instant::now() >= deadline {
                    tool_command::terminate(worker);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "RAM-disk setup timed out",
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })();
        drop(lock);
        match setup {
            Ok(journal) => Ok(Self { directory, journal }),
            Err(error) => {
                let _ = cleanup_with_retry(&directory);
                Err(error)
            }
        }
    }
}
