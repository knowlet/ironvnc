# Build and verification matrix

The `Native binaries` GitHub Actions workflow builds all six native targets on
pull requests and pushes to `main`. Per-run Actions artifacts are retained for
14 days. Each job checks its Rust host triple and runner architecture, builds
the exact source head, runs release tests, exercises the actual binary against
synthetic loopback RFB servers, and checks `--help`. Cross-compilation is not
counted as native execution.

After all six jobs succeed for a `main` push, the publication job verifies the
complete set of target archives and SHA256 files, checks the embedded build
manifests for the exact commit, target and CI checks, and publishes a GitHub
[Nightly prerelease](https://github.com/knowlet/ironvnc/releases) tagged
`nightly-<full-commit-SHA>`. These tags/releases are not overwritten after
publication; interrupted draft publications can be retried. Release assets
remain available independently of Actions' 14-day artifact retention. Nightly
is triggered by successful `main` pushes, not by a scheduled daily build.

| Runner | Native target | Package |
| --- | --- | --- |
| Ubuntu 24.04 x64 | `x86_64-unknown-linux-gnu` | tar.gz |
| Ubuntu 24.04 ARM64 | `aarch64-unknown-linux-gnu` | tar.gz |
| macOS 15 Intel | `x86_64-apple-darwin` | tar.gz |
| macOS 15 Apple Silicon | `aarch64-apple-darwin` | tar.gz |
| Windows Server 2022 x64 | `x86_64-pc-windows-msvc` | zip |
| Windows 11 ARM64 | `aarch64-pc-windows-msvc` | zip |

See the current run for each target's result. Runner availability is documented
by [GitHub](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
These environments establish a tested baseline, not compatibility with older
glibc, macOS, or Windows versions. Binaries are unsigned and are not notarized.
macOS artifacts are executables, not installer/app bundles.

Archives contain the executable, project licenses, dependency notices, source
metadata, documentation, lockfile, `BUILD-INFO.json` and `SHA256SUMS`. A separate
SHA256 file covers each archive. Build info records source SHA, toolchain,
native runner, binary digest, and workflow URL.

## Reproduce locally

```sh
cargo fmt --all -- --check
CARGO_BUILD_JOBS=2 cargo test --locked --release --all-targets
CARGO_BUILD_JOBS=2 cargo build --locked --release
python3 scripts/rfb_fixture.py --binary target/release/ironvnc
```

The standard-library fixture checks 3.3/3.7/3.8 None and VNCAuth, a vendor-style
003.889 banner, explicit true-colour negotiation, Raw/Tight/ZRLE frames, and
rejection of malformed banners, unsupported-only Apple security offers, oversized lengths and invalid
rectangles. Its 15 cases include a valid black frame and assert that headless
mode does not send keyboard or pointer input. It never captures or connects to
a real desktop.

The optional RSA-AES fixture uses an already available Python `cryptography`
installation; it is not a runtime dependency of IronVNC or an automatic package
installation. Its independent protocol implementation tests synthetic RA2
connections locally. See its `--help` for invocation.

```sh
python3 scripts/ra2_fixture.py --binary target/release/ironvnc --verbose
```

Its 20 cases cover all four RSA-AES security types, both credential subtypes,
UTF-8 credentials, invalid subtypes and modified MACs. EAX is independently
composed from CMAC/CTR and checked against three published known vectors.
Post-authentication RFB fields are deliberately split across encrypted records.

The optional Apple ARD fixture also uses Python `cryptography`, independently
combining Python integer DH arithmetic, MD5 and AES-128-ECB. It validates the
actual binary's encrypted credentials, public value and a complete decoded
framebuffer against a synthetic `003.889` server offering `[30, 33, 36, 35]`.

```sh
python3 scripts/ard_fixture.py --binary target/release/ironvnc --verbose
```

Cases include 1024/2048/4096-bit groups (including generator 5), a 129-byte padded
group that exercises exact-width secret hashing, UTF-8 credential boundaries,
missing credentials, mixed security offers, fresh random exchanges, malformed
DH parameters, truncated messages and authentication denials. Rust unit tests
also check the published noVNC credential ciphertext vector and the protocol's
length/NUL/parameter boundaries; those run in the native CI matrix.

## Limits

- CI is headless: no GUI, real desktop capture, SFTP server, system clipboard,
  keyboard-layout, accessibility permission, Gatekeeper or notarization test.
- No real RealVNC/TigerVNC/Apple desktop was contacted in this task. Synthetic
  protocol tests do not imply certification with those products.
- Desktop size is capped at 32 million pixels; clipboard text at 8 MiB. Oversized
  messages and invalid coordinates close the connection.
- Sessions continue to store plaintext passwords. Existing RA2 and SSH host-key
  trust behavior is unchanged. Use only appropriately trusted endpoints.
- VeNCrypt, Apple security types 33/35/36 and Apple private screen encodings remain unsupported; see
  [PROTOCOL.md](PROTOCOL.md). There is no claim of complete security audit.
- Command/Ctrl shortcuts use egui key and Copy/Cut/Paste events without
  forwarding the Paste event's local text. If the native egui clipboard backend
  cannot read any text, it may suppress the paste key-down entirely; this
  upstream event limitation is not recoverable by the viewer's input layer.

Per-commit results, artifact URLs, and merge evidence are recorded in the fork's
pull request and its Actions run, so this document does not confuse one commit's
results with a later source revision.
