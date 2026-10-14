#!/usr/bin/env python3
"""Independently exercise RSA-AES RFB against a synthetic loopback server.

Usage:
  python3 scripts/ra2_fixture.py --binary target/debug/ironvnc
  python3 scripts/ra2_fixture.py --self-test

Public wire specifications, read independently of the viewer implementation:
  https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst
    sections 7.2.6--7.2.10: RSA-AES, credential subtypes, AES-EAX records
  https://www.rfc-editor.org/rfc/rfc6143.html
    sections 7.1, 7.3--7.7: RFB 3.8, initialization, Raw rectangles
  https://web.cs.ucdavis.edu/~rogaway/papers/eax.pdf
    section 3 and appendix G: EAX construction and independent known vectors

Dependencies: Python standard library and the already installed `cryptography`
package (RSA PKCS1v15, AES-CTR, AES-CMAC). No package is installed or downloaded.
The local EAX composition is verified against the original paper's vectors.
The fixture never imports viewer code, captures a desktop, binds externally,
or uses real credentials. All RSA keys and passwords are synthetic.

The prose's 256-bit variant omits a random-length adjustment. TigerVNC's official
server resolves this ambiguity: writeRandom/readRandom use keySize / 8 and the
256-bit branch hashes 32 bytes from each peer. Accordingly, the default is 32.
  https://github.com/TigerVNC/tigervnc/blob/v1.15.0/common/rfb/SSecurityRSAAES.cxx
--random-bytes-256=16 explicitly probes a literal reading of the incomplete prose;
the selected policy is printed. No ironvnc source was consulted.
This optional fixture is not needed by the basic dependency-free RFB fixture.
"""

import argparse
import concurrent.futures
import hashlib
import hmac
import os
import socket
import struct
import subprocess
import sys

try:
    import cryptography
    from cryptography.hazmat.primitives import cmac
    from cryptography.hazmat.primitives.asymmetric import padding, rsa
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
except ImportError as error:
    raise SystemExit(
        "This optional fixture requires an existing cryptography installation; "
        "no dependencies were installed. The basic rfb_fixture.py is independent."
    ) from error


SECURITY_NAMES = {5: "RA2", 6: "RA2ne", 129: "RA2_256", 130: "RA2ne_256"}
PIXEL_FORMAT = struct.pack(
    ">BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0
)
RAW_PIXELS = bytes.fromhex("0000ff00 00ff0000 ff000000 ffffff00")
TIMEOUT = 10


def require(condition, message):
    """Keep all protocol assertions active even under python -O."""
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


def xor_bytes(*values):
    return bytes(a ^ b ^ c for a, b, c in zip(*values))


def omac(key, domain, message):
    context = cmac.CMAC(algorithms.AES(key))
    context.update(bytes(15) + bytes([domain]) + message)
    return context.finalize()


def eax_encrypt(key, nonce, header, plaintext):
    nonce_tag = omac(key, 0, nonce)
    context = Cipher(algorithms.AES(key), modes.CTR(nonce_tag)).encryptor()
    ciphertext = context.update(plaintext) + context.finalize()
    tag = xor_bytes(nonce_tag, omac(key, 1, header), omac(key, 2, ciphertext))
    return ciphertext, tag


def eax_decrypt(key, nonce, header, ciphertext, tag):
    nonce_tag = omac(key, 0, nonce)
    expected = xor_bytes(nonce_tag, omac(key, 1, header), omac(key, 2, ciphertext))
    require(hmac.compare_digest(tag, expected), "invalid AES-EAX authentication tag")
    context = Cipher(algorithms.AES(key), modes.CTR(nonce_tag)).decryptor()
    return context.update(ciphertext) + context.finalize()


