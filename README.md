# file-lease

Give a program a file path without first writing the payload to disk.
`file-lease` creates memory-backed files on Linux and macOS, keeps their paths
available while a chosen process needs them, and cleans up when the lease ends.

Use it when a program expects a path to a credential, certificate, or generated
configuration file. The consumer opens the path normally; it needs no library
integration or descriptor-passing protocol. A separate helper holds the storage
and watches the process you select as its owner.

This is currently an **unpublished standalone crate**, with a 0.1.0 API.

## Quick start

Build and install the helper from a checkout of this repository:

```sh
git clone https://github.com/cachix/file-lease.git
cd file-lease
cargo install --path . --locked
```

Deploy that matching `file-lease` executable alongside your application or on
PATH. The helper handshake rejects older, incompatible versions before any
payload is sent; upgrade the library and helper together.
To use the library before publication, add a path dependency pointing to
your checkout:

```toml
[dependencies]
file-lease = { path = "../file-lease" }
```

This example gives `cat` a generated configuration file and keeps the lease
alive until the command finishes. Run it on Linux or macOS with the helper
installed:

```rust,no_run
use file_lease::FileLease;
use std::{io, process::Command};

fn main() -> io::Result<()> {
    let lease = FileLease::new(std::process::id());
    let path = lease.materialize(b"endpoint = 'https://example.com'\n", None)?;

    let status = Command::new("cat").arg(&path).status()?;
    drop(lease); // Request cleanup now that the consumer has finished.

    if !status.success() {
        return Err(io::Error::other("consumer failed"));
    }
    Ok(())
}
```

Keep the lease alive for every read the consumer may make, including later
reopens. A `PathBuf` alone does not retain the file. In this example the owner is
the launcher, so its exit also triggers cleanup even if `cat` is still running.

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

## How it works

The first allocation starts a helper and establishes a private control channel.
The library sends the payload over that channel, and the helper creates the
memory-backed file and returns its absolute path. The selected owner process
and helper are separate: the helper can keep serving paths after the launcher
exits, provided ownership has been handed off to a live process.

### Linux

Each payload gets its own anonymous file created with `memfd_create()`. The
helper sizes the file, maps it read-only with `MAP_SHARED`, excludes the mapping
from ordinary process core dumps with `MADV_DONTDUMP`, and calls `mlock()` to
populate and lock its zero-filled pages **before writing the payload**. It then
writes the bytes and seals the file against writes, growth, shrinkage, and
changes to its seals. File permissions are `0400`.

The returned path is `/proc/<helper-pid>/fd/<fd>`. A consumer opens that path
normally without inheriting a descriptor. The helper holds the descriptor and
locked mapping, and watches the selected owner through a pidfd. Releasing a file
closes its descriptor and unmaps its pages; owner exit or lease cleanup releases
all remaining files. Killing the helper also closes its descriptors and removes
these paths.

