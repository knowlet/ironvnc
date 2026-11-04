# Full-frame dirty-update performance investigation

## Scope and evidence

2026-10-27; baseline `d10af413874ac78b3a1b9045efe33611e49eae7d`.
The decoder records dirty bounds while consuming a complete RFB update, then
calls `Framebuffer::sub_rgba` for each bound **after all decoding has finished**.
Consequently, if any one bound is the complete framebuffer, that snapshot
already contains every later Raw write, CopyRect result, or other decoded change
in the update. Copying and enqueuing the remaining regions repeats final pixels.

The change is intentionally narrow: after decoding, scan for one exact
`(0, 0, framebuffer_width, framebuffer_height)` bound and retain only that bound
if present. Empty/single-rectangle lists return without scanning. Lists without
a full-frame rectangle keep their exact bounds and order, even if their union
covers the desktop. The operation is O(rectangle count), with no extra allocation.
No decoder work, resource budget, update request, event ordering, texture API,
queue capacity, or repaint scheduling is changed.

`DesktopSize` already clears old dirty bounds and reallocates the framebuffer.
The reduction runs after that handling and cannot span resizes or separate RFB
updates. The cast back to `u16` uses dimensions originating from validated RFB
`u16` dimensions; it does not introduce a new dimension source.

## Dependencies inspected

Pinned egui/epaint/ecolor 0.31.1 use upstream commit
`1669e52a7ccfc3489c1b0999b9ed48894a0b3887`, licensed MIT OR Apache-2.0.
The package VCS metadata, actual source, license declaration, and bundled
third-party licenses were checked; no upstream implementation was copied.

