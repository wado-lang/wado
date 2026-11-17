# WEP: The `spec` Command and Reference Search

## Context

The specification and the cheatsheet live in `docs/` of the repository. A user
with only the `wado` binary cannot read them, and the binary they have is the
version the documents must match. An agent writing Wado has the same problem
and no browser to fall back on.

`wado doc` already answers for the standard library, but only by module: a
reader must know that `unwrap_or` lives in `core:prelude` before asking.

## Decision

### `wado spec` reads the bundled specification

The binary embeds `docs/spec-*.md` and `docs/cheatsheet.md`, and `wado spec`
prints them as Markdown.

```sh
wado spec                      # the documents, in reading order
wado spec effects              # docs/spec-effects.md
wado spec effects#handlers     # that section, with its subsections
wado spec cheatsheet           # docs/cheatsheet.md
```

A document is named by its file name without the `spec-` prefix and the `.md`
suffix. A section is named by the anchor GitHub gives its heading, so the
fragment of a link between the documents (`./spec-effects.md#handlers`) works,
and so does one copied from a rendered page.

The output is raw Markdown, so it pipes into a pager or a Markdown renderer.
Links stay as written; they are relative to `docs/` in the repository.

The WEPs and the generated stdlib pages are not bundled. A WEP records how a
rule came to be, not the rule. A stdlib page is what `wado doc` prints from the
compiler already.

### `spec` and `doc` stay apart

`wado doc` is generated from code: its input is a module, and its output is a
reference page. `wado spec` is written by hand: its input is a document, and
what it prints is normative.

Folding the documents into `wado doc` would make its argument a file, a module,
or a document, told apart by spelling. `wado doc spec:effects` would read like
a module `spec:effects`. A flag (`wado doc --spec effects`) avoids that
confusion but still puts two kinds of input under one command. Two commands
keep each argument one kind of thing.

### `--search` on both

`wado spec --search <query>` searches the specification and the cheatsheet, or
the document or section named beside it. `wado doc --search <query>` searches the API
of every documented stdlib module, or of the modules and files named beside it.
Each searches only what it prints. A search that finds nothing in one points at
the other.

A query is split on whitespace into terms, compared without regard to case. An
entry matches when it holds every term. Ranking favours a term in a heading or
a name over one in the body, and keeps document order among equals. A hit
starts with the argument that prints it: a `spec` hit names the section, and a
`doc` hit names the module whose page holds the item, then the item and its
signature.

```text
$ wado spec --search "let else"
control-flow#let-else  Control Flow › Let Else
    `let PATTERN = EXPR else { ... };` binds a refutable pattern whose bindings
```

```text
$ wado doc --search unwrap_or
core:prelude  Option::unwrap_or
    pub fn unwrap_or(self, default: T) -> T
```

A function's attributes print above its signature, so a hit on a name
`#[unavailable]` reserves says why it is absent.

### A built-in name is taken for good

`spec` becomes a built-in subcommand, so a `wado-spec` on `PATH` is no longer
reachable as `wado spec` (see [External Subcommands](./wep-2026-09-19-external-subcommands.md)).
