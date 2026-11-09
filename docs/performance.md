# Performance

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
amortizes volume creation today.


## Shared-volume measurements

A [native macOS 26 comparison](https://github.com/cachix/file-lease/actions/runs/42591111094)
used three alternating rounds, with 30 sequential joins and twelve concurrent
joins per variant. Medians for overlapping independent leases were:

| macOS 26 operation | Separate volumes | Shared volume |
| --- | ---: | ---: |
| Sequential join, including first file | 263.68 ms | 6.64 ms |
| Independent sequential drop | 35.26 ms | 0.90 ms |
| Concurrent join | 422.50 ms | 10.05 ms |
| Initial cold setup | 253.39 ms | 302.57 ms |

Each round created fifteen distinct volumes with the baseline and one with the
prototype. Cold setup still attaches and formats a RAM disk and adds the manager
process; this design accelerates reuse, not the first lease. All native isolation
and recovery tests passed on macOS 26 and macOS 15. The measurements below
document the evolution of the default shared-volume backend.

`cargo run --locked --release --example
shared_volume_timings` measures cold setup, sequential joins while an anchor lease
remains live, concurrent joins, independent drops, and final teardown. CI compares
this harness against a fixed unpooled baseline on macOS 26 and macOS 15, and
records both durations and the number of distinct volumes created. A separate
24 MiB payload comparison forces spillover while checking cleanup and payload
integrity. Run it locally with `-- --samples 3 --parallel 4 --payload-mib 24`.

The same run tested spillover with 24 MiB payloads. One anchor, three sequential
leases, and four overlapping concurrent leases created eight distinct volumes in
the baseline and three in the pool. macOS 26 median sequential joins were
287.10 ms versus 160.44 ms; concurrent joins were 1544.82 ms versus 565.95 ms.
These are only three sequential and four concurrent samples per variant, run
baseline first. macOS 15 concurrent joins were slower in the pool, 483.88 ms
versus 441.08 ms, so this is diagnostic evidence rather than a general throughput
claim. That version serialized allocation and could write and remove a partial
file on a full volume before trying the next volume. Its final teardown included
every retained volume: the macOS 26 anchor drop took 182.26 ms for the pool versus
49.34 ms for the baseline's remaining volume.

Both macOS hosts passed 48 unit tests, three helper integration tests, and three
documentation tests, including bounded oversized allocations, released-space
reuse, cross-volume scope isolation and handoff, and orphan recovery after both
volume guardians and the manager were killed.

A [follow-up native comparison](https://github.com/cachix/file-lease/actions/runs/42592834712)
tested free-space preflight and reclaiming surplus empty volumes against that
previous pool (`f42cf6d`) and the same separate-volume baseline. Each variant ran
in three rotating-order rounds on the same runner. macOS 26 medians were:

| Operation | Previous pool | Preflight and reclamation |
| --- | ---: | ---: |
| Small-file sequential join (30 samples) | 4.36 ms | 4.32 ms |
| 24 MiB sequential join (9 samples) | 134.41 ms | 102.47 ms |
| 24 MiB concurrent join (12 samples) | 563.42 ms | 413.98 ms |
| Final anchor drop (3 samples) | 101.35 ms | 63.86 ms |

Reclamation moves some detach latency into individual drops. For the 24 MiB
concurrent workload, median drop latency was 1.87 ms versus 2.03 ms, but p95
increased from 2.06 ms to 40.45 ms when a surplus volume was detached. Both pools
created three distinct volumes per round; the new policy retains at most one
empty volume instead of every volume until shutdown. On macOS 15, concurrent
joins improved from 418.86 ms to 353.05 ms. These synthetic samples establish the
tradeoff for this workload, not a general throughput or memory-footprint claim.
Both macOS hosts passed 49 unit tests, three helper integration tests, and three
documentation tests. Linux CI also passed. The public API is unchanged.

