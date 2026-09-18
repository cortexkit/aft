# Views lazy navigation read benchmark — September 2026

> **Provisional: loaded host.** The required one-minute load average ceiling of 3 was not met. The shared fleet host stayed loaded despite multiple wait periods, and no idle window was available. Observed competing consumers before the run included other `cargo`/`rustc` and `wasm-opt` builds, Bun, `ck-aft`, `opencode-trace-warnings`, XProtect, and a Parallels VM. The benchmark therefore reports the interleaved relative result, but the absolute 234/228 ms closing thresholds are **not decidable** from this run.

## Subject and method

- Machine: `Darwin Ismets-MacBook-Pro.local 27.0.0`, arm64, Apple M5 Max.
- Opencode subject: `5716f8ba60e79ec60ec485b6e5291c0b0bc1f252`.
- Published generation: `21-79945-1789275371885739000-7-c3d6ca11eca18e25d2e58e52629921215cdea1d4f23ce9278941c1a13faa95d1`.
- Release test binary: `target/release/deps/views_lazy_navigation_profile-ea3d62423a69c456`.
- Binary SHA-256: `bc03a0da0fd5bfde67cfec8e9519d9cce0c20b47dcb2e5367e8ec070ecf30b16`.
- Artifacts: one published manifest, its `callgraph.sqlite` blob store, and its `derived-<generation>.sqlite`, all from the same completed opencode branch drill.
- Query depth: 3.
- Query population: 54 symbols, 18 from each deterministic in-degree third.
- Repetitions: 3. Within each repetition the arms ran in this order: materialized, lazy cold, lazy warm. This interleaving prevents an entire arm from being measured in a separate load period.
- Lazy cache: bounded to two assembled generations. Cold clears it before every callers query, impact query, and projection; warm retains it across the query set.
- Bytes read: Darwin `proc_pid_rusage` `ri_diskio_bytesread` delta for each complete arm. These are physical process reads, so later runs can be lower when the OS page cache is warm.
- Correctness: every lazy callers and impact result was serialized canonically and asserted byte-equal to the materialized result. The whole-root tier-2 projection was checked the same way. Across the three runs this covered 648 navigation comparisons and 6 projection comparisons without divergence.

The measurement invocation was:

```sh
AFT_HUNT_ROOT=/Users/ufukaltinok/Work/OSS/opencode \
AFT_HUNT_STORAGE=/Users/ufukaltinok/.cache/aft-views-soak/0f3900af641f5248/views-on-storage \
AFT_LAZY_READ_REPORT="$PWD/target/views-lazy-read-report.json" \
RUSTFLAGS='--cfg aft_views_lazy_benchmark' \
cargo test -p agent-file-tools --test views_lazy_navigation_profile \
  --release -- --ignored --nocapture
```

Set `AFT_LAZY_READ_REQUIRE_IDLE=1` to restore the strict load-average refusal and re-record the absolute table only when the one-minute load average is at most 3. The measured run completed all workloads, comparisons, and console tables, then failed its optional JSON copy because the originally supplied relative output directory did not exist from the test process's working directory. The instrument now creates that parent directory; this post-measurement reporting failure did not affect the timings or equality assertions below.

## Deterministic selection

The selector orders all unambiguous symbols with positive caller counts by `(in_degree, file, scoped_name)`, splits that ordered population into thirds, and takes 18 evenly spaced entries from each third.