def self_test():
    # Bellare/Rogaway/Wagner, appendix G, vectors 1, 2, and 10. The last
    # message crosses an AES block boundary and checks CTR advancement.
    vectors = [
        (
            "233952dee4d5ed5f9b9c6d6ff80ff478",
            "62ec67f9c3a4a407fcb2a8c49031a8b3",
            "6bfb914fd07eae6b", "", "",
            "e037830e8389f27b025a2d6527e79d01",
        ),
        (
            "91945d3f4dcbee0bf45ef52255f095a4",
            "becaf043b0a23d843194ba972c66debd",
            "fa3bfd4806eb53fa", "f7fb", "19dd",
            "5c4c9331049d0bdab0277408f67967e5",
        ),
        (
            "8395fcf1e95bebd697bd010bc766aac3",
            "22e7add93cfc6393c57ec0b3c17d6b44",
            "126735fcc320d25a", "ca40d7446e545ffaed3bd12a740a659ffbbb3ceab7",
            "cb8920f87a6c75cff39627b56e3ed197c552d295a7",
            "cfc46afc253b4652b1af3795b124ab6e",
        ),
    ]
    for vector in vectors:
        key, nonce, header, plaintext, ciphertext, tag = map(bytes.fromhex, vector)
        require(eax_encrypt(key, nonce, header, plaintext) == (ciphertext, tag),
                "EAX does not match the original published vector")
        require(eax_decrypt(key, nonce, header, ciphertext, tag) == plaintext,
                "EAX known-vector decryption failed")
        corrupt = tag[:-1] + bytes([tag[-1] ^ 1])
        try:
            eax_decrypt(key, nonce, header, ciphertext, corrupt)
        except AssertionError:
            pass
        else:
            raise AssertionError("EAX accepted a modified authentication tag")
    print("PASS AES-EAX: three published known vectors, block crossing, modified-tag rejection")


class Records:
    """RFB's byte stream over independent, authenticated RSA-AES records."""

    def __init__(self, conn, read_key, write_key):
        self.conn = conn
        self.read_key = read_key
        self.write_key = write_key
        self.read_index = 0
        self.write_index = 0
        self.buffer = bytearray()

    def recv(self, size):
        while not self.buffer:
            header = exact(self.conn, 2)
            length = struct.unpack(">H", header)[0]
            ciphertext = exact(self.conn, length)
            tag = exact(self.conn, 16)
            plaintext = eax_decrypt(
                self.read_key, self.read_index.to_bytes(16, "little"),
                header, ciphertext, tag,
            )
            self.read_index += 1
            self.buffer.extend(plaintext)
        block = bytes(self.buffer[:size])
        del self.buffer[:size]
        return block

    def send_record(self, plaintext, corrupt_mac=False):
        require(len(plaintext) <= 65535, "fixture record too large")
        header = struct.pack(">H", len(plaintext))
        ciphertext, tag = eax_encrypt(
            self.write_key, self.write_index.to_bytes(16, "little"),
            header, plaintext,
        )
        self.write_index += 1
        if corrupt_mac:
            tag = tag[:-1] + bytes([tag[-1] ^ 1])
        self.conn.sendall(header + ciphertext + tag)

    def sendall(self, data):
        # Deliberately split RFB fields across authenticated records. A record
        # boundary is never an RFB message boundary, per the public document.
        offset = 0
        for count in (1, 2, 7, 13):
            if offset == len(data):
                return
            part = data[offset:offset + count]
            self.send_record(part)
            offset += len(part)
        if offset < len(data):
            self.send_record(data[offset:])


def public_wire(key):
    numbers = key.public_numbers()
    size = (key.key_size + 7) // 8
    return (struct.pack(">I", key.key_size)
            + numbers.n.to_bytes(size, "big")
            + numbers.e.to_bytes(size, "big"))


def receive_public_key(conn):
    bits = struct.unpack(">I", exact(conn, 4))[0]
    require(1024 <= bits <= 8192, f"unreasonable fixture client RSA size: {bits}")
    size = (bits + 7) // 8
    modulus = exact(conn, size)
    exponent = exact(conn, size)
    key = rsa.RSAPublicNumbers(
        int.from_bytes(exponent, "big"), int.from_bytes(modulus, "big")
    ).public_key()
    return key, struct.pack(">I", bits) + modulus + exponent


