use super::helper::{self, above_stdio, reap_when_exited};
use super::protocol::{self, ALLOCATE, CLOSE, HANDOFF, MAX_MESSAGE_BYTES, RELEASE};
use super::*;
use crate::error::{ErrorKind, tagged};
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    process::{Child, Command},
};

#[cfg(target_os = "linux")]
use super::linux::{Storage, watch_owner};
#[cfg(all(target_os = "macos", not(feature = "shared-volumes")))]
use super::macos::Storage;
#[cfg(target_os = "macos")]
use super::macos::watch_owner;
#[cfg(all(target_os = "macos", feature = "shared-volumes"))]
use super::volume_pool::RemoteStorage as Storage;

const SOCKET_ENV: &str = "_FILE_LEASE_KEEPER_SOCKET";
const OWNER_ENV: &str = "_FILE_LEASE_KEEPER_OWNER";
// Version 4 adds the build identity to the handshake. Reject older helpers
// before sending payloads rather than accepting an unknown storage contract.
#[cfg(not(all(target_os = "macos", feature = "shared-volumes")))]
const READY: &[u8; 4] = b"SFL4";
#[cfg(all(target_os = "macos", feature = "shared-volumes"))]
// Shared-volume helpers require a separate inherited scope channel.
const READY: &[u8; 4] = b"SFL5";

// FileLease owns this behind its shared mutex. Mutable access serializes the
// request stream, handoff state, and child lifecycle together.
pub(super) struct Keeper {
    stream: UnixStream,
    child: Option<Child>,
    handed_off: bool,
    #[cfg(all(target_os = "macos", feature = "shared-volumes"))]
    pool: Arc<super::volume_pool::Pool>,
}

fn valid_pid(pid: u32) -> io::Result<()> {
    if pid == 0 || pid > i32::MAX as u32 {
        Err(tagged(
            ErrorKind::InvalidOwner,
            "Invalid file lease owner PID",
        ))
    } else {
        Ok(())
    }
}

fn invalid_owner(error: io::Error) -> io::Error {
    tagged(
        ErrorKind::InvalidOwner,
        format!("Cannot watch file lease owner: {error}"),
    )
}

impl Keeper {
    #[cfg(test)]
    pub(super) fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    pub(super) fn spawn(owner: u32, executable: Option<&Path>) -> io::Result<Self> {
        let _span = super::profiling::Span::new("lease.helper_spawn");
        valid_pid(owner)?;
        #[cfg(target_os = "macos")]
        let owner_info = super::macos::process_info(owner).map_err(invalid_owner)?;
        #[cfg(target_os = "macos")]
        let owner_arg = owner.to_string();
        #[cfg(target_os = "linux")]
        let owner = above_stdio(watch_owner(owner).map_err(invalid_owner)?)?;
        let (stream, remote) = UnixStream::pair()?;
        let mut stream = above_stdio(stream)?;
        let remote = above_stdio(remote)?;
        let socket_fd = remote.as_raw_fd();
        #[cfg(target_os = "linux")]
        let owner_fd = owner.as_raw_fd();
        #[cfg(target_os = "linux")]
        let owner_arg = owner_fd.to_string();
        let executable = match executable {
            Some(path) => path.to_path_buf(),
            None => {
                #[cfg(test)]
                {
                    std::env::current_exe()?
                }
                #[cfg(not(test))]
                {
                    // Never search PATH: every payload is sent to this process.
                    let sibling = std::env::current_exe()?.with_file_name("file-lease");
                    if !sibling.is_file() {
                        return Err(tagged(
                            ErrorKind::Helper,
                            format!(
                                "File lease helper not found at {}; install it there or \
                                 call with_helper_executable",
                                sibling.display()
                            ),
                        ));
                    }
                    sibling
                }
            }
        };
        #[cfg(all(target_os = "macos", feature = "shared-volumes"))]
        let pool = super::volume_pool::Pool::get(&executable)?;
        #[cfg(all(target_os = "macos", feature = "shared-volumes"))]
        let volume_channel = above_stdio(pool.connect()?)?;
        let mut command = Command::new(executable);
        #[cfg(all(target_os = "macos", feature = "shared-volumes"))]
        command.env(
            super::volume_pool::channel_env(),
            volume_channel.as_raw_fd().to_string(),
        );
        command
            .env(SOCKET_ENV, socket_fd.to_string())
            .env(OWNER_ENV, owner_arg);
        #[cfg(target_os = "macos")]
        command.env(
            "_FILE_LEASE_OWNER_BIRTH",
            format!(
                "{}:{}",
                owner_info.pbi_start_tvsec, owner_info.pbi_start_tvusec
            ),
        );
        // kqueues are not inherited across fork. macOS registers its exit
        // watcher inside the new helper and validates the process birth time.
        #[cfg(target_os = "linux")]
        let inherited = vec![remote.into(), owner];
        #[cfg(target_os = "macos")]
        let inherited = vec![remote.into()];
        #[cfg(all(target_os = "macos", feature = "shared-volumes"))]
        let inherited = {
            let mut descriptors = inherited;
            descriptors.push(volume_channel);
            descriptors
        };
        let child = helper::spawn(&mut command, inherited, &mut stream, READY)?;
        Ok(Self {
            stream,
            child: Some(child),
            handed_off: false,
            #[cfg(all(target_os = "macos", feature = "shared-volumes"))]
            pool,
        })
    }

