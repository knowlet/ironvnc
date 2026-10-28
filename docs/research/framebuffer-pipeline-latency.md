# Reproducible CPU pipeline latency

## Research decision (2026-10-28)

Start from PR #6 head `6b8ae5e8810f54d7bd6baadf7f9e33c63b95b9b0`.
Keep two update credits, FIFO fences, rectangle ordering, and existing bounds.
The existing throughput test does not measure native display FPS or input latency.

Inspected the real client/event/UI paths and pinned egui 0.31.1 upstream source
([commit 1669e52](https://github.com/emilk/egui/tree/1669e52a7ccfc3489c1b0999b9ed48894a0b3887)).
`ColorImage::from_rgba_unmultiplied` allocates/converts pixel storage; Color32 already
fast-paths opaque alpha. `TextureHandle::set_partial` queues a CPU image delta;
it does not upload or present pixels. TextureManager retains partial deltas until
the end of a pass. Do not silently replace partial updates with full/coalesced ones.
Initial framebuffer alpha is zero, so blanket opaque conversion is incorrect.
Use existing Rust/Tokio/egui and Python standard library, no added dependencies.
No GPL code was copied. egui is MIT/Apache-2.0.

## Measurement boundaries

`--profile-cpu` uses the production protocol client and actual `VncApp::process_events`
in an egui context, but creates **no native window or GPU**. It accepts only numeric
loopback hosts, reads no saved sessions, and requires no credentials. It drains and
drops texture deltas once per CPU pass. All metrics are wall-clock observations:

- `receive_ns`: read-await wall time inside an update, including scheduler waiting,
  buffered reads and possible transport decryption; not physical network or CPU time.
- `decode_apply_ns`: parsing/decoding/framebuffer mutation, **including** receive.
- `snapshot_ns`: decoded rectangle snapshot allocations/copies.
- `queue_ns`: metadata enqueue to actual UI consumption, including polling delay.
- `texture_conversion_ns`: ColorImage conversion/allocation.
- `texture_manager_ns`: set_partial locking and CPU delta enqueue, not GPU upload.
- `pipeline_ns`: first server-update byte received to CPU completion fence; excludes
  server scheduling and time waiting before an update's first byte.
- `credit_wait_ns`: acquisition inside an unsolicited update only; normal withheld
  requests are not represented here. Source schedule-to-completion includes that wait.
- `gpu_present_ns`: null. No GPU upload, compositor, screen or photon timing claimed.

Spans overlap: never sum receive and decode_apply. Neither subtracting receive from
wall-time decode nor a CPU-only egui pass constitutes native GPU verification.
The loopback fixture's source timestamps and completion Unix timestamps use the
same machine's wall clock; clock adjustment can invalidate such a run. They measure
source schedule/send-to-CPU completion, not input-to-photon latency.

Normal connections skip profiling clocks. Diagnostic metadata is bounded by the same
two credits; completed sample storage is capped at 10,000, and emitted after timing.
Do not extrapolate these microbenchmark results to WAN latency or native GUI FPS.

`framebuffer_ns` instruments coarse Raw blits and CopyRect moves, not per-pixel
clocks. `framebuffer_complete=false` for other codecs or resizes; do not infer their
mutation cost from it. `decode_residual_ns` is inclusive decode/apply minus read-await
and coarse mutation only when coverage is complete; this remains unattributed wall
time (including allocations/scheduling), **not measured CPU cycles**. Resize UI
preparation precedes the profile metadata and is not isolated by this fixture.

## Reproduction

Build with `cargo build --release --locked -j2`. Both arms must include identical
measurement code. The reference is `0524df23bb4f3860acb8d45445a413859c32593a`
(PR head `6b8ae5e` plus measurement only), **not an unmodified 6b8 executable**.
The final measured candidate is `47e3fa6350a5c7f0b9f3d627952ba1ab0ad31885`: snapshot commit `d269ee7`, Raw conversion commit `c48c01d`, and fixed-array chunk lint cleanup.
Retain separately named binaries before rebuilding the candidate.

```
python3 scripts/pipeline_latency_bench.py --self-test
python3 scripts/pipeline_latency_bench.py --verify /path/to/candidate --output verify-new
python3 scripts/pipeline_latency_bench.py --baseline /path/to/baseline --candidate /path/to/candidate --output copyrect-new --blocks 3 --updates 120 --warmup 5
python3 scripts/pipeline_latency_bench.py --baseline /path/to/baseline --candidate /path/to/candidate --output raw-new --blocks 3 --updates 120 --warmup 5 --workload raw
```

Each workload runs three ABBA blocks at both 1080p and 4K: six runs/arm/resolution,
120 ordered updates/run, first five excluded from latency/throughput but retained
in completion counts. Fixed 60 Hz source schedule and 16 ms CPU consumer polling;
no simultaneous build/benchmark jobs. The source does not discard overdue updates.
Throughput excludes warmup and can temporarily exceed 60/s while catching up after
initialization. Output includes per-update JSON, source timestamps, stderr, binary
and fixture SHA-256, per-run p50/p95 and viewer peak RSS. No p99 claim with this N.
The separate 64x32 pixel oracle reconstructs actual egui deltas and compares all
final RGBA pixels using FNV-1a64; it is never enabled for performance comparisons.


## Scope and validation limits

The benchmark bypasses widget layout/native rendering and exercises the real
network decoder, framebuffer snapshots, FIFO event handler and egui CPU texture
preparation. Both arms use identical instrumentation; its overhead versus an
uninstrumented GUI has **not** been quantified. No inference about input latency,
WAN behavior, GPU upload, compositor scheduling or physical screen refresh follows.
The cloud native-GUI attempt was not retried; native GPU acceptance remains open.

The fixture-derived decoded snapshot bound remains two times 3840×2160×4 =
66,355,200 bytes (63.28125 MiB), **not an observed outstanding-byte peak** and not
an RSS cap. RSS includes framebuffer, wire buffers, texture deltas, allocator,
application and profiling state. Peak RSS is measured for the viewer process only;
it excludes the fixture server. Reducing a copy does not change ordering or credit
release boundaries, and no GPU wait/coalescing/extra credits were introduced.

Validation: 131 release tests, 15 actual-client interoperability fixtures, separate
actual-egui pixel oracle (12 updates, all final pixels), and malformed-server CPU
profile returning exit 1 with 0/1 completions. Performance completion/order checks
are not pixel-oracle checks of all 5,760 benchmark updates. Strict Clippy remains
failing with the same 18 `(message, source file)` diagnostics as original `6b8ae5e`
on Rust 1.99.1; no new diagnostic remains. Existing warnings were not suppressed.

## Paired results (2026-10-28, assistant Linux cloud)

AMD EPYC 9V74, Rust 1.99.1, optimized locked build, no concurrent build workload.
Each row compares the measurement reference to final code `47e3fa6`. Every arm in
all four cases completed **720/720 updates (6×120), zero aborts, FIFO sequence valid**:
48 runs / 5,760 updates total. Table values are **medians of six per-run statistics**,
including per-run nearest-rank percentiles; they are not pooled percentiles.

| Workload | CPU updates/s | Source-send → CPU p50 / p95 (ms) | Viewer peak RSS (MiB, median run) |
|---|---:|---:|---:|
| 1080p CopyRect | 59.99 → 59.98 | 12.10 / 19.59 → 11.52 / 19.04 | 48.1 → 48.3 |
| 4K CopyRect | 62.21 → 61.03 | 27.41 / 34.31 → 25.18 / 33.71 | 229.9 → 198.6 |
| 1080p Raw | 53.54 → 59.91 | 27.38 / 37.20 → 15.84 / 23.48 | 52.0 → 59.9 |
| 4K Raw | 12.20 → 37.79 | 98.61 / 118.64 → 42.56 / 53.40 | 166.9 → 135.4 |

For 4K Raw, residual decode/apply wall time p50 fell **59.62 → 6.49 ms**, while
read-await and framebuffer blit remain separate costs. The exact negotiated BGRX
fast path avoids three generic scaling divisions per pixel and reuses the input
allocation for RGBA instead of allocating a second full image. It retains the
wire-padding-is-not-alpha rule (output alpha 255), exact format guard and generic
fallback. This supports a conversion/allocation bottleneck in this Raw workload,
not a claim that backpressure or GPU rendering was the root cause.

For 4K CopyRect, snapshot p50 fell **5.23 → 3.51 ms** (32.9%), while framebuffer
moves and texture preparation still cost several milliseconds. The safe snapshot
change initializes each output byte once rather than zeroing then overwriting it.
The 60 Hz source is near the CPU consumer ceiling; there is **no demonstrated
throughput/FPS improvement** here. An earlier complete paired run at `c48c01d`
(before equivalent fixed-array chunk cleanup) had worse CopyRect source-send p95
33.39 → 36.96 ms; final p95 is slightly better. Consequently **no robust CopyRect
tail-latency win** is claimed. RSS is also workload-dependent: 1080p Raw RSS rose.

The overloaded Raw fixture retains overdue source ticks, so scheduled-source to
CPU latency includes source backlog, unlike source-send latency. These are finite,
ordered stress tests, not a model of every real VNC server's damage policy.
See [machine-readable results](framebuffer-pipeline-latency-results.json) for all
stage statistics, RSS ranges and executable hashes. Identical instrumentation in
both arms supports comparison, but instrumentation overhead itself is unquantified.

Next acceptance step: a permitted native-GPU environment should timestamp texture
upload/submission and presentation separately, with identical server/input traces.
The current evidence does not close that acceptance gap or justify relaxing the
two-credit backpressure mechanism.
