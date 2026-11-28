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
A file Git's attributes mark `linguist-generated` is left out too: a generator
repeats its own shapes by construction.
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

Standard error ends with how many files it read and how fast:

```text
516 files (1 generated left out), 18.5 MB, 728 groups
read 77.9s, compared 5.2s, total 83.1s: 0.22 MB/s
```

Every two members of a group are alike, and of sizes within the similarity of
each other. A group is reported at the largest nodes that are, so a member's
inner nodes are not listed again. A pattern of alternatives counts as one
token: an exhaustive match lists every case of its enum, so two matches over
one enum share those whatever their arms do. A node that repeats one small
shape, such as a long list of literals, is not compared: its few distinct runs
make any two such nodes look alike. Nor is test code, a Rust item under `#[test]` or `#[cfg(test)]` or a Wado `test` block: a
fixture repeats its shape on purpose. A test module in a file of its own
(`#[cfg(test)] mod tests;`) is compared, since nothing reads which file a module
declaration names.

## Recall

`tools/recall.wado` replays past commits on this repository and counts those
where a group joins a node the commit deleted to another node. Commits whose
subject names a deduplication are measured against feature commits, a baseline
for chance. A commit that deletes no hunk of three lines or more is not
counted. At 40 tokens and similarity 0.8:

| Files | Measure       | Deduplications | Feature commits |
| ----- | ------------- | -------------- | --------------- |
| Rust  | shape         | 38% of 289     | 15% of 194      |
| Rust  | shape + names | 44% of 289     | 23% of 194      |
| Wado  | shape         | 39% of 72      | 12% of 122      |
| Wado  | shape + names | 43% of 72      | 14% of 122      |

Names find a few more deduplications, and in Rust more chance matches as well,
so they are not on by default. A match on a feature commit is often a real copy
nobody folded yet, so the baseline overstates the noise.

The commit lists are selected from `e9bace24ce3` by their subjects:

```sh
R=e9bace24ce3
git log $R --no-merges --format='%h %s' -- '*.rs' | grep -E '^[0-9a-f]+ refactor' |
  grep -i -E 'dedup|duplicat|one implementation|share|reuse|unif|consolidat|twin|copies|one (walk|helper|resolver|query|path|loop|place|function|table|source)|single (helper|walk|source|place)|fold .* into|merge .* into|extract|hoist' > rs-dedup.txt
git log $R --no-merges --format='%h %s' -- '*.rs' | grep -E '^[0-9a-f]+ feat' |
  grep -v -i -E 'dedup|duplicat|share|reuse|unif|consolidat|one (walk|helper|place)|fold|merge|extract|hoist|refactor' |
  shuf -n 286 --random-source=<(yes 2) > rs-feat.txt
P=(-- '*.wado' ':!wado-compiler/tests/**' ':!**/generated/**')
git log $R --no-merges --format='%h %s' -E -i --grep='^refactor.*(dedup|duplicat|shar|unif|consolidat|hoist|one (helper|place|implementation))' "${P[@]}" |
  grep -v -F -f <(cut -d' ' -f1 rs-dedup.txt) > wado-dedup.txt
git log $R --no-merges --format='%h %s' -E -i --grep='^feat' "${P[@]}" | head -200 > wado-feat.txt
```

Each cell replays one list, with `--names` for the shape + names rows:

```sh
for N in '' --names; do
  for L in dedup feat; do
    wado run package-windrow/tools/recall.wado -- --commits rs-$L.txt --files '*.rs' $N
    wado run package-windrow/tools/recall.wado -- --commits wado-$L.txt --files '*.wado' $N
  done
done
```

Below 30 tokens a match is about as likely by chance as on a deduplication, hence
the default of 40.
