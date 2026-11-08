//! Experimental inherited-channel volume manager. The public lease protocol and
//! owner watchers remain in each individual lease helper.
#[cfg(target_os = "linux")]
use super::linux::Storage;
#[cfg(target_os = "macos")]
use super::macos::Storage;
use std::{
    collections::{HashMap, HashSet},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

const CONTROL: &str = "_FILE_LEASE_VOLUME_CONTROL";
#[cfg(target_os = "macos")]
const CHANNEL: &str = "_FILE_LEASE_VOLUME_CHANNEL";
const READY: &[u8; 4] = b"FLV1";
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PATH: usize = 16384;

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
        let remote = super::platform::above_stdio(remote)?;
        let fd = remote.as_raw_fd();
        let mut command = Command::new(executable);
        command
            .env(CONTROL, fd.to_string())
            // Old test helpers use this descriptor to advertise their version.
            .env("_FILE_LEASE_KEEPER_SOCKET", fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        #[cfg(test)]
        command
            .args(["--exact", "tests::keeper_entry", "--nocapture"])
            .stderr(Stdio::inherit());
        let mut child = command.spawn()?;
        drop(remote);
        let mut ready = [0; 4];
        if control.read_exact(&mut ready).is_err() || &ready != READY {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(
                "Incompatible file lease keeper (volume manager)",
            ));
        }
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
                super::platform::reap_when_exited(child);
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
    if unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
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
        if fd < 3 {
            return Err(io::Error::other("Invalid volume scope descriptor"));
        }
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            stream: Some(stream),
        })
    }
    pub(crate) fn materialize(&mut self, bytes: &[u8], suffix: &str) -> io::Result<PathBuf> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| io::Error::other("Volume scope closed"))?;
        stream.write_all(&[1])?;
        stream.write_all(&(bytes.len() as u64).to_le_bytes())?;
        stream.write_all(&(suffix.len() as u32).to_le_bytes())?;
        stream.write_all(suffix.as_bytes())?;
        stream.write_all(bytes)?;
        let mut status = [0];
        stream.read_exact(&mut status)?;
        let message = read_string(stream, MAX_PATH)?;
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
        stream.write_all(&[4])?;
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
            stream.write_all(&[3])?;
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

fn write_string(stream: &mut UnixStream, value: &str) -> io::Result<()> {
    if value.len() > MAX_PATH {
        return Err(io::Error::other("Oversized volume reply"));
    }
    stream.write_all(&(value.len() as u32).to_le_bytes())?;
    stream.write_all(value.as_bytes())
}
fn read_string(stream: &mut UnixStream, maximum: usize) -> io::Result<String> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > maximum {
        return Err(io::Error::other("Oversized volume message"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(io::Error::other)
}

struct State {
    storage: Storage,
    active: usize,
    controller: bool,
}
fn finish(state: &mut State) {
    if !state.controller && state.active == 0 {
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
    if fd < 3 {
        return Err(io::Error::other("Invalid volume control descriptor"));
    }
    let mut control = unsafe { UnixStream::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // The manager handles payloads, so disable ordinary process core dumps.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut limit) } < 0 {
        return Err(io::Error::last_os_error());
    }
    limit.rlim_cur = 0;
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let state = Arc::new(Mutex::new(State {
        storage: Storage::default(),
        active: 0,
        controller: true,
    }));
    control.write_all(READY)?;
    while let Some(channel) = receive_fd(&control)? {
        state
            .lock()
            .map_err(|_| io::Error::other("Volume state poisoned"))?
            .active += 1;
        let shared = state.clone();
        std::thread::Builder::new()
            .name("volume-scope".into())
            .spawn(move || serve(channel, shared))?;
        control.write_all(&[1])?;
    }
    let mut state_guard = state
        .lock()
        .map_err(|_| io::Error::other("Volume state poisoned"))?;
    state_guard.controller = false;
    finish(&mut state_guard);
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
                1 => {
                    let mut length = [0; 8];
                    stream.read_exact(&mut length)?;
                    let length = u64::from_le_bytes(length);
                    if length > MAX_BYTES {
                        return Err(io::Error::other("Oversized volume payload"));
                    }
                    let suffix = read_string(&mut stream, 80)?;
                    if !suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                    {
                        return Err(io::Error::other("Invalid volume suffix"));
                    }
                    let mut bytes = zeroize::Zeroizing::new(vec![0; length as usize]);
                    stream.read_exact(&mut bytes)?;
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
                                if message.len() <= MAX_PATH {
                                    &message
                                } else {
                                    "Volume allocation failed"
                                },
                            )?;
                        }
                    }
                }
                4 => {
                    let path = PathBuf::from(read_string(&mut stream, MAX_PATH)?);
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
                3 => {
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
        state.active -= 1;
        finish(&mut state);
    } else {
        std::process::exit(1);
    }
}
fn cleanup_paths(paths: &mut HashSet<PathBuf>, state: &Mutex<State>) -> io::Result<()> {
    let mut state = state
        .lock()
        .map_err(|_| io::Error::other("Volume state poisoned"))?;
    for path in paths.iter() {
        state.storage.release(path)?;
    }
    paths.clear();
    Ok(())
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
    fn registrations_require_a_descriptor_and_valid_frame() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&[9]).unwrap();
        assert!(receive_fd(&server).is_err());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn independent_handoff_and_keeper_death_preserve_neighbours() {
        use crate::FileLease;
        use std::time::{Duration, Instant};
        let pool = Pool::get(&std::env::current_exe().unwrap()).unwrap();
        let first = FileLease::new(std::process::id());
        let second = FileLease::new(std::process::id());
        let one = first.materialize(b"scope-one", None).unwrap();
        let two = second.materialize(b"scope-two", None).unwrap();
        assert_eq!(one.parent(), two.parent());
        let first_pid = first.keeper.lock().unwrap().as_ref().unwrap().pid();
        assert_eq!(unsafe { libc::kill(first_pid as _, libc::SIGKILL) }, 0);
        drop(first);
        let deadline = Instant::now() + Duration::from_secs(10);
        while one.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!one.exists());
        assert_eq!(std::fs::read(&two).unwrap(), b"scope-two");
        let mut consumer = Command::new("sleep").arg("60").spawn().unwrap();
        second.handoff_to(consumer.id()).unwrap();
        drop(second);
        assert_eq!(std::fs::read(&two).unwrap(), b"scope-two");
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
}