def expect_closed(conn):
    try:
        require(conn.recv(1) == b"", "viewer sent data after invalid security input")
    except ConnectionResetError:
        pass


def receive_credentials(records, subtype, username, password):
    username_size = exact(records, 1)[0]
    username_bytes = exact(records, username_size)
    password_size = exact(records, 1)[0]
    password_bytes = exact(records, password_size)
    expected_username = username.encode("utf-8") if subtype == 1 else b""
    require(username_bytes == expected_username,
            f"username bytes differ: expected {expected_username!r}, got {username_bytes!r}")
    require(password_bytes == password.encode("utf-8"), "synthetic password bytes differ")
    # Equality checks above verify bytes and lengths rather than Unicode
    # character counts. The U8 fields already limit each field to 255 bytes.
    return username_size, password_size


def send_desktop(channel):
    require(exact(channel, 1) == b"\x01", "ClientInit must request shared access")
    name = b"Synthetic independent RSA-AES fixture"
    channel.sendall(struct.pack(">HH", 2, 2) + PIXEL_FORMAT
                    + struct.pack(">I", len(name)) + name)
    # Accept valid initial client messages without assuming their order. The
    # fixture advertises a supported natural format, so SetPixelFormat may be
    # omitted, but if present it must explicitly use this BGRX representation.
    saw_encodings = False
    for _ in range(16):
        kind = exact(channel, 1)[0]
        if kind == 0:
            exact(channel, 3)
            require(exact(channel, 16) == PIXEL_FORMAT, "unsupported fixture pixel request")
        elif kind == 2:
            exact(channel, 1)
            count = struct.unpack(">H", exact(channel, 2))[0]
            exact(channel, count * 4)
            saw_encodings = True
        elif kind == 3:
            incremental, x, y, width, height = struct.unpack(">BHHHH", exact(channel, 9))
            require((incremental, x, y, width, height) == (0, 0, 0, 2, 2),
                    "expected initial full 2x2 update request")
            break
        else:
            raise AssertionError(f"unexpected initial client message {kind}")
    else:
        raise AssertionError("no initial framebuffer request")
    require(saw_encodings, "no SetEncodings received")
    channel.sendall(struct.pack(">BBHHHHHi", 0, 0, 1, 0, 0, 2, 2, 0) + RAW_PIXELS)
    # A test client may issue another update request before exiting. Consume
    # its stream, checking any remaining encrypted records as they arrive.
    try:
        while channel.recv(1024):
            pass
    except (ConnectionResetError, AssertionError) as error:
        if isinstance(error, AssertionError) and str(error) != "EOF after 0/2 bytes":
            raise


def serve(listener, private_key, security, subtype, username, password,
          failure, random_bytes_256):
    with listener.accept()[0] as conn:
        conn.settimeout(TIMEOUT)
        conn.sendall(b"RFB 003.008\n")
        require(exact(conn, 12) == b"RFB 003.008\n", "expected RFB 3.8")
        conn.sendall(bytes([1, security]))
        require(exact(conn, 1) == bytes([security]), "wrong security type selected")

        server_wire = public_wire(private_key.public_key())
        conn.sendall(server_wire)
        client_key, client_wire = receive_public_key(conn)
        random_size = random_bytes_256 if security in (129, 130) else 16
        server_random = os.urandom(random_size)
        encrypted = client_key.encrypt(server_random, padding.PKCS1v15())
        conn.sendall(struct.pack(">H", len(encrypted)) + encrypted)
        encrypted_size = struct.unpack(">H", exact(conn, 2))[0]
        require(encrypted_size == private_key.key_size // 8,
                "client RSA ciphertext length is not the server modulus size")
        client_random = private_key.decrypt(exact(conn, encrypted_size), padding.PKCS1v15())
        require(len(client_random) == random_size,
                f"client random has {len(client_random)} bytes; public-document policy "
                f"for this run expects {random_size} (see --random-bytes-256)")

        digest = hashlib.sha256 if security in (129, 130) else hashlib.sha1
        key_size = 32 if security in (129, 130) else 16
        client_session_key = digest(server_random + client_random).digest()[:key_size]
        server_session_key = digest(client_random + server_random).digest()[:key_size]
        records = Records(conn, client_session_key, server_session_key)
        records.send_record(digest(server_wire + client_wire).digest())
        require(exact(records, digest().digest_size) == digest(client_wire + server_wire).digest(),
                "client public-key transcript hash differs")
        require(not records.buffer, "viewer sent authentication data before the subtype")
        records.send_record(bytes([subtype]), corrupt_mac=(failure == "mac"))
        if failure:
            expect_closed(conn)
            return {"failure_rejected": failure}

        lengths = receive_credentials(records, subtype, username, password)
        require(not records.buffer, "unexpected bytes after the credential payload")
        channel = conn if security in (6, 130) else records
        channel.sendall(bytes(4))  # SecurityResult: encrypted for RA2, plain for RA2ne.
        send_desktop(channel)
        return {"credential_byte_lengths": lengths}


