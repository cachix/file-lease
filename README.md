# file-lease

**Memory-backed files with ordinary paths and process-owned lifetimes.**

Pass credentials, certificates, and generated configuration to programs that
expect a file. `file-lease` creates a read-only, memory-backed file and keeps its
path available until the lease ends or its selected owner process exits.

The consumer opens the path normally. It needs no library integration and does
not need to inherit a file descriptor. Allocation never falls back to a regular
temporary file.

## Get started

The crate is not yet published. Build the helper from a checkout:

```sh
git clone https://github.com/cachix/file-lease.git
cd file-lease
cargo install --path . --locked
```

Add the checkout as a dependency in your application:

```toml
[dependencies]
file-lease = { path = "../file-lease" }
```

With the helper installed, create a file and pass its path to a consumer:

```rust,no_run
use file_lease::FileLease;
use std::{io, process::Command};

fn main() -> io::Result<()> {
    let lease = FileLease::new(std::process::id());
    let path = lease.materialize(b"endpoint = 'https://example.com'\n", Some(".toml"))?;

    let status = Command::new("cat").arg(&path).status()?;
    drop(lease);

    if !status.success() {
        return Err(io::Error::other("consumer failed"));
    }
    Ok(())
}
```

Keep the lease alive for every read, including later reopens. A `PathBuf` alone
does not retain the file.

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
| `release(&path)` | Revoke one file and keep the rest. |
| `handoff()` | Retain existing files until the original owner exits. |
| `handoff_to(pid)` | Transfer existing files to another live process. |

Hand off before replacing the launcher with `exec` or letting it exit. After
handoff, no clone can allocate more files. Handing off an empty lease has no
effect.

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
    let path = lease.materialize(b"application data\n", None)?;
    lease.handoff()?;
    drop(lease); // The helper now retains the file until this PID exits.

    Err(Command::new("cat").arg(&path).exec())
}
# #[cfg(not(unix))]
# fn main() {}
```

For a detached consumer, call `handoff_to(consumer_pid)` while both the current
owner and consumer are still alive. Complete the handoff before letting the
launcher exit. If a program daemonizes, use the PID of the process that will
actually need the files.

## Helper deployment

By default, the library looks for `file-lease` beside the current executable,
then on PATH. Use `with_keeper_executable` to select an explicit helper path;
configure it before cloning the lease.

Applications can also host the helper in their own executable. Dispatch its
internal invocations before argument parsing:

```rust,no_run
use file_lease::{FileLease, maybe_run_keeper};

fn main() -> std::io::Result<()> {
    if let Some(code) = maybe_run_keeper() {
        std::process::exit(code);
    }

    let lease = FileLease::new(std::process::id())
        .with_keeper_executable(std::env::current_exe()?);
    let path = lease.materialize(b"application data", None)?;
    assert_eq!(std::fs::read(&path)?, b"application data");
    Ok(())
}
```

Library and helper versions and feature configurations must match. The helper
wire protocol and recovery format are internal.

## Platform support

| Platform | Storage | Paths |
| --- | --- | --- |
| Linux | Sealed `memfd` files with pages locked before payload writes. | `/proc/<helper-pid>/fd/<fd>`; suffix hints are omitted. |
| macOS | Private HFS RAM volumes, shared across overlapping leases by default. | Ordinary file paths; suffix hints are preserved. |
| Windows | Not supported yet. | Allocation returns `Unsupported`. |

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
