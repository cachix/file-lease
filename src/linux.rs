use memmap2::{Advice, MmapOptions, MmapRaw};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Write},
    os::fd::{AsRawFd, OwnedFd},
    path::PathBuf,
};

pub(super) fn watch_owner(pid: u32) -> io::Result<OwnedFd> {
    let pid = rustix::process::Pid::from_raw(pid as i32)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid owner PID"))?;
    Ok(rustix::process::pidfd_open(
        pid,
        rustix::process::PidfdFlags::empty(),
    )?)
}

#[derive(Default)]
pub(super) struct Storage {
    files: HashMap<PathBuf, LockedFile>,
    next_fd: i32,
    retired: Vec<i32>,
}
impl Storage {
    pub(super) fn materialize(&mut self, bytes: &[u8], _suffix: &str) -> io::Result<PathBuf> {
        // Protect revoked numbers before creating either the memfd or its
        // mapping reader. Moving only the final descriptor is too late: a
        // reader could reopen an intermediate descriptor and retain the new
        // file before its payload is written. Proc-fd sockets cannot be opened
        // as files. Release these reservations when allocation finishes so
        // revoked paths remain absent between operations.
        let _reservations = self.reserve_retired()?;
        let mut file = anonymous_file(bytes)?;
        // Never reuse a revoked pathname for a later allocation in this helper.
        file.file = File::from(rustix::io::fcntl_dupfd_cloexec(
            &file.file,
            self.next_fd.max(3),
        )?);
        self.next_fd = file
            .file
            .as_raw_fd()
            .checked_add(1)
            .ok_or_else(|| io::Error::other("File lease descriptor space exhausted"))?;
        let path = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            file.file.as_raw_fd()
        ));
        self.files.insert(path.clone(), file);
        Ok(path)
    }

    fn reserve_retired(&self) -> io::Result<Vec<OwnedFd>> {
        if self.retired.is_empty() {
            return Ok(Vec::new());
        }
        let placeholder: OwnedFd = std::os::unix::net::UnixDatagram::unbound()?.into();
        let mut reservations = Vec::new();
        for &fd in &self.retired {
            // Duplication fills the lowest free slot at or above fd. If a
            // watcher or the placeholder already occupies it, discard the copy.
            let reservation = rustix::io::fcntl_dupfd_cloexec(&placeholder, fd)?;
            if reservation.as_raw_fd() == fd {
                reservations.push(reservation);
            }
        }
        reservations.push(placeholder);
        Ok(reservations)
    }
    pub(super) fn release(&mut self, path: &std::path::Path) -> io::Result<()> {
        let file = self
            .files
            .remove(path)
            .ok_or_else(|| io::Error::other("Path does not belong to this lease"))?;
        self.retired.push(file.file.as_raw_fd());
        Ok(())
    }

    #[cfg(all(test, feature = "shared-volumes"))]
    pub(super) fn contains(&self, path: &std::path::Path) -> bool {
        self.files.contains_key(path)
    }

    #[cfg(all(test, feature = "shared-volumes"))]
    pub(super) fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub(super) fn cleanup(&mut self) -> io::Result<()> {
        self.retired
            .extend(self.files.values().map(|file| file.file.as_raw_fd()));
        self.files.clear();
        Ok(())
    }
}

// Close the descriptor before unmapping and unlocking its pages on drop.
struct LockedFile {
    file: File,
    _mapping: Option<MmapRaw>,
}

fn locked_mapping(file: &File, length: usize) -> io::Result<MmapRaw> {
    // A read-only descriptor prevents VM_MAYWRITE on this shared mapping,
    // allowing F_SEAL_WRITE while the mapping remains locked and alive.
    let reader = File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    #[cfg(test)]
    tests::mapping_checkpoint();
    // Use a raw mapping: the file is written after mapping and locking, so no
    // Rust references into its contents may be created during initialization.
    let mapping = MmapOptions::new().len(length).map_raw_read_only(&reader)?;
    mapping.advise(Advice::DontDump)?;
    // Populate and lock zero-filled pages before any secret bytes reach the
    // memfd. Dropping the mapping also unlocks pages, including on error paths.
    mapping.lock().map_err(|error| {
        io::Error::other(format!(
            "Could not lock secret file pages into RAM; check RLIMIT_MEMLOCK: {error}"
        ))
    })?;
    Ok(mapping)
}

