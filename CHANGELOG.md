# Changelog

## [Unreleased]

- Add an opt-in macOS shared-volume prototype for overlapping independent
  leases, retaining separate cleanup, process-exit revocation, and owner handoff
  within one process.

- Add optional metadata-only phase profiling for file-lease latency diagnostics,
  including individual macOS file operations.

- Provide process-owned memory-backed files on Linux and macOS with automatic
  cleanup, owner handoff, and recovery of orphaned macOS RAM volumes.
- Maintain file leases independently of SecretSpec, with the same 0.1.0 API.
