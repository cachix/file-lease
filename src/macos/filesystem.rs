//! Mount security checks and descriptor-relative payload removal.
use rustix::fs::{AtFlags, unlinkat};
use std::{fs, io};

pub(super) const REQUIRED_MOUNT_FLAGS: u32 =
    (libc::MNT_NOEXEC | libc::MNT_NOSUID | libc::MNT_NODEV | libc::MNT_DONTBROWSE) as u32;

pub(super) fn validate_mount_flags(flags: u32) -> io::Result<()> {
    if flags & REQUIRED_MOUNT_FLAGS != REQUIRED_MOUNT_FLAGS
        || flags & libc::MNT_IGNORE_OWNERSHIP as u32 != 0
    {
        return Err(io::Error::other("RAM-disk security mount flags changed"));
    }
    Ok(())
}

pub(super) fn unlink_payload(root: &fs::File, name: impl rustix::path::Arg) -> io::Result<()> {
    match unlinkat(root, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn c_string(bytes: &[libc::c_char]) -> &[u8] {
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    // c_char has the same representation and alignment as u8.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast(), length) }
}
