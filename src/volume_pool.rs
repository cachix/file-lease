//! Inherited-channel macOS volume manager. The public lease protocol and
//! owner watchers remain in each individual lease helper.
use super::helper;
#[cfg(target_os = "linux")]
use super::linux::Storage;
#[cfg(target_os = "macos")]
use super::macos::Storage;
use super::protocol::{
    ALLOCATE, CLOSE, MAX_MESSAGE_BYTES, RELEASE, read_allocation, read_string, write_allocation,
    write_string,
};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

const CONTROL: &str = "_FILE_LEASE_VOLUME_CONTROL";
#[cfg(target_os = "macos")]
const CHANNEL: &str = "_FILE_LEASE_VOLUME_CHANNEL";
const READY: &[u8; 4] = b"FLV1";

pub(crate) struct Pool {
    control: Mutex<Option<UnixStream>>,
    child: Mutex<Option<Child>>,
    detached: AtomicBool,
}

impl Pool {
    pub(crate) fn get(executable: &Path) -> io::Result<Arc<Self>> {
        type Pools = HashMap<(u32, PathBuf), Weak<Pool>>;
        static POOLS: OnceLock<Mutex<Pools>> = OnceLock::new();
        let mut pools = POOLS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| io::Error::other("Volume pool registry poisoned"))?;
        pools.retain(|_, pool| pool.strong_count() != 0);
        let key = (std::process::id(), executable.to_owned());
        if let Some(pool) = pools.get(&key).and_then(Weak::upgrade) {
            let alive = pool
                .child
                .lock()
                .map_err(|_| io::Error::other("Volume manager child poisoned"))?
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
            if alive {
                return Ok(pool);
            }
        }
        let (mut control, remote) = UnixStream::pair()?;
        let remote = helper::above_stdio(remote)?;
        let fd = remote.as_raw_fd();
        let mut command = Command::new(executable);
        command
            .env(CONTROL, fd.to_string())
            // Old test helpers use this descriptor to advertise their version.
            .env("_FILE_LEASE_KEEPER_SOCKET", fd.to_string());
        let child = helper::spawn(&mut command, vec![remote.into()], &mut control, READY)?;
        let pool = Arc::new(Self {
            control: Mutex::new(Some(control)),
            child: Mutex::new(Some(child)),
            detached: AtomicBool::new(false),
        });
        pools.insert(key, Arc::downgrade(&pool));
        Ok(pool)
    }

    pub(crate) fn connect(&self) -> io::Result<OwnedFd> {
        let (local, remote) = UnixStream::pair()?;
        let mut guard = self
            .control
            .lock()
            .map_err(|_| io::Error::other("Volume pool control poisoned"))?;
        let stream = guard
            .as_mut()
            .ok_or_else(|| io::Error::other("Volume pool closed"))?;
        send_fd(stream, local.as_raw_fd())?;
        let mut ack = [0];
        stream.read_exact(&mut ack)?;
        if ack != [1] {
            return Err(io::Error::other("Volume scope registration failed"));
        }
        Ok(remote.into())
    }

    #[cfg(test)]
    pub(crate) fn pid(&self) -> u32 {
        self.child.lock().unwrap().as_ref().unwrap().id()
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn handoff(&self) {
        self.detached.store(true, Ordering::Release);
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        // Closing this channel prevents future registrations. Existing lease
        // helpers keep their own scope channels, including after handoff/exec.
        if let Ok(control) = self.control.get_mut() {
            drop(control.take());
        }
        if let Ok(child) = self.child.get_mut()
            && let Some(mut child) = child.take()
        {
            if self.detached.load(Ordering::Acquire) {
                helper::reap_when_exited(child);
            } else {
                let _ = child.wait();
            }
        }
    }
}