- [`TextureManager::set`](https://github.com/emilk/egui/blob/1669e52a7ccfc3489c1b0999b9ed48894a0b3887/crates/epaint/src/textures.rs)
  retains every partial delta, including full-sized `set_partial` calls.
  Only full `ImageDelta` operations discard older queued deltas.
- [`ColorImage::from_rgba_unmultiplied`](https://github.com/emilk/egui/blob/1669e52a7ccfc3489c1b0999b9ed48894a0b3887/crates/epaint/src/image.rs)
  allocates a `Color32` vector for each input rectangle.
- [`Color32::from_rgba_unmultiplied`](https://github.com/emilk/egui/blob/1669e52a7ccfc3489c1b0999b9ed48894a0b3887/crates/ecolor/src/color32.rs)
  already fast-paths alpha 255. This investigation does not claim that ordinary
  opaque VNC pixels pay expensive gamma-conversion costs.

Unbounded event channels and the UI's 16 ms repaint request remain separate
hypotheses, not changes in this patch. A repaint request is not a guaranteed
frame interval or a measurement of presentation latency.

## Three distinct verification layers

1. **Production wire regressions:** tests call the actual `VncClient` on
   synthetic loopback RFB exchanges. They verify final pixels, non-full dirty
   lists, full-frame coverage at different positions, later Raw/CopyRect writes,
   same-size/growing resizes, request flags, and immutable snapshots across
   updates. Removing only the collapse call makes the new full-frame wire test
   fail: the original path emits two rectangles where one is sufficient.
2. **Reproducible source-equivalent CPU benchmark:**
   `scripts/full_frame_update_bench.rs` includes the production `Framebuffer`
   and uses the actual pinned crossbeam/egui APIs. It models the small dirty-list
   reduction; it does **not** call private `VncApp::process_events` or the full
   protocol client. Both variants are checked by independently replaying the
   pending texture deltas and comparing every output pixel.
3. **Additional isolated diagnostic:** an instrumented production-client
   loopback harness checked resize, final-pixel, CopyRect, and update-boundary
   behavior. It is not the committed benchmark, so its timing numbers are not
   published here as independently reproducible results.

None of these layers uploads a GPU texture or presents a desktop frame. A native
window/render measurement was unavailable in the restricted cloud environment.
No desktop FPS, real-server interoperability, or send-to-screen improvement is
claimed. These workloads are synthetic, not recordings of the user's desktop;
the frequency of redundant full-frame rectangles in real sessions is unknown.

### Unresolved overload limitation

The additional bounded producer/CPU-consumer diagnostic also detected growing
backlog under a synthetic high-rate 4K CopyRect workload. Its fixture stopped at
its own retained-payload safety limit; this is not a new limit in IronVNC. Faster
decoding can expose the still-unbounded UI queue rather than make the complete
pipeline faster. The exact-full-frame reduction does **not** address non-full
CopyRect backpressure, event wakeups, or the UI's drain-all behavior. Therefore
this patch is not evidence that overload, memory growth, or end-to-end latency
has been resolved. Those need a separately scoped, reproducible investigation;
no queue architecture change is included here.

## CPU benchmark method and bounded results

Linux x86_64 cloud host, Intel Xeon Platinum 8573C, Rust 1.99.1
(`58cf8f9c35b355199727627d07ae34a4e2846d67`), LLVM 23.1.1. The existing toolchain
and release-built locked dependencies were reused. Three warm-ups and 21 samples
per variant; baseline/candidate order alternates each repetition. Builds and
other test workers were idle during sampling, but shared-host scheduling and
cache effects still create noise. No affinity or real-time scheduling was used.

The bounded duplicate-full-frame cases remain within the production 32 Mi-pixel
per-update budget. Counts and summed pending image payload sizes are exact;
they are not RSS, allocator requests, wire bytes, or measured GPU traffic.

| Workload | Pending deltas, before → after | Pending image bytes, before → after |
| --- | ---: | ---: |
| 1080p, eight full-frame rectangles | 8 → 1 | 66,355,200 → 8,294,400 |
| 4K, four full-frame rectangles | 4 → 1 | 132,710,400 → 33,177,600 |
| 1080p, one full frame plus 128×128 damage | 2 → 1 | 8,359,936 → 8,294,400 |
| 4K, one full frame plus 128×128 damage | 2 → 1 | 33,243,136 → 33,177,600 |
| 1080p, 64px disjoint tiles | 510 → 510 | 8,294,400 → 8,294,400 |
| 4K, 64px disjoint tiles | 2,040 → 2,040 | 33,177,600 → 33,177,600 |
| One 64×16 text rectangle, either desktop size | 1 → 1 | 4,096 → 4,096 |
| CopyRect scroll plus bottom-strip damage | 2 → 2 | One framebuffer, unchanged |

The initial exploratory CPU run observed lower median extraction and image-prep
cost for the repeated-full-frame stress cases. Ordinary full-frame-plus-small-
damage updates did **not** show a reliable timing improvement, and unchanged
controls varied. The deterministic reduction of redundant work is the supported
claim; these measurements do not justify a general viewer-speedup percentage.

Timed `extraction` includes selected `sub_rgba` allocations/copies and channel
send; `dequeue` is `try_iter().collect()`; `prep` includes validity checks,
`ColorImage` conversion, `set_partial`, and dropping consumed raw byte vectors.
Texture initialization, delta retrieval/final freeing, source dirty-list
construction, and correctness oracles are outside timed regions. The benchmark
prints p50/p95 stage timings and count/byte totals as CSV; per-stage percentiles
must not be added and described as a measured end-to-end percentile.

### Reproduce

Run from a checkout containing the benchmark with the locked release build
available. The example assumes a native target and default `target/` directory;
adjust `deps` for `CARGO_TARGET_DIR` or an explicit Cargo target triple.

```bash
set -eu
cargo build --locked --release --jobs 2
out=$(mktemp -d)
deps="$PWD/target/release/deps"
# Start with a clean target directory, or select the exact current-build rlibs
# if stale build variants make either glob ambiguous.
set -- "$deps"/libegui-*.rlib
test "$#" -eq 1; egui_rlib=$1
set -- "$deps"/libcrossbeam_channel-*.rlib
test "$#" -eq 1; channel_rlib=$1
IRONVNC_FRAMEBUFFER_RS="$PWD/src/framebuffer.rs" \
  rustc --edition 2021 -C opt-level=3 scripts/full_frame_update_bench.rs \
  -L "dependency=$deps" --extern "egui=$egui_rlib" \
  --extern "crossbeam_channel=$channel_rlib" -o "$out/full-frame-bench"
"$out/full-frame-bench" > "$out/results.csv"
cat "$out/results.csv"
```

This adds no package dependency or network service. A failed pixel assertion
must be investigated, not removed to make timing output complete. Repeated
full-frame stress is a bounded reproduction, not a claim about normal update
frequency. The sparse text, disjoint tiles, overlapping non-full regions, and
CopyRect scrolling controls must retain their output shapes and bytes.

## Local verification

All checks used the toolchain above, the unchanged lockfile, and Cargo jobs
capped at two. The production snapshot and candidate occupied separate
worktrees. No remote device, external VNC target, or user credentials were used.

| Check | Result |
| --- | --- |
| `cargo test --release --all-targets --locked --offline --jobs 2` | Exit 0: 103 passed, 0 failed/ignored/filtered (98 existing plus five new tests). |
| `cargo build --release --locked --offline --jobs 2 --bin ironvnc` | Exit 0; existing `ProvidePassword` dead-code warning only. |
| `python3 scripts/rfb_fixture.py --binary <candidate-release-binary>` | Exit 0: all 15 existing synthetic loopback cases passed. |
| `cargo fmt --all -- --check`, plus `rustfmt --edition 2021 --check scripts/full_frame_update_bench.rs` | Both exit 0. |
| Final portable CPU benchmark, optimized | Exit 0: all 17 paired workload cases and additional small CopyRect pixel oracles passed; counts/bytes match the table above. |
| Standalone benchmark `clippy-driver ... -D warnings` with the same release dependencies | Exit 0, no new-harness lint findings. |
| Baseline and candidate `cargo clippy --release --all-targets --locked --offline --jobs 2 -- -D warnings` | Both exit 101 with the same 18 unique diagnostic message/file/line/column pairs. No new or removed diagnostics. Repository-wide strict lint is **not green**. |
| Negative control, separate worktree with only the production collapse call removed | Expected exit 101: the new exact wire test failed (1 failed, 102 filtered), observing two rectangles rather than one. No candidate file was overwritten. |

The existing strict-Clippy findings occur in pre-existing code and were neither
suppressed nor repaired in this bounded change. The new Rust helper, tests, and
standalone benchmark introduce no diagnostics. These local checks do not assert
remote CI status, presentation performance, or readiness to merge; the known
non-full-frame overload limitation above remains unresolved.
