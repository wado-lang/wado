# Windrow

Windrow finds code that repeats one shape. It reads each file with a Gale
grammar, and reports the nodes of the tree that are alike up to renamed names
and changed literals, to a similarity you set. Two nodes' similarity estimates
the Jaccard index of their sets of six-token runs: 1 for a copy, less as the
operators, calls and structure part. The estimate is the share of 64 MinHash
values the two sets agree on, so two sets that differ can still score 1.

It reads only the tree, never types or name resolution, so it runs fast and
works for any language Gale has a grammar for. It reads Rust and Wado.

## Usage

```sh
wado run package-windrow/src/main.wado -- [--min-tokens N] [--similarity S] [--names] <path>…
```

A path is a `.rs` or `.wado` file, or a directory walked for them. The walk
skips a directory named `target` or `build`, or one whose name starts with `.`.
`--min-tokens` is the smallest node compared, in tokens, at least 6; the
default is 40.
`--similarity` is the least similarity reported, in (0, 1]; the default is 0.8.
`--names` also counts two nodes alike by the names they call, types, methods
and paths, when both call at least eight and are somewhat alike in shape: one
logic written as a loop and as an iterator chain shares those, not its runs.
Each group prints its largest member's size, the least similarity between two
of its members, and every member's lines, the groups that repeat the most
tokens first:

```text
747 tokens x 2, similarity 0.89
  wado-compiler/src/nir.rs:1582-1697
  wado-compiler/src/tir.rs:4295-4404
```

Every two members of a group are alike, and of sizes within the similarity of
each other. A group is reported at the largest nodes that are, so a member's
inner nodes are not listed again. A node that repeats one small shape, such as
a long or-pattern of enum cases, is not
compared: its few distinct runs make any two such nodes look alike. Nor is test
code, a Rust item under `#[test]` or `#[cfg(test)]` or a Wado `test` block: a
fixture repeats its shape on purpose. A test module in a file of its own
(`#[cfg(test)] mod tests;`) is compared, since nothing reads which file a module
declaration names.

## Recall

`tools/recall.wado` replays past commits on this repository and counts those
where a group joins a node the commit deleted to another node. Commits whose
subject names a deduplication are measured against feature commits, a baseline
for chance. At 40 tokens and similarity 0.8:

| Files | Measure       | Deduplications | Feature commits |
| ----- | ------------- | -------------- | --------------- |
| Rust  | shape         | 36% of 286     | 18% of 193      |
| Rust  | shape + names | 43% of 286     | 27% of 193      |
| Wado  | shape         | 36% of 70      | 9% of 116       |
| Wado  | shape + names | 49% of 70      | 18% of 116      |

Names find more deduplications, and about as many more chance matches, so they
are not on by default. A match on a feature commit is often a real copy nobody
folded yet, so the baseline overstates the noise.

Below 30 tokens a match is about as likely by chance as on a deduplication, hence
the default of 40.
