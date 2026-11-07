# Changelog

## [Unreleased]

- Experiment with retaining macOS payload write descriptors until release to
  defer HFS close-time flushes; each live file retains one descriptor.

- Add optional metadata-only phase profiling for file-lease latency diagnostics,
  including individual macOS file operations.

- Provide process-owned memory-backed files on Linux and macOS with automatic
  cleanup, owner handoff, and recovery of orphaned macOS RAM volumes.
- Maintain file leases independently of SecretSpec, with the same 0.1.0 API.