    pub(super) fn materialize(&mut self, bytes: &[u8], suffix: &str) -> io::Result<PathBuf> {
        let _span = super::profiling::Span::new("lease.allocate");
        let stream = &mut self.stream;
        if self.handed_off {
            return Err(tagged(
                ErrorKind::HandedOff,
                "File lease has been handed off",
            ));
        }
        protocol::write_allocation(stream, bytes, suffix)?;
        let path = protocol::KEEPER_REPLY.read(stream)?.map_err(|message| {
            tagged(
                ErrorKind::Storage,
                format!("File keeper could not allocate secret storage: {message}"),
            )
        })?;
        if path.is_empty() {
            return Err(tagged(
                ErrorKind::Storage,
                "File keeper could not allocate secret storage",
            ));
        }
        Ok(PathBuf::from(path))
    }

    pub(super) fn release(&mut self, path: &Path) -> io::Result<()> {
        let path = path
            .to_str()
            .filter(|path| path.len() <= MAX_MESSAGE_BYTES)
            .ok_or_else(|| tagged(ErrorKind::Release, "Path does not belong to this lease"))?;
        let stream = &mut self.stream;
        stream.write_all(&[RELEASE])?;
        protocol::write_string(stream, path)?;
        protocol::read_ack(stream, || {
            tagged(ErrorKind::Release, "File lease release failed")
        })
    }

    pub(super) fn handoff_to(&mut self, owner: Option<u32>) -> io::Result<()> {
        if let Some(pid) = owner {
            valid_pid(pid)?;
        }
        let stream = &mut self.stream;
        if !self.handed_off || owner.is_some() {
            stream.write_all(&[HANDOFF])?;
            stream.write_all(&owner.unwrap_or(0).to_le_bytes())?;
            protocol::read_ack(stream, || {
                tagged(ErrorKind::Handoff, "File lease handoff failed")
            })?;
            self.handed_off = true;
            #[cfg(all(target_os = "macos", feature = "shared-volumes"))]
            self.pool.handoff();
        }
        Ok(())
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        let _span = super::profiling::Span::new("lease.cleanup");
        if let Some(mut child) = self.child.take() {
            if self.handed_off {
                reap_when_exited(child);
            } else {
                // Graceful shutdown also unmounts macOS volumes. If the keeper
                // was killed, its macOS guardian independently cleans up.
                let _ = self.stream.write_all(&[CLOSE]);
                let _ = child.wait();
            }
        }
    }
}

pub(super) fn maybe_run_keeper() -> Option<i32> {
    #[cfg(target_os = "macos")]
    if let Some(result) = super::macos::maybe_run_setup() {
        return Some(result.map_or_else(|_| 1, |_| 0));
    }
    #[cfg(all(
        feature = "shared-volumes",
        any(target_os = "macos", all(test, target_os = "linux"))
    ))]
    if let Some(code) = super::volume_pool::maybe_run() {
        return Some(code);
    }
    let socket = std::env::var(SOCKET_ENV).ok()?;
    Some(run_keeper(&socket).map_or_else(
        |error| {
            eprintln!("File lease keeper: {error}");
            1
        },
        |_| 0,
    ))
}

