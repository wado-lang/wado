---
name: rust-move
description: Move Rust items (a function, a type, a method of an inherent impl) to another file with `wado-dev-tools rust-move`, which widens the visibilities and repeats the imports the move needs from what rust-analyzer resolves. Read before moving Rust code between files.
---

# rust-move

`wado-dev-tools rust-move` moves items into another file. rust-analyzer has no
such refactoring, so the tool does the move itself. It asks the server where
each item starts and ends, who refers to it, and what the names in its body
resolve to.

```sh
tools/dev-bin.sh wado-dev-tools rust-move [--workspace DIR] [--dry-run] --to FILE LOCATOR...
```

- `--to FILE` is the destination, which must exist and be a module already.
- `--dry-run` prints every step and writes nothing.
- `--workspace DIR` and the locators are `rust-rename`'s. A member of a generic
  `impl` block is named by its position: the symbol notation does not resolve
  it.

Name every item of one move in one call. An item the call moves is not treated
as left behind, so nothing is widened for a reference between two of them.

## What moves

A top-level item, or a member of an inherent `impl` block. Each goes with its
doc comment and attributes.

- A member lands at the end of the destination's last `impl` block with the
  same header. Where the destination has none, the tool opens one.
- A top-level item lands at the end of the file, ahead of a trailing
  `mod tests`.
- An `impl` block left with nothing in it is removed. A comment or a macro
  invocation keeps it.

## What it fixes

- A moved item that a file other than the destination refers to gets
  `pub(super)`, or `pub(crate)` where that does not reach.
- A private item of the source file that the moved code reads gets the same. A
  top-level one is imported into the destination.
- Each `use` of the source file that binds a name the moved code writes is
  repeated in the destination.
- A top-level item the source file still refers to is imported back into it.

## What it leaves

The tool reports each of these as a `by hand:` line where it can see one.

- A trait imported for its methods alone: no name in the moved code says it.
  `cargo check` names the trait.
- The imports the source file no longer reads. `cargo clippy --fix` removes
  them.
- A path in a third file that names a moved top-level item.
- A relative import, or an import of a source item, between files that are not
  siblings. An import through `self` or a child module of the source file is
  relative to it even between siblings.
- A glob import of the source file.
- A name the moved code reads that the destination already binds to something
  else: the moved code would read the destination's.
- A `macro_rules!` macro of the source file that the moved code invokes.
- A path in the destination that names a moved top-level item through the file
  it left.
- A visibility a declaration already writes that does not reach as far as the
  move needs (`pub(self)`, or `pub(super)` where `pub(crate)` is needed).

Then format, since the tool appends imports unsorted:

```sh
cargo check --workspace --all-targets
mise run format
```

## What it refuses

- A member of a trait `impl`, which cannot leave its block.
- An item inside a function, a trait or a nested module.
- An item that shares a line with other code.
- A top-level item the destination already declares one of, by name and kind.
