# Reference Specifications

Vendored copies of the specifications that define the formats and types handled
by the `lib/core/` modules, so implementation comments can cite exact sections
offline.

## IETF RFCs

Verbatim copies of the IETF RFCs.

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

## TC39 Temporal

Vendored for the `core:temporal` module (see
`docs/wep-2026-06-05-core-temporal.md`):

- `tc39-temporal.md` — TC39 Temporal proposal specification (Stage 4, to be
  merged into ECMA-262 / ECMA-402). Retrieved 2026-06-06.

Source: <https://tc39.es/proposal-temporal/>. Unlike the RFCs, the authoritative
document is rendered HTML (ecmarkup), not plain text; this is a Markdown
rendering of the single-page spec, kept for offline citation of section numbers,
abstract operations, and normative algorithm steps. The original BSD license is
preserved at the end of the file. The URL above remains authoritative — refresh
the copy when re-aligning with later editions. The MVP defines only the
`Instant` and `ZonedDateTime` types; the broader spec is vendored to anchor the
deferred follow-ups (`now()`, RFC 3339 parse/format, civil field accessors,
`Duration` arithmetic).
