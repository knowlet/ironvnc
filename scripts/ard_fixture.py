#!/usr/bin/env python3
"""Exercise Apple ARD authentication against independent loopback RFB servers.

Usage:
  python3 scripts/ard_fixture.py --binary target/debug/ironvnc
  python3 scripts/ard_fixture.py --self-test

Wire references, independent of the viewer's Rust implementation:
  https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst
    Diffie-Hellman Authentication (security type 30)
  https://github.com/novnc/noVNC/blob/4a2cc175ae8af50fe8cb85a05d40908c01c43f6a/core/rfb.js
    _negotiateARDAuth: 128-byte AES-ECB ciphertext, then fixed-width public key
  https://github.com/LibVNC/libvncserver/blob/98aaee0560baa0332339b2ef464ea96ed9b9b28c/src/common/crypto_openssl.c
    dh_compute_shared_key: preserve leading-zero bytes before MD5
  https://www.rfc-editor.org/rfc/rfc2409.html#section-6.2
  https://www.rfc-editor.org/rfc/rfc3526.html#section-3
  https://www.rfc-editor.org/rfc/rfc3526.html#section-5
    The public 1024-, 2048-, and 4096-bit MODP primes below
  https://github.com/d3cker/RealVNC-macOS-patch
    A modern macOS capture offers generator 5 and a 512-byte group-16 prime

Requires an existing Python cryptography installation for AES-ECB. DH uses
stdlib integer arithmetic and MD5 uses hashlib; no viewer code is imported.
This fixture installs nothing, contacts no external endpoint, uses only
synthetic credentials, and does not establish real-macOS interoperability.
The 128..512-byte DH limit is the viewer's resource and compatibility policy;
the type-30 wire format itself does not prescribe that accepted range.
Embedded NUL credentials cannot be supplied through OS argv; they must be
covered by the viewer's own API-level tests.
"""

import argparse
import concurrent.futures
import hashlib
import secrets
import socket
import struct
import subprocess

try:
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
except ImportError as error:
    raise SystemExit(
        "This optional fixture requires an existing cryptography installation; "
        "no dependencies were installed. The basic rfb_fixture.py is independent."
    ) from error


GROUP2 = int("""
    FFFFFFFF FFFFFFFF C90FDAA2 2168C234 C4C6628B 80DC1CD1
    29024E08 8A67CC74 020BBEA6 3B139B22 514A0879 8E3404DD
    EF9519B3 CD3A431B 302B0A6D F25F1437 4FE1356D 6D51C245
    E485B576 625E7EC6 F44C42E9 A637ED6B 0BFF5CB6 F406B7ED
    EE386BFB 5A899FA5 AE9F2411 7C4B1FE6 49286651 ECE65381
    FFFFFFFF FFFFFFFF
""".replace(" ", "").replace("\n", ""), 16)
GROUP14 = int("""
    FFFFFFFF FFFFFFFF C90FDAA2 2168C234 C4C6628B 80DC1CD1
    29024E08 8A67CC74 020BBEA6 3B139B22 514A0879 8E3404DD
    EF9519B3 CD3A431B 302B0A6D F25F1437 4FE1356D 6D51C245
    E485B576 625E7EC6 F44C42E9 A637ED6B 0BFF5CB6 F406B7ED
    EE386BFB 5A899FA5 AE9F2411 7C4B1FE6 49286651 ECE45B3D
    C2007CB8 A163BF05 98DA4836 1C55D39A 69163FA8 FD24CF5F
    83655D23 DCA3AD96 1C62F356 208552BB 9ED52907 7096966D
    670C354E 4ABC9804 F1746C08 CA18217C 32905E46 2E36CE3B
    E39E772C 180E8603 9B2783A2 EC07A28F B5C55DF0 6F4C52C9
    DE2BCBF6 95581718 3995497C EA956AE5 15D22618 98FA0510
    15728E5A 8AACAA68 FFFFFFFF FFFFFFFF
""".replace(" ", "").replace("\n", ""), 16)
GROUP16 = int("""
    FFFFFFFF FFFFFFFF C90FDAA2 2168C234 C4C6628B 80DC1CD1
    29024E08 8A67CC74 020BBEA6 3B139B22 514A0879 8E3404DD
    EF9519B3 CD3A431B 302B0A6D F25F1437 4FE1356D 6D51C245
    E485B576 625E7EC6 F44C42E9 A637ED6B 0BFF5CB6 F406B7ED
    EE386BFB 5A899FA5 AE9F2411 7C4B1FE6 49286651 ECE45B3D
    C2007CB8 A163BF05 98DA4836 1C55D39A 69163FA8 FD24CF5F
    83655D23 DCA3AD96 1C62F356 208552BB 9ED52907 7096966D
    670C354E 4ABC9804 F1746C08 CA18217C 32905E46 2E36CE3B
    E39E772C 180E8603 9B2783A2 EC07A28F B5C55DF0 6F4C52C9
    DE2BCBF6 95581718 3995497C EA956AE5 15D22618 98FA0510
    15728E5A 8AAAC42D AD33170D 04507A33 A85521AB DF1CBA64
    ECFB8504 58DBEF0A 8AEA7157 5D060C7D B3970F85 A6E1E4C7
    ABF5AE8C DB0933D7 1E8C94E0 4A25619D CEE3D226 1AD2EE6B
    F12FFA06 D98A0864 D8760273 3EC86A64 521F2B18 177B200C
    BBE11757 7A615D6C 770988C0 BAD946E2 08E24FA0 74E5AB31
    43DB5BFC E0FD108E 4B82D120 A9210801 1A723C12 A787E6D7
    88719A10 BDBA5B26 99C32718 6AF4E23C 1A946834 B6150BDA
    2583E9CA 2AD44CE8 DBBBC2DB 04DE8EF9 2E8EFC14 1FBECAA6
    287C5947 4E6BC05D 99B2964F A090C3A2 233BA186 515BE7ED
    1F612970 CEE2D7AF B81BDD76 2170481C D0069127 D5B05AA9
    93B4EA98 8D8FDDC1 86FFB7DC 90A6C08F 4DF435C9 34063199
    FFFFFFFF FFFFFFFF
""".replace(" ", "").replace("\n", ""), 16)

