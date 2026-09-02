# Implementation Plan: Pressure-Aware Standing Index Scheduler

## Goal

Keep configured standing indexes progressing across many roots without making the AFT daemon unresponsive or consuming laptop power continuously. Replace the current submit-every-root loop with a process-wide fair scheduler. Make the safe resource policy the default. Add an explicit user-only performance policy for operators who accept unrestricted background power use.

## Configuration Contract

Add this user-tier configuration:

```jsonc
{
  "index": {
    "resource_policy": "balanced",
    "roots": []
  }
}
```

`resource_policy` accepts:

| Value | Behavior |
|---|---|
| `balanced` | Default. Admit bounded background slices only while the host has adequate resources. Pause new slices on battery-saving or high-pressure signals. Keep interactive readers independent. |
| `performance` | Ignore battery and host-pressure admission signals. Keep queue bounds, cancellation, publication fences, and background thread priority demotion. |

The field remains user-only with `index.roots`. Project configuration cannot weaken the machine owner policy. Unknown values fail config validation. Existing configurations resolve to `balanced`.

## Architecture

### Process-Wide Fair Scheduler

Replace the loop in `StandingActor::tick` that submits every active root. Add scheduler state owned by `StandingActor`:

- A stable ring ordered by the normalized standing-root entry order.
- A cursor that advances after each admitted slice.
- Per-root artifact-kind progress in fixed `search`, `semantic`, `callgraph` order.
- At most the available cold-build slots worth of submitted standing slices.
- One coalesced executor job per selected root.

Use deficit round robin with one unit per bounded artifact slice. A root that yields because the cold limiter or resource policy denies admission retains its position without accumulating unbounded credit. A root that completes a slice advances behind the other runnable roots. A removed root loses its scheduler state. A paused session-owned root leaves the runnable ring and resumes at its prior artifact cursor after unbind.

The scheduler never waits in an executor worker. It first checks resource admission and then uses the existing immediate standing cold-build acquire. The 250 ms tick only performs cheap admission and scheduling checks. It does not write artifact state. A denied root is reconsidered on a later tick.

### Resource Admission

Add `crates/aft/src/resource_policy.rs` as a platform adapter with a small pure decision core.

`balanced` admits a new slice only when all available authoritative signals permit it:

- Linux: use `/sys/class/power_supply/*/type` plus `online` or `status` to detect external power. Use `/proc/pressure/cpu`, `/proc/pressure/memory`, and `/proc/pressure/io` for pressure stall information when available. Use `sysinfo`-independent standard-library reads.
- macOS: use IOKit power-source state and `getloadavg` or host statistics through existing platform FFI patterns. Do not execute subprocesses.
- Windows: use `GetSystemPowerStatus` and system load or memory status through direct Win32 FFI.
- Unsupported or unreadable signals: fail conservatively for a portable host-pressure signal, but do not classify a desktop with no battery as battery-powered. Record a named unknown signal in telemetry.

The decision core applies hysteresis. It requires consecutive healthy samples before resuming and pauses immediately on a hard battery-saving or memory-pressure signal. Sampling occurs on the standing tick and is cached for a bounded interval. It never runs on an interactive request path.

`performance` bypasses this admission decision only. It does not bypass the cold-build concurrency limit, executor caps, cancellation, writer leases, publication epochs, or `thread_priority` demotion.

Do not expose numeric pressure thresholds in configuration in this change. Keep one supported safe policy and one explicit bypass. Thresholds must be based on platform semantics and measured acceptance tests, not arbitrary user knobs.

## Resumable Artifact Slices

The slices solve two separate problems. First, they bound how long one root owns a scarce cold-build slot, which lets other roots make progress. Second, they preserve completed expensive work across rotation, cancellation, daemon restart, or supersession. A slice is a substantial unit of artifact work, not one scheduler tick.

Persist only after a slice performs real work and reaches an existing safe commit boundary. Do not write while idle, denied, or waiting. Coalesce cursor metadata with the slice output and rate-limit metadata-only checkpoints. The 250 ms scheduler cadence must never become a 250 ms disk-write cadence.

### Callgraph

Reuse the existing durable staging database and corpus fingerprint in `crates/aft/src/callgraph_store/mod.rs`.

Refactor the internal cold-build stage loop into `resume_cold_build_slice` with a bounded work budget. Return `Progress`, `Complete`, `Superseded`, or `Failed`. Stop at existing durable boundaries:

