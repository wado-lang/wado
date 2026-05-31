# Reference Specifications

Verbatim copies of the IETF RFCs that define the formats implemented by
`lib/core/zlib.wado`. They are vendored so the implementation comments can cite
exact sections offline.

- `rfc1950.txt` — ZLIB Compressed Data Format Specification (zlib container)
- `rfc1951.txt` — DEFLATE Compressed Data Format Specification
- `rfc1952.txt` — GZIP File Format Specification

Source: <https://www.rfc-editor.org/>. RFCs are published for unlimited
distribution by the IETF Trust.

These documents specify only the bitstream formats; they define neither encoder
heuristics nor conformance test vectors. Spec-anchored decode tests therefore
live in `lib/core/zlib_test.wado`, with reference cross-validation in
`tests/zlib_interop.rs`.
