//! Bounded invocations of macOS volume tools, preserving the setup lock.
use crate::{command as tool_command, profiling::Span};
use std::{
    io,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(super) const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) fn command(program: &str, args: &[&std::ffi::OsStr]) -> io::Result<()> {
    command_until(program, args, Instant::now() + TOOL_TIMEOUT)
}

pub(super) fn command_until(
    program: &str,
    args: &[&std::ffi::OsStr],
    deadline: Instant,
) -> io::Result<()> {
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
