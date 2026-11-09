# subc egress head-of-line investigation (2026-07-14)

## Result

AFT has one FIFO writer queue and one writer task per subc connection. Every
session/channel on that connection shares them. A completed tool response has no
writer-lane priority over Push frames that are already queued. Under a slow
consumer, a reproduced 42.146 s response egress consisted of 7.150 s waiting to
enqueue, 34.996 s waiting in the shared writer queue, and 0.013 ms in its own
transport write. The reliable reserve/backoff loop therefore contributed, but
the dominant delay was cross-session/cross-frame head-of-line (HOL) waiting.

Large frames matter because an in-progress frame cannot be interleaved with
another frame. The saturated run recorded a 786,782-byte response spending
15.858 s in `write_all`; every queued response behind it waited too. However,
the 42 s small-response examples were not slow because of their own size.

Ten CPU-burning processes increased executor-queue p95 from 1.658 ms to
793.502 ms, but writer-egress p95 was 7.757 ms versus 16.135 ms in the
corresponding idle run. This run does not support Tokio writer-task starvation
as the source of a tens-of-seconds egress stall. The permanent instrumentation below makes that diagnosis
visible if it occurs in production.

## Exact serialization points

1. A connection creates one bounded `mpsc` writer queue with capacity 256 and
   starts one writer task (`crates/aft/src/subc/mod.rs:84`,
   `crates/aft/src/subc/mod.rs:1597-1599`). All route/session senders are clones
   of that connection-wide sender.
2. The writer performs one `rx.recv()` at a time and awaits the complete
   contiguous `write_all` before receiving the next item
   (`crates/aft/src/subc/mod.rs:2208-2255`). That is the byte-serialization
   point. Frames from different channels cannot progress concurrently on this
   connection.
3. A completed tool call builds its full JSON response frame, then uses the
   reliable sender (`crates/aft/src/subc/mod.rs:3417-3445`). The response body is
   serialized before enqueue (`crates/aft/src/subc/wire.rs:401-429`), so that
   work appears in `egress_enqueue`.
4. Reliable sends first use `try_reserve`. A full queue enters a 250 ms
   `reserve()` timeout, then sleeps with exponential backoff from 10 ms to
   250 ms and retries without dropping the frame
   (`crates/aft/src/subc/mod.rs:69-72`, `crates/aft/src/subc/mod.rs:107-108`,
   `crates/aft/src/subc/wire.rs:177-287`). A non-reliable control send gets only
   one 250 ms reserve attempt (`crates/aft/src/subc/wire.rs:289-312`).
5. Reliable Push input is limited to 32 logical Push events per route-loop turn
   (`crates/aft/src/subc/mod.rs:86-88`,
   `crates/aft/src/subc/push.rs:806-872`), but one event can fan out to every
   matching route (`crates/aft/src/subc/push.rs:681-735`). Push enqueue itself is
   a non-blocking `try_reserve`; full queues move reliable Pushes to retry
   buffers and drop/coalesce lossy pressure (`crates/aft/src/subc/push.rs:270-299`).
6. Once a Push or response has a writer-queue slot, the writer has no frame-class
   or route priority. The route loop's biased select handles socket input before
   the reliable and lossy Push receivers (`crates/aft/src/subc/mod.rs:1773-1775`,
   `crates/aft/src/subc/mod.rs:1800-2027`,
   `crates/aft/src/subc/mod.rs:2027-2050`), but response tasks enqueue directly
   and are not a select arm. The queue position acquired by the producer is the
   effective priority. Already-queued Push frames remain ahead of a response.

The existing B2 ordering is intentional: `RouteBindAck` is reliably enqueued
before buffered route Push replay (`crates/aft/src/subc/mod.rs:2613-2633`), and
`route_bind_ack_precedes_route_egress_in_writer_queue` locks this invariant
(`crates/aft/src/subc/mod.rs:4506`). Any writer scheduler must preserve that
barrier.

