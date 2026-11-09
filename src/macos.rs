//! macOS RAM-volume storage and process-owned cleanup.
mod recovery;
mod setup;
mod storage;

pub(super) use setup::maybe_run_setup;
pub(super) use storage::Storage;

use crate::{command as tool_command, profiling::Span};
use rustix::fs::{AtFlags, unlinkat};
use std::{
    fs, io,
    mem::MaybeUninit,
    os::fd::{FromRawFd, OwnedFd},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const REQUIRED_MOUNT_FLAGS: u32 =
    (libc::MNT_NOEXEC | libc::MNT_NOSUID | libc::MNT_NODEV | libc::MNT_DONTBROWSE) as u32;

fn validate_mount_flags(flags: u32) -> io::Result<()> {
    if flags & REQUIRED_MOUNT_FLAGS != REQUIRED_MOUNT_FLAGS
        || flags & libc::MNT_IGNORE_OWNERSHIP as u32 != 0
    {
        return Err(io::Error::other("RAM-disk security mount flags changed"));
    }
    Ok(())
}

fn unlink_payload(root: &fs::File, name: impl rustix::path::Arg) -> io::Result<()> {
    match unlinkat(root, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn c_string(bytes: &[libc::c_char]) -> &[u8] {
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    // c_char has the same representation and alignment as u8.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast(), length) }
}

pub(super) fn watch_owner(pid: u32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::kqueue() };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let queue = unsafe { OwnedFd::from_raw_fd(fd) };
    crate::helper::close_on_exec(fd)?;
    let event = libc::kevent {
        ident: pid as _,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    if unsafe { libc::kevent(fd, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(queue)
}

pub(super) fn process_info(pid: u32) -> io::Result<libc::proc_bsdinfo> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let result = unsafe {
        libc::proc_pidinfo(
            pid as _,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as _,
        )
    };
    if result as usize != size {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.assume_init() })
}

const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
fn command(program: &str, args: &[&std::ffi::OsStr]) -> io::Result<()> {
    command_until(program, args, Instant::now() + TOOL_TIMEOUT)
}

fn command_until(program: &str, args: &[&std::ffi::OsStr], deadline: Instant) -> io::Result<()> {
    let _span = Span::new(match program {
        "/sbin/newfs_hfs" => "tool.format",
        "/sbin/mount" => "tool.mount",
        "/sbin/umount" => "tool.unmount",
        "/usr/bin/hdiutil" => "tool.detach",
        _ => "tool.other",
    });
    // Volume tools operate only on metadata. Avoid pipe-draining threads which
    // could themselves wait forever if a tool leaves descendants holding pipes.
    tool_command::run_until(
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            // During setup stdout is the lock file; all in-flight tools must
            // retain it if the guardian dies. After setup stdout is /dev/null.
            .stdout(Stdio::inherit())
            .stderr(Stdio::null()),
        deadline.min(Instant::now() + TOOL_TIMEOUT),
    )
    .map_err(|error| io::Error::new(error.kind(), format!("{program}: {error}")))
}

#[cfg(test)]
mod tests;