def run_case(args, private_key, security, subtype, username="alice", password="secret",
             failure=None):
    label = f"{SECURITY_NAMES[security]} ({security}), subtype {subtype}"
    if failure:
        label += f", reject {failure}"
    elif username != "alice":
        label += ", UTF-8 credentials"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(TIMEOUT)
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(serve, listener, private_key, security, subtype,
                                 username, password, failure, args.random_bytes_256)
            command = [args.binary, "--test", "-H127.0.0.1", "--port",
                       str(listener.getsockname()[1]), f"-U{username}", f"-P{password}"]
            result = subprocess.run(command, capture_output=True, text=True, timeout=TIMEOUT + 5)
            output = result.stdout + result.stderr
            try:
                server_result = future.result(timeout=TIMEOUT + 1)
            except Exception as error:
                raise AssertionError(f"{label}: {error}\nViewer exit {result.returncode}:\n{output}") from error
    if failure:
        require(result.returncode == 1 and "PASS:" not in output
                and "panicked" not in output.lower(),
                f"{label}: viewer unexpectedly accepted invalid input\n{output}")
    else:
        require(result.returncode == 0 and "PASS:" in output
                and "2x2" in output and "4/4 non-black" in output,
                f"{label}: expected a successful synthetic frame\n{output}")
    print(f"PASS {label}: {server_result}")
    if args.verbose:
        print(f"Viewer exit {result.returncode}:\n{output.rstrip()}")


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", help="existing viewer executable; never built by this script")
    parser.add_argument("--self-test", action="store_true", help="only verify EAX published vectors")
    parser.add_argument("--security-types", type=int, choices=SECURITY_NAMES, nargs="+",
                        default=list(SECURITY_NAMES), help="default: 5 6 129 130")
    parser.add_argument("--random-bytes-256", type=int, choices=(16, 32), default=32,
                        help="32 follows official TigerVNC; 16 probes incomplete prose")
    parser.add_argument("--verbose", action="store_true", help="include captured viewer output")
    args = parser.parse_args()
    if not args.self_test and not args.binary:
        parser.error("--binary is required unless --self-test is used")
    print(f"Dependency provenance: Python {sys.version.split()[0]}; "
          f"cryptography {cryptography.__version__} at {cryptography.__file__}")
    print("Wire provenance: rfbproto RSA-AES sections; RFC 6143; "
          "Bellare/Rogaway/Wagner EAX appendix G. No viewer source imported.")
    print(f"256-bit random policy: {args.random_bytes_256} bytes")
    self_test()
    if args.self_test:
        return
    private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    count = 0
    for security in args.security_types:
        for subtype in (1, 2):
            run_case(args, private_key, security, subtype)
            count += 1
        run_case(args, private_key, security, 1, "測試者", "密碼🔐")
        count += 1
        run_case(args, private_key, security, 3, failure="subtype")
        count += 1
        run_case(args, private_key, security, 1, failure="mac")
        count += 1
    print(f"All {count} independent RSA-AES loopback cases passed.")


if __name__ == "__main__":
    main()