fn anonymous_file(bytes: &[u8]) -> io::Result<LockedFile> {
    use rustix::fs::{MemfdFlags, Mode, SealFlags, fchmod, fcntl_add_seals, memfd_create};

    let fd = memfd_create(
        "file-lease",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    let file = File::from(fd);
    fchmod(&file, Mode::RUSR)?;
    file.set_len(bytes.len() as u64)?;
    let mut file = LockedFile {
        file,
        _mapping: None,
    };
    if !bytes.is_empty() {
        file._mapping = Some(locked_mapping(&file.file, bytes.len())?);
    }
    file.file.write_all(bytes)?;
    fcntl_add_seals(
        &file.file,
        SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
    )?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use crate::FileLease;
    use std::{fs, process::Command};

    thread_local! {
        static REVOKED_PATH: std::cell::RefCell<Option<std::path::PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn mapping_checkpoint() {
        REVOKED_PATH.with_borrow(|path| {
            if let Some(path) = path {
                assert!(fs::File::open(path).is_err(), "revoked path reopened");
            }
        });
    }

    #[test]
    fn revoked_paths_cannot_capture_intermediate_allocation_descriptors() {
        let mut storage = super::Storage::default();
        let revoked = storage.materialize(b"original", "").unwrap();
        storage.release(&revoked).unwrap();
        assert!(!revoked.exists());
        // Reopen at the point where the new memfd's read-only descriptor is
        // live, before its payload is written. Retaining that descriptor would
        // expose the new bytes even after the helper moves its final handle.
        REVOKED_PATH.with_borrow_mut(|path| *path = Some(revoked.clone()));
        let next = storage.materialize(b"new secret", "").unwrap();
        REVOKED_PATH.with_borrow_mut(|path| *path = None);
        assert_ne!(revoked, next);
        assert!(!revoked.exists());
        assert_eq!(fs::read(next).unwrap(), b"new secret");
    }

    fn page_size() -> usize {
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(size > 0);
        size as usize
    }

    fn keeper_pid(lease: &FileLease) -> u32 {
        lease.keeper.lock().unwrap().as_ref().unwrap().pid()
    }

    fn locked_bytes(pid: u32) -> usize {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        status
            .lines()
            .find(|line| line.starts_with("VmLck:"))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<usize>()
            .unwrap()
            * 1024
    }

    #[test]
    fn lease_pages_are_locked_excluded_from_dumps_and_unlocked_on_release() {
        let lease = FileLease::new(std::process::id());
        let first = lease.materialize(b"first", None).unwrap();
        let pid = keeper_pid(&lease);
        let page = page_size();
        assert_eq!(locked_bytes(pid), page);
        let bytes = vec![b'x'; page + 1];
        let second = lease.materialize(&bytes, None).unwrap();
        assert_eq!(fs::read(&second).unwrap(), bytes);
        assert_eq!(locked_bytes(pid), page * 3);
        let maps = fs::read_to_string(format!("/proc/{pid}/smaps")).unwrap();
        let flags = maps
            .split_once("/memfd:file-lease")
            .unwrap()
            .1
            .lines()
            .find(|line| line.starts_with("VmFlags:"))
            .unwrap();
        for flag in ["lo", "dd"] {
            assert!(
                flags.split_whitespace().any(|value| value == flag),
                "missing {flag}: {flags}"
            );
        }
        lease.release(&first).unwrap();
        assert_eq!(locked_bytes(pid), page * 2);
        lease.release(&second).unwrap();
        assert_eq!(locked_bytes(pid), 0);
        let empty = lease.materialize(b"", None).unwrap();
        assert!(fs::read(empty).unwrap().is_empty());
        assert_eq!(locked_bytes(pid), 0);
    }

    #[test]
    fn memlock_limit_child() {
        let Ok(mode) = std::env::var("_FILE_LEASE_MEMLOCK_TEST") else {
            return;
        };
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let caps = status
            .lines()
            .find(|line| line.starts_with("CapEff:"))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        if u64::from_str_radix(caps, 16).unwrap() & (1 << 14) != 0 {
            // Privileged processes deliberately bypass this kernel limit.
            eprintln!("Skipping RLIMIT_MEMLOCK assertions: CAP_IPC_LOCK is effective");
            return;
        }
        let budget = if mode == "zero" { 0 } else { page_size() };
        let limit = libc::rlimit {
            rlim_cur: budget as _,
            rlim_max: budget as _,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &limit) }, 0);
        let lease = FileLease::new(std::process::id());
        if budget == 0 {
            let error = lease
                .materialize(b"must-not-be-pageable", None)
                .unwrap_err();
            assert!(error.to_string().contains("RLIMIT_MEMLOCK"), "{error}");
            // Failure must leave the helper usable and the reply stream aligned.
            let empty = lease.materialize(b"", None).unwrap();
            assert!(fs::read(empty).unwrap().is_empty());
            assert_eq!(locked_bytes(keeper_pid(&lease)), 0);
        } else {
            let first = lease.materialize(b"budget-fixture", None).unwrap();
            let pid = keeper_pid(&lease);
            let error = lease.materialize(b"over-budget", None).unwrap_err();
            assert!(error.to_string().contains("RLIMIT_MEMLOCK"), "{error}");
            assert_eq!(fs::read(&first).unwrap(), b"budget-fixture");
            assert_eq!(locked_bytes(pid), budget);
            lease.release(&first).unwrap();
            assert_eq!(locked_bytes(pid), 0);
            let next = lease.materialize(b"reclaimed-budget", None).unwrap();
            assert_ne!(first, next);
            assert_eq!(fs::read(&next).unwrap(), b"reclaimed-budget");
            lease.release(&next).unwrap();
            assert_eq!(locked_bytes(pid), 0);
        }
    }

    #[test]
    fn insufficient_lock_budget_fails_closed_and_release_reclaims_it() {
        for mode in ["zero", "page"] {
            assert!(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "linux::tests::memlock_limit_child",
                        "--nocapture"
                    ])
                    .env("_FILE_LEASE_MEMLOCK_TEST", mode)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
}