| Stratum | Symbol | In-degree |
|---|---|---:|
| low | `.opencode/tool/github-pr-search.ts::githubFetch` | 1 |
| low | `packages/ai/src/protocols/utils/openai-image.ts::dimensions` | 1 |
| low | `packages/app/e2e/performance/timeline/session-timeline-benchmark.fixture.ts::session` | 1 |
| low | `packages/app/src/home/sessions/view.tsx::closeEditor` | 1 |
| low | `packages/app/src/session/files/list.tsx::directory` | 1 |
| low | `packages/app/src/settings/workspaces/workspaces.tsx::sessionCount` | 1 |
| low | `packages/cli/src/cpu-profile.ts::run` | 1 |
| low | `packages/client/src/effect/generated/client.ts::adaptGroupMcp` | 1 |
| low | `packages/console/app/src/routes/black/subscribe/[plan].tsx::Success` | 1 |
| low | `packages/core/src/config/markdown.ts::sanitize` | 1 |
| low | `packages/core/src/plugin/provider/github-copilot.ts::oauth` | 1 |
| low | `packages/core/src/v1/config/migrate.ts::encodeAgent` | 1 |
| low | `packages/desktop/src/main/wsl/start.ts::makeWsl` | 1 |
| low | `packages/merman/src/flowchart/layout.ts::intersects` | 1 |
| low | `packages/server/src/auth.ts::required` | 1 |
| low | `packages/session-ui/src/v2/components/session-review-v2.tsx::toolbarStart` | 1 |
| low | `packages/stats/app/src/routes/stats-shell.tsx::newsletterErrorMessage` | 1 |
| low | `packages/tui/src/component/startup-loading.tsx::StartupLoading` | 1 |
| mid | `packages/tui/src/component/tab-pulse.tsx::fadeOut` | 1 |
| mid | `packages/tui/src/mini/tool.ts::markdownBody` | 1 |
| mid | `packages/ui/src/components/file-icon.tsx::dottedSuffixesDesc` | 1 |
| mid | `script/raw-changelog.ts::reverted` | 1 |
| mid | `packages/app/e2e/regression/cross-server-tab-close.spec.ts::session` | 2 |
| mid | `packages/app/src/session/composer/queue.ts::edit` | 2 |
| mid | `packages/app/src/shell/updates/release-notes.tsx::last` | 2 |
| mid | `packages/console/app/src/component/header.tsx::isDarkMode` | 2 |
| mid | `packages/core/src/permission.ts::create` | 2 |
| mid | `packages/core/test/state-replay.test.ts::expected` | 2 |
| mid | `packages/merman/src/state/routing.ts::alternateBodySafeTransitionPlan` | 2 |
| mid | `packages/session-ui/src/v2/components/session-review-v2.tsx::SessionReviewV2Sidebar` | 2 |
| mid | `packages/tui/src/component/session-tabs.tsx::itemStatus` | 2 |
| mid | `packages/tui/src/routes/session/form.tsx::selectTabFromMouse` | 2 |
| mid | `packages/web/src/components/share/part.tsx::getDiagnostics` | 2 |
| mid | `packages/app/src/session/review/review-diff-kinds.ts::reviewDiffKinds` | 3 |
| mid | `packages/console/core/src/model.ts::validate` | 3 |
| mid | `packages/desktop/src/main/wsl/runtime.ts::requireSuccess` | 3 |
| high | `packages/desktop/src/main/wsl/runtime.ts::withTimeout` | 3 |
| high | `packages/stats/core/src/honeycomb-backfill.ts::number` | 3 |
| high | `packages/ui/src/components/text-reveal.stories.tsx::text` | 3 |
| high | `packages/app/src/session/timeline/model.ts::loadOlderTimeline` | 4 |
| high | `packages/core/test/plugin/supervisor-reload.test.ts::greeter` | 4 |
| high | `packages/tui/src/component/terminal-pane.tsx::sameSize` | 4 |
| high | `packages/app/src/session/review/review-diff-kinds.ts::normalizePath` | 5 |
| high | `packages/server/test/fetch.test.ts::create` | 5 |
| high | `packages/app/src/runtime/server/global-sync/utils.ts::normalizeProjectInfo` | 6 |
| high | `packages/stats/core/src/domain/home.ts::aggregateByModel` | 6 |
| high | `packages/core/test/location-filesystem.test.ts::withTmp` | 7 |
| high | `packages/core/test/pty/pty-session.test.ts::attachCollecting` | 8 |
| high | `packages/tui/src/util/model.ts::switchLabel` | 9 |
| high | `packages/tui/src/mini/stream-v2.subagent.ts::notifyDetail` | 11 |
| high | `packages/stats/app/src/routes/model-catalog.ts::formatCatalogLabName` | 14 |
| high | `packages/tui/src/ui/animation.ts::jump` | 19 |
| high | `packages/core/src/bus.ts::log` | 33 |
| high | `packages/util/src/effect/layer-node.ts::make` | 1581 |

## Navigation latency

Times are milliseconds. Each row contains 18 samples.

