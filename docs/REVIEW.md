# PR review and implementation provenance

Reviewed on 2026-10-14. Integration is restricted to `knowlet/ironvnc`.
No comment, review submission, or code mutation was made in `hkder/ironvnc`.

## Pinned inputs

- Fork and upstream `main`: `999e00e3a3672efdbf8e8f307e7bd60875dee67e`.
- [Upstream PR #1](https://github.com/hkder/ironvnc/pull/1): head
  `3655c078f606578ef4f64e768dde4eea14e97732`, from
  `elvisdragonmao/ironvnc:feat/vnc-username`.
- Eight changed files, 192 additions and 61 deletions.
- No repository or ancestor `AGENTS.md`, `.agents/skills`, or existing workflow
  was present in this isolated checkout. The initial working tree was clean.
- The base and contribution use `MIT OR Apache-2.0`. Both license texts and
  existing copyright notices are retained. No dependency was added or updated.

## Verdict: reject the exact submitted head

**P2: a successful connection can replace another saved account's identity.**
With named Alice and Bob sessions sharing a host and port, launching Bob loads
the correct credentials. After ServerInit, `DesktopName → remember_session()`
finds the first session by host/port alone and updates its username. If Alice is
first, Alice becomes Bob. Password replacement already existed in the base;
the submitted change also replaces the account identity.

Locations in the submitted head:
[load](https://github.com/hkder/ironvnc/blob/3655c078f606578ef4f64e768dde4eea14e97732/src/main.rs#L88),
[save trigger](https://github.com/hkder/ironvnc/blob/3655c078f606578ef4f64e768dde4eea14e97732/src/app.rs#L263),
[identity lookup/update](https://github.com/hkder/ironvnc/blob/3655c078f606578ef4f64e768dde4eea14e97732/src/app.rs#L720).

Two source-exposed reviewers agreed on this static finding. The submitted head
was not built and is not merged or cherry-picked. No other newly introduced
defect was established; this is not a claim of exhaustive security review.

## Independent replacement boundary

The `cleanroom_username` implementer started with no inherited conversation.
Its inputs were the unmodified base files, public RFB documentation, and this
behavioral contract: negotiate credentials only after RA2 subtype is known;
use UTF-8 byte lengths; reject unknown subtypes and oversized fields; preserve
the selected saved-session identity; do not guess credentials when multiple
accounts share an endpoint; support separate VNC and SSH usernames.

It explicitly confirmed it had not opened the PR, its diff, commits, or review
branch. A subsequent team-status lookup exposed the reviewers' defect summary,
line numbers and URLs, but no implementation text. This exposure is recorded
here; the implementer did not follow those URLs. Reviewers who read the original
implementation did not write the replacement. The integration owner read the
base implementation and the review findings, but did not read the PR code or
copy its patch. This is a documented source-isolation process, not a legal
certification of independent authorship.

The replacement retains existing base cryptography and UI infrastructure; the
independence claim applies to the new username/session behavior, not the entire
pre-existing application. Changes remain under the project's dual license.

## Root causes addressed in the base

- Decoders were using a server-selected indexed format without a colour map.
  The client now requests its supported true-colour format explicitly.
- Tight compression-control nibbles were reversed, the 12-byte compression
  threshold was off by one, and Tight RGB was interpreted as BGR.
- Packed ZRLE indices used incorrect bit positions; invalid palette indices and
  runs could panic. Bounds, compressed lengths, and tile output are checked.
- Standard clipboard bytes were decoded as UTF-8 instead of Latin-1. Extended
  UTF-8 clipboard negotiation is implemented separately with limits.
- macOS Command and non-Latin-1 text require the correct RFB keysyms.

Validation evidence and explicit unsupported areas are in [SUPPORT.md](SUPPORT.md).
