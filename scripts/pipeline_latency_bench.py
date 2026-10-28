#!/usr/bin/env python3
"""Paired, loopback-only RFB-to-CPU-egui benchmark; NOT native display FPS.

Both binaries must have the same measurement-only harness. The fixture uses no
credentials, desktop capture, display server, or third-party Python packages.
A scheduled source tick is paced independently of request arrival; the server
never drops/coalesces updates, so scheduled-to-completion includes source backlog.
"""
import argparse
import concurrent.futures
import hashlib
import json
import math
import os
from pathlib import Path
import socket
import signal
import struct
import subprocess
import sys
import time

PIXEL_FORMAT = struct.pack(">BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0)


def exact(conn, size):
    result = bytearray()
    while len(result) < size:
        block = conn.recv(size - len(result))
        if not block:
            raise EOFError(f"EOF after {len(result)}/{size} bytes")
        result.extend(block)
    return bytes(result)


def rect(x, y, width, height, encoding):
    return struct.pack(">HHHHi", x, y, width, height, encoding)


def initial_update(width, height):
    # A nonuniform row repeated vertically: full RAW decode, no compression.
    row = b"".join(bytes((x % 251, (x * 7) % 251, (x * 13) % 251, 0))
                   for x in range(width))
    return b"\x00\x00\x00\x01" + rect(0, 0, width, height, 0) + row * height


def copy_update(width, height, sequence):
    # Move the old image right, then refresh the exposed stripe. Both rectangles
    # are needed; the viewer must preserve CopyRect order and every source tick.
    pixel = bytes((sequence % 251, (sequence * 7) % 251, (sequence * 13) % 251, 0))
    return (b"\x00\x00\x00\x02" + rect(1, 0, width - 1, height, 1)
            + struct.pack(">HH", 0, 0) + rect(0, 0, 1, height, 0) + pixel * height)


def serve(listener, width, height, count, hz, timeout, records, workload="copyrect"):
    with listener.accept()[0] as conn:
        conn.settimeout(timeout)
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        conn.sendall(b"RFB 003.008\n")
        if exact(conn, 12) != b"RFB 003.008\n":
            raise AssertionError("RFB 3.8 required")
        conn.sendall(b"\x01\x01")
        if exact(conn, 1) != b"\x01":
            raise AssertionError("None authentication required")
        conn.sendall(bytes(4))
        if exact(conn, 1) != b"\x01":
            raise AssertionError("shared ClientInit required")
        name = b"IronVNC bounded CPU latency fixture"
        conn.sendall(struct.pack(">HH", width, height) + PIXEL_FORMAT
                     + struct.pack(">I", len(name)) + name)
        if exact(conn, 4) != bytes(4) or exact(conn, 16) != PIXEL_FORMAT:
            raise AssertionError("unexpected pixel format")
        if exact(conn, 2) != b"\x02\x00":
            raise AssertionError("expected SetEncodings")
        enc_count = struct.unpack(">H", exact(conn, 2))[0]
        encodings = struct.unpack(f">{enc_count}i", exact(conn, enc_count * 4))
        if not {0, 1}.issubset(encodings):
            raise AssertionError("Raw and CopyRect must be supported")
        # Allocate initialization outside timing; retain only this one packet.
        first = initial_update(width, height)
        origin_mono = origin_wall = None
        for sequence in range(1, count + 1):
            request = exact(conn, 10)
            if request[0] != 3:
                raise AssertionError(f"unexpected client message {request[0]}; no input allowed")
            if struct.unpack(">HHHH", request[2:]) != (0, 0, width, height):
                raise AssertionError("unexpected update request bounds")
            now = time.monotonic_ns()
            if origin_mono is None:
                origin_mono, origin_wall = now, time.time_ns()
            scheduled_mono = origin_mono + round((sequence - 1) * 1e9 / hz)
            delay = (scheduled_mono - now) / 1e9
            if delay > 0:
                time.sleep(delay)
            packet = first if sequence == 1 or workload == "raw" else copy_update(width, height, sequence)
            sent_mono, sent_wall = time.monotonic_ns(), time.time_ns()
            conn.sendall(packet)
            records.append({"sequence": sequence,
                            "scheduled_unix_ns": origin_wall + scheduled_mono - origin_mono,
                            "sent_unix_ns": sent_wall, "sent_monotonic_ns": sent_mono,
                            "send_done_monotonic_ns": time.monotonic_ns(),
                            "request_backlog_ns": max(0, now - scheduled_mono),
                            "wire_bytes": len(packet)})
        # Keep the peer alive until all CPU completions have been consumed.
        try:
            while conn.recv(4096):
                pass
        except ConnectionResetError:
            pass


def quantiles(values):
    if not values:
        return {"n": 0, "p50": None, "p95": None}
    ordered = sorted(values)
    # Nearest-rank percentiles: deterministic, useful for small bounded samples.
    return {"n": len(values), "p50": ordered[math.ceil(len(values) * .50) - 1],
            "p95": ordered[math.ceil(len(values) * .95) - 1]}


def summarize(rows, source, expected, warmup, elapsed, rss_kib, returncode, error):
    updates = [row for row in rows if row.get("type") == "update"]
    sequences = [row.get("sequence") for row in updates]
    valid_order = sequences == list(range(1, len(updates) + 1))
    source_by_id = {row["sequence"]: row for row in source}
    measured = [row for row in updates if row.get("sequence", 0) > warmup]
    stage_names = sorted({key for row in measured for key, value in row.items()
                          if key.endswith("_ns") and key != "completed_unix_ns"
                          and isinstance(value, (int, float))})
    stages = {key[:-3] + "_ms": quantiles([row[key] / 1e6 for row in measured if key in row])
              for key in stage_names}
    for name, stamp in (("source_scheduled_to_cpu_ms", "scheduled_unix_ns"),
                        ("source_sent_to_cpu_ms", "sent_unix_ns")):
        values = [(row["completed_unix_ns"] - source_by_id[row["sequence"]][stamp]) / 1e6
                  for row in measured if "completed_unix_ns" in row
                  and row["sequence"] in source_by_id]
        # Negative wall-clock differences indicate a clock/sample error, not speed.
        stages[name] = quantiles(values) if all(value >= 0 for value in values) else {
            "n": len(values), "p50": None, "p95": None, "clock_error": True}
    completed_times = [row["completed_unix_ns"] for row in measured
                       if "completed_unix_ns" in row]
    throughput = None
    if len(completed_times) > 1 and completed_times[-1] > completed_times[0]:
        throughput = (len(completed_times) - 1) * 1e9 / (completed_times[-1] - completed_times[0])
    return {"expected": expected, "completed": len(updates), "warmup_updates": warmup,
            "complete": returncode == 0 and len(updates) == expected and valid_order and not error,
            "fifo_sequence_valid": valid_order, "returncode": returncode, "error": error,
            "elapsed_seconds": elapsed, "cpu_updates_per_second": throughput,
            "child_peak_rss_kib": rss_kib,
            "max_update_payload_bytes": max((row.get("payload_bytes", 0) for row in updates), default=0),
            "theoretical_two_credit_snapshot_bound_bytes":
                2 * max((row.get("payload_bytes", 0) for row in updates), default=0),
            "snapshot_bound_note": "fixture-derived bound, not observed outstanding bytes or total memory",
            "latency_ms": stages}


def rss_backend():
    """Select before binding sockets; BSD time does not support GNU -f/-o."""
    if sys.platform not in ("linux", "darwin"):
        raise RuntimeError("CPU benchmark RSS/process cleanup supports Linux and macOS only")
    return "gnu-time" if sys.platform == "linux" and Path("/usr/bin/time").exists() else "resource"


def rss_command(command, rss_file, backend):
    if backend == "gnu-time":
        return ["/usr/bin/time", "-f", "%M", "-o", str(rss_file), "--"] + command
    # Fresh process per run: RUSAGE_CHILDREN must not retain earlier maxima.
    # Linux getrusage(2): KiB. XNU calcru uses resident_size_max (bytes).
    # https://man7.org/linux/man-pages/man2/getrusage.2.html
    # https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_resource.c
    # https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/mach/task_info.h
    divisor = 1024 if sys.platform == "darwin" else 1
    wrapper = ("import resource,subprocess,sys; "
               "r=subprocess.run(sys.argv[3:]); "
               "rss=resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss; "
               "open(sys.argv[1],'w').write(str(int(rss)//int(sys.argv[2]))); "
               "sys.exit(r.returncode)")
    return [sys.executable, "-c", wrapper, str(rss_file), str(divisor)] + command


def run_one(binary, width, height, args, label, directory):
    backend = rss_backend()  # Reject unsupported hosts before binding/launching.
    source, rows = [], []
    error = None
    rss_kib = None
    started = time.monotonic()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(args.timeout)
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(serve, listener, width, height, args.updates,
                                 args.hz, args.timeout, source, args.workload)
            command = [str(Path(binary).resolve()), "--profile-cpu", "--host", "127.0.0.1",
                       "--port", str(listener.getsockname()[1]), "--profile-updates",
                       str(args.updates), "--profile-refresh-ms", str(args.refresh_ms)]
            if getattr(args, "verify", None):
                command.append("--profile-verify-pixels")
            rss_file = directory / f"{label}.rss.txt"
            command = rss_command(command, rss_file, backend)
            env = dict(os.environ, IRONVNC_PROFILE="1")
            try:
                process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                           text=True, env=env, start_new_session=True)
                try:
                    stdout, stderr = process.communicate(timeout=args.timeout)
                    returncode = process.returncode
                except subprocess.TimeoutExpired:
                    # Kill the wrapper AND viewer; never leave a timed-out load running.
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    stdout, stderr = process.communicate()
                    returncode, error = process.returncode, "client timeout"
            except OSError as exc:
                stdout, stderr, returncode, error = "", str(exc), None, "client launch failed"
            try:
                future.result(timeout=args.timeout)
            except Exception as exc:
                error = f"{error + '; ' if error else ''}server: {type(exc).__name__}: {exc}"
            if rss_file.exists():
                rss_lines = rss_file.read_text().splitlines()
                if rss_lines and rss_lines[-1].isdigit():
                    rss_kib = int(rss_lines[-1])
    for line in stdout.splitlines():
        try:
            row = json.loads(line)
            if isinstance(row, dict):
                rows.append(row)
        except json.JSONDecodeError:
            pass
    (directory / f"{label}.stdout.jsonl").write_text(stdout)
    (directory / f"{label}.stderr.txt").write_text(stderr)
    (directory / f"{label}.source.jsonl").write_text("".join(json.dumps(row) + "\n" for row in source))
    summary = summarize(rows, source, args.updates, args.warmup,
                        time.monotonic() - started, rss_kib, returncode, error)
    summary.update(label=label, binary=str(Path(binary).resolve()), width=width, height=height)
    if getattr(args, "verify", None):
        pixel_rows = [row for row in rows if row.get("type") == "pixels"]
        expected_hash = expected_pixels_hash(width, height, args.updates)
        verified = (len(pixel_rows) == 1 and pixel_rows[0].get("width") == width
                    and pixel_rows[0].get("height") == height
                    and pixel_rows[0].get("fnv1a64") == expected_hash)
        summary.update(pixel_oracle_pass=verified, expected_fnv1a64=expected_hash,
                       actual_pixel_rows=pixel_rows)
        summary["complete"] = summary["complete"] and verified
    (directory / f"{label}.summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary), flush=True)
    return summary



def expected_pixels_hash(width, height, count):
    """Independent final RGBA oracle, not used during any timed performance run."""
    row = [bytes(((x * 13) % 251, (x * 7) % 251, x % 251, 255))
           for x in range(width)]
    for sequence in range(2, count + 1):
        row = [bytes(((sequence * 13) % 251, (sequence * 7) % 251,
                      sequence % 251, 255))] + row[:-1]
    value = 0xcbf29ce484222325
    for byte in b"".join(row) * height:
        value = ((value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
    return f"{value:016x}"


def rss_self_test():
    import tempfile
    import unittest
    from unittest.mock import patch

    class RssTests(unittest.TestCase):
        def test_platform_selection(self):
            for platform, exists, expected in (("linux", True, "gnu-time"),
                                               ("linux", False, "resource"),
                                               ("darwin", True, "resource"),
                                               ("darwin", False, "resource")):
                with patch.object(sys, "platform", platform), patch.object(Path, "exists", return_value=exists):
                    self.assertEqual(rss_backend(), expected)

        def test_gnu_command_layout(self):
            self.assertEqual(rss_command(["viewer", "--profile-cpu"], Path("sample.rss"), "gnu-time"),
                             ["/usr/bin/time", "-f", "%M", "-o", "sample.rss", "--", "viewer", "--profile-cpu"])

        def test_unsupported_before_socket_or_child(self):
            with patch.object(sys, "platform", "win32"), patch.object(socket, "socket") as sock, patch.object(subprocess, "Popen") as child:
                with self.assertRaisesRegex(RuntimeError, "Linux and macOS"):
                    run_one("unused", 3, 2, None, "unused", Path("unused"))
                sock.assert_not_called()
                child.assert_not_called()

        def test_darwin_conversion_synthetic(self):
            # Execute the generated wrapper with synthetic resource data, not
            # Darwin native evidence. This also checks argv and child exit code.
            import types
            with patch.object(sys, "platform", "darwin"):
                command = rss_command(["fake-child"], "unused", "resource")
            fake_resource = types.SimpleNamespace(RUSAGE_CHILDREN=-1, getrusage=lambda _: types.SimpleNamespace(ru_maxrss=5 * 1024))
            with patch.dict(sys.modules, resource=fake_resource), patch.object(sys, "argv", ["-c"] + command[3:]), patch.object(subprocess, "run", return_value=types.SimpleNamespace(returncode=7)), patch("builtins.open", unittest.mock.mock_open()) as output:
                with self.assertRaises(SystemExit) as exit_result:
                    exec(command[2], {})
                self.assertEqual(exit_result.exception.code, 7)
                output().write.assert_called_once_with("5")

        @unittest.skipUnless(sys.platform in ("linux", "darwin"), "native resource requires Unix")
        def test_native_resource_fresh_runs(self):
            with tempfile.TemporaryDirectory() as tmp:
                peaks = []
                for size in (64 * 1024 * 1024, 1024):
                    output = Path(tmp) / str(size)
                    command = rss_command([sys.executable, "-c", f"x=bytearray({size}); print('child-ok')"], output, "resource")
                    result = subprocess.run(command, capture_output=True, text=True, timeout=15)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.strip(), "child-ok")
                    peaks.append(int(output.read_text()))
                self.assertGreater(peaks[0], 32 * 1024)
                self.assertLess(peaks[0], 512 * 1024)  # catches Darwin bytes mislabeled as KiB
                self.assertLess(peaks[1], peaks[0])  # no previous child high-water contamination

        @unittest.skipUnless(sys.platform == "linux" and Path("/usr/bin/time").exists(), "GNU time Linux only")
        def test_native_gnu_time(self):
            with tempfile.TemporaryDirectory() as tmp:
                output = Path(tmp) / "rss"
                result = subprocess.run(rss_command([sys.executable, "-c", "x=bytearray(8*1024*1024)"], output, "gnu-time"), capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertGreater(int(output.read_text()), 0)

        @unittest.skipUnless(sys.platform in ("linux", "darwin"), "native resource requires Unix")
        def test_native_resource_failure_exit(self):
            with tempfile.TemporaryDirectory() as tmp:
                output = Path(tmp) / "rss"
                result = subprocess.run(rss_command([sys.executable, "-c", "raise SystemExit(7)"], output, "resource"), capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 7)
                self.assertGreater(int(output.read_text()), 0)

    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(RssTests))
    if not result.wasSuccessful():
        raise SystemExit(1)


def self_test():
    rss_self_test()
    assert len(initial_update(3, 2)) == 4 + 12 + 24
    assert copy_update(3, 2, 2)[:4] == b"\x00\x00\x00\x02"
    assert len(copy_update(3, 2, 2)) == 4 + 12 + 4 + 12 + 8
    assert quantiles([3, 1, 2]) == {"n": 3, "p50": 2, "p95": 3}
    rows = [{"type": "update", "sequence": n, "completed_unix_ns": n * 10_000_000,
             "payload_bytes": 24, "queue_ns": 2_000_000} for n in range(1, 4)]
    source = [{"sequence": n, "sent_unix_ns": n * 10_000_000 - 1_000_000,
               "scheduled_unix_ns": n * 10_000_000 - 2_000_000} for n in range(1, 4)]
    summary = summarize(rows, source, 3, 1, .1, 100, 0, None)
    assert summary["complete"] and summary["cpu_updates_per_second"] == 100
    assert summary["latency_ms"]["source_sent_to_cpu_ms"]["p95"] == 1
    assert not summarize(rows[:2], source, 3, 1, .1, None, 0, None)["complete"]
    assert not summarize(rows[::-1], source, 3, 1, .1, None, 0, None)["complete"]
    assert not summarize(rows, source, 3, 1, .1, None, 1, "failure")["complete"]
    records = []
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(5)
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(serve, listener, 3, 2, 3, 60, 5, records)
            with socket.create_connection(listener.getsockname(), timeout=5) as conn:
                assert exact(conn, 12) == b"RFB 003.008\n"
                conn.sendall(b"RFB 003.008\n")
                assert exact(conn, 2) == b"\x01\x01"
                conn.sendall(b"\x01")
                assert exact(conn, 4) == bytes(4)
                conn.sendall(b"\x01")
                header = exact(conn, 24)
                assert b"CPU" in exact(conn, struct.unpack(">I", header[20:24])[0])
                conn.sendall(bytes(4) + PIXEL_FORMAT + struct.pack(">BBHii", 2, 0, 2, 0, 1))
                for sequence in range(1, 4):
                    conn.sendall(struct.pack(">BBHHHH", 3, int(sequence > 1), 0, 0, 3, 2))
                    expected = initial_update(3, 2) if sequence == 1 else copy_update(3, 2, sequence)
                    assert exact(conn, len(expected)) == expected
            future.result(timeout=5)
    assert [row["sequence"] for row in records] == [1, 2, 3]
    print("PASS: packet layouts, percentiles, completion, FIFO/failure summaries, loopback protocol")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify", metavar="BINARY", help="separate 64x32, 12-update pixel oracle; not performance")
    parser.add_argument("--baseline")
    parser.add_argument("--candidate")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--workload", choices=["copyrect", "raw"], default="copyrect")
    parser.add_argument("--updates", type=int, default=120)
    parser.add_argument("--warmup", type=int, default=5)
    parser.add_argument("--hz", type=float, default=60)
    parser.add_argument("--refresh-ms", type=int, default=16)
    parser.add_argument("--blocks", type=int, default=3, help="ABBA blocks per resolution")
    parser.add_argument("--resolutions", nargs="+", choices=["1080p", "4k"], default=["1080p", "4k"])
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    try:
        rss_backend()
    except RuntimeError as exc:
        parser.error(str(exc))
    if not (0 < args.hz <= 1000 and 1 <= args.refresh_ms <= 1000 and 1 <= args.timeout <= 3600):
        parser.error("invalid pacing/refresh/timeout parameters")
    if args.verify:
        if not args.output or args.baseline or args.candidate:
            parser.error("--verify requires --output and cannot be combined with baseline/candidate")
        args.updates, args.warmup, args.workload = 12, 0, "copyrect"
        args.output.mkdir(parents=True, exist_ok=False)
        summary = run_one(args.verify, 64, 32, args, "pixel-oracle", args.output)
        if not summary["complete"]:
            raise SystemExit(1)
        return
    if not args.baseline or not args.candidate or not args.output:
        parser.error("--baseline, --candidate and --output are required")
    if not (2 <= args.updates <= 10000 and 0 <= args.warmup < args.updates - 1
            and 0 < args.hz <= 1000 and 1 <= args.refresh_ms <= 1000
            and 1 <= args.blocks <= 20 and 1 <= args.timeout <= 3600):
        parser.error("invalid bounded workload/warmup/refresh/timeout parameters")
    args.output.mkdir(parents=True, exist_ok=False)
    manifest = {"arguments": vars(args) | {"output": str(args.output)},
                "started_unix_ns": time.time_ns(), "python": os.sys.version,
                "platform": os.uname()._asdict() if hasattr(os.uname(), "_asdict") else list(os.uname()),
                "cpu_count": os.cpu_count(), "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                "measurement": "CPU-egui only; no GPU upload/present/display timing",
                "clock": "same-host UNIX ns for cross-process latency; monotonic source pacing",
                "stage_units": "latency_ms values are milliseconds; input *_ns are nanoseconds"}
    for arm in ("baseline", "candidate"):
        manifest[arm + "_sha256"] = hashlib.sha256(Path(getattr(args, arm)).read_bytes()).hexdigest()
    (args.output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    summaries = []
    for resolution in args.resolutions:
        width, height = (1920, 1080) if resolution == "1080p" else (3840, 2160)
        for block in range(args.blocks):
            for position, arm in enumerate(("baseline", "candidate", "candidate", "baseline")):
                label = f"{resolution}-block{block + 1}-{position + 1}-{arm}"
                summaries.append(run_one(getattr(args, arm), width, height, args, label, args.output))
    (args.output / "summary.json").write_text(json.dumps(summaries, indent=2) + "\n")
    if not all(row["complete"] for row in summaries):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
