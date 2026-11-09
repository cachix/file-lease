# Usage and deployment

## Ownership

The owner is the process whose exit should end access, identified by its PID.
Choose the launcher when it waits for its consumers, a shell when files belong
to a shell session, or a detached manager when it must outlive the launcher.
`ProcessInfo` provides metadata for applications that need to identify their
consuming shell or manager.

| Operation | Effect |
| --- | --- |
| `FileLease::new(pid)` | Select an owner. Storage and the helper are allocated lazily on the first `materialize` call. |
| `clone()` | Share the same helper and files. |
| Drop the last clone, before handoff | Request cleanup and wait for the helper to finish. |
| `handoff()` | Keep existing files until the original owner exits, even after every clone is dropped. |
| `handoff_to(pid)` | Transfer existing files to another owner and retain them until that process exits. |

`release(&path)` revokes one file without ending the lease. Owner exit triggers
cleanup whether or not the lease has been handed off.
After handoff, no clone can allocate additional files. Handoff on an empty
lease does nothing; materialize files before handing them off.

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

The helper wire protocol and recovery format are internal. Deploy matching
library and helper versions. They are not a public interoperability contract.

## Alternatives

| Alternative | When it fits | What `file-lease` adds |
| --- | --- | --- |
| [`tempfile`](https://crates.io/crates/tempfile) | Temporary files on a filesystem you choose. | Memory-backed storage without a disk-backed fallback, plus cleanup tied to a selected process. Named temporary files otherwise rely on destructors for cleanup. |
| [`memfd`](https://crates.io/crates/memfd), [`memfile`](https://crates.io/crates/memfile) | Creating and sealing anonymous files on supported systems. | A helper that hosts paths, owner monitoring, handoff, and a macOS backend. |
| [`memfd_path`](https://crates.io/crates/memfd_path) | An in-memory Linux file accessed through `/proc/self/fd/...`. | Paths hosted independently of the launcher, without requiring the consumer to inherit the file descriptor. |
| [`shared_memory`](https://crates.io/crates/shared_memory), [`native-ipc`](https://crates.io/crates/native-ipc) | Sharing mappings between cooperating processes. | File paths that ordinary programs can open without shared-memory integration. |

