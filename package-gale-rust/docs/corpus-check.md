# The Corpus Check

The corpus check parses a list of Rust files and prints one `ok` / `ng` line
each — path, diagnostic count, line, message, and the first diagnostic's rule
stack — plus a summary on stderr. The stack is the part that carries
information: recovery restarts at an item boundary, so most failures report the
same `expected 'extern'` whatever went wrong inside, and only the stack names
the rule.

```sh
git ls-files '*.rs' > target/rs-corpus.txt
wado run package-gale-rust/tools/corpus_check.wado -- --paths-from target/rs-corpus.txt
```

`--paths-from` reads one path per line, the only form that survives a path
with a space.

`--tree-hash` appends a hash of each file's tree to its line. Two runs whose
outputs are identical built the same trees, which is the check for a change
that must not move a prediction.

Every `.rs` this repository tracks parses clean, so any `ng` line is a
regression. Each failing file reports exactly one diagnostic — it dies once and
recovery carries the rest — so the count is files, not errors.

The inline-path detector that `mise run check-rust-paths` runs needs the same
clean corpus, since a file it cannot parse is reported rather than counted.
