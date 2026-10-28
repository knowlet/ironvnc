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
