# WEP: External Subcommands

## Context

`wado` is one binary with a fixed set of subcommands. The only `wasi:webgpu`
host links a GPU stack of 31 crates, about 38 s of a clean release build and
3.1 MB of binary, on a wasmtime a generation past the workspace pin
([`wasi:webgpu` Bindings](./wep-2026-09-19-wasi-webgpu.md)), and nothing else
`wado` does wants it. So the runner is its own binary, `wado-run-with-webgpu`,
reached as `wado run-with-webgpu`, the way cargo and git reach theirs.

## Decision

### An unknown name is looked up on `PATH`

The builtin table answers first, so an external only ever adds a name: a
`wado-run` on `PATH` is inert, a directory earlier in `PATH` cannot change what
`wado build` does, and that is what makes the mechanism safe to have at all.

An unknown name takes the first `wado-<name>` that is a regular executable file
across the `PATH` directories in order. One outside lowercase ASCII, digits and
`-` is never searched for, and one found nowhere fails saying where it looked.
Empty and relative entries are skipped, since `execvp` reads an empty entry as
the current directory and resolves a relative one against it. `wado` runs the
absolute path it resolved rather than a bare name, whose OS search starts on
Windows at the application and current directories.

### The child owns the rest of the command line

It receives every argument after its name, verbatim and unparsed, so its flags
are its own, and `WADO` names the running binary by absolute path so a child
that compiles calls back to the `wado` the user invoked. Unix execs it, handing
over the exit status and every signal; Windows spawns, waits, and exits with
its status.

### `wado --list` and `wado help <name>`

`--list`, which `--help` points at, names builtins and externals together and
marks an external that a builtin shadows, since nothing else shows either. A
builtin shows its description and an external its path, all `wado` knows
without running it. It is a flag because a builtin name is taken for good: a
`wado commands` would retire that name from the external namespace, while
`--list` retires nothing.

`help <name>` answers from a builtin's usage text and runs an external with
`--help`, since only the child knows its options.

## Roadmap

All of it ships, including an `External Commands` section in the `wado-cli`
skill.

## Known gaps

- An external's one-line description. Only running it would tell, and `--list`
  will not spawn every candidate it found, so it shows the path. cargo shows
  names alone.
- Nothing distinguishes a `wado-<name>` written for this mechanism from any
  other file named that way. cargo has the same gap, and closing it means a
  marker `wado` can read out of the file.

Neither gap is a safety problem. What a defined protocol would buy instead is
[Subcommand Plugin Forms](./wep-2026-09-19-subcommand-plugin-forms.md).
