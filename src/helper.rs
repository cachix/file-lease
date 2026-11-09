//! Process lifecycle shared by the lease helper and volume manager.

use std::{
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) fn close_on_exec(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Adopt an inherited descriptor and prevent it from reaching subprocesses.
///
/// # Safety
/// If `fd` is open and above stdio, the caller must exclusively own it and
/// relinquish ownership on success. Invalid descriptors are rejected before
/// constructing an owned handle.
pub(crate) unsafe fn take_inherited(fd: RawFd) -> io::Result<OwnedFd> {
    if fd < 3 {
        return Err(io::Error::other("Invalid inherited helper descriptor"));
    }
    close_on_exec(fd)?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// Descriptors inherited through exec must not collide with child stdio setup.
pub(crate) fn above_stdio<T: Into<OwnedFd> + From<OwnedFd>>(descriptor: T) -> io::Result<T> {
    let descriptor = descriptor.into();
    if descriptor.as_raw_fd() >= 3 {
        return Ok(descriptor.into());
    }
    let duplicate = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) }.into())
}

/// Keep inherited descriptors alive through spawn, then close the parent's
/// copies before waiting for the version handshake or EOF. Callers relocate
/// inherited descriptors above stdio before passing them here.
pub(crate) fn spawn(
    command: &mut Command,
    inherited: Vec<OwnedFd>,
    stream: &mut UnixStream,
    expected: &[u8; 4],
) -> io::Result<Child> {
    spawn_with_timeout(
        command,
        inherited,
        stream,
        expected,
        Duration::from_secs(10),
    )
}

fn spawn_with_timeout(
    command: &mut Command,
    inherited: Vec<OwnedFd>,
    stream: &mut UnixStream,
    expected: &[u8; 4],
    timeout: Duration,
) -> io::Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let descriptors: Vec<_> = inherited.iter().map(AsRawFd::as_raw_fd).collect();
    unsafe {
        command.pre_exec(move || {
            for fd in &descriptors {
                if libc::fcntl(*fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    #[cfg(test)]
    command
        .args(["--exact", "tests::keeper_entry", "--nocapture"])
        .stderr(Stdio::inherit());
    let mut child = command.spawn()?;
    drop(inherited);
    if let Err(error) = read_ready(stream, expected, timeout) {
        let _ = child.kill();
        // Even a killed child can take time to exit. Reap it asynchronously so
        // a failed startup cannot leave the caller waiting without a bound.
        reap_when_exited(child);
        return Err(error);
    }
    Ok(child)
}

fn read_ready(stream: &mut UnixStream, expected: &[u8; 4], timeout: Duration) -> io::Result<()> {
    let previous_timeout = stream.read_timeout()?;
    let deadline = Instant::now() + timeout;
    let handshake = (|| {
        let mut ready = [0; 4];
        let mut received = 0;
        while received < ready.len() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(startup_timeout)?;
            stream.set_read_timeout(Some(remaining))?;
            match stream.read(&mut ready[received..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "File lease keeper exited before readiness",
                    ));
                }
                Ok(count) => received += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(startup_timeout());
                }
                Err(error) => return Err(error),
            }
        }
        if &ready == expected {
            Ok(())
        } else {
            Err(io::Error::other("Incompatible file lease keeper"))
        }
    })();
    // Startup's timeout must not change subsequent allocation or cleanup IO.
    let restored = stream.set_read_timeout(previous_timeout);
    handshake.and(restored)
}

fn startup_timeout() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "File lease keeper startup timed out",
    )
}

pub(crate) fn disable_core_dumps() -> io::Result<()> {
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut limit) } < 0 {
        return Err(io::Error::last_os_error());
    }
    limit.rlim_cur = 0;
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn reap_when_exited(child: Child) {
    use std::sync::{OnceLock, mpsc};
    static REAPER: OnceLock<mpsc::Sender<Child>> = OnceLock::new();
    let sender = REAPER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Child>();
        std::thread::spawn(move || {
            let mut children = Vec::new();
            loop {
                match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(child) => children.push(child),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
            }
        });
        sender
    });
    let _ = sender.send(child);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::fd::IntoRawFd, time::Duration};

    #[test]
    fn silent_helper_startup_times_out_without_waiting_for_its_exit() {
        let (mut stream, remote) = UnixStream::pair().unwrap();
        let remote = above_stdio(remote).unwrap();
        let start = Instant::now();
        let error = spawn_with_timeout(
            Command::new("sh").args(["-c", "exec sleep 60"]),
            vec![remote.into()],
            &mut stream,
            b"TEST",
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(stream.read_timeout().unwrap(), None);
        // The killed helper must close its inherited channel as it exits.
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(
            stream.read_exact(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn partial_readiness_times_out_and_restores_the_socket_timeout() {
        let (mut stream, mut peer) = UnixStream::pair().unwrap();
        let original = Some(Duration::from_secs(2));
        stream.set_read_timeout(original).unwrap();
        peer.write_all(b"T").unwrap();
        let error = read_ready(&mut stream, b"TEST", Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(stream.read_timeout().unwrap(), original);
    }

    #[test]
    fn successful_readiness_restores_the_socket_timeout() {
        let (mut stream, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"TESTnext").unwrap();
        read_ready(&mut stream, b"TEST", Duration::from_secs(2)).unwrap();
        assert_eq!(stream.read_timeout().unwrap(), None);
        let mut next = [0; 4];
        stream.read_exact(&mut next).unwrap();
        assert_eq!(&next, b"next");
    }

    #[test]
    fn inherited_channels_restore_close_on_exec_and_transfer_ownership() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let fd = stream.into_raw_fd();
        // Model a descriptor whose close-on-exec flag was cleared for startup.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
        let mut inherited = UnixStream::from(unsafe { take_inherited(fd) }.unwrap());
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        peer.write_all(b"owned").unwrap();
        let mut bytes = [0; 5];
        inherited.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"owned");
        drop(inherited);
        assert_eq!(peer.read(&mut bytes).unwrap(), 0);
    }

    #[test]
    fn invalid_inherited_descriptors_are_rejected() {
        for fd in [-1, 0, 1, 2, i32::MAX] {
            assert!(unsafe { take_inherited(fd) }.is_err());
        }
    }

    #[test]
    fn helper_exit_before_handshake_reports_eof() {
        let (mut stream, remote) = UnixStream::pair().unwrap();
        // A leaked parent copy would keep the socket open after the helper
        // exits. Bound the read so that regression fails instead of hanging.
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let remote = above_stdio(remote).unwrap();
        let error = spawn(
            &mut Command::new("true"),
            vec![remote.into()],
            &mut stream,
            b"TEST",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}