fn send_fd(stream: &UnixStream, fd: i32) -> io::Result<()> {
    let mut byte = [1u8];
    let mut vector = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as _) } as _;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as _) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<i32>(), fd);
        if libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) != 1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn receive_fd(stream: &UnixStream) -> io::Result<Option<OwnedFd>> {
    let mut byte = [0u8];
    let mut vector = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as _;
    let received = loop {
        let result = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
        if result >= 0 {
            break result;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    if received == 0 {
        return Ok(None);
    }
    // Own every received descriptor immediately, even for malformed messages.
    let mut descriptors = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let bytes =
                    ((*header).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                for index in 0..bytes / std::mem::size_of::<i32>() {
                    let fd =
                        std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<i32>().add(index));
                    descriptors.push(OwnedFd::from_raw_fd(fd));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if byte != [1] || descriptors.len() != 1 || message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("Invalid volume scope registration"));
    }
    let descriptor = descriptors.pop().unwrap();
    helper::close_on_exec(descriptor.as_raw_fd())?;
    Ok(Some(descriptor))
}

pub(crate) struct RemoteStorage {
    stream: Option<UnixStream>,
}
impl RemoteStorage {
    #[cfg(target_os = "macos")]
    pub(crate) fn from_env() -> io::Result<Self> {
        let fd: i32 = std::env::var(CHANNEL)
            .map_err(io::Error::other)?
            .parse()
            .map_err(io::Error::other)?;
        // The launcher transfers this private channel exclusively to the scope.
        let stream = UnixStream::from(unsafe { helper::take_inherited(fd)? });
        Ok(Self {
            stream: Some(stream),
        })
    }
    pub(crate) fn materialize(&mut self, bytes: &[u8], suffix: &str) -> io::Result<PathBuf> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| io::Error::other("Volume scope closed"))?;
        write_allocation(stream, bytes, suffix)?;
        let mut status = [0];
        stream.read_exact(&mut status)?;
        let message = read_string(stream, MAX_MESSAGE_BYTES)?;
        if status == [1] {
            Ok(PathBuf::from(message))
        } else {
            Err(io::Error::other(message))
        }
    }
    pub(crate) fn release(&mut self, path: &Path) -> io::Result<()> {
        let path = path
            .to_str()
            .ok_or_else(|| io::Error::other("Non-UTF-8 volume path"))?;
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| io::Error::other("Volume scope closed"))?;
        stream.write_all(&[RELEASE])?;
        write_string(stream, path)?;
        let mut ack = [0];
        stream.read_exact(&mut ack)?;
        if ack == [1] {
            Ok(())
        } else {
            Err(io::Error::other(
                "Path does not belong to this volume scope",
            ))
        }
    }
    pub(crate) fn cleanup(&mut self) -> io::Result<()> {
        if let Some(mut stream) = self.stream.take() {
            stream.write_all(&[CLOSE])?;
            let mut ack = [0];
            stream.read_exact(&mut ack)?;
            if ack != [1] {
                return Err(io::Error::other("Volume scope cleanup failed"));
            }
        }
        Ok(())
    }
}
impl Drop for RemoteStorage {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

// Each backend tracks its own paths. Keep at most one empty volume warm for
// reuse; surplus empty volumes are detached when their last path is released.
#[derive(Default)]
struct VolumeSet {
    volumes: Vec<Storage>,
}
impl VolumeSet {
    fn materialize(&mut self, bytes: &[u8], suffix: &str) -> io::Result<PathBuf> {
        for volume in &mut self.volumes {
            match volume.materialize(bytes, suffix) {
                Ok(path) => return Ok(path),
                Err(error) => {
                    // Only filesystem capacity exhaustion permits spillover.
                    // If an empty volume cannot fit the request, another fresh
                    // volume cannot help. This also bounds oversized retries.
                    if error.raw_os_error() != Some(libc::ENOSPC) || volume.is_empty() {
                        return Err(error);
                    }
                }
            }
        }
        // At most one new volume per request. Retain it even on failure, so a
        // later request can reuse it and final cleanup includes its journal.
        self.volumes.push(Storage::default());
        self.volumes.last_mut().unwrap().materialize(bytes, suffix)
    }

    fn release(&mut self, path: &Path) -> io::Result<()> {
        let index = self
            .volumes
            .iter()
            .position(|volume| volume.contains(path))
            .ok_or_else(|| io::Error::other("Unknown pooled path"))?;
        let volume = &mut self.volumes[index];
        volume.release(path)?;
        if volume.is_empty()
            && self
                .volumes
                .iter()
                .filter(|volume| volume.is_empty())
                .count()
                > 1
        {
            let mut empty = self.volumes.remove(index);
            // Payload revocation already succeeded. A failed detach remains
            // journaled for the guardian and later recovery; it must not turn
            // maintenance of an empty mount into revocation of live neighbours.
            if empty.cleanup().is_err() {
                eprintln!("File-lease empty-volume cleanup deferred to recovery");
            }
        }
        Ok(())
    }

