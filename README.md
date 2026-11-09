# file-lease

[![CI](https://github.com/cachix/file-lease/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/cachix/file-lease/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/file-lease.svg)](https://crates.io/crates/file-lease)
[![Documentation](https://docs.rs/file-lease/badge.svg)](https://docs.rs/file-lease)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/cachix/file-lease/blob/main/LICENSE)
[![Rust: 1.92+](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://github.com/cachix/file-lease/blob/main/Cargo.toml)

**Memory-backed files for secrets, with automatic cleanup tied to a process.**

Linux uses sealed `memfd` files with locked memory, exposed through
`/proc/<helper-pid>/fd/<fd>`. macOS uses private HFS RAM disks that provide
ordinary filesystem paths. Neither backend falls back to regular disk-backed
temporary files.

Pass credentials, certificates, and generated configuration to programs that
expect a file. `file-lease` creates a read-only, memory-backed file and keeps its
path available until the lease ends or its selected owner process exits.

The consumer opens the path normally. It needs no library integration and does
not need to inherit a file descriptor.

## Get started

Requires Rust 1.92 or newer. Add the library to your application:

```toml
[dependencies]
file-lease = "1.0"
```

Install the matching helper executable:

```sh
cargo install file-lease --version 1.0.0 --locked
```

Use the same version and feature configuration for the library and helper.
You can also [embed the helper in your application](#helper-deployment).
To build the helper from a checkout, run `cargo install --path . --locked`
from the repository root and use that checkout as a path dependency.

With the helper installed, create a file and pass its path to a consumer:

```rust,no_run
use file_lease::{FileLease, FileOptions};
use std::{io, process::Command};

fn main() -> io::Result<()> {
    let lease = FileLease::new(std::process::id());
    let file = lease.materialize_with(
        b"endpoint = 'https://example.com'\n",
        &FileOptions::new().suffix(".toml"),
    )?;

    let status = Command::new("cat").arg(file.path()).status()?;
    drop(lease);

    if !status.success() {
        return Err(io::Error::other("consumer failed"));
    }
    Ok(())
}
```

Keep the lease alive for every read, including later reopens. A `LeasedFile`
names a file but does not retain it.

## Control the lifetime

Storage is allocated lazily on the first `materialize` call. The selected owner
is identified by its PID and watched by a separate helper. Use the launcher PID
when it waits for its consumers, or a shell or manager PID for longer-lived
files. `ProcessInfo` helps identify a consuming shell or manager.

| Operation | Behaviour |
| --- | --- |
| `FileLease::new(pid)` | Tie the lease to a process. Its exit triggers cleanup. |
| `clone()` | Share the same lease and files. |
| Drop the last clone | Clean up and wait, unless the lease has been handed off. |
| `release(&file)` | Revoke one file and keep the rest. |
| `handoff()` | Retain existing files until the original owner exits. |
| `handoff_to(pid)` | Transfer existing files to another live process. |

Hand off before replacing the launcher with `exec` or letting it exit. After
handoff, no clone can allocate more files. Handing off an empty lease has no
effect.

Owners are identified by PID, so choose a process that is alive when the first
file is created. A PID that has already exited and been reused would tie the
lease to the unrelated process.

Operations return `file_lease::Error`, whose `kind()` distinguishes invalid
owners, suffixes and sizes, handed-off leases, helper and storage failures, and
rejected releases or handoffs. It converts into `io::Error` for `?`.

### Replacing the launcher with the consumer

Unix `exec` replaces the current executable without changing its PID. Hand off
before `exec` so the helper keeps the files alive after the launcher's Rust
objects disappear. This example replaces the launcher with `cat`:

```rust,no_run
use file_lease::FileLease;
use std::{io, process::Command};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
fn main() -> io::Result<()> {
    let lease = FileLease::new(std::process::id());
    let file = lease.materialize(b"application data\n")?;
    lease.handoff()?;
    drop(lease); // The helper now retains the file until this PID exits.

    Err(Command::new("cat").arg(file.path()).exec())
}
# #[cfg(not(unix))]
# fn main() {}
```

For a detached consumer, call `handoff_to(consumer_pid)` while both the current
owner and consumer are still alive. Complete the handoff before letting the
launcher exit. If a program daemonizes, use the PID of the process that will
actually need the files.

## Helper deployment

By default, the library runs `file-lease` from the directory of the current
executable. It never searches PATH, since the helper receives every payload.
Use `with_helper_executable` to select another path; configure it before
cloning the lease.

Applications can also host the helper in their own executable. Dispatch its
internal invocations before argument parsing:

```rust,no_run
use file_lease::{FileLease, maybe_run_helper};

fn main() -> std::io::Result<()> {
    if let Some(code) = maybe_run_helper() {
        std::process::exit(code);
    }

    let lease = FileLease::new(std::process::id())
        .with_helper_executable(std::env::current_exe()?);
    let file = lease.materialize(b"application data")?;
    assert_eq!(std::fs::read(&file)?, b"application data");
    Ok(())
}
```

Library and helper versions and feature configurations must match. The helper
reports its build at startup, and a mismatch fails with `ErrorKind::Helper`
before any payload is sent. The wire protocol and recovery format are internal.
Helper readiness has a ten-second deadline; a helper that does not complete
the startup handshake is terminated and allocation returns `TimedOut`.

## Platform support

| Platform | Storage | Paths |
| --- | --- | --- |
| Linux | Sealed `memfd` files with pages locked before payload writes. | `/proc/<helper-pid>/fd/<fd>`; suffix hints are omitted. |
| macOS | Private HFS RAM volumes, shared across overlapping leases by default. | Ordinary file paths; suffix hints are preserved. |
| Windows | Not supported yet. | Allocation returns `Unsupported`. |

Other operating systems also return `Unsupported` on allocation.

On Linux, configure sufficient `RLIMIT_MEMLOCK`; allocation fails if pages cannot
be locked. Consumers must be able to open the helper's proc-fd paths directly.
Programs that canonicalize those paths to the deleted backing name may fail.

On macOS, each volume has 64 MiB including filesystem overhead. The pool adds
volumes when needed, reuses free space, and detaches surplus empty volumes.
Each lease has independent cleanup, while a volume-manager failure affects all
leases in that pool. Guardians and recovery journals handle orphaned volumes.
macOS provides no additional memory-lock guarantee.

[Architecture and guarantees](https://github.com/cachix/file-lease/blob/main/docs/architecture.md)
describes owner monitoring, crash recovery, capacity, and the separate-volume
build option. [Performance measurements](https://github.com/cachix/file-lease/blob/main/docs/performance.md)
cover startup, reuse, and cleanup costs on macOS 15 and 26.

## Limits

Payloads are limited to 64 MiB per request. Suffixes accept up to 80 ASCII
letters, digits, dots, underscores, or hyphens. macOS pool growth has no fixed
total capacity limit.

Revoking a path cannot erase bytes already copied by a reader. Existing open
readers may retain contents; source, transport, and consumer buffers are not
locked. Memory-backed storage does not protect against every diagnostic capture
or hibernation image.

## Development

See [contribution and testing instructions](https://github.com/cachix/file-lease/blob/main/docs/development.md).
CI exercises Linux, macOS 15, and macOS 26 with default features, all features,
and the separate-volume opt-out.

Licensed under [Apache 2.0](https://github.com/cachix/file-lease/blob/main/LICENSE).