## Instrumentation

`ToolCallPhaseDurations` now separates:

- `egress_enqueue`: response-finalized to writer-queue insertion. This includes
  response-envelope serialization and all reliable reserve/backoff waits.
- `egress_queue`: insertion to writer-task dequeue.
- `egress_prepare`: dequeue to entry into the existing transport writer.
- `egress_write`: the awaited contiguous `write_all` call.
- `egress`: response-finalized to completed transport write.

Each tool-call log also includes actual `frame_bytes`, queue depth at insertion,
whether a write was active, whether the queue was observed full, and the number
of 250 ms reserve timeouts (`crates/aft/src/logging.rs:502-569`). The trace is
carried in the existing queue item without a per-frame heap allocation or
syscall. Clock reads are added only to traced tool responses; Push/control
frames only pay one relaxed writer-active load at enqueue and two relaxed stores
around their write. The existing bounded
perf-window lock still occurs once after response egress.

The ignored storm rig is at
`crates/aft/tests/integration/subc_storm_test.rs:1019-1119`. It opens one
connection, binds N sessions, alternates one large response with three small
responses, can emit Push bursts, and controls both the initial read pause and
inter-frame read delay. It emits one `EGRESS_OBS` row per response and grouped
summaries. Environment variables are documented by
`EgressMeasureConfig::from_env` in the same file.

## Measurement method and raw data

Host: `tests-MacBook-Pro.local`, Apple M1 Max (`arm64`), macOS 26.5.2. The
measurement lock contained `bg_1d44c6cd F-3 rerun` for every final-code run and
was removed afterward. The release-profile command was:

```text
cargo test --features test-hooks --release -p agent-file-tools --test integration \
  subc_egress_hol_measurement -- --ignored --nocapture
```

The exact per-response output is in
[`data/subc-egress-m1-2026-07-14.tsv`](data/subc-egress-m1-2026-07-14.tsv).
It has 801 raw rows. Durations are milliseconds. `queue_ms` is executor queue
latency; `egress_queue_ms` is writer queue latency. Percentiles below use the
same floor-index calculation as the rig.

| scenario | uptime/load at start | sessions | responses | small/large requested | reader | Pushes per large |
|---|---|---:|---:|---|---|---:|
| `depth1` | up 7d 1:58; 2.20 1.92 1.98 | 1 | 1 | 4 KiB / 4 KiB | immediate | 0 |
| `depth32` | up 7d 1:58; 2.20 1.92 1.98 | 8 | 32 | 4 KiB / 256 KiB | immediate | 2 |
| `depth128` | up 7d 1:58; 2.20 1.92 1.98 | 8 | 128 | 4 KiB / 256 KiB | immediate | 2 |
| `size-controlled` | up 7d 1:58; 2.20 1.92 1.98 | 8 | 128 | 4 KiB / 256 KiB | 500 ms pause, then 20 ms/frame | 0 |
| `saturation` | up 7d 1:59; 2.09 1.92 1.98 | 8 | 384 | 4 KiB / 256 KiB | 1 s pause, then 100 ms/frame | 2 |
| `cpu-saturation` | up 7d 2:00; 1.65 1.80 1.92 start, 3.68 2.22 2.07 end | 8 | 128 | 4 KiB / 256 KiB | immediate | 2; plus 10 `yes` workers |

The encoded frames were 12,632 bytes for a requested 4 KiB payload and 786,782
bytes for a requested 256 KiB payload. Actual frame size, not requested payload,
is used below.

### Queue-depth sweep with an immediate reader

