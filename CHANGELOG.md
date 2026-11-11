# Changelog

## [Unreleased]

- Add `FileOptions::filename` to expose an exact filename through a symlink in
  a private temporary directory on Linux and macOS. The helper cleans up links
  on release or owner exit, including after handoff; payloads remain memory-backed.
  Filename validation errors use `ErrorKind::InvalidFilename`. Rebuild the helper
  with the library because the internal handshake has changed.

- Always share macOS RAM volumes across overlapping leases and remove the
  `shared-volumes` feature. Builds that disabled default features to get
  separate volumes now share them; builds naming the feature must drop it.
  Linux helpers no longer reject libraries built with a different feature set.

## [0.1.0] - 2026-11-09

- Stop searching PATH for the helper. Without `with_helper_executable`, the
  helper must be installed beside the current executable.

- Check the helper's crate version and feature set during startup, rejecting
  mismatched helpers before sending payloads.

- Establish the public API. `materialize` returns a `LeasedFile` handle and takes
  no suffix; use `materialize_with` and `FileOptions` for one. `release` takes
  a `LeasedFile`. Operations return a typed `Error` with an `ErrorKind`, which
  converts into `io::Error`. `maybe_run_keeper` and `with_keeper_executable`
  are renamed to `maybe_run_helper` and `with_helper_executable`.
  `ProcessInfo` is `#[non_exhaustive]` and implements `Clone` and `Debug`;
  `FileLease` implements `Debug`. The `profiling` feature is renamed to
  `unstable-profiling` and is not covered by semver.

- Fail the macOS shared volume manager closed when graceful scope cleanup
  cannot revoke a path, allowing its guardian to recover the storage.

- Recover macOS lease directories after a failed RAM disk attachment, and
  after another volume reuses a detached lease's disk number.

- Treat macOS payloads deleted by their consumers as released, so one deleted
  file no longer stops the shared volume manager and revokes every other lease.

- Share macOS RAM volumes across overlapping leases by default, with capacity
  spillover, independent file cleanup, and reclamation of surplus empty volumes.
  Disable default features to use separate volumes instead.

- Avoid known-full macOS shared volumes before writing payloads and reclaim
  surplus empty volumes while retaining one for fast reuse.

- Allow macOS shared leases to spill into additional RAM volumes
  when capacity is exhausted, reusing released space across leases.

- Add macOS shared-volume storage for overlapping independent
  leases, retaining separate cleanup, process-exit revocation, and owner handoff
  within one process.

- Add optional metadata-only phase profiling for file-lease latency diagnostics,
  including individual macOS file operations.

- Provide process-owned memory-backed files on Linux and macOS with automatic
  cleanup, owner handoff, and recovery of orphaned macOS RAM volumes.
- Maintain file leases independently of SecretSpec, with the same 0.1.0 API.