PIXEL_FORMAT = struct.pack(
    ">BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0
)
RAW_PIXELS = bytes.fromhex("0000ff00 00ff0000 ff000000 ffffff00")
TIMEOUT = 10


def require(condition, message):
    """Keep checks active when Python is run with -O."""
    if not condition:
        raise AssertionError(message)


def exact(conn, size):
    result = bytearray()
    while len(result) < size:
        block = conn.recv(size - len(result))
        if not block:
            raise AssertionError(f"EOF after {len(result)}/{size} bytes")
        result.extend(block)
    return bytes(result)


def expect_closed(conn):
    try:
        require(conn.recv(1) == b"", "viewer sent data after rejected ARD input")
    except ConnectionResetError:
        pass


def aes_decrypt(key, ciphertext):
    context = Cipher(algorithms.AES(key), modes.ECB()).decryptor()
    return context.update(ciphertext) + context.finalize()


def self_test():
    # FIPS 197 appendix C.1, independently published AES-128 known vector.
    key = bytes.fromhex("000102030405060708090a0b0c0d0e0f")
    ciphertext = bytes.fromhex("69c4e0d86a7b0430d8cdb78070b4c55a")
    require(aes_decrypt(key, ciphertext) == bytes.fromhex(
        "00112233445566778899aabbccddeeff"
    ), "AES-128 known-vector decryption failed")
    require([p.bit_length() for p in (GROUP2, GROUP14, GROUP16)] == [1024, 2048, 4096],
            "fixture MODP prime was transcribed with an incorrect width")
    print("PASS independent AES-128 vector and public MODP group widths")


def send_desktop(conn):
    require(exact(conn, 1) == b"\x01", "ClientInit must request shared access")
    name = b"Independent synthetic Apple ARD fixture"
    conn.sendall(struct.pack(">HH", 2, 2) + PIXEL_FORMAT
                 + struct.pack(">I", len(name)) + name)
    saw_raw = False
    for _ in range(16):
        kind = exact(conn, 1)[0]
        if kind == 0:
            exact(conn, 3)
            require(exact(conn, 16) == PIXEL_FORMAT, "unsupported pixel format request")
        elif kind == 2:
            exact(conn, 1)
            count = struct.unpack(">H", exact(conn, 2))[0]
            require(count <= 256, "unreasonable SetEncodings count")
            encodings = struct.unpack(f">{count}i", exact(conn, count * 4))
            saw_raw = 0 in encodings
        elif kind == 3:
            require(exact(conn, 9) == struct.pack(">BHHHH", 0, 0, 0, 2, 2),
                    "expected initial full 2x2 framebuffer request")
            break
        else:
            raise AssertionError(f"unexpected initial client message {kind}")
    else:
        raise AssertionError("no initial framebuffer request")
    require(saw_raw, "viewer did not offer Raw encoding")
    conn.sendall(struct.pack(">BBHHHHHi", 0, 0, 1, 0, 0, 2, 2, 0) + RAW_PIXELS)
    # The headless test may ask for another update before reporting success.
    # It must never inject pointer or keyboard input to obtain the first frame.
    try:
        while True:
            kind = conn.recv(1)
            if not kind:
                return
            require(kind == b"\x03", "headless test injected remote input")
            exact(conn, 9)
    except ConnectionResetError:
        pass


