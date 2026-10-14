# Supplemental dependency notices

These files restore license/notice documents absent from the crates already
pinned by Cargo.lock. They do not add or update dependencies. Each directory's
`NOTICE-SOURCES.json` records the URL, revision evidence and SHA256 for every
supplement. Packaging verifies these hashes and fails if a reachable dependency
has no notice document or license declaration.

Most supplements come from the published crate's `.cargo_vcs_info.json` revision.
Exceptions explicitly identify version-tag inference, payload matching, or a
crate's exact Cargo license declaration paired with the corresponding standard
SPDX license text. Standard license texts are not presented as original
copyright notices. Published notice files take precedence over supplements.

The package's target-specific `THIRD-PARTY.json` preserves declared SPDX
expressions, selected permissive alternatives, repositories, versioned source
download links, and published source revisions. Font licenses and ring's nested
notices are included. The unmodified MPL-2.0 `option-ext` source is included in
the binary archive so its source-availability obligation is satisfied.

The project licenses apply to IronVNC. Dependency notices retain their own terms.
