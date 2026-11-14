# `core:icu` Development Guide

`core:icu` is the facade in `wado-compiler/lib/core/icu.wado` over the ICU4X
component this directory builds. [README.md](./README.md) says how the asset is
built and what it exports, [case-mapping.md](./case-mapping.md) the simple case
mappings, and [TODO.md](./TODO.md) the roadmap and the known gaps.

[WEP: `core:icu`](../docs/wep-2026-08-09-core-icu.md) is complete, and nothing
edits it any more. A design that changes one of its decisions is written in this
directory and names the WEP's section. No new WEP is opened for `core:icu`.

A change to `wit-properties/`, `src/` or `gen/` ends with
`mise run update-bundled-icu`, which regenerates `gen/`'s outputs and
`wado-compiler/lib/core/icu.wat`.
