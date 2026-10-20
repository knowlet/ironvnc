# Public protocol baseline

Research date: 2026-10-19. The public interoperability baseline is RFB 3.8,
with 3.3 and 3.7 compatibility, per the
[RealVNC specification entry](https://help.realvnc.com/hc/en-us/articles/360002720337-RFB-protocol-specification)
and [RFC 6143](https://datatracker.ietf.org/doc/html/rfc6143).
RealVNC also describes a commercial
[RFB 6.0](https://help.realvnc.com/hc/en-us/articles/360002320638-How-does-VNC-technology-work);
this implementation does not claim that proprietary protocol.

The client validates version banners, selects a supported handshake, requests
32-bit, depth-24 little-endian true colour, and advertises implemented encodings.
Standard CutText uses Latin-1. Extended Clipboard negotiates UTF-8 text via
capabilities and bounded compressed messages. Wire layouts for these extensions,
Tight, ZRLE, and RSA-AES come from the community
[rfbproto specification](https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst),
which [TigerVNC](https://tigervnc.org/) identifies as its protocol reference.
This extension reference is not an IETF standard.

Per [RFC 6143 section 7.5.6](https://www.rfc-editor.org/rfc/rfc6143#section-7.5.6),
ClientCutText announces local clipboard contents; it does not execute a paste.
The explicit **Paste text →VNC** action instead sends paired standard KeyEvent
messages. Latin-1 and Unicode keysyms are shared with normal text input; CRLF,
CR and LF map to Return, and tab maps to Tab. Input is fully validated and
limited to 64 KiB of UTF-8 before queuing, producing at most 1 MiB of key-event
data. Other control characters are rejected. Held keys/modifiers are released
before the bounded batch. No clipboard extension or remote paste chord is
needed, and no per-character application acknowledgement is implied.

The `003.889` compatibility mapping also follows observable behavior in the
official [noVNC implementation](https://github.com/novnc/noVNC/blob/master/core/rfb.js).
AES-256 RSA-AES random lengths follow the official
[TigerVNC 1.15.0 server](https://github.com/TigerVNC/tigervnc/blob/v1.15.0/common/rfb/SSecurityRSAAES.cxx):
32 bytes per peer, resolving an omission in the extension prose.

VNC DES authentication follows the bit-reversed key convention in
[verified RFC erratum 4951](https://www.rfc-editor.org/errata/eid4951).
RA2 supports username/password subtype 1 and password-only subtype 2;
unknown subtypes and fields exceeding 255 UTF-8 bytes are rejected.
RA2 host keys retain the existing unpinned trust behavior.

VeNCrypt/TLS, Apple security types 33/35/36, Apple private encodings, high-performance
Screen Sharing, image/file clipboard transfer, and commercial RFB 6.0 are not
implemented. Unsupported-only security offers fail explicitly.

## macOS boundary

Apple documents a
[VNC password access option](https://support.apple.com/guide/mac-help/turn-screen-sharing-on-or-off-mh11848/mac)
and [third-party VNC viewer access](https://support.apple.com/guide/remote-desktop/virtual-network-computing-access-and-control-apde0dd523e/mac).
These documents do not publish a complete ARD authentication wire specification.
IronVNC implements ARD security type 30 using the community
[Diffie-Hellman Authentication specification, section 7.2.13](https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst#diffie-hellman-authentication),
cross-checked against upstream
[noVNC](https://github.com/novnc/noVNC/blob/4a2cc175ae8af50fe8cb85a05d40908c01c43f6a/core/rfb.js)
and [LibVNC](https://github.com/LibVNC/libvncserver/blob/98aaee0560baa0332339b2ef464ea96ed9b9b28c/src/libvncclient/rfbclient.c).

The server sends a big-endian 16-bit generator, a 16-bit key size in bytes,
then the modulus and server public value, each exactly that size. The client
uses fresh OS randomness for its DH private value. MD5 of the shared secret,
left-padded to exactly the advertised byte width, provides an AES-128 key.
The encrypted payload is two 64-byte, UTF-8, NUL-terminated fields with random
padding, encrypted with AES-128-ECB without additional block padding. The client
sends those 128 bytes followed by its fixed-width public value, then consumes
SecurityResult. These legacy cipher choices are mandated by this protocol.

The implementation accepts key sizes **128 through 512 bytes inclusive**, with
an actual modulus of at least 1024 bits. It supports the older 1024-bit/generator-2
case and the 4096-bit/generator-5 case described in a developer's
[modern Apple capture](https://github.com/d3cker/RealVNC-macOS-patch).
That capture is third-party interoperability evidence, not an Apple specification.
Lengths are bounded before allocation or exponentiation; even/small moduli,
out-of-range generators/public values and degenerate exchanges fail explicitly.
Arithmetic uses RustCrypto `crypto-bigint` with fixed-precision exponentiation.
These are structural checks, not primality/subgroup certification or server
identity verification. ARD encrypts credentials only; the ensuing RFB session
remains unencrypted. Use trusted endpoints or an authenticated encrypted tunnel.

Synthetic tests cover the exact `[30, 33, 36, 35]` offer, `003.889` fallback,
standard 3.7/3.8, 1024/2048/4096-bit groups, padded fixed-width secrets, credential
limits and failure handling. They do not establish interoperability with a
particular real macOS server build. ARD type 30 support does not implement
security types 33/35/36, Apple private encodings or high-performance Screen Sharing.

Authentication type 30 does not negotiate Apple's private pasteboard protocol.
An [independent macOS 15.7.3 interoperability report](https://github.com/sp00nznet/vncfree/blob/main/docs/macos.md)
observed ignored ClientCutText after `003.889` to `003.008` negotiation with
type 30; this is a specific implementation report, not a claim about every
Mac server. **Paste text →VNC** provides a standard-keyboard fallback for that
case. IronVNC does not infer clipboard support from authentication or the
viewer's own operating system.

The native macOS viewer maps Command to Meta and encodes Unicode keysyms.
GUI rendering, system clipboard integration, keyboard layouts, permission
dialogs, and real Apple servers require separate manual acceptance testing.
