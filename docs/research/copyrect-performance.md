# CopyRect performance investigation

## Scope and baseline

2026-10-27; source baseline `f10c8a947d390a62aad3b551ba4d01087bb5b096`.
The only production caller of `Framebuffer::copy_rect` is the CopyRect decoder,
which checks both source and destination bounds before calling it. The baseline
framebuffer implementation snapshots every source row into `Vec<Vec<u8>>`, then
copies those rows to the destination. Investigate this bounded CPU/allocation
cost, not authentication, network negotiation, texture upload, or screen capture.

## References reviewed before implementation

| Reference | Relevant mechanism | License / decision |
| --- | --- | --- |
| [TigerVNC `PixelBuffer.cxx`, `ModifiablePixelBuffer::copyRect`](https://github.com/TigerVNC/tigervnc/blob/ed951a4127ab50a8ad1a0ace72c8debc52a14635/common/rfb/PixelBuffer.cxx) | Validates bounds; overlap-safe row moves; moves bottom-up when copying down and top-down when copying up. | GPL-2.0-or-later header and GPLv2 `LICENCE.TXT`. General design reference only; no source copied or translated. |
| [Rust 1.99.1 `slice::copy_within`](https://github.com/rust-lang/rust/blob/58cf8f9c35b355199727627d07ae34a4e2846d67/library/core/src/slice/mod.rs#L4347-L4389) | Safe bounds-checked overlapping copy; implementation delegates to `ptr::copy`, without allocation. | Apache-2.0 OR MIT, verified in `COPYRIGHT`. Use the standard API directly, not a copied implementation. |

## Decision and validation plan

First measure the unchanged source using an optimized Rust harness. If the
allocation cost is confirmed, replace only the temporary rows with directional
`copy_within` calls. Keep protocol bounds checks and the wire behavior unchanged;
add no dependency and no unsafe production code. Verify against an independent
immutable-snapshot oracle, including every valid rectangle of small asymmetric
framebuffers, all overlap directions, diagonal moves, zero dimensions, self
copies, narrow rectangles, and edge-aligned rectangles. Preserve allocation
instrumentation separately from timings so counting overhead cannot inflate the
reported speedup.

Synthetic 1080p/4K scrolling timings establish only the cost of this framebuffer
operation on this cloud CPU. They do not establish desktop FPS, network latency,
real Mac interoperability, GPU upload performance, or whole-viewer speedup.

## Measurement method and results

Measured on 2026-10-27 in the assistant's Linux x86_64 cloud environment:
Intel Xeon Platinum 8573C, Rust 1.99.1 (`58cf8f9c35b355199727627d07ae34a4e2846d67`),
LLVM 23.1.1, glibc 2.41. The existing toolchain was reused. Both source variants use the
same `rustc --edition 2021 -C opt-level=3` harness and default system allocator.
No network, GUI, authentication, desktop capture, or GPU is involved.

The measured baseline framebuffer SHA256 is
`08cb93dbac23e22df4ef342ecc846a1f2ceb2f531d551febbdc440703726fa00`;
the candidate framebuffer SHA256 is
`81ac1e437258e419432efdcb3f1be6c3cd843516cbd80b8ec748c4e7e843eb79`;
the shared harness SHA256 is
`4c72075906605fdae0462266a765e84e955df3a9a41c97fb04fdb6ff79fcf506`.

For each workload, run baseline/candidate/candidate/baseline (ABBA), each in a
fresh process pinned to allowed CPU 8. Each process allocates and initializes
one framebuffer, performs 16 untimed warm-up copies, then measures 501 repeated
copies on that framebuffer. The table is the **range of the two per-process
medians**, not a confidence interval. Per-operation p95 values are retained in
the raw CSV below. Builds and other tests were not running during this final
measurement; the shared cloud host can still introduce scheduling noise.

These are warmed, repeated-copy **microbenchmarks**, not cold-image or whole
viewer measurements. Repeated copies can make the image contents repetitive;
no compression is involved in this operation. Other allocators, CPUs, workload
order, cache state, and real update mixes can change the results. In particular,
these numbers do not establish desktop FPS or end-to-end latency improvement.

Allocation counts use a separate `--cfg allocation_counts` executable that
wraps `System`; it is never used for timing. Its output reports allocation calls
and the sum of **requested allocation bytes per CopyRect operation**, not RSS,
peak memory, bytes copied, or allocations elsewhere in the viewer. The timing
executable does not contain the counting allocator. All eight candidate cases
measured zero allocation calls and zero requested bytes.

| Workload | Baseline median (µs) | Candidate median (µs) | Baseline allocations | Baseline requested bytes |
| --- | ---: | ---: | ---: | ---: |
| `small_tile` | 0.844–0.879 | 0.101–0.101 | 33 | 4,864 |
| `1080p_scroll_up` | 3127.188–3136.101 | 312.832–319.049 | 1,065 | 8,197,056 |
| `1080p_scroll_down` | 3124.843–3145.111 | 322.978–324.321 | 1,065 | 8,197,056 |
| `4k_scroll_up` | 13450.281–13492.456 | 1362.417–1377.029 | 2,145 | 32,983,296 |
| `4k_scroll_down` | 13394.188–13441.807 | 1379.251–1389.449 | 2,145 | 32,983,296 |
| `1080p_horizontal` | 3133.307–3146.167 | 299.560–303.265 | 1,081 | 8,251,200 |
| `window_diagonal` | 1348.714–1353.480 | 154.971–155.200 | 721 | 3,703,680 |
| `narrow_column` | 27.889–27.891 | 2.478–2.535 | 1,065 | 29,792 |

1080p/4K vertical-scroll medians decreased by at least 89.6% when conservatively
comparing each workload's slowest candidate median with its fastest baseline
median. This claim applies only to the operation and method above. CopyRect
still has to copy pixel bytes, and downstream `sub_rgba`/GUI work is unchanged.

### Reproduce

Use a checkout containing this harness and a Rust toolchain in `PATH`:

```bash
set -eu
baseline=f10c8a947d390a62aad3b551ba4d01087bb5b096
out=$(mktemp -d)
git show "$baseline:src/framebuffer.rs" > "$out/baseline.rs"
for variant in baseline fixed; do
  source="$PWD/src/framebuffer.rs"
  if [ "$variant" = baseline ]; then source="$out/baseline.rs"; fi
  IRONVNC_FRAMEBUFFER_RS="$source" rustc --edition 2021 -C opt-level=3 \
    scripts/copyrect_bench.rs -o "$out/$variant"
  IRONVNC_FRAMEBUFFER_RS="$source" rustc --edition 2021 -C opt-level=3 \
    --cfg allocation_counts scripts/copyrect_bench.rs -o "$out/alloc-$variant"
done
"$out/alloc-baseline"
"$out/alloc-fixed" --assert-zero-allocations
# Choose a CPU allowed by the current host; this measurement used 8.
CPU=8
for case in small_tile 1080p_scroll_up 1080p_scroll_down 4k_scroll_up \
  4k_scroll_down 1080p_horizontal window_diagonal narrow_column; do
  for variant in baseline fixed fixed baseline; do
    echo "$case $variant"
    taskset -c "$CPU" "$out/$variant" --case="$case"
  done
done
```

`taskset` is Linux-specific; omit it on other systems and record that difference.
Framebuffer construction, initialization, console output, sample allocation and
sorting are outside the timed regions. Timing still includes the `Instant`
measurement overhead and the same `black_box`/call overhead in both variants.

### Correctness and negative control

The three added tests check 10,052 valid rectangle scenarios against an
independent immutable pixel snapshot. The exhaustive small-frame test covers
all source/destination positions and sizes on 0×0, 1×1, 5×4, and 4×5 frames.
Separate 1080p and 4K tests cover vertical and horizontal shifts, diagonal
window moves, a narrow column, and full-frame self-copy. Input bytes mix both
high and low index bits to avoid repeating at common row strides.

The exact same tests passed against the original and candidate implementations.
A local negative-control variant intentionally forced downward copies to run
top-down. All three tests failed as expected; the 1080p and 4K tests identified
incorrect bytes at offsets 245760 and 491520 respectively. This mutant is not
part of the patch. The valid-input contract is explicit; historical behavior
for invalid direct calls is not an equivalence claim. The production decoder's
source/destination rejection remains unchanged.

### Raw final timing output

`order` identifies ABBA position for each case; `fixed` means the candidate.
`median_ns` and `p95_ns` are per-operation durations from each 501-sample process.

```csv
case,order,variant,samples,median_ns,p95_ns
small_tile,1,baseline,501,879,888
small_tile,2,fixed,501,101,125
small_tile,3,fixed,501,101,127
small_tile,4,baseline,501,844,871
1080p_scroll_up,1,baseline,501,3127188,3763809
1080p_scroll_up,2,fixed,501,319049,492562
1080p_scroll_up,3,fixed,501,312832,455162
1080p_scroll_up,4,baseline,501,3136101,3865039
1080p_scroll_down,1,baseline,501,3124843,3985210
1080p_scroll_down,2,fixed,501,324321,482124
1080p_scroll_down,3,fixed,501,322978,443805
1080p_scroll_down,4,baseline,501,3145111,3653461
4k_scroll_up,1,baseline,501,13450281,15178371
4k_scroll_up,2,fixed,501,1362417,1721541
4k_scroll_up,3,fixed,501,1377029,1691680
4k_scroll_up,4,baseline,501,13492456,15526419
4k_scroll_down,1,baseline,501,13441807,15452280
4k_scroll_down,2,fixed,501,1389449,1751515
4k_scroll_down,3,fixed,501,1379251,1736386
4k_scroll_down,4,baseline,501,13394188,15229144
1080p_horizontal,1,baseline,501,3146167,3641801
1080p_horizontal,2,fixed,501,303265,439442
1080p_horizontal,3,fixed,501,299560,405972
1080p_horizontal,4,baseline,501,3133307,3719110
window_diagonal,1,baseline,501,1353480,1796480
window_diagonal,2,fixed,501,154971,230662
window_diagonal,3,fixed,501,155200,263617
window_diagonal,4,baseline,501,1348714,1675775
narrow_column,1,baseline,501,27889,45355
narrow_column,2,fixed,501,2478,2785
narrow_column,3,fixed,501,2535,2815
narrow_column,4,baseline,501,27891,29474
```

## Local verification record

All commands ran on the toolchain/host above, with Cargo jobs capped at two and
an unchanged `Cargo.lock`. The isolated source snapshot contained the tracked
source/build files verified against the baseline Git blob hashes; no existing
checkout or user modifications were overwritten.

| Check | Result |
| --- | --- |
| Baseline `cargo test --locked --release --all-targets --jobs 2` | Exit 0: 95 passed, 0 failed/ignored/filtered. |
| Candidate same release-test command | Exit 0: 98 passed, 0 failed/ignored/filtered (the three new framebuffer tests included). |
| Candidate `cargo build --locked --release --bin ironvnc --jobs 2` | Exit 0; existing `ProvidePassword` dead-code warning in unchanged `src/connection.rs`. |
| Candidate `python3 scripts/rfb_fixture.py --binary <release-binary>` | Exit 0: all 15 synthetic localhost RFB cases passed. No real desktop or external VNC target used. |
| Candidate `cargo fmt --all -- --check`, plus `rustfmt --edition 2021 --check scripts/copyrect_bench.rs` | Exit 0 for both. |
| Standalone framebuffer tests, optimized and unoptimized | Exit 0: 3 passed each, no ignored/filtered tests. Original source plus the identical oracle tests also passed. |
| Standalone `clippy-driver --edition 2021 --test src/framebuffer.rs ... -D warnings`, and both benchmark configurations | Exit 0; no lint warnings in the changed Rust code. |
| Baseline and candidate `cargo clippy --locked --release --all-targets --jobs 2 -- -D warnings` | Both exit 101 with the same 18 diagnostic message/location pairs, all in unchanged files. No new diagnostics. Repository-wide strict lint is **not green**. |

The existing strict-Clippy findings include `ProvidePassword` dead code,
manual `div_ceil`, excessive argument counts, identical `if` blocks,
`chunks_exact` suggestions, `map_or`, checked-division, and `.err().expect()`
suggestions. They were neither suppressed nor changed as part of this bounded
performance patch. These local results are not a claim about a remote commit's
CI; that must be verified separately after publication.
