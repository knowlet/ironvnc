# IronVNC

A free, open-source **VNC (RFB) viewer written in Rust**, with an egui interface,
public RFB encodings, RSA-AES and Apple ARD authentication, and an SFTP file browser.

> Iron oxidizes into rust. 🦀

## Features

- **Public protocol baseline:** RFB 3.3 / 3.7 / 3.8 with validated version negotiation and explicit true-colour pixel-format selection. The vendor-style `003.889` banner falls back to public RFB 3.8.
- **Encodings:** Raw, CopyRect, Hextile, ZRLE, Tight (JPEG/Fill/Palette/Gradient), and DesktopSize. Cursor payloads can be consumed; the GUI uses its own pointer marker.
- **Authentication:**
  - None
  - VNC Authentication (DES challenge/response)
  - **RA2 / RSA-AES** (types 5, 6, 129, 130), including AES-256 and username/password or password-only credential subtypes, with full-session or handshake-only encryption.
  - **Apple ARD** (type 30), using a Mac account username and password, including 4096-bit DH groups and generator 5 offered by modern Screen Sharing servers.
- **Session manager** — saved connections with credentials; single click opens each in its own window. Connections auto-save on connect.
- **MobaXterm-style SFTP panel** — browse the remote host over SSH, drag-and-drop to upload, download with a click. Runs alongside the VNC session, independent of the VNC vendor.
- **Clipboard:** **Paste text →VNC** types local clipboard text into the remote app without requiring clipboard sharing, including Unicode keysyms, Return and Tab. **Sync clipboard** separately offers Latin-1 standard CutText or negotiated UTF-8 Extended Clipboard text. Non-Latin-1 characters become `?` when syncing to basic-only servers. Image/file clipboard transfer is unsupported.
- **macOS input:** Command maps to Meta; Unicode text uses RFB Unicode keysyms. Apple private security types 33/35/36 and high-performance Screen Sharing are unsupported.
- **Quality-of-life:** scale-to-fit rendering, scroll-wheel support, a Ctrl+Alt+Del button, desktop-name window titles, and partial-framebuffer updates.

## Install / Build

Requires a [Rust toolchain](https://rustup.rs/).

```bash
git clone https://github.com/knowlet/ironvnc
cd ironvnc
cargo build --locked --release --jobs 2
# binary at target/release/ironvnc
```

The [native binary workflow](https://github.com/knowlet/ironvnc/actions/workflows/ci.yml)
builds and runs headless tests on Linux, macOS, and Windows, on x64 and ARM64.
Each successful push to `main` publishes six tested archives as an immutable
[Nightly prerelease](https://github.com/knowlet/ironvnc/releases) tagged
`nightly-<full-commit-SHA>`. Every archive has a separate SHA256 checksum file.
Pull-request builds remain downloadable as Actions artifacts for 14 days.
Nightly archives include the binary, licenses, dependency notices, source/build
provenance, and SHA256 checksums. These are unsigned executables, not installers;
see the [support matrix](docs/SUPPORT.md) for verification limits.

## Usage

Launch with no arguments for the session manager:

```bash
ironvnc
```

Or connect directly:

```bash
ironvnc --host 192.168.1.50            # defaults to port 5900
ironvnc --host 192.168.1.50 --port 5901
ironvnc --host 192.168.1.50 --username alice
ironvnc --session "My Desktop"          # open a saved session by name
```

If the server requires a password, IronVNC prompts for it in a popup and (on success) saves the session so you don't have to re-enter it. Saved sessions live in `sessions.json` **next to the executable** — the path is shown at the bottom of the session manager.

Apple ARD and RA2 servers requesting an account also prompt for a missing VNC username.
Use `-U` / `--username` for VNC; `--user` remains the SSH/SFTP option. Saved
sessions preserve their identity when accounts share an endpoint. Ambiguous
ad-hoc credential lookup requires an explicit session or username. Existing
session files remain readable.

### macOS Screen Sharing

For a server offering `[30, 33, 36, 35]`, IronVNC selects ARD type 30 and prompts
for the missing account fields. Use the Mac's account name and login password:

```bash
ironvnc --host mac.local --username alice
```

The account must be allowed access in the Mac's Screen Sharing or Remote
Management settings. See [Apple's Screen Sharing guide](https://support.apple.com/guide/mac-help/turn-screen-sharing-on-or-off-mh11848/mac).
When both legacy VNC authentication and ARD are offered, specifying `--username`
selects account authentication; a password-only connection retains the legacy
VNC choice. ARD credentials may each contain at most 63 UTF-8 bytes and must not
contain embedded NULs; longer fields fail explicitly rather than being truncated.
The existing `003.889` banner fallback continues to negotiate public RFB 3.8.

### Pasting text

To insert text copied on your computer, select the destination field in the
remote desktop, then click **Paste text →VNC**. This sends the text as keyboard
input and works without remote clipboard support. The former **Paste →VNC**
button only offered clipboard contents to the server; it did not insert text.
Native macOS Screen Sharing can ignore that standard clipboard transfer.

The typing action supports up to **64 KiB of UTF-8 text**. It releases held
remote keys and modifiers before typing, restores viewer focus without moving
the remote cursor, and shows clipboard/read/send errors in the status bar.
Empty text, oversized input and unsupported control characters are rejected
before any text is sent. CRLF, CR and LF each become one Enter press; tabs
become Tab presses. **In a terminal or form, these presses can execute a command
or submit/navigate the form.** This action is keyboard input, so applications'
bracketed-paste handling does not apply; the remote keyboard layout, input
method and application still determine how the text is received.

Use **Sync clipboard** when the server supports standard or Extended Clipboard
and you want to transfer clipboard contents for pasting inside the remote app.
The server must support the transfer; an offer is not a delivery acknowledgement.
Normal **Cmd+V / Ctrl+V** continues to use the remote clipboard. To insert local
text into a Mac that ignores clipboard sync, use **Paste text →VNC**.

### Headless self-test

```bash
ironvnc --test -H <host> -P <password>            # connect, verify a framebuffer decodes, exit
ironvnc --test -H <host> -U <username> -P <password> # Apple ARD / RA2 account authentication
ironvnc --sftp-test -H <host> --user <u> -P <pw>  # verify SSH/SFTP connectivity
```

`--test` accepts a valid black frame and never sends pointer or keyboard input.
For isolated synthetic interoperability tests, run
`python3 scripts/rfb_fixture.py --binary target/release/ironvnc`.

## Security notes

- `sessions.json` stores passwords in plaintext and is **gitignored** — do not commit it.
- The RA2 host key is currently accepted without pinning (convenient for trusted networks); certificate/known-hosts verification is a planned improvement.
- ARD encrypts the credential exchange only; it does not verify the server's identity or encrypt subsequent screen/input/clipboard traffic. Use a trusted network or an authenticated encrypted tunnel.
- VeNCrypt (TLS), Apple security types 33/35/36 and Apple private screen encodings are not implemented.

See [public protocol scope](docs/PROTOCOL.md), [review/provenance](docs/REVIEW.md),
and [verification limits](docs/SUPPORT.md). Synthetic protocol tests do not
certify interoperability with real Apple, RealVNC, or TigerVNC desktops.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