- File extraction inventory batches.
- Extraction batches capped by the existing file and byte limits.
- Resolution windows capped by the existing reference limit.
- Dispatch and publication barriers.

Commit the stage cursor in the same transaction as completed stage work before returning `Progress`. Preserve existing corpus-change restart and same-corpus adoption behavior. Do not issue a cursor-only commit when no stage work completed.

### Search

Add a durable search staging manifest under the existing transient build directory. Key it by the artifact cache key, corpus fingerprint, search format version, ignore-rule fingerprint, and max-file-size policy.

Split `build_streaming_index` into resumable phases:

1. Stable file inventory and metadata snapshot.
2. Bounded file collection and trigram spill-segment generation.
3. Bounded merge runs into staged postings and lookup sections.
4. Header, checksum, fsync, and atomic publication.

Persist the next inventory index with completed spill or merge output after each work slice. Do not persist on scheduler ticks or denied admission. A matching successor adopts the staging manifest. A changed fingerprint discards the staging generation. Published readers continue to use the previous complete generation until the final atomic swap.

### Semantic

Add a semantic staging file under the existing semantic cache root. Key it by corpus fingerprint plus `SemanticIndexFingerprint`, chunking version, and model table epoch.

Split build work into:

1. Bounded source collection to stable chunk records.
2. Bounded embedding batches using the configured backend batch limit.
3. Append-only persisted embedding records with per-batch checksum.
4. Final deterministic assembly and atomic semantic cache publication.

Resume only when every fingerprint component matches. Persist an embedding checkpoint only after a completed backend batch, and combine its cursor with the appended embedding records. Do not write on scheduler ticks. Truncate an incomplete final record after a crash. Never expose partial semantic results. Preserve query cache isolation and existing cancellation checks.

## Code Changes

### Configuration

- `crates/aft/src/config.rs`: add `IndexResourcePolicy` and `IndexConfig.resource_policy`, defaulting to `Balanced`.
- `crates/aft/src/config_resolve.rs`: add `RawIndex.resource_policy`, enforce user-only ownership, resolve the default, and report invalid values.
- `packages/opencode-plugin/src/config.ts`: add the duplicated Zod enum and default-preserving index schema field.
- `packages/pi-plugin/src/config.ts`: add the same schema contract.
- `assets/aft.schema.json`: regenerate the public schema through the existing schema build path.

### Scheduling and Admission

- `crates/aft/src/subc/standing.rs`: replace submit-all ticking with fair runnable-root selection, artifact cursors, bounded slice dispatch, and resource admission.
- `crates/aft/src/resource_policy.rs`: add platform sampling, cached snapshots, hysteresis, pure admission decisions, and telemetry types.
- `crates/aft/src/lib.rs`: register the new module.
- `crates/aft/src/cold_build_limiter.rs`: expose the current available standing capacity or a non-consuming admission query if the scheduler needs it. Keep the immediate permit API authoritative inside the serialized job.
- `crates/aft/src/subc/health.rs`: expose policy, power state, pressure state, pause reason, runnable-root count, scheduler cursor, slice completions, yields, and resumes.
- `crates/aft/src/logging.rs`: add the same compact standing-scheduler fields to busy executor diagnostics.

### Artifact Slices

- `crates/aft/src/callgraph_store/mod.rs`: expose one durable bounded cold-build slice.
- `crates/aft/src/search_index.rs`: add the staging manifest, resumable spill/merge phases, and atomic finalization.
- `crates/aft/src/semantic_index.rs`: add persisted chunk/embedding batches and deterministic finalization.
- `crates/aft/src/context.rs` and `crates/aft/src/subc/standing.rs`: route each selected artifact slice through the matching resume API and commit standing verification only after complete publication.

### Documentation

- `docs/config.md`: document standing roots, `balanced`, `performance`, the user-only boundary, and the fact that performance still respects safety and correctness bounds.
- `ARCHITECTURE.md`: document fair slice scheduling, resource admission, durable resume, and publication visibility.
- `STRUCTURE.md`: list the resource-policy module and staging responsibilities.

## TDD Task List

### Phase 1: Configuration and Decision Core

- [ ] Add failing Rust config tests for omitted, balanced, performance, invalid, and project-tier stripping.
- [ ] Add failing OpenCode and Pi config tests for the same contract.
- [ ] Implement `IndexResourcePolicy` through all config surfaces.
- [ ] Add failing pure decision tests for AC power, battery saving, pressure, unknown signals, hysteresis, and performance bypass.
- [ ] Implement the platform-neutral resource decision core and platform samplers.