| scenario/size | count | observed queue depth | egress p50 / p95 / max | write p95 / max |
|---|---:|---:|---:|---:|
| depth1, 12,632 B | 1 | 1 | 0.039 / 0.039 / 0.039 | 0.013 / 0.013 |
| depth32, 12,632 B | 24 | 1-31 | 3.286 / 3.415 / 3.483 | 0.020 / 0.053 |
| depth32, 786,782 B | 8 | 1-28 | 3.252 / 3.302 / 3.309 | 0.108 / 2.582 |
| depth128, 12,632 B | 96 | 2-255 | 8.428 / 16.135 / 16.187 | 0.018 / 0.068 |
| depth128, 786,782 B | 32 | 4-256 | 8.442 / 16.123 / 16.210 | 0.255 / 5.193 |

Push fan-out explains why measured queue depth can exceed the response count.
With a healthy reader, total egress is governed mainly by queue position; large
frames have measurably higher own-write time but do not produce long stalls.

### Frame size at matched queue depths

`size-controlled` removes Push traffic and applies the same slow-reader policy
to both sizes.

| insertion depth | frame bytes | count | egress p50 / p95 | own write p50 / p95 / max | writer queue p50 |
|---:|---:|---:|---:|---:|---:|
| 1-32 | 12,632 | 28 | 596.522 / 964.986 | 0.009 / 0.021 / 0.023 | 595.259 |
| 1-32 | 786,782 | 9 | 596.283 / 964.187 | 2.969 / 185.839 / 583.649 | 594.518 |
| 33-64 | 12,632 | 23 | 1360.014 / 1583.978 | 0.014 / 0.054 / 0.064 | 1356.419 |
| 33-64 | 786,782 | 9 | 1359.870 / 1586.572 | 102.107 / 182.999 / 207.084 | 1355.921 |
| 65-96 | 12,632 | 27 | 2154.597 / 2517.238 | 0.019 / 0.023 / 0.025 | 2153.381 |
| 65-96 | 786,782 | 9 | 2155.102 / 2516.980 | 0.421 / 183.608 / 206.199 | 2153.224 |
| 97-128 | 12,632 | 18 | 2704.135 / 3070.685 | 0.013 / 0.046 / 0.052 | 2703.074 |
| 97-128 | 786,782 | 5 | 2886.740 / 2887.455 | 182.536 / 184.358 / 185.084 | 2702.816 |

Size strongly affects the frame's own blocked write, while total egress for both
sizes rises with queue depth. A large write transfers its cost to later small
frames as queue waiting.

### Saturation and the 42-second reproduction

The saturated run produced these grouped results:

| frame bytes | count | queue-full traces | max reserve timeouts | egress p50 / p95 / max | enqueue p95 / max | writer-queue p95 / max | write p95 / max |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 12,632 | 288 | 237 | 74 | 42972.108 / 61234.519 / 62885.976 | 34949.465 / 36466.853 | 44915.958 / 45743.307 | 0.020 / 0.057 |
| 786,782 | 96 | 78 | 74 | 42971.940 / 61234.276 / 62885.773 | 34949.475 / 36466.836 | 44090.415 / 45742.691 | 828.480 / 15857.967 |

Eight adjacent frames around 42 seconds show the decomposition directly:

| channel/corr | frame bytes | egress | enqueue | writer queue | own write | depth | reserve timeouts |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1/800176 | 786,782 | 42146.898 | 7151.885 | 34174.767 | 820.124 | 145 | 16 |
| 2/800177 | 12,632 | 42147.054 | 7151.890 | 34995.149 | 0.010 | 146 | 16 |
| 3/800178 | 12,632 | 42147.113 | 7151.876 | 34995.224 | 0.010 | 147 | 16 |
| 4/800179 | 12,632 | 42147.166 | 7151.863 | 34995.290 | 0.009 | 148 | 16 |
| 5/800180 | 786,782 | 42145.906 | 7150.096 | 34995.364 | 0.324 | 149 | 16 |
| 6/800181 | 12,632 | 42146.008 | 7150.091 | 34995.900 | 0.013 | 150 | 16 |
| 7/800182 | 12,632 | 42146.064 | 7150.079 | 34995.971 | 0.011 | 151 | 16 |
| 8/800183 | 12,632 | 42146.110 | 7150.053 | 34996.040 | 0.013 | 152 | 16 |

