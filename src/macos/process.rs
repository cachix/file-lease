//! Process identity and kernel exit notifications.
use std::{
    io,
    mem::MaybeUninit,
    os::fd::{FromRawFd, OwnedFd},
};

pub(crate) fn watch_owner(pid: u32) -> io::Result<OwnedFd> {
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

pub(crate) fn process_info(pid: u32) -> io::Result<libc::proc_bsdinfo> {
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
