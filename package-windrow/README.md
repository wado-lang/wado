# Windrow

Windrow finds code that repeats one shape. It reads each file with a Gale
grammar, and reports every node of the tree that another node copies up to
renamed names and changed literals. A copy with a changed operator, call or
structure is a different shape.

It reads only the tree, never types or name resolution, so it runs fast and
works for any language Gale has a grammar for. Rust is the first.

## Usage

```sh
wado run package-windrow/src/main.wado -- [--min-tokens N] <path>…
```

A path is a `.rs` file or a directory walked for them. `--min-tokens` is the
smallest node reported, in tokens; the default is 100. Each group prints its
size and every copy's lines, the groups that repeat the most tokens first:

```text
275 tokens x 2
  wado-compiler/src/synthesis/traits.rs:3856-3897
  wado-compiler/src/synthesis/traits.rs:3902-3943
```

A group is reported at the largest node that repeats, so a copy's inner nodes
are not listed again.
