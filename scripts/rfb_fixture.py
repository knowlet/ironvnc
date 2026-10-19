#!/usr/bin/env python3
"""Exercise the actual viewer against synthetic, loopback-only RFB servers.

Wire expectations come from RFC 6143 (including erratum 4951) and the
rfbproto Tight encoding specification, independently of the viewer decoder.
No display server, desktop capture, credentials, or third-party Python packages.
"""
import argparse
import concurrent.futures
import socket
import struct
import subprocess
import zlib


PIXEL_FORMAT = struct.pack(
    ">BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0
)


def exact(conn, size):
    result = bytearray()
    while len(result) < size:
        block = conn.recv(size - len(result))
        if not block:
            raise AssertionError(f"EOF after {len(result)}/{size} bytes")
        result.extend(block)
    return bytes(result)


def serve(listener, banner, auth, encoding, failure, black):
    with listener.accept()[0] as conn:
        conn.settimeout(15)
        conn.sendall(banner)
        if failure == "banner":
            # Invalid banners must be rejected, without trying legacy auth.
            assert conn.recv(12) == b""
            return
        minor = 8 if banner == b"RFB 003.889\n" else int(banner[8:11])
        assert exact(conn, 12) == f"RFB 003.{minor:03d}\n".encode()
        if failure == "unsupported-apple":
            conn.sendall(bytes([3, 33, 36, 35]))
            assert conn.recv(1) == b"", "an unsupported Apple security type was selected"
            return
        if minor == 3:
            conn.sendall(struct.pack(">I", auth))
        else:
            conn.sendall(bytes([1, auth]))
            assert exact(conn, 1) == bytes([auth])
        if auth == 2:
            conn.sendall(bytes(range(16)))
            # Independently generated with LibreSSL DES-ECB, reversed bits of
            # each byte of 'password'; covers verified RFC erratum 4951.
            assert exact(conn, 16).hex() == "b866924125c8eebb9debc1db61c538e2"
        if auth == 2 or minor == 8:
            conn.sendall(bytes(4))
        assert exact(conn, 1) == b"\x01", "ClientInit must request a shared session"
        name = b"Synthetic RFB fixture"
        # Deliberately offer an indexed format; the client must explicitly
        # request its supported true-colour format before asking for pixels.
        indexed = struct.pack(">BBBBHHHBBB3x", 8, 8, 0, 0, 0, 0, 0, 0, 0, 0)
        conn.sendall(struct.pack(">HH", 2, 2) + indexed)
        if failure == "name":
            conn.sendall(struct.pack(">I", 0xFFFFFFFF))
            assert conn.recv(1) == b""
            return
        conn.sendall(struct.pack(">I", len(name)) + name)
        assert exact(conn, 4) == bytes(4), "SetPixelFormat must precede updates"
        assert exact(conn, 16) == PIXEL_FORMAT
        assert exact(conn, 2) == b"\x02\x00", "expected SetEncodings"
        count = struct.unpack(">H", exact(conn, 2))[0]
        encodings = struct.unpack(f">{count}i", exact(conn, count * 4))
        assert encoding in encodings
        assert exact(conn, 10) == struct.pack(">BBHHHH", 3, 0, 0, 0, 2, 2)
        if failure == "clipboard":
            conn.sendall(b"\x03\x00\x00\x00" + struct.pack(">I", 0x7FFFFFFF))
            assert conn.recv(1) == b""
            return
        if failure == "rectangle":
            conn.sendall(struct.pack(">BBHHHHHi", 0, 0, 1, 1, 1, 2, 2, 0))
            assert conn.recv(1) == b""
            return
        if encoding == 0:
            # BGRX: red, green, blue, white.
            payload = bytes(16) if black else bytes.fromhex("0000ff00 00ff0000 ff000000 ffffff00")
        elif encoding == 7:
            # Tight fill control: subtype is the HIGH nibble; RGB is always
            # red, green, blue for the 32bpp/depth24 Tight special case.
            payload = bytes.fromhex("80 ff0000")
        elif encoding == 16:
            # ZRLE solid tile, compact pixel BGR.
            data = zlib.compress(bytes.fromhex("01 0000ff"))
            payload = struct.pack(">I", len(data)) + data
        else:
            raise AssertionError(encoding)
        conn.sendall(struct.pack(">BBHHHHHi", 0, 0, 1, 0, 0, 2, 2, encoding) + payload)
        # Let the viewer consume the frame and disconnect on success.
        try:
            while True:
                message = conn.recv(1)
                if not message:
                    break
                assert message == b"\x03", "headless test injected remote input"
                exact(conn, 9)
        except ConnectionResetError:
            pass


def run_case(binary, label, banner=b"RFB 003.008\n", auth=1, encoding=0, failure=None, black=False):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(20)
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(serve, listener, banner, auth, encoding, failure, black)
            command = [binary, "--test", "--host", "127.0.0.1", "--port",
                       str(listener.getsockname()[1])]
            if auth == 2:
                command += ["--password", "password"]
            result = subprocess.run(command, capture_output=True, encoding="utf-8", timeout=25)
            future.result(timeout=20)
        output = result.stdout + result.stderr
        if failure:
            assert result.returncode == 1 and "PASS:" not in output and "panicked" not in output.lower(), (label, output)
        else:
            assert result.returncode == 0 and "PASS:" in output, (label, output)
            expected = "0/4 non-black" if black else "4/4 non-black"
            assert "2x2" in output and expected in output, (label, output)
        print(f"PASS {label}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()
    for minor in (3, 7, 8):
        for auth in (1, 2):
            run_case(args.binary, f"RFB 3.{minor} security {auth}",
                     f"RFB 003.{minor:03d}\n".encode(), auth)
    run_case(args.binary, "003.889 public RFB fallback", b"RFB 003.889\n")
    run_case(args.binary, "Tight fill", encoding=7)
    run_case(args.binary, "ZRLE solid", encoding=16)
    run_case(args.binary, "black frame without remote input", black=True)
    run_case(args.binary, "malformed banner rejected", b"NOT AN RFB!\n", failure="banner")
    for failure in ("unsupported-apple", "name", "clipboard", "rectangle"):
        run_case(args.binary, f"{failure} rejected", failure=failure)
    print("All 15 synthetic loopback interoperability cases passed.")


if __name__ == "__main__":
    main()
