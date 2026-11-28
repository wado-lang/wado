# Windrow

Windrow finds code that repeats one shape. It reads each file with a Gale
grammar, and reports the nodes of the tree that are alike up to renamed names
and changed literals, to a similarity you set. Two nodes' similarity is the
Jaccard index of their sets of six-token runs: 1 for a copy, less as the
operators, calls and structure part.

It reads only the tree, never types or name resolution, so it runs fast and
works for any language Gale has a grammar for. Rust is the first.

## Usage

```sh
wado run package-windrow/src/main.wado -- [--min-tokens N] [--similarity S] <path>…
```

A path is a `.rs` file or a directory walked for them. `--min-tokens` is the
smallest node compared, in tokens; the default is 100. `--similarity` is the
least similarity reported, in (0, 1]; the default is 0.8. Each group prints its
largest member's size, the least similarity between two of its members, and
every member's lines, the groups that repeat the most tokens first:

```text
747 tokens x 2, similarity 0.89
  wado-compiler/src/nir.rs:1582-1697
  wado-compiler/src/tir.rs:4295-4404
```

Every two members of a group are alike, and a group is reported at the largest
nodes that are, so a member's inner nodes are not listed again. A node that
repeats one small shape, such as a long or-pattern of enum cases, is not
compared: its few distinct runs make any two such nodes look alike.
