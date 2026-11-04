# Bound framebuffer updates retained by a slow consumer

## Scope and behavior

This change builds on `1b6d9e1b7f4f26536ed5fb9ac269d491d87b64a8`.
The existing exact-full-frame reduction only removes redundant rectangles inside
one update. It does not stop non-full CopyRect updates from accumulating when the
network/decoder produces them faster than the UI prepares textures.

Each connection now has **two framebuffer-update credits** using the existing
Tokio semaphore dependency. A credit is acquired before issuing a request or
reading an unsolicited update payload. It stays owned by that update through:

1. decode and framebuffer mutation;
2. FIFO resize/pixel event delivery;
3. a trailing `VncEvent::UpdateComplete(OwnedSemaphorePermit)`;
4. the consumer's preparation of all preceding events.

The non-cloneable permit is released at the completion fence, not when events are
removed from the channel. The real UI collects a fixed-length event snapshot,
then prepares `ColorImage` / `set_partial` updates in order. It releases each
fence **before `end_pass`**, after its preceding pixel preparation. Newly produced
updates remain for a later UI pass. The headless consumer acknowledges fences too.

There is no cross-update pixel coalescing, update dropping, blocking Crossbeam
send, dependency addition, encoding change, or new server-rejection policy.
The existing per-update rectangle/pixel/resize limits remain in effect.

### Waiting and cancellation

When no credit is available, the next framebuffer request is withheld. The
existing 5 ms polling loop continues to send input/clipboard commands and read
server controls. Resize keeps a full-refresh request pending until that request
is actually sent, including across unsolicited updates.

An unsolicited framebuffer header also obeys the credit limit. Its payload waits
without allocating more decoded update data, while outgoing commands remain
serviceable. Later incoming server controls cannot pass that payload on the same
TCP stream; this is a documented head-of-line limitation, not a rejection.

`VncConnection::disconnect` and owner `Drop` send both the existing Disconnect
command and a retained `Notify::notify_one` cancellation. The worker's biased,
cancellation-first select drops the entire client future/stream. This covers
cancellation before the worker starts, while waiting for a credit, and after a
credit has been granted but a payload is incomplete. The command remains
necessary to wake synchronous password/credential waits.

Old completion fences cannot grant credits to a replacement connection. Failed
decode and failed event delivery release their locally owned permits. Direct
internal `VncClient::connect` callers must maintain the consumer or explicitly
send Disconnect/drop the command producer while retaining completion fences;
they do not own the production connection's separate cancellation notification.

### What is not claimed

Two credits bound the number of retained/in-progress framebuffer update batches,
including drained-but-not-prepared events. The existing 32 Mi-pixel (32 × 1024 × 1024 pixels) per-update
pixel budget bounds their RGBA snapshots. This is **not a total process RSS bound**:
persistent framebuffer storage, decoder temporaries, egui texture deltas, GPU
resources, clipboard/control events, and other application state are separate.
The general control-event channel remains unbounded.

CPU-egui tests do not establish native GUI/GPU rendering, presentation latency,
or behavior on a user's devices. No display workaround is part of this change.

## Deterministic regressions

The test-only patch on the unmodified baseline reproduces the defect: after two
updates with a paused consumer, the client sends a third request. Its assertion
fails with 0 passed, 1 failed, and 103 existing tests filtered out.

The candidate adds thirteen real-client backpressure/lifecycle tests and two
actual `VncApp::process_events` CPU-egui tests:

- queued and drained-but-held updates both consume credits; releasing one allows
  exactly one additional update, with no mutation of earlier snapshots;
- input, server controls, disconnect, and command-owner loss remain live under
  pressure;
- unsolicited updates cannot bypass the gate;
- empty, cursor-only, resize-only, and later pixel updates complete in FIFO order;
- a pending full refresh survives an unsolicited empty update after resize;
- reconnect uses fresh credits despite retained old-generation fences;
- owner drop and explicit disconnect cancel incomplete payloads; one regression
  waits for a real DesktopSize event before dropping the owner, proving the
  decoder has passed the credit gate;
- immediate cancellation and a synchronous password prompt both terminate;
- persistent ZRLE state survives a pause and fragmented next payload;
- decode errors release only their reserved credit;
- the actual UI's reconstructed texture deltas contain the expected pixels after
  resize, and a mandatory observed repaint callback verifies credits were not
  returned while collecting the batch.

The previous assertion fixtures now consume/drop completion fences rather than
retaining flow-control tokens in their historical event lists. The production
limit is not bypassed for tests.

## Local verification (2026-10-27)

Linux x86_64, Rust/Cargo 1.99.1, locked dependencies, offline Cargo cache:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Exit 0. |
| `cargo test --release --all-targets --locked --offline` | Exit 0: **118 passed**, 0 failed/ignored/filtered (103 existing + 15 new). |
| `cargo build --release --locked --offline --bin ironvnc` | Exit 0; the existing `ProvidePassword` dead-code warning remains. |
| `python3 scripts/rfb_fixture.py --binary <candidate-release-binary>` | Exit 0: **15/15** loopback interoperability cases passed. |
| Baseline and candidate `cargo clippy --release --workspace --all-targets --locked --offline --message-format=json -- -D warnings` | Both exit 101: the same **18 distinct source-site diagnostics**, no new or removed findings. Strict repository lint is **not green**. |
| `git diff --check` | Exit 0. |

Clippy comparison first deduplicates repeated binary/test emissions at the same
source site. It then matches code, message, file, highlighted source and columns,
allowing line offsets to move. Two identical ARD test snippets at different
source lines remain two diagnostics; merging those sites incorrectly reports 17.

Raw commands, final logs, source/binary SHA256, the red baseline, and the complete
Clippy mapping are retained in the local `backpressure` evidence directory.
Independent guarded CPU-consumer stress results are recorded separately; the
unit/fixture results above are not substituted for that measurement.

## Independent guarded CPU-consumer check

A separate loopback harness used the actual connection/client/decoder plus a
CPU-egui consumer matching the application's finite snapshot and **in-loop,
pre-`end_pass` fence release**. It did not exercise a native renderer. Primary
stress kept 3840×2160 CopyRect scrolling, 60 target updates/s, 16 ms polling,
60 updates per run, three alternating pairs, and the unchanged 384 MiB decoded
payload / 1 GiB sampled-RSS guards. A 120-update run was separately labeled.

| Workload | Baseline `1b6d9e1` | Two-credit candidate |
| --- | --- | --- |
| 4K, three 60-update runs | 1 completed; 2 hit decoded-payload guard | **3/3 completed** |
| 4K, 120-update run | Hit decoded-payload guard | **Completed** |
| 1080p ordinary control | **3/3 completed** | **3/3 completed** |

The candidate retained at most two decoded payloads (about 63.3 MiB in this 4K
workload). Under overload, requests paced production to approximately **19–32
updates/s**, not 60 FPS: preventing backlog is not evidence of faster decoding or
native presentation. Both versions sustained approximately 60 updates/s in the
ordinary 1080p control. Independent exhaustive-pixel/FIFO checks and held-credit
input, explicit-disconnect, and owner-drop controls also passed.

The separate `ironvnc-backpressure-evidence-20261027` evidence bundle contains
copied-source instrumentation, harness, runner, raw final results, source/binary
hashes and preserved earlier fixture failures. An initial fixture incorrectly
required an extra request after all expected updates had completed; teardown was
corrected to accept clean disconnect, and the complete matrix was rerun without
changing workload or guards. Optional stricter whole-batch fence retention is
reported separately and is not used for the comparisons above.
