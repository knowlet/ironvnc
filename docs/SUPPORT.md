# Build and verification matrix

Artifacts are produced by the `Native binaries` GitHub Actions workflow, retained
for 14 days, without creating releases or deployments. Each job checks its Rust
host triple and runner architecture, builds the exact source head, runs release
tests, exercises the actual binary against synthetic loopback RFB servers, and
checks `--help`. Cross-compilation is not counted as native execution.

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
rejection of malformed banners, unsupported ARD, oversized lengths and invalid
rectangles. It never captures or connects to a real desktop.

The optional RSA-AES fixture uses an already available Python `cryptography`
installation; it is not a runtime dependency of IronVNC or an automatic package
installation. Its independent protocol implementation tests synthetic RA2
connections locally. See its `--help` for invocation.

## Limits

- CI is headless: no GUI, real desktop capture, SFTP server, system clipboard,
  keyboard-layout, accessibility permission, Gatekeeper or notarization test.
- No real RealVNC/TigerVNC/Apple desktop was contacted in this task. Synthetic
  protocol tests do not imply certification with those products.
- Desktop size is capped at 32 million pixels; clipboard text at 8 MiB. Oversized
  messages and invalid coordinates close the connection.
- Sessions continue to store plaintext passwords. Existing RA2 and SSH host-key
  trust behavior is unchanged. Use only appropriately trusted endpoints.
- VeNCrypt and Apple private protocols remain unsupported; see
  [PROTOCOL.md](PROTOCOL.md). There is no claim of complete security audit.

Per-commit results, artifact URLs, and merge evidence are recorded in the fork's
pull request and its Actions run, so this document does not confuse one commit's
results with a later source revision.
