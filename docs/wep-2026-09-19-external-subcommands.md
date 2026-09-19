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

### A description and a marker are developer experience, not safety

Two things an external cannot supply: the one-line description `--list` leaves
blank, and anything that tells a `wado-<name>` written for this mechanism from
any other file named that way. Docker (a `docker-cli-plugin-metadata` handshake
in `~/.docker/cli-plugins`), Helm (a `plugin.yaml` in `$HELM_PLUGINS`) and krew
(an index manifest with a description, a version and a sha256) answer both, and
all left `PATH` to do it. Those that stayed answer neither: git says nothing,
`kubectl plugin list` only warns of a file that is not executable, shadowed by
an earlier entry, or colliding with a builtin, and
[cargo#10662](https://github.com/rust-lang/cargo/issues/10662) is open,
proposing an ELF note read without running the binary.

Neither is a safety gap. Whoever can write `wado-<name>` into a `PATH`
directory has already won, since the user will run that file, and asking a
candidate for its description is no more dangerous, since `wado <name>` runs
that same file anyway. cargo#10662 calls that a security risk; the risk was
taken at install. Its cost is time, one spawn per candidate on `--list`, hence
the marker read out of the file instead. Provenance is settled at install too,
by krew's sha256 and its index, and `wado` installs nothing. Safety here is the
two rules above: a builtin is never overridden, and no empty or relative `PATH`
entry supplies a subcommand.

### A defined protocol would buy a second plugin form

Demanding one adds no safety either. It buys a subcommand that need not be a
native binary, and a component is the form to reach for: its name, description
and version are metadata, so `--list` fills its blank column without starting
anything; one artifact serves every platform; and `wado` already hosts
wasmtime. Today that means a subcommand written in Wado, which nothing else
lets extend the tool that compiles it. The sandbox it would run in is not a
defense of the mechanism, and would be one only if a component were the sole
form accepted, since whoever can write to a `PATH` directory writes the native
form instead.

The `PATH` form is not replaced either way: `wado-run-with-webgpu` links a
native GPU stack that no component can. The component form is suggested here
and not designed here.

## Roadmap

The `PATH` form ships, including an `External Commands` section in the
`wado-cli` skill. Nothing about the component form is committed work.

## Known gaps

- An external's one-line description, and a marker telling one written for this
  mechanism from any other file named that way. cargo has both gaps, and the
  one cure the ecosystems show is a directory the tool installs into, which
  `PATH` is not.
- The component plugin form, in full. A `.wasm` file is not executable, so the
  plugins need a directory of their own. What a plugin exports, how its
  metadata is read without instantiating it, what the host grants it, and which
  form wins under one name are open.
