---
name: wado-package-standard
description: "Where a Wado package keeps each kind of file: the root files, docs/, tests/, tools/, assets/, and what AGENTS.md holds. Read before creating a package (`wado new`), or adding, moving or renaming a file or directory in a `package-*/` directory."
---

# The Wado Package Standard

[`docs/spec-package-standard.md`](../../../docs/spec-package-standard.md) is
the standard. Read it first. This skill adds what it asks of an agent.

- Create a package with `wado new`, never by hand; the `wado-cli` skill says
  how. Then add the package to the Repository Map in the root `AGENTS.md`.
- Ask the user before creating a directory the standard does not name. Once
  approved, the package's `AGENTS.md` says what it holds.
- A file that moves takes every reference to it along: `use` and
  `#include_str` paths, `wado.toml`, `mise.toml`, `.github/workflows/`,
  `.gitattributes`, `.coderabbit.yaml` and the links in Markdown. Grep the
  repository for the old path, not only the package.
- Every package's `AGENTS.md` names this skill, so a change there loads it.
- A test moved out of a source file into `tests/` reaches only `pub` and
  `internal` items. A test that needs a file-private item stays in its file.
