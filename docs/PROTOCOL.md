# Public protocol baseline

Research date: 2026-10-14. The public interoperability baseline is RFB 3.8,
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

VeNCrypt/TLS, ARD private authentication, Apple private encodings, high-performance
Screen Sharing, image/file clipboard transfer, and commercial RFB 6.0 are not
implemented. Unsupported-only security offers fail explicitly.

## macOS boundary

Apple documents a
[VNC password access option](https://support.apple.com/guide/mac-help/turn-screen-sharing-on-or-off-mh11848/mac)
and [third-party VNC viewer access](https://support.apple.com/guide/remote-desktop/virtual-network-computing-access-and-control-apde0dd523e/mac).
These documents do not publish a complete ARD authentication wire specification.
A synthetic `003.889` banner test establishes fallback behavior only; it does
not establish interoperability with a particular macOS server build. RA2
username support does not imply Apple account authentication support.

The native macOS viewer maps Command to Meta and encodes Unicode keysyms.
GUI rendering, system clipboard integration, keyboard layouts, permission
dialogs, and real Apple servers require separate manual acceptance testing.