def serve(listener, username, password, prime, generator, key_size, failure,
          banner, offered):
    with listener.accept()[0] as conn:
        conn.settimeout(TIMEOUT)
        conn.sendall(banner)
        minor = 7 if banner == b"RFB 003.007\n" else 8
        require(exact(conn, 12) == f"RFB 003.{minor:03d}\n".encode(),
                f"expected public RFB 3.{minor} reply")
        conn.sendall(bytes([len(offered), *offered]))
        require(exact(conn, 1) == b"\x1e", "viewer did not select Apple ARD type 30")

        if failure == "keylen":
            # Only send the header: reject before waiting for an unreasonable
            # body or allocating according to the untrusted length.
            conn.sendall(struct.pack(">HH", generator, key_size))
            expect_closed(conn)
            return None

        private = secrets.randbelow(prime - 3) + 2
        public = pow(generator, private, prime)
        wire_prime = prime
        wire_generator = generator
        if failure == "generator-zero":
            wire_generator = 0
        elif failure == "generator-one":
            wire_generator = 1
        elif failure == "modulus-zero":
            wire_prime = 0
        elif failure == "modulus-one":
            wire_prime = 1
        elif failure == "modulus-even":
            wire_prime -= 1
        elif failure == "public-zero":
            public = 0
        elif failure == "public-one":
            public = 1
        elif failure == "public-p-minus-one":
            public = prime - 1
        elif failure == "public-p":
            public = prime
        wire = (struct.pack(">HH", wire_generator, key_size)
                + wire_prime.to_bytes(key_size, "big")
                + public.to_bytes(key_size, "big"))

        if failure and failure.startswith("truncated-"):
            sizes = {"truncated-header": 3, "truncated-modulus": 4 + key_size - 1,
                     "truncated-public": 4 + key_size * 2 - 1}
            conn.sendall(wire[:sizes[failure]])
            conn.shutdown(socket.SHUT_WR)
            expect_closed(conn)
            return None
        try:
            # Split the header and fields across writes; the stream has no
            # packet-boundary guarantees, so readers must use exact lengths.
            for begin, end in ((0, 1), (1, 3), (3, 11), (11, len(wire))):
                conn.sendall(wire[begin:end])
        except (BrokenPipeError, ConnectionResetError):
            # Invalid parameters or credentials may be rejected before the
            # complete challenge arrives. The process must still fail cleanly.
            if not failure or failure in ("auth-denied", "auth-truncated", "auth-reason-length"):
                raise
            return None

        if failure and failure not in ("auth-denied", "auth-truncated", "auth-reason-length"):
            expect_closed(conn)
            return None

        # Wire order is ciphertext first, public key second. The independent
        # server then derives MD5 from the full-width, big-endian DH secret.
        ciphertext = exact(conn, 128)
        client_wire = exact(conn, key_size)
        client_public = int.from_bytes(client_wire, "big")
        require(1 < client_public < prime - 1, "invalid client DH public value")
        shared = pow(client_public, private, prime).to_bytes(key_size, "big")
        key = hashlib.md5(shared).digest()
        credentials = aes_decrypt(key, ciphertext)
        filler = []
        for field, expected, label in (
            (credentials[:64], username, "username"),
            (credentials[64:], password, "password"),
        ):
            value, separator, rest = field.partition(b"\x00")
            require(separator == b"\x00", f"ARD {label} has no NUL terminator")
            require(value == expected.encode("utf-8"), f"ARD {label} UTF-8 bytes differ")
            filler.append(rest)

        if failure == "auth-denied":
            reason = b"Synthetic ARD credentials rejected"
            # RFB 3.7 has no failure-reason field after SecurityResult. An ARD
            # client must consume the status in both 3.7 and 3.8, without
            # waiting for a 3.8-only reason on a 3.7 connection.
            conn.sendall(struct.pack(">I", 1))
            if minor == 8:
                conn.sendall(struct.pack(">I", len(reason)) + reason)
            expect_closed(conn)
        elif failure == "auth-truncated":
            conn.sendall(b"\x00\x00")
            conn.shutdown(socket.SHUT_WR)
            expect_closed(conn)
        elif failure == "auth-reason-length":
            conn.sendall(struct.pack(">II", 1, 0xFFFFFFFF))
            expect_closed(conn)
        else:
            conn.sendall(bytes(4))
            send_desktop(conn)
        return client_wire, tuple(filler)


