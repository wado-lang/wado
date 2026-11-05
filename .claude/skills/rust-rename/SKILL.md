---
name: rust-rename
description: Rename a Rust symbol across the workspace (an item, a method, a field, a module, a local variable) with `wado-dev-tools rust-rename`, which edits where rust-analyzer resolves the name rather than where the text matches. Read before any Rust rename.
---

# rust-rename

`wado-dev-tools rust-rename` asks rust-analyzer to rename a symbol and applies
the edit it returns, moving a renamed module's file and directory too.

```sh
scripts/dev-bin.sh wado-dev-tools rust-rename [--workspace DIR] [--dry-run] LOCATOR NEW_NAME
```

- `--workspace DIR` is the Cargo workspace to load, the current directory by
  default. `wado-run-webgpu/` is a workspace of its own, so a rename there
  passes `--workspace wado-run-webgpu`.
- `--dry-run` prints each site as ``file:line:col: `old` -> `new` `` and
  writes nothing.
- The server is the `rust-analyzer` that rustup resolves in `DIR`, so
  `rust-toolchain.toml` pins its version.

## Locators

A position is `FILE:LINE:COLUMN`, 1-based, with the column in bytes as
`rg --column` prints it. It reaches anything, a local variable included.

A symbol is a Rust path from the crate down, as rustdoc's intra-doc links write
one. The crate may be written as its package name (`wado-compiler`).

| Names                      | Notation                     |
| -------------------------- | ---------------------------- |
| An item                    | `my_crate::geo::Point`       |
| A method or assoc. item    | `my_crate::geo::Point::area` |
| A field or enum variant    | `my_crate::geo::Point::x`    |
| A module                   | `my_crate::geo`              |
| A trait member, every impl | `my_crate::geo::Shape::area` |

`Type::name` names an inherent member only. A trait-impl member is named
through its trait, and the rename reaches the trait and every impl. A local
variable has no path, so it takes a position. A notation that names no symbol,
or several, fails and lists the candidates' positions to pass instead.

## Cost

Each call starts rust-analyzer, which loads the workspace before it renames,
running `cargo check` for the build scripts and proc macros. Loading is nearly
the whole call, so a rename of thousands of sites costs about what a rename of
two does. Run it in the background.

## What it leaves

rust-analyzer does not rename these, so the old name stays:

- a `macro_rules!` body, whose tokens name nothing until the macro expands;
- code behind a `cfg` the Cargo workspace does not enable;
- comments and string literals.

Find them and fix each with the editing tools:

```sh
cargo check --workspace --all-targets
rg -n '\bOLD_NAME\b'
```

## What it refuses

Nothing is written when the rename would edit a file outside `DIR`, as renaming
the member of a standard library trait's impl would edit the sysroot. A module
rename stops before moving a file onto an existing path.