    fn cleanup(&mut self) -> io::Result<()> {
        let mut failure = None;
        // A failure on one volume must not prevent revocation on the rest.
        for volume in &mut self.volumes {
            if let Err(error) = volume.cleanup() {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

struct State {
    storage: VolumeSet,
    // The controller and each scope keep the manager alive until disconnect.
    connections: usize,
}
fn disconnect(state: &mut State) {
    state.connections -= 1;
    if state.connections == 0 {
        let result = state.storage.cleanup();
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
}

pub(crate) fn maybe_run() -> Option<i32> {
    let fd = std::env::var(CONTROL).ok()?;
    Some(run(&fd).map_or_else(
        |error| {
            eprintln!("File-lease volume manager: {error}");
            1
        },
        |_| 0,
    ))
}
fn run(fd: &str) -> io::Result<()> {
    let fd: i32 = fd.parse().map_err(io::Error::other)?;
    // The launcher transfers this private channel exclusively to the manager.
    let mut control = UnixStream::from(unsafe { helper::take_inherited(fd)? });
    // The manager handles payloads, so disable ordinary process core dumps.
    helper::disable_core_dumps()?;
    let state = Arc::new(Mutex::new(State {
        storage: VolumeSet::default(),
        connections: 1,
    }));
    control.write_all(READY)?;
    while let Some(channel) = receive_fd(&control)? {
        state
            .lock()
            .map_err(|_| io::Error::other("Volume state poisoned"))?
            .connections += 1;
        let shared = state.clone();
        std::thread::Builder::new()
            .name("volume-scope".into())
            .spawn(move || serve(channel, shared))?;
        control.write_all(&[1])?;
    }
    let mut state_guard = state
        .lock()
        .map_err(|_| io::Error::other("Volume state poisoned"))?;
    disconnect(&mut state_guard);
    drop(state_guard);
    // Live scopes remain after the launcher exits or drops handed-off leases.
    loop {
        std::thread::park();
    }
}
fn serve(channel: OwnedFd, state: Arc<Mutex<State>>) {
    let mut stream = UnixStream::from(channel);
    let mut paths = HashSet::new();
    let result = (|| -> io::Result<()> {
        loop {
            let mut op = [0];
            match stream.read_exact(&mut op) {
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                result => result?,
            }
            match op[0] {
                ALLOCATE => {
                    let (bytes, suffix) = read_allocation(&mut stream)?;
                    let allocated = state
                        .lock()
                        .map_err(|_| io::Error::other("Volume state poisoned"))?
                        .storage
                        .materialize(&bytes, &suffix);
                    match allocated {
                        Ok(path) => {
                            paths.insert(path.clone());
                            stream.write_all(&[1])?;
                            write_string(
                                &mut stream,
                                path.to_str()
                                    .ok_or_else(|| io::Error::other("Non-UTF-8 volume path"))?,
                            )?;
                        }
                        Err(error) => {
                            stream.write_all(&[0])?;
                            let message = error.to_string();
                            write_string(
                                &mut stream,
                                if message.len() <= MAX_MESSAGE_BYTES {
                                    &message
                                } else {
                                    "Volume allocation failed"
                                },
                            )?;
                        }
                    }
                }
                RELEASE => {
                    let path = PathBuf::from(read_string(&mut stream, MAX_MESSAGE_BYTES)?);
                    let released = paths.contains(&path)
                        && state
                            .lock()
                            .map_err(|_| io::Error::other("Volume state poisoned"))?
                            .storage
                            .release(&path)
                            .is_ok();
                    if released {
                        paths.remove(&path);
                    }
                    stream.write_all(&[u8::from(released)])?;
                }
                CLOSE => {
                    cleanup_paths(&mut paths, &state)?;
                    stream.write_all(&[1])?;
                    return Ok(());
                }
                _ => return Err(io::Error::other("Invalid volume operation")),
            }
        }
    })();
    let cleanup = cleanup_paths(&mut paths, &state);
    if cleanup.is_err() {
        // A scope whose readable names cannot be revoked fails the shared
        // manager closed. Its guardian recovers the volume after process exit.
        std::process::exit(1);
    }
    let _ = result;
    if let Ok(mut state) = state.lock() {
        disconnect(&mut state);
    } else {
        std::process::exit(1);
    }
}
fn cleanup_paths(paths: &mut HashSet<PathBuf>, state: &Mutex<State>) -> io::Result<()> {
    let mut state = state
        .lock()
        .map_err(|_| io::Error::other("Volume state poisoned"))?;
    let mut failure = None;
    // One unrevocable path must not leave this scope's other paths readable.
    for path in paths.drain() {
        if let Err(error) = state.storage.release(&path) {
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(target_os = "macos")]
pub(crate) fn channel_env() -> &'static str {
    CHANNEL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(pool: &Pool) -> RemoteStorage {
        let descriptor = pool.connect().unwrap();
        RemoteStorage {
            stream: Some(UnixStream::from(descriptor)),
        }
    }

    #[test]
    fn scopes_cannot_release_each_others_files_and_eof_revokes_only_one_scope() {
        let executable = std::env::current_exe().unwrap();
        let pool = Pool::get(&executable).unwrap();
        let again = Pool::get(&executable).unwrap();
        assert!(Arc::ptr_eq(&pool, &again));
        let mut first = remote(&pool);
        let mut second = remote(&pool);
        let one = first.materialize(b"first-pool-fixture", "").unwrap();
        let two = second.materialize(b"second-pool-fixture", "").unwrap();
        assert_ne!(one, two);
        assert!(first.release(&two).is_err());
        assert_eq!(std::fs::read(&two).unwrap(), b"second-pool-fixture");
        // Abrupt disconnect rather than a graceful cleanup request.
        drop(first.stream.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while one.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!one.exists());
        assert_eq!(std::fs::read(&two).unwrap(), b"second-pool-fixture");
        second.cleanup().unwrap();
        assert!(!two.exists());
        drop(again);
        drop(pool);
    }

    #[test]
    fn scope_cleanup_revokes_every_path_despite_one_failure() {
        let state = Mutex::new(State {
            storage: VolumeSet::default(),
            connections: 1,
        });
        let path = state
            .lock()
            .unwrap()
            .storage
            .materialize(b"scope-cleanup-fixture", "")
            .unwrap();
        let mut paths = HashSet::from([PathBuf::from("/not-a-pooled-path"), path.clone()]);
        assert!(cleanup_paths(&mut paths, &state).is_err());
        assert!(paths.is_empty());
        assert!(!path.exists());
        state.lock().unwrap().storage.cleanup().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn consumer_deleted_payloads_do_not_fail_the_shared_manager() {
        let pool = Pool::get(&std::env::current_exe().unwrap()).unwrap();
        let mut first = remote(&pool);
        let mut second = remote(&pool);
        let released = first.materialize(b"released-fixture", "").unwrap();
        let closed = first.materialize(b"closed-fixture", "").unwrap();
        let neighbour = second.materialize(b"neighbour-fixture", "").unwrap();
        std::fs::remove_file(&released).unwrap();
        std::fs::remove_file(&closed).unwrap();
        first.release(&released).unwrap();
        // An acknowledged close proves the manager survived revoking a
        // missing name rather than exiting and revoking every scope.
        first.cleanup().unwrap();
        assert!(matches!(
            pool.child.lock().unwrap().as_mut().unwrap().try_wait(),
            Ok(None)
        ));
        assert_eq!(std::fs::read(&neighbour).unwrap(), b"neighbour-fixture");
        let next = second.materialize(b"after-delete-fixture", "").unwrap();
        assert_eq!(std::fs::read(&next).unwrap(), b"after-delete-fixture");
        second.cleanup().unwrap();
        assert!(!neighbour.exists());
        assert!(!next.exists());
    }

    #[test]
    fn registrations_require_a_descriptor_and_valid_frame() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&[9]).unwrap();
        assert!(receive_fd(&server).is_err());
    }

    #[test]
    fn controller_disconnect_keeps_scopes_alive_until_the_last_disconnect() {
        use std::time::Duration;
        use wait_timeout::ChildExt;

        let pool = Pool::get(&std::env::current_exe().unwrap()).unwrap();
        let mut first = remote(&pool);
        let mut second = remote(&pool);
        let one = first.materialize(b"first-scope", "").unwrap();
        let two = second.materialize(b"last-scope", "").unwrap();
        // Close registration while retaining the child handle for a bounded wait.
        drop(pool.control.lock().unwrap().take());
        first.cleanup().unwrap();
        assert!(!one.exists());
        assert_eq!(std::fs::read(&two).unwrap(), b"last-scope");
        let three = second.materialize(b"after-controller-exit", "").unwrap();
        // EOF on the final scope must revoke its paths and exit the manager.
        drop(second.stream.take());
        let mut child = pool.child.lock().unwrap();
        let child = child.as_mut().unwrap();
        let status = child.wait_timeout(Duration::from_secs(10)).unwrap();
        if status.is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert!(
            status
                .expect("Manager outlived its final connection")
                .success()
        );
        assert!(!two.exists());
        assert!(!three.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn independent_handoff_and_keeper_death_preserve_neighbours() {
        use crate::FileLease;
        use std::time::{Duration, Instant};
        let pool = Pool::get(&std::env::current_exe().unwrap()).unwrap();
        let first = FileLease::new(std::process::id());
        let second = FileLease::new(std::process::id());
        let payload = vec![55; 40 * 1024 * 1024];
        let one = first.materialize(&payload, None).unwrap();
        let two = second.materialize(&payload, None).unwrap();
        assert_ne!(one.parent(), two.parent());
        let first_pid = first.keeper.lock().unwrap().as_ref().unwrap().pid();
        assert_eq!(unsafe { libc::kill(first_pid as _, libc::SIGKILL) }, 0);
        drop(first);
        let deadline = Instant::now() + Duration::from_secs(10);
        while one.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!one.exists());
        assert_eq!(std::fs::read(&two).unwrap(), payload);
        let mut consumer = Command::new("sleep").arg("60").spawn().unwrap();
        second.handoff_to(consumer.id()).unwrap();
        drop(second);
        assert_eq!(std::fs::read(&two).unwrap(), payload);
        assert_eq!(unsafe { libc::kill(pool.pid() as _, 0) }, 0);
        consumer.kill().unwrap();
        consumer.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while two.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!two.exists());
        drop(pool);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn full_shared_volume_rejects_one_allocation_without_revoking_other_scopes() {
        let pool = Pool::get(&std::env::current_exe().unwrap()).unwrap();
        let mut first = remote(&pool);
        let mut second = remote(&pool);
        let path = first.materialize(b"neighbour-fixture", "").unwrap();
        assert!(second.materialize(&vec![42; 64 * 1024 * 1024], "").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"neighbour-fixture");
        let next = second.materialize(b"after-full-fixture", "").unwrap();
        assert_eq!(std::fs::read(&next).unwrap(), b"after-full-fixture");
        second.cleanup().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"neighbour-fixture");
        first.cleanup().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn spillover_reuses_freed_space_and_bounds_impossible_requests() {
        let mut storage = VolumeSet::default();
        let payload = vec![51; 40 * 1024 * 1024];
        let one = storage.materialize(&payload, "").unwrap();
        let two = storage.materialize(&payload, "").unwrap();
        assert_ne!(one.parent(), two.parent());
        assert_eq!(storage.volumes.len(), 2);
        assert_eq!(std::fs::read(&one).unwrap(), payload);
        assert_eq!(std::fs::read(&two).unwrap(), payload);
        storage.release(&one).unwrap();
        assert!(!one.exists());
        let next = storage.materialize(&payload, "").unwrap();
        assert_eq!(one.parent(), next.parent());
        assert_eq!(storage.volumes.len(), 2);
        let oversized = vec![52; crate::protocol::MAX_SECRET_BYTES as usize];
        for _ in 0..2 {
            let error = storage.materialize(&oversized, "").unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
            assert_eq!(storage.volumes.len(), 3);
        }
        assert_eq!(std::fs::read(&two).unwrap(), payload);
        assert_eq!(std::fs::read(&next).unwrap(), payload);
        let mounts: Vec<_> = [&two, &next]
            .into_iter()
            .map(|path| path.parent().unwrap().to_owned())
            .collect();
        storage.cleanup().unwrap();
        assert!(mounts.iter().all(|mount| !mount.exists()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn full_volume_preflight_preserves_namespace_and_surplus_empty_volumes_detach() {
        let mut storage = VolumeSet::default();
        let payload = vec![56; 40 * 1024 * 1024];
        let one = storage.materialize(&payload, "").unwrap();
        let two = storage.materialize(&payload, "").unwrap();
        let three = storage.materialize(&payload, "").unwrap();
        let mounts: Vec<_> = [&one, &two, &three]
            .into_iter()
            .map(|path| path.parent().unwrap().to_owned())
            .collect();
        assert_eq!(storage.volumes.len(), 3);
        // A preflight rejection does not consume a file name. Each populated
        // volume's first successful path is still secret-1-, despite spillover.
        assert_eq!(one.file_name().unwrap(), "secret-1-");
        assert_eq!(two.file_name().unwrap(), "secret-1-");
        assert_eq!(three.file_name().unwrap(), "secret-1-");
        storage.release(&one).unwrap();
        assert!(mounts[0].exists());
        storage.release(&two).unwrap();
        assert!(!mounts[1].exists());
        assert_eq!(storage.volumes.len(), 2);
        assert_eq!(std::fs::read(&three).unwrap(), payload);
        let next = storage.materialize(&payload, "").unwrap();
        assert_eq!(next.parent(), one.parent());
        assert_eq!(storage.volumes.len(), 2);
        assert!(storage.release(&two).is_err());
        storage.release(&three).unwrap();
        storage.release(&next).unwrap();
        assert_eq!(storage.volumes.len(), 1);
        assert_eq!(mounts.iter().filter(|mount| mount.exists()).count(), 1);
        storage.cleanup().unwrap();
        assert!(mounts.iter().all(|mount| !mount.exists()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scopes_spanning_volumes_revoke_only_their_own_paths() {
        let pool = Pool::get(&std::env::current_exe().unwrap()).unwrap();
        let mut first = remote(&pool);
        let mut second = remote(&pool);
        let payload = vec![53; 40 * 1024 * 1024];
        let one = first.materialize(&payload, "").unwrap();
        let two = first.materialize(&payload, "").unwrap();
        assert_ne!(one.parent(), two.parent());
        let neighbour = second.materialize(b"neighbour", "").unwrap();
        assert!(second.release(&two).is_err());
        first.cleanup().unwrap();
        assert!(!one.exists());
        assert!(!two.exists());
        assert_eq!(std::fs::read(&neighbour).unwrap(), b"neighbour");
        let next = second.materialize(&payload, "").unwrap();
        assert_eq!(one.parent(), next.parent());
        assert_eq!(std::fs::read(&next).unwrap(), payload);
        second.cleanup().unwrap();
        assert!(!next.exists());
    }

    #[test]
    fn dead_manager_is_not_reused_for_new_scopes() {
        let executable = std::env::current_exe().unwrap();
        let pool = Pool::get(&executable).unwrap();
        let mut original = remote(&pool);
        let path = original.materialize(b"old-manager", "").unwrap();
        assert_eq!(unsafe { libc::kill(pool.pid() as _, libc::SIGKILL) }, 0);
        pool.child.lock().unwrap().as_mut().unwrap().wait().unwrap();
        let next_pool = Pool::get(&executable).unwrap();
        assert!(!Arc::ptr_eq(&pool, &next_pool));
        let mut next = remote(&next_pool);
        let next_path = next.materialize(b"replacement-manager", "").unwrap();
        assert_eq!(std::fs::read(&next_path).unwrap(), b"replacement-manager");
        drop(original);
        drop(pool);
        next.cleanup().unwrap();
        assert!(!next_path.exists());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!path.exists());
    }
    #[test]
    fn disconnect_during_payload_revokes_only_the_interrupted_scope() {
        let pool = Pool::get(&std::env::current_exe().unwrap()).unwrap();
        let mut first = remote(&pool);
        let mut second = remote(&pool);
        let one = first.materialize(b"interrupted-fixture", "").unwrap();
        let two = second.materialize(b"surviving-fixture", "").unwrap();
        let mut stream = first.stream.take().unwrap();
        stream.write_all(&[1]).unwrap();
        stream.write_all(&128u64.to_le_bytes()).unwrap();
        stream.write_all(&0u32.to_le_bytes()).unwrap();
        stream.write_all(b"partial").unwrap();
        drop(stream);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while one.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!one.exists());
        assert_eq!(std::fs::read(&two).unwrap(), b"surviving-fixture");
        second.cleanup().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn one_owners_exit_does_not_revoke_another_owners_scope() {
        use crate::FileLease;
        let anchor = FileLease::new(std::process::id());
        let one = anchor.materialize(b"live-owner-fixture", None).unwrap();
        let mut consumer = Command::new("sleep").arg("60").spawn().unwrap();
        let other = FileLease::new(consumer.id());
        let two = other.materialize(b"exiting-owner-fixture", None).unwrap();
        assert_eq!(one.parent(), two.parent());
        other.handoff().unwrap();
        consumer.kill().unwrap();
        consumer.wait().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while two.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!two.exists());
        assert_eq!(std::fs::read(&one).unwrap(), b"live-owner-fixture");
        let next = anchor.materialize(b"after-neighbour-exit", None).unwrap();
        assert_eq!(std::fs::read(&next).unwrap(), b"after-neighbour-exit");
        drop(other);
        drop(anchor);
        assert!(!one.exists());
        assert!(!next.exists());
    }
}
