# WEP: External Subcommands

## Context

`wado` is one binary with a fixed set of subcommands. Running a `wasi:webgpu`
program needs a host, and the host that exists links a GPU stack — 31 crates,
about 38 s of a clean release build and 3.1 MB of binary, on a wasmtime one
generation ahead of the workspace pin
([`wasi:webgpu` Bindings](./wep-2026-09-19-wasi-webgpu.md)). Nothing else a
`wado` invocation does wants any of that.

So the runner is its own binary, `wado-run-with-webgpu`, and `wado
run-with-webgpu` reaches it. cargo and git resolve their own subcommands the
same way, so a user meets a familiar shape.

## Decision

### A name `wado` does not know is looked up on `PATH`

`wado <name>` first asks the builtin table. Only a name it does not hold reaches
the search, which looks for a file called `wado-<name>` in the directories
`PATH` lists, in order, and runs the first one that is a regular file and
executable. A `<name>` that is not lowercase ASCII, digits and `-` is not a
subcommand and is never searched for.

`PATH` entries that are empty or relative are skipped. An empty entry means the
current directory to `execvp`, and a relative entry resolves against it, so
either lets the directory a user happens to stand in decide what `wado foo`
runs. `wado` resolves the absolute path itself and runs that path, rather than
handing a bare name to the operating system's own search, which on Windows
looks in the application directory and the current directory first.

### A builtin is never overridden

The builtin table answers first, so an external subcommand only ever adds a
name. A `wado-run` on `PATH` is inert: `wado run` is the builtin, whatever the
`PATH` holds. This is what makes the mechanism safe to have at all. A directory
earlier in `PATH` cannot change what `wado build` does.

A name that is neither builtin nor on `PATH` fails as it does today, and the
message says that `wado-<name>` was looked for on `PATH`.

### The child is handed the rest of the command line

`wado-<name>` receives every argument after the subcommand name, verbatim and
unparsed: `wado` reads none of them, so an external subcommand's flags are its
own. The environment carries `WADO`, the absolute path of the running binary,
so a child that wants to compile calls back to the same `wado` the user invoked
rather than searching for one.

On Unix `wado` execs the child, replacing itself, so the exit status and every
signal belong to the child directly. On Windows it spawns, waits, and exits
with the child's status.

### `wado --list` covers both

`--list` names builtin and external subcommands together, since a user who
installed one has no other way to see that `wado` found it. It also marks an
external that a builtin shadows, which is otherwise invisible: the file is on
`PATH`, `wado` will never run it, and nothing else says so. A builtin shows its
description and an external its path, which is all `wado` knows of it without
running it.

It is a flag rather than a subcommand because a builtin name is taken for good.
A `wado commands` would retire `commands` from the external namespace, while
`--list` sits beside `--help` and `--version` and retires nothing. `wado --help`
carries a line pointing at it.

### `wado help <name>` is `wado <name> --help`

A builtin answers from its own usage text. An external is run with `--help`,
since only the child knows its options. A name that is neither fails the way an
unknown command does.

## Roadmap

- [x] Resolve an unknown subcommand through `PATH` and run it, with the
      skipping and the absolute-path rules above, and say in the
      unknown-command error where it looked.
- [x] `wado --list`, covering builtins, externals, and shadowed externals.
- [x] `wado help <name>`, for a builtin and an external alike.
- [x] An `External Commands` section in the `wado-cli` skill, including `WADO`.

## Known gaps

- An external's one-line description. A builtin carries its own, and the only
  way to obtain an external's is to run it, which `--list` will not do to every
  candidate it found, so it shows the path instead. cargo shows names alone.
- Nothing distinguishes a `wado-<name>` written for this mechanism from any
  other file on `PATH` that happens to be named that way. cargo has the same
  gap, and closing it means a marker `wado` can read out of the file.

Neither gap is a safety problem. What a defined protocol would buy instead is
[Subcommand Plugin Forms](./wep-2026-09-19-subcommand-plugin-forms.md).
