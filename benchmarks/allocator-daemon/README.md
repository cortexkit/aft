# AFT allocator daemon benchmark

Compare the parent system-allocator build with the mimalloc build under the same long-lived SubC daemon workload.

This benchmark is an evidence protocol. It does not contain accepted allocator results. Record results only after both arms run on the same host with the same repository roots and configuration.

## Coverage boundary

The mimalloc arm installs mimalloc through Rust `GlobalAlloc`. Rust-owned heap allocations use mimalloc. Native libraries can still allocate through the platform allocator. This includes SQLite, tree-sitter, ONNX Runtime, and other C or C++ dependencies unless their build explicitly routes `malloc` through mimalloc.

The idle relief pass therefore covers both domains:

- `mi_collect(true)` releases unused mimalloc pages.
- `malloc_trim(0)` requests glibc native-heap relief on Linux.
- `malloc_zone_pressure_relief(NULL, 0)` requests native-zone relief on macOS.

Process RSS, macOS physical footprint, SQLite bytes, and subsystem estimates remain independent checks. Mimalloc statistics do not represent the full process.

## Required arms

| Arm | Build | Purpose |
|---|---|---|
| `system` | Parent commit of the mimalloc change | Baseline platform allocator behavior |
| `mimalloc` | PR branch | Rust allocator change with dual-domain idle relief |

Build both binaries from clean worktrees. Do not compare binaries with different AFT features or root-index code.

## Required workload

Use at least seven real Git roots. Include small, medium, and large roots. Use the same absolute root paths and selected search, semantic, and callgraph indexes for both arms.

Run these phases in order:

1. **Cold build**: Clear only AFT index storage. Start the isolated SubC daemon. Wait until every selected root artifact reaches a terminal state.
2. **Steady serving**: Issue a fixed reader corpus at a fixed rate while the daemon remains bound. Include read, grep, glob, outline, and callgraph queries.
3. **Idle eviction**: Close every route. Wait for the configured idle-root eviction boundary. Confirm that the daemon reports each root eviction.
4. **Post-relief idle**: Keep the daemon alive for at least two allocator scan intervals. Do not submit new work.

Use an isolated connection file, config root, data root, and log root for each arm. Never point this benchmark at the production SubC daemon.

## Sampling

Sample at five-second intervals. Record these columns:

```text
timestamp,arm,phase,pid,rss_bytes,phys_footprint_bytes,vm_swap_bytes,cpu_percent,thread_count,open_routes,live_actor_roots,allocator_slack_bytes,allocator_slack_measured,sqlite_bytes,total_attributed_bytes
```

Linux obtains RSS and swap from `/proc/<pid>/status`. macOS obtains RSS and physical footprint from `proc_pidinfo` and `proc_pid_rusage`, matching AFT's `memory.rs` implementation. Obtain allocator, SQLite, root, and route values from the existing SubC health memory and runtime rollups. Keep field names and byte units unchanged.

Capture these events with timestamps:

- daemon ready
- each root artifact completion
- steady-serving start and stop
- each idle-root eviction
- each allocator pressure-relief log
- daemon shutdown

## Controls

- Use the same host without other build or indexing work.
- Run the arms in alternating order across at least three pairs.
- Reboot or allow the host to return to the same memory-pressure baseline before each pair.
- Keep power mode, CPU governor, semantic backend, model cache, and root revisions fixed.
- Preserve model downloads between arms. Clear generated AFT indexes between arms.
- Exclude a pair when either arm has a root failure, daemon restart, transport timeout, or changed Git revision.

## Report

Report each pair separately and then report the median difference. Include:

- peak RSS during cold build
- peak macOS physical footprint during cold build
- p50 and p99 reader latency during steady serving
- artifact build completion time
- RSS and physical footprint immediately before eviction
- RSS and physical footprint after each relief pass
- final RSS, physical footprint, and swap after post-relief idle
- allocator slack, SQLite bytes, and attributed bytes at every phase boundary

Do not use RSS alone on macOS. `MADV_FREE` can leave reclaimable pages visible in RSS after the allocator surrendered them. Physical footprint is the user-visible held-memory check for that platform.

Do not claim that mimalloc reclaims native allocations from mimalloc statistics. Attribute a reduction to the combined relief pass unless a dedicated native-allocation experiment isolates the allocator domain.
