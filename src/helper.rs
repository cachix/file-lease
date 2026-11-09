//! Process lifecycle shared by the lease helper and volume manager.

use std::{
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
};

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
    let mut ready = [0; 4];
    let handshake = stream.read_exact(&mut ready).and_then(|_| {
        if &ready == expected {
            Ok(())
        } else {
            Err(io::Error::other("Incompatible file lease keeper"))
        }
    });
    if let Err(error) = handshake {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
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
    use std::time::Duration;

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