def run_case(binary, label, *, username="fixture-user", password="fixture-password",
             prime=GROUP2, generator=2, key_size=128, failure=None,
             banner=b"RFB 003.889\n", offered=(30, 33, 36, 35), error_contains=None,
             expected_exit=1):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(TIMEOUT)
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(serve, listener, username, password, prime, generator,
                                 key_size, failure, banner, offered)
            command = [binary, "--test", "--host", "127.0.0.1", "--port",
                       str(listener.getsockname()[1])]
            if username is not None:
                command += ["--username", username]
            if password is not None:
                command += ["--password", password]
            result = subprocess.run(command, capture_output=True, encoding="utf-8",
                                    timeout=TIMEOUT + 5)
            try:
                capture = future.result(timeout=TIMEOUT + 1)
            except Exception as error:
                raise AssertionError(
                    f"{label}: independent server failed: {error}\n"
                    f"Viewer output:\n{result.stdout}{result.stderr}"
                ) from error
        output = result.stdout + result.stderr
        if failure:
            require(result.returncode == expected_exit and "PASS:" not in output
                    and "panicked" not in output.lower(), f"{label}: {output}")
            if error_contains:
                require(error_contains in output, f"{label}: missing error detail: {output}")
        else:
            require(result.returncode == 0 and "PASS:" in output, f"{label}: {output}")
            require("2x2" in output and "4/4 non-black" in output, f"{label}: {output}")
        print(f"PASS {label}")
        return capture


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if not args.binary and not args.self_test:
        parser.error("provide --binary and/or --self-test")
    self_test()
    if not args.binary:
        return

    captures = [run_case(args.binary, f"Apple 003.889 [30,33,36,35] login {index}")
                for index in (1, 2)]
    require(captures[0][0] != captures[1][0], "client reused its ephemeral DH key")
    require(captures[0][1] != captures[1][1], "client reused ARD credential filler")
    print("PASS fresh client DH key and credential filler across sessions")
    cases = [
        ("public RFB 3.8, type 30 only", {"banner": b"RFB 003.008\n", "offered": (30,)}),
        ("RFB 3.7 ARD consumes SecurityResult", {"banner": b"RFB 003.007\n"}),
        ("RFB 3.7 ARD denial has no reason field",
         {"banner": b"RFB 003.007\n", "failure": "auth-denied"}),
        ("explicit username selects ARD over classic VNC",
         {"offered": (2, 30, 33, 36, 35)}),
        ("explicit username selects ARD over None",
         {"offered": (1, 30, 33, 36, 35)}),
        ("2048-bit MODP challenge", {"prime": GROUP14, "key_size": 256}),
        ("modern 4096-bit MODP challenge, generator 5",
         {"prime": GROUP16, "key_size": 512, "generator": 5}),
        ("129-byte challenge preserves shared-secret leading zero", {"key_size": 129}),
        ("UTF-8 credentials", {"username": "測試用戶", "password": "測試密碼🔑"}),
        ("exactly 63 UTF-8 bytes in each credential",
         {"username": "用" * 21, "password": "密" * 21}),
        ("64-byte username rejected", {"username": "u" * 64, "failure": "credentials"}),
        ("64-byte password rejected", {"password": "p" * 64, "failure": "credentials"}),
        ("multibyte username over 63 bytes rejected",
         {"username": "用" * 22, "failure": "credentials"}),
        ("multibyte password over 63 bytes rejected",
         {"password": "密" * 22, "failure": "credentials"}),
        ("missing username rejected",
         {"username": None, "failure": "credentials", "expected_exit": 2}),
        ("missing password rejected",
         {"password": None, "failure": "credentials", "expected_exit": 2}),
        ("authentication denial reports server reason",
         {"failure": "auth-denied", "error_contains": "Synthetic ARD credentials rejected"}),
        ("truncated SecurityResult rejected", {"failure": "auth-truncated"}),
        ("oversized authentication reason rejected", {"failure": "auth-reason-length"}),
    ]
    for key_size in (0, 127, 513, 65535):
        cases.append((f"DH key length {key_size} rejected before body",
                      {"key_size": key_size, "failure": "keylen"}))
    for failure in ("generator-zero", "generator-one", "modulus-zero", "modulus-one",
                    "modulus-even", "public-zero", "public-one", "public-p-minus-one",
                    "public-p", "truncated-header", "truncated-modulus", "truncated-public"):
        cases.append((f"{failure} rejected", {"failure": failure}))
    for label, options in cases:
        run_case(args.binary, label, **options)
    print(f"All {len(cases) + 2} synthetic loopback ARD interoperability cases passed.")


if __name__ == "__main__":
    main()
