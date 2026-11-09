# Development

`cargo test --locked -- --test-threads=1` runs the storage, owner-exit,
ownership-transfer, helper-crash, closed-descriptor, and macOS setup-recovery
tests. CI runs default, all-feature, and no-default-feature builds on native
Linux and macOS hosts.

The README example is a Cargo documentation test. CI also compiles the examples
in `docs/usage.md` with rustdoc so the longer deployment examples stay checked.

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
