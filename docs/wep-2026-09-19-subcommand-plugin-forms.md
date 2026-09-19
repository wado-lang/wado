# WEP: Subcommand Plugin Forms

## Context

[External Subcommands](./wep-2026-09-19-external-subcommands.md) leaves two
gaps: a `wado-<name>` on `PATH` carries no one-line description, and nothing
tells one written for the mechanism from any other file named that way.

Docker (a `docker-cli-plugin-metadata` handshake in `~/.docker/cli-plugins`),
Helm (a `plugin.yaml` in `$HELM_PLUGINS`) and krew (an index manifest with a
description, a version and a sha256) all answer them, and all left `PATH` to do
it. Those that stayed answer neither: git says nothing, and `kubectl plugin
list` only warns of a file that is not executable, shadowed by an earlier
entry, or colliding with a builtin.
[cargo#10662](https://github.com/rust-lang/cargo/issues/10662), proposing an
ELF note read without running the binary, is open.

## Decision

### The two gaps are developer experience, not safety

Whoever can write `wado-<name>` into a `PATH` directory has already won, since
the user will run that file. Closing the gaps makes the mechanism pleasanter,
not safer, and asking a candidate for its description is no more dangerous,
since `wado <name>` runs that same file anyway. cargo#10662 calls that a
security risk; the risk was taken at install. Its cost is time, one spawn per
candidate on `--list`, hence the marker read out of the file instead.
Provenance is settled at install too, by krew's sha256 and its index, and
`wado` installs nothing. Safety here is the two rules already decided: a
builtin is never overridden, and no empty or relative `PATH` entry supplies a
subcommand.

### A defined protocol buys a second plugin form

Demanding one adds no safety either. It buys a subcommand that need not be a
native binary, and a component is the form to reach for: its name, description
and version are metadata, so `--list` fills its blank column without starting
anything; one artifact serves every platform; and `wado` already hosts
wasmtime. Today that means a subcommand written in Wado, which nothing else
lets extend the tool that compiles it.

The sandbox it would run in is not a defense of the mechanism, and would be one
only if a component were the sole form accepted, since whoever can write to a
`PATH` directory writes the native form instead.

### Both forms stay

The `PATH` form is not replaced: the case it was built for,
`wado-run-with-webgpu`, links a native GPU stack
([`wasi:webgpu` Bindings](./wep-2026-09-19-wasi-webgpu.md)) that no component
can. The component form is suggested here and not designed here.

## Roadmap

Nothing here is committed work; the `PATH` form ships under
[External Subcommands](./wep-2026-09-19-external-subcommands.md).

## Known gaps

- The component plugin form, in full. A `.wasm` file is not executable, so the
  plugins need a directory of their own. What a plugin exports, how its
  metadata is read without instantiating it, what the host grants it, and which
  form wins under one name are open.
- Both gaps stay open on `PATH`. The one cure the ecosystems show is a
  directory the tool installs into, which `PATH` is not.