The helper inherits `RLIMIT_MEMLOCK`. Nonempty files consume their lengths
rounded up to whole pages; empty files need no mapping. Allocation fails if
locking fails, with no pageable or disk-backed fallback. Configure a sufficient
memory-lock limit before launching the application. Releasing a file returns
its lock budget. See [`mlock(2)`](https://man7.org/linux/man-pages/man2/mlock.2.html).

Suffix hints are accepted but omitted from Linux paths. Consumers must open the
proc-fd path directly. Programs that canonicalize it to the memfd's deleted
backing name may reject it. For example, PHP's ordinary `fopen()` and
`file_get_contents()` are affected. Check the actual consumer before relying on
this backend. Access also depends on the consumer's ability to open the helper's
proc-fd entries, including process visibility and permission restrictions.

Descriptor numbers are never reused within a lease, so a revoked path cannot
expose a later file. Total allocations per lease are bounded by the helper's
file-descriptor limit. Use a new lease for each resolution or refresh batch.
Existing open readers can retain contents after pathname revocation until they
close their descriptors.

### macOS

The helper creates a private 64 MiB RAM disk, formats it as HFS, and mounts it
using the system's `hdiutil`, `newfs_hfs`, and `mount` tools. The volume has
ownership enabled and uses `noexec,nosuid,nodev,nobrowse` mount flags. Its root
is private (`0700`), and payload files are read-only (`0400`). The returned path
names an ordinary file on this volume and preserves the requested suffix.

Before allocating or releasing a file, the helper pins the volume directory and
verifies its filesystem identity, ownership, permissions, and mount flags.
Directory-relative operations prevent a missing or replaced mount from
redirecting payload writes or cleanup into the disk-backed mount directory.
The owner is watched with `kqueue` process-exit notifications, with process birth
time checks to avoid confusing a reused PID with the original owner.

Normal cleanup unlinks the files, unmounts the volume, and detaches the RAM disk.
The setup worker remains alive as a guardian in a separate process group and
cleans up if the helper is killed. Volume commands have ten-second deadlines;
failed cleanup is retried up to three times with backoff. If both processes die,
or cleanup keeps failing, a later lease retries recovery. No background service
is installed.

Recovery journals contain metadata, never payload bytes, under
`~/Library/Application Support/file-lease`. A unique lease identifier registered
with the RAM disk allows recovery to identify an interrupted attachment before
its device name was recorded. Recovery checks identities and conservatively
preserves attachments it cannot identify safely. Failures before attachment
starts remove unused metadata.

Startup recovery shares a five-second budget across scanning and volume-tool
waits, leaving unfinished work for a later lease. Individual OS calls can still
take longer. Persistent cleanup failures are recorded in the lease directory's
private `cleanup.error` file and removed with the recovered lease. Detached
helpers do not retain application output pipes for diagnostics.

The 64 MiB capacity includes filesystem overhead and all files in that lease.
A full volume rejects new allocations and removes partial files while preserving
existing payloads. The RAM-disk backend provides no additional memory-lock
guarantee.

### Other platforms

Allocation currently returns `io::ErrorKind::Unsupported` on Windows and other
operating systems. Windows shared-memory mappings are not ordinary file paths,
and `FILE_ATTRIBUTE_TEMPORARY` does not prevent disk writes. Preserving this
crate's path API and avoiding a disk-backed file requires a memory filesystem
backend, such as one built on [WinFsp](https://github.com/winfsp/winfsp).
See Microsoft's documentation on
[named shared memory](https://learn.microsoft.com/en-us/windows/win32/memory/creating-named-shared-memory)
and [temporary files](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew).

### Common limits

Payloads are limited to 64 MiB per request. Suffixes are limited to 80 ASCII
letters, digits, dots, underscores, or hyphens. Allocation failures never fall
back to ordinary temporary files.

The Linux lock covers only the helper's live file pages. Source buffers,
transport buffers, and consumer copies are not locked. Readers surviving
revocation can retain bytes after the helper unlocks them. Memory-backed storage
does not protect against hibernation images or every diagnostic capture, and
pathname revocation cannot erase bytes already copied by a reader.

## Performance measurements

A [native CI run on 2026-11-06](https://github.com/cachix/file-lease/actions/runs/42271739247)
measured release builds on ARM64 runners with 256-byte payloads. Each sequential
sample creates one lease and twenty additional files; ten leases give ten setup
and cleanup measurements and 200 additional-file measurements.

| Sequential operation, median | macOS 15 | macOS 26 |
| --- | ---: | ---: |
| First file, including setup | 209 ms | 290 ms |
| Additional file | 0.36 ms | 0.59 ms |
| Cleanup | 22 ms | 38 ms |

The additional-file p95 was 0.49 ms and 1.99 ms respectively, but maxima reached
42 ms and 44 ms. Instrumented measurements, pooled across ten sequential and four
concurrent leases, located the long tail in closing the payload's write
descriptor. Its p95 was about 39-40 ms, while payload writes took 4-8 microseconds
median. [Apple's kernel source](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_vnops.c)
flushes HFS files when their last writer closes, consistent with this behavior;
these measurements do not include kernel stack traces proving the flush's cost.

RAM-disk attachment was the largest individual setup phase. IOKit identity
lookups took roughly 0.3-0.6 ms median, rather than causing the 40 ms spikes.
These are diagnostic samples, not performance guarantees: runner load matters,
and instrumented builds add IO overhead. Raw records and summaries are attached
to the linked run.

### Retained-writer experiment

An [A/B run on 2026-11-07](https://github.com/cachix/file-lease/actions/runs/42397249815)
compared the current implementation with
[`perf/macos-retained-writers`](https://github.com/cachix/file-lease/tree/perf/macos-retained-writers).
Both versions used the same release-build harness on each runner, alternating
order across three rounds. Each variant supplied 30 sequential leases with 600
additional files, plus 12 concurrent leases with 240 additional files. Half the
files were released individually; the remainder were revoked by dropping the
lease. Values below are milliseconds, current implementation to retained writers.

| Measurement | macOS 15 | macOS 26 |
| --- | ---: | ---: |
| Concurrent additional-file p95 | 41.66 → 4.14 | 43.01 → 4.57 |
| Concurrent individual-release p95 | 2.57 → 42.39 | 1.91 → 43.96 |
| Sequential cleanup median | 25.72 → 23.48 | 23.27 → 24.88 |
| Sequential timed-lifecycle median | 288.69 → 253.62 | 263.56 → 272.79 |

Timed lifecycle sums setup, all allocations, individual releases, and cleanup;
it excludes verification reads. Retaining writers smooths allocation latency but
moves stalls into explicit release, with no consistent overall improvement
across these runners. It also retains a descriptor for every live file. The
candidate passed all native tests, including descriptor cleanup, crash recovery,
and volume exhaustion. It remains an experiment; the default implementation
continues to close writers after allocation.

### Attachment experiment, macOS 26

A [native run on 2026-11-08](https://github.com/cachix/file-lease/actions/runs/42471350859/attempts/2)
compared attachment changes with the current implementation, using the same
release-build harness and alternating execution order across three rounds.
macOS 26 was the optimization target; macOS 15 supplied compatibility coverage.

Forcing `hdiutil attach -kernel` failed with `Function not implemented` on the
macOS 26 runner. It could not complete setup, so it was rejected rather than
benchmarked as a working backend. The native tool help and direct probes are
included in the run's artifacts.

A narrower candidate passed storage and recovery tests: `-noverify -noautofsck`
omits image checksum and filesystem checks for the newly created blank RAM disk.
It preserves lease-ID tagging and all later device and mount validation. Each
variant contributed 30 sequential leases with 600 additional files, plus twelve
concurrent leases with 240 additional files.

| macOS 26 measurement, median | Current | Omit unused checks |
| --- | ---: | ---: |
| Sequential first file, including setup | 226.01 ms | 228.86 ms |
| Sequential timed lifecycle | 260.36 ms | 269.04 ms |
| Concurrent first file, including setup | 257.30 ms | 306.92 ms |

These diagnostic samples show no startup improvement. Both candidates remain
on [`perf/macos-attachment`](https://github.com/cachix/file-lease/tree/perf/macos-attachment)
and its history; the default attachment behavior is unchanged. Reusing a lease
amortizes volume creation today. Sharing a volume across independent leases
would need a separate ownership and isolation design.

## Shared-volume prototype

The experimental `shared-volumes` feature shares one macOS RAM volume across
independent leases created by the same process and matching helper executable.
Each lease retains its own helper, process-exit watcher, and private inherited
channel to the volume manager. Paths belong to one scope: another scope cannot
release them. Dropping a lease revokes only its paths; handing it off keeps its
helper and scope alive until its selected owner exits.

The last scope and controller closing triggers volume teardown. There is no idle
cache or installed service, so nonoverlapping leases still pay cold setup costs.
The prototype shares a 64 MiB capacity budget and a volume-manager failure domain.
A lease helper crash revokes only its scope; a manager crash revokes all scopes,
with the guardian and existing orphan recovery handling volume cleanup. It is
not enabled by default, and helpers must be built with the matching feature.
Linux behavior is unchanged.

A [native macOS 26 comparison](https://github.com/cachix/file-lease/actions/runs/42472849767)
used three alternating rounds, with 30 sequential joins and twelve concurrent
joins per variant. Medians for overlapping independent leases were:

| macOS 26 operation | Separate volumes | Shared volume |
| --- | ---: | ---: |
| Sequential join, including first file | 236.82 ms | 6.67 ms |
| Independent sequential drop | 32.94 ms | 1.12 ms |
| Concurrent join | 304.20 ms | 9.07 ms |
| Initial cold setup | 237.70 ms | 274.46 ms |

Each round created fifteen distinct volumes with the baseline and one with the
prototype. Cold setup still attaches and formats a RAM disk and adds the manager
process; this design accelerates reuse, not the first lease. All native isolation
and recovery tests passed on macOS 26 and macOS 15. The opt-in prototype remains
on [`perf/macos-shared-volume`](https://github.com/cachix/file-lease/tree/perf/macos-shared-volume)
while shared-capacity and manager-failure tradeoffs are evaluated.

`cargo run --locked --release --features shared-volumes --example
shared_volume_timings` measures cold setup, sequential joins while an anchor lease
remains live, concurrent joins, independent drops, and final teardown. CI compares
this harness against a fixed unpooled baseline on macOS 26 and macOS 15, and
records both durations and the number of distinct volumes created.

## Development

`cargo test --locked -- --test-threads=1` runs the storage, owner-exit,
ownership-transfer, helper-crash, closed-descriptor, and macOS setup-recovery
tests. CI runs these on native Linux and macOS hosts.

`cargo run --locked --release --example lifecycle_timings` measures first-file
setup, additional-file allocation, and cleanup with synthetic 256-byte files.
By default it runs ten sequential leases and four parallel leases, with twenty
additional files per lease. Override counts with `-- --samples 20 --files 50
--parallel 4`. Compilation is outside the timers. These are diagnostic timings,
not performance thresholds or measurements of the RAM disk's memory footprint.

For internal phase timings, build with `--features profiling` and set
`_FILE_LEASE_PROFILE_DIR` to an existing private directory. Each process appends
CSV records containing only a fixed phase name, PID, and elapsed microseconds.
This optional feature is disabled by default and never records payloads or
paths. Nested spans include their children and diagnostic IO, so their totals
must not be added. Compare with an uninstrumented release build when evaluating
performance. CI runs both builds, summarizes median/p95/range values, and uploads
raw CSV files as `lifecycle-timings-*` artifacts.

CI runs the complete native test suite, including crash recovery and full RAM-volume
regressions, on Linux, macOS 15, and macOS 26. Contributions should pass
`cargo fmt --all -- --check` and `cargo clippy --locked --all-targets -- -D warnings`
as well as the test command above. Changes assisted by AI should disclose that
assistance in the pull request.
