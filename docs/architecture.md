# Architecture

The first allocation starts a helper and establishes a private control channel.
The library sends the payload over that channel, and the helper creates the
memory-backed file and returns its absolute path. The selected owner process
and helper are separate: the helper can keep serving paths after the launcher
exits, provided ownership has been handed off to a live process.

## Linux

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

## macOS

By default, a volume manager shares private 64 MiB RAM disks among overlapping
leases from the same process and helper executable. It formats each disk as HFS
and mounts it using the system's `hdiutil`, `newfs_hfs`, and `mount` tools. The volume has
ownership enabled and uses `noexec,nosuid,nodev,nobrowse` mount flags. Its root
is private (`0700`), and payload files are read-only (`0400`). The returned path
names an ordinary file on this volume and preserves the requested suffix.

Before allocating or releasing a file, the helper pins the volume directory and
verifies its filesystem identity, ownership, permissions, and mount flags.
Directory-relative operations prevent a missing or replaced mount from
redirecting payload writes or cleanup into the disk-backed mount directory.
The owner is watched with `kqueue` process-exit notifications, with process birth
time checks to avoid confusing a reused PID with the original owner.

Lease cleanup unlinks only that lease's files. The pool reuses free space, adds
volumes when needed, and detaches surplus empty volumes while retaining one for
reuse. Closing the last scope and controller tears down the remaining volumes.
Each volume's setup worker remains alive as a guardian in a separate process
group and cleans up if the volume manager is killed. Volume commands have
ten-second deadlines;
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

Each volume's 64 MiB capacity includes filesystem overhead. The pool checks
free space before writing and spills into another volume on ENOSPC. A payload
that cannot fit an empty volume fails; partial files are removed while existing
payloads remain available. Pool growth has no fixed total capacity limit. The
RAM-disk backend provides no additional memory-lock guarantee.

A lease helper crash affects its own scope; a volume-manager crash affects every
scope in that pool. See [Shared macOS volumes](#shared-macos-volumes) for the
sharing boundary and the separate-volume build option.

## Other platforms

Allocation currently returns `io::ErrorKind::Unsupported` on Windows and other
operating systems. Windows shared-memory mappings are not ordinary file paths,
and `FILE_ATTRIBUTE_TEMPORARY` does not prevent disk writes. Preserving this
crate's path API and avoiding a disk-backed file requires a memory filesystem
backend, such as one built on [WinFsp](https://github.com/winfsp/winfsp).
See Microsoft's documentation on
[named shared memory](https://learn.microsoft.com/en-us/windows/win32/memory/creating-named-shared-memory)
and [temporary files](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew).

## Common limits

Payloads are limited to 64 MiB per request. Suffixes are limited to 80 ASCII
letters, digits, dots, underscores, or hyphens. Allocation failures never fall
back to ordinary temporary files.

The Linux lock covers only the helper's live file pages. Source buffers,
transport buffers, and consumer copies are not locked. Readers surviving
revocation can retain bytes after the helper unlocks them. Memory-backed storage
does not protect against hibernation images or every diagnostic capture, and
pathname revocation cannot erase bytes already copied by a reader.

## Shared macOS volumes

The default `shared-volumes` feature shares macOS RAM volumes across
independent leases created by the same process and matching helper executable.
Each lease retains its own helper, process-exit watcher, and private inherited
channel to the volume manager. Paths belong to one scope: another scope cannot
release them. Dropping a lease revokes only its paths; handing it off keeps its
helper and scope alive until its selected owner exits.

The last scope and controller closing triggers volume teardown. There is no idle
cache or installed service, so nonoverlapping leases still pay cold setup costs.
Each physical volume has a 64 MiB capacity including filesystem overhead. When
an allocation exhausts a populated volume, the manager tries the other volumes
and creates at most one additional volume for that request. An allocation that
cannot fit an empty volume fails without creating more volumes on retry. Only
ENOSPC triggers spillover; other failures are returned. Released space is reused,
and the pool keeps at most one empty volume attached for reuse. Surplus empty
volumes are detached synchronously when their last path is released. Failed
detaches remain journaled for recovery without revoking other scopes. The manager
checks available space on the validated volume before creating a payload file;
write errors remain authoritative if filesystem overhead still causes ENOSPC.
There is no fixed pool capacity limit, so peak RAM use scales with the number of volumes needed.

The pool has a shared volume-manager failure domain.
A lease helper crash revokes only its scope; a manager crash revokes all scopes,
with the guardian and existing orphan recovery handling volume cleanup. It is
enabled by default. Helpers and library consumers must be built with the same
feature configuration. Linux behavior is unchanged.

To use separate macOS volumes for each lease, set `default-features = false`
on the library dependency and build the helper with `--no-default-features`.
Cargo features are additive: another dependency enabling `shared-volumes` also
enables pooling for your application.

