# Changelog

## [Unreleased]

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