| Run | Arm | Kind | Stratum | p50 | p95 | max |
|---:|---|---|---|---:|---:|---:|
| 1 | materialized | callers | low | 120.520 | 384.004 | 384.004 |
| 1 | materialized | callers | mid | 247.670 | 639.962 | 639.962 |
| 1 | materialized | callers | high | 308.878 | 9897.059 | 9897.059 |
| 1 | materialized | impact | low | 135.943 | 577.856 | 577.856 |
| 1 | materialized | impact | mid | 237.468 | 571.302 | 571.302 |
| 1 | materialized | impact | high | 302.249 | 9575.536 | 9575.536 |
| 1 | lazy cold | callers | low | 20150.320 | 31355.763 | 31355.763 |
| 1 | lazy cold | callers | mid | 20890.346 | 24061.960 | 24061.960 |
| 1 | lazy cold | callers | high | 21771.702 | 29441.758 | 29441.758 |
| 1 | lazy cold | impact | low | 20124.154 | 26061.538 | 26061.538 |
| 1 | lazy cold | impact | mid | 20585.315 | 27622.714 | 27622.714 |
| 1 | lazy cold | impact | high | 21747.722 | 36167.913 | 36167.913 |
| 1 | lazy warm | callers | low | 0.011 | 25194.514 | 25194.514 |
| 1 | lazy warm | callers | mid | 0.015 | 0.036 | 0.036 |
| 1 | lazy warm | callers | high | 0.017 | 1.830 | 1.830 |
| 1 | lazy warm | impact | low | 0.239 | 1.237 | 1.237 |
| 1 | lazy warm | impact | mid | 0.294 | 2.093 | 2.093 |
| 1 | lazy warm | impact | high | 0.257 | 32.555 | 32.555 |
| 2 | materialized | callers | low | 136.831 | 547.369 | 547.369 |
| 2 | materialized | callers | mid | 391.026 | 891.535 | 891.535 |
| 2 | materialized | callers | high | 369.951 | 9424.488 | 9424.488 |
| 2 | materialized | impact | low | 200.963 | 608.536 | 608.536 |
| 2 | materialized | impact | mid | 410.849 | 812.984 | 812.984 |
| 2 | materialized | impact | high | 314.234 | 9872.136 | 9872.136 |
| 2 | lazy cold | callers | low | 19189.338 | 25143.103 | 25143.103 |
| 2 | lazy cold | callers | mid | 18248.196 | 24694.293 | 24694.293 |
| 2 | lazy cold | callers | high | 19998.666 | 22493.822 | 22493.822 |
| 2 | lazy cold | impact | low | 18819.825 | 24935.969 | 24935.969 |
| 2 | lazy cold | impact | mid | 18282.531 | 35090.184 | 35090.184 |
| 2 | lazy cold | impact | high | 19711.018 | 21552.213 | 21552.213 |
| 2 | lazy warm | callers | low | 0.013 | 32160.009 | 32160.009 |
| 2 | lazy warm | callers | mid | 0.018 | 0.060 | 0.060 |
| 2 | lazy warm | callers | high | 0.030 | 2.754 | 2.754 |
| 2 | lazy warm | impact | low | 0.344 | 3.230 | 3.230 |
| 2 | lazy warm | impact | mid | 0.403 | 2.588 | 2.588 |
| 2 | lazy warm | impact | high | 0.446 | 16.279 | 16.279 |
| 3 | materialized | callers | low | 342.826 | 1222.068 | 1222.068 |
| 3 | materialized | callers | mid | 370.027 | 1661.714 | 1661.714 |
| 3 | materialized | callers | high | 358.362 | 8718.891 | 8718.891 |
| 3 | materialized | impact | low | 254.329 | 1119.479 | 1119.479 |
| 3 | materialized | impact | mid | 369.975 | 1413.296 | 1413.296 |
| 3 | materialized | impact | high | 407.287 | 12129.095 | 12129.095 |
| 3 | lazy cold | callers | low | 22947.232 | 32941.182 | 32941.182 |
| 3 | lazy cold | callers | mid | 22413.936 | 26747.114 | 26747.114 |
| 3 | lazy cold | callers | high | 20170.148 | 26375.146 | 26375.146 |
| 3 | lazy cold | impact | low | 21073.232 | 37414.073 | 37414.073 |
| 3 | lazy cold | impact | mid | 21521.960 | 30897.601 | 30897.601 |
| 3 | lazy cold | impact | high | 20875.108 | 29133.525 | 29133.525 |
| 3 | lazy warm | callers | low | 0.011 | 24373.251 | 24373.251 |
| 3 | lazy warm | callers | mid | 0.014 | 0.036 | 0.036 |
| 3 | lazy warm | callers | high | 0.019 | 2.109 | 2.109 |
| 3 | lazy warm | impact | low | 0.234 | 1.170 | 1.170 |
| 3 | lazy warm | impact | mid | 0.349 | 5.236 | 5.236 |
| 3 | lazy warm | impact | high | 0.286 | 12.211 | 12.211 |

The first lazy-warm low-stratum callers query assembles the generation and therefore appears as the p95/max for that stratum. All subsequent retained-cache navigation calls are warm. No unmeasured pre-warm was inserted.

