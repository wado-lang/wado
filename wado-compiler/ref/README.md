# Reference Specifications

Verbatim copies of the IETF RFCs that define the formats handled by the
`lib/core/` modules. They are vendored so the implementation comments can cite
exact sections offline.

Implemented by `lib/core/zlib.wado`:

- `rfc1950.txt` — ZLIB Compressed Data Format Specification (zlib container)
- `rfc1951.txt` — DEFLATE Compressed Data Format Specification
- `rfc1952.txt` — GZIP File Format Specification

Vendored for the planned `core:cbor` module (see
`docs/wep-2026-06-05-core-cbor.md`):

- `rfc8949.txt` — Concise Binary Object Representation (CBOR), STD 94
  (obsoletes RFC 7049)

Source: <https://www.rfc-editor.org/>. RFCs are published for unlimited
distribution by the IETF Trust.

These documents specify only the wire formats; they define neither encoder
heuristics nor conformance test vectors. Spec-anchored decode tests therefore
live alongside each implementation (e.g. `lib/core/zlib_test.wado`), with
reference cross-validation in `tests/` where applicable.