### Phase 2: Fair Scheduler

- [ ] Add failing standing unit tests proving deterministic rotation, no root starvation, removal, session pause/resume, denied-admission retry, and bounded submissions.
- [ ] Implement the runnable ring, cursor, per-kind state, and slice completion feedback.
- [ ] Add failing telemetry tests for policy and pause reasons.
- [ ] Implement health and logging projection.

### Phase 3: Callgraph Slices

- [ ] Add a failing test that stops after one durable callgraph slice and resumes in a new store instance.
- [ ] Add failing same-corpus adoption and changed-corpus restart tests at each stage boundary.
- [ ] Refactor the existing stage loop into the bounded resume API.

### Phase 4: Search Slices

- [ ] Add failing crash/resume tests for inventory, spill generation, merge, and pre-publication boundaries.
- [ ] Add failing changed-corpus and corrupt-manifest rejection tests.
- [ ] Implement the staged search format and bounded resume API.
- [ ] Prove byte-equivalent logical query results against the existing monolithic builder.

### Phase 5: Semantic Slices

- [ ] Add failing resume tests across chunk collection, embedding batches, and finalization.
- [ ] Add failing fingerprint, table-epoch, partial-record, and cancellation tests.
- [ ] Implement semantic staging and bounded resume.
- [ ] Prove result equivalence and that no partial result is visible.

### Phase 6: End-to-End Load Contract

- [ ] Extend `crates/aft/tests/standing_roots_acceptance_test.rs` with many roots and all artifact kinds.
- [ ] Extend `crates/aft/tests/integration/subc_storm_test.rs` to prove reader and health latency while roots rotate and pause.
- [ ] Add a performance-policy case that ignores simulated battery and pressure signals while preserving queue and cold-build bounds.
- [ ] Run the release-calibrated storm gate.
- [ ] Run the complete Rust and bridge regression suites.
- [ ] Update the configuration and architecture documentation.

## Acceptance Criteria

- Given more runnable roots than cold-build slots, each root completes bounded slices in deterministic rotation without starvation.
- Given `resource_policy: balanced` and a battery-saving or high-pressure signal, no new standing slice starts. Interactive reads and health checks remain responsive.
- Given recovery to a healthy state, hysteresis prevents rapid pause/resume oscillation and standing work resumes automatically.
- Given `resource_policy: performance`, standing work ignores battery and pressure admission while all correctness and concurrency bounds remain active.
- Given a daemon restart or superseded builder, matching search, semantic, and callgraph staging resumes from the last committed boundary.
- Given a corpus or model fingerprint change, incompatible staging is rejected and rebuilt.
- Given an incomplete staging artifact, readers continue to use the prior complete generation.
- Given static-root load, the release storm meets its existing retry-free latency contracts.

## Verification

Run these gates after the focused RED/GREEN cycles:

```bash
cargo test -p agent-file-tools standing_roots
cargo test -p agent-file-tools callgraph_store
cargo test -p agent-file-tools search_index
cargo test -p agent-file-tools semantic_index
cargo test -p agent-file-tools --test integration subc_storm_test
bun test packages/opencode-plugin/src/__tests__/config.test.ts packages/pi-plugin/src/__tests__/config.test.ts
AFT_GATE_PHASES=storm scripts/rust-test-gate.sh
cargo test -p agent-file-tools
bun test packages/aft-bridge packages/opencode-plugin packages/pi-plugin
```

The load test must record per-root slice counts, maximum reader latency, health latency, pause duration, and process CPU time. Compare `balanced` and `performance` with the same root corpus. Treat these as acceptance evidence rather than permanent fixed thresholds unless the existing release storm already defines a limit.

## Risks

- Risk: A monolithic search or semantic build defeats root fairness. Implement true intra-kind resume before claiming fairness complete.
- Risk: A partial staging format corrupts published readers. Keep staging generation-specific and publish only through the existing atomic generation swap.
- Risk: Platform signals differ or disappear. Keep the decision core explicit about unknown data and expose the reason in health telemetry.
- Risk: A performance bypass disables correctness controls. Limit the bypass to resource admission only.
- Risk: Frequent durable checkpoints increase write amplification. Measure staging writes in the acceptance test and use existing natural batch boundaries.