## Three-run spread

Each cell is `minimum–maximum` across the three repetitions, in milliseconds.

| Arm | Kind | Stratum | p50 spread | p95 spread | max spread |
|---|---|---|---:|---:|---:|
| materialized | callers | low | 120.520–342.826 | 384.004–1222.068 | 384.004–1222.068 |
| materialized | callers | mid | 247.670–391.026 | 639.962–1661.714 | 639.962–1661.714 |
| materialized | callers | high | 308.878–369.951 | 8718.891–9897.059 | 8718.891–9897.059 |
| materialized | impact | low | 135.943–254.329 | 577.856–1119.479 | 577.856–1119.479 |
| materialized | impact | mid | 237.468–410.849 | 571.302–1413.296 | 571.302–1413.296 |
| materialized | impact | high | 302.249–407.287 | 9575.536–12129.095 | 9575.536–12129.095 |
| lazy cold | callers | low | 19189.338–22947.232 | 25143.103–32941.182 | 25143.103–32941.182 |
| lazy cold | callers | mid | 18248.196–22413.936 | 24061.960–26747.114 | 24061.960–26747.114 |
| lazy cold | callers | high | 19998.666–21771.702 | 22493.822–29441.758 | 22493.822–29441.758 |
| lazy cold | impact | low | 18819.825–21073.232 | 24935.969–37414.073 | 24935.969–37414.073 |
| lazy cold | impact | mid | 18282.531–21521.960 | 27622.714–35090.184 | 27622.714–35090.184 |
| lazy cold | impact | high | 19711.018–21747.722 | 21552.213–36167.913 | 21552.213–36167.913 |
| lazy warm | callers | low | 0.011–0.013 | 24373.251–32160.009 | 24373.251–32160.009 |
| lazy warm | callers | mid | 0.014–0.018 | 0.036–0.060 | 0.036–0.060 |
| lazy warm | callers | high | 0.017–0.030 | 1.830–2.754 | 1.830–2.754 |
| lazy warm | impact | low | 0.234–0.344 | 1.170–3.230 | 1.170–3.230 |
| lazy warm | impact | mid | 0.294–0.403 | 2.093–5.236 | 2.093–5.236 |
| lazy warm | impact | high | 0.257–0.446 | 12.211–32.555 | 12.211–32.555 |

## Read volume and whole-root tier-2 projection

| Run | Arm | 1m load average | Bytes read | Projection wall time (ms) |
|---:|---|---:|---:|---:|
| 1 | materialized | 5.84 | 175,247,360 | 1628.928 |
| 1 | lazy cold | 10.55 | 1,142,652,928 | 27736.773 |
| 1 | lazy warm | 17.74 | 8,052,736 | 370.078 |
| 2 | materialized | 15.80 | 278,269,952 | 2034.330 |
| 2 | lazy cold | 13.00 | 1,141,415,936 | 24834.685 |
| 2 | lazy warm | 15.91 | 2,617,344 | 544.895 |
| 3 | materialized | 21.52 | 23,736,320 | 1177.060 |
| 3 | lazy cold | 20.72 | 101,552,128 | 25554.444 |
| 3 | lazy warm | 27.20 | 2,629,632 | 527.052 |

Projection spread was 1177.060–2034.330 ms materialized, 24834.685–27736.773 ms lazy cold, and 370.078–544.895 ms lazy warm. Byte-read spread was 23,736,320–278,269,952 materialized, 101,552,128–1,142,652,928 lazy cold, and 2,617,344–8,052,736 lazy warm.

## Closing-rule verdict

The worst observed lazy-cold p95 was **32,941.182 ms for callers** and **37,414.073 ms for impact**. The corresponding worst materialized p95 values were 9,897.059 ms and 12,129.095 ms, so lazy cold was **3.33×** and **3.08×** slower respectively under the same interleaved loaded-host run.

**Provisional relative verdict: materialized navigation stays.** The direct manifest join rebuilds the generation on every cold query, and remained more than three times slower than materialized navigation under shared contention. Warm retained-cache reads were generally sub-millisecond to low-millisecond after the first generation assembly, but that does not rescue the required cold arm.

The observed absolute values are far above the panel's `~500 ms` branch, but the run violated the required `load average <= 3` precondition. Therefore the literal `234/228 ms` versus `~500 ms` closing rule is not declared final here. Re-run with `AFT_LAZY_READ_REQUIRE_IDLE=1` on an idle host to produce the final absolute verdict; until then this result supports optimizing materialized emission/delete rather than scheduling refs/edges removal.