The largest individual components in the raw run were 36.467 s to enqueue,
45.743 s in queue, and 15.858 s in a large-frame write.

### Hypothesis verdicts

- **(a) Shared-connection HOL: supported and dominant.** The reproduced 42 s
  small responses spent about 35.00 s in the one queue shared by all eight
  channels, with depths 146-152 and an active writer.
- **(b) Reserve-timeout backoff: supported as a contributor under saturation.**
  The same responses spent about 7.15 s before enqueue and had each timed out 16
  reserve attempts. The worst trace timed out 74 times and spent 36.47 s before
  insertion.
- **(c) The delayed response's own frame write: rejected for the 42 s small
  frames, but large writes amplify HOL.** Their own writes were 0.009-0.013 ms.
  A different 786,782-byte frame blocked in `write_all` for 15.858 s, delaying
  frames behind it.
- **(d) Writer-task scheduler starvation: not supported by the controlled CPU
  run.** With ten CPU burners, p95 egress was 7.757 ms (versus 16.135 ms in the
  corresponding immediate-reader run), while executor-queue p95 increased from
  1.658 ms to 793.502 ms. CPU contention throttled work before the writer; it did
  not create a seconds-long writer stall.

## Fix proposal (not implemented)

1. **Keep one physical writer, but schedule complete frames before the FIFO.**
   Add bounded per-route queues and a small scheduler that gives control frames
   first service, then uses deficit round-robin across route responses, with a
   bounded reliable-Push share. This directly addresses the measured
   cross-session HOL without allowing concurrent byte writes on one TCP stream.
   Reserve memory globally so 256 per-route slots do not multiply the current
   cap. Preserve the RouteBindAck-to-route barrier required by B2.
2. **Give ready tool responses precedence over ordinary Push at scheduler
   admission, not absolute starvation priority.** Once queued, lossy Push should
   remain replaceable/sheddable; reliable Push should retain bounded forward
   progress. The current 32-event producer drain budget is insufficient because
   one event can fan out to many routes and all admitted frames become equal
   FIFO entries.
3. **Cap one large response's uninterrupted socket occupancy only through a
   framed/streaming response design.** Merely chunking calls to `write` cannot
   interleave another frame's bytes without violating frame integrity. If the
   consumer can reassemble `STREAM_DATA`, split large response bodies into wire
   frames and schedule chunks fairly. Otherwise preserve contiguous frame order
   and accept one-frame HOL; writer parallelism on the same connection is not a
   valid fix. Separate physical connections per channel would remove that HOL
   but is a substantially larger routing/lifecycle change.

### Wire-v2 flags

The frozen v2 envelope already has Priority and AdmissionClass bits, so no
header change is needed. In `subc-protocol` 0.9.0, `EXPEDITE` is an admission
class, not a priority value; `SHEDDABLE` is legal only for `PUSH` and
`STREAM_DATA`. See the versioned definitions for
[Priority](https://docs.rs/subc-protocol/0.9.0/src/subc_protocol/lib.rs.html#210-227),
[AdmissionClass](https://docs.rs/subc-protocol/0.9.0/src/subc_protocol/lib.rs.html#230-247),
and [decoder validation](https://docs.rs/subc-protocol/0.9.0/src/subc_protocol/lib.rs.html#425-480).
The transport writes frames sequentially and does not inspect flags for local
reordering ([subc-transport frame I/O](https://docs.rs/subc-transport/0.4.0/src/subc_transport/frame_io.rs.html#49-116)).

Consequently, marking a response EXPEDITE cannot bypass AFT's local writer
queue; the daemon sees the bit only after the measured bottleneck. Marking
eligible lossy Pushes SHEDDABLE may help daemon-side pressure but cannot remove
AFT-local queued Pushes. Responses are delivery-required and cannot be marked
SHEDDABLE. These bits are complementary downstream hints, not the primary fix.