fn run_keeper(socket: &str) -> io::Result<()> {
    let socket: i32 = socket.parse().map_err(io::Error::other)?;
    let owner: i32 = std::env::var(OWNER_ENV)
        .map_err(io::Error::other)?
        .parse()
        .map_err(io::Error::other)?;
    // This descriptor is inherited exclusively by the keeper executable.
    let mut stream = UnixStream::from(unsafe { helper::take_inherited(socket)? });
    #[cfg(target_os = "linux")]
    let owner = {
        if socket == owner {
            return Err(io::Error::other("Invalid file keeper owner descriptor"));
        }
        // The launcher transfers a separate, exclusively owned pidfd.
        unsafe { helper::take_inherited(owner)? }
    };
    #[cfg(target_os = "macos")]
    let owner = {
        valid_pid(owner as u32)?;
        let expected = std::env::var("_FILE_LEASE_OWNER_BIRTH").map_err(io::Error::other)?;
        let verify = || -> io::Result<()> {
            let info = super::macos::process_info(owner as u32)?;
            if format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec) != expected {
                return Err(io::Error::other(
                    "File lease owner process changed during startup",
                ));
            }
            Ok(())
        };
        verify()?;
        let watch = watch_owner(owner as u32)?;
        verify()?;
        watch
    };
    helper::disable_core_dumps()?;
    #[cfg(all(target_os = "macos", feature = "shared-volumes"))]
    let storage = Storage::from_env()?;
    #[cfg(not(all(target_os = "macos", feature = "shared-volumes")))]
    let storage = Storage::default();
    let storage = Arc::new(Mutex::new(storage));
    let mut owner_watch = OwnerWatch::start(owner, storage.clone())?;
    helper::write_ready(&mut stream, READY)?;
    let mut handed_off = false;
    loop {
        let mut op = [0];
        match stream.read_exact(&mut op) {
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                if handed_off {
                    loop {
                        let _ = &owner_watch;
                        std::thread::park();
                    }
                }
                break;
            }
            result => result?,
        }
        match op[0] {
            ALLOCATE => {
                if handed_off {
                    return Err(io::Error::other("Cannot allocate after file lease handoff"));
                }
                let (bytes, suffix) = protocol::read_allocation(&mut stream)?;
                let path = storage
                    .lock()
                    .map_err(|_| io::Error::other("Storage lock poisoned"))?
                    .materialize(&bytes, &suffix);
                match path {
                    Ok(path) => {
                        let path = path
                            .to_str()
                            .ok_or_else(|| io::Error::other("Non-UTF-8 file lease path"))?;
                        protocol::KEEPER_REPLY.write(&mut stream, Ok(path))?;
                    }
                    Err(error) => {
                        eprintln!("file-lease storage: {error}");
                        protocol::KEEPER_REPLY.write(&mut stream, Err(&error.to_string()))?;
                    }
                }
            }
            HANDOFF => {
                let mut pid = [0; 4];
                stream.read_exact(&mut pid)?;
                let pid = u32::from_le_bytes(pid);
                if pid != 0 {
                    let Ok(next) = valid_pid(pid)
                        .and_then(|_| watch_owner(pid))
                        .and_then(|watch| OwnerWatch::start(watch, storage.clone()))
                    else {
                        protocol::write_ack(&mut stream, false)?;
                        continue;
                    };
                    drop(owner_watch);
                    owner_watch = next;
                }
                handed_off = true;
                protocol::write_ack(&mut stream, true)?;
            }
            RELEASE => {
                let path = protocol::read_string(&mut stream, MAX_MESSAGE_BYTES)?;
                let result = storage
                    .lock()
                    .map_err(|_| io::Error::other("Storage lock poisoned"))?
                    .release(Path::new(&path));
                protocol::write_ack(&mut stream, result.is_ok())?;
            }
            CLOSE => break,
            _ => return Err(io::Error::other("Invalid file keeper operation")),
        }
    }
    drop(owner_watch);
    storage
        .lock()
        .map_err(|_| io::Error::other("Storage lock poisoned"))?
        .cleanup()
}

struct OwnerWatch {
    cancel: UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl OwnerWatch {
    fn start(owner: OwnedFd, storage: Arc<Mutex<Storage>>) -> io::Result<Self> {
        let (cancel, listener) = UnixStream::pair()?;
        let thread = std::thread::spawn(move || {
            let mut descriptors = [
                libc::pollfd {
                    fd: owner.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            loop {
                let result =
                    unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
                if result > 0 {
                    if descriptors[1].revents != 0 {
                        return;
                    }
                    if descriptors[0].revents != 0 {
                        let result = storage
                            .lock()
                            .map_err(|_| io::Error::other("Storage lock poisoned"))
                            .and_then(|mut storage| storage.cleanup());
                        std::process::exit(if result.is_ok() { 0 } else { 1 });
                    }
                }
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    if let Ok(mut storage) = storage.lock() {
                        let _ = storage.cleanup();
                    }
                    std::process::exit(1);
                }
            }
        });
        Ok(Self {
            cancel,
            thread: Some(thread),
        })
    }
}
impl Drop for OwnerWatch {
    fn drop(&mut self) {
        let _ = self.cancel.write_all(&[1]);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
