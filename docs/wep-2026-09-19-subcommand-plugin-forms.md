# WEP: Subcommand Plugin Forms

## Context

`wado` resolves a subcommand it does not know as `wado-<name>` on `PATH`
([External Subcommands](./wep-2026-09-19-external-subcommands.md)). That WEP
leaves two gaps: an external carries no one-line description, and nothing tells
a file written for the mechanism from any other file on `PATH` named that way.

Other ecosystems answer both, and each answers by leaving `PATH`. Docker
requires a `docker-cli-plugin-metadata` handshake and looks only in
`~/.docker/cli-plugins`. Helm reads a `plugin.yaml` beside the binary in
`$HELM_PLUGINS`. krew installs from a curated index whose manifest carries a
short description, a version, and a sha256. kubectl and git stay on `PATH` and
answer neither; `kubectl plugin list` only warns about a file that is not
executable, one an earlier `PATH` entry shadows, and one whose name collides
with a builtin. cargo has not answered either: [cargo#10662](https://github.com/rust-lang/cargo/issues/10662)
proposes reading an ELF note rather than running the binary, and is open.

## Decision

### Those two gaps are about developer experience, not safety

A file named `wado-<name>` in a `PATH` directory is already an executable the
user will run. Whatever it then tells `wado` about itself, it tells after
winning. Closing the two gaps therefore makes the mechanism pleasanter and not
safer, and running a candidate to ask it for a description makes it no more
dangerous, since `wado <name>` runs that same file anyway. cargo#10662 calls
the handshake a security risk; the risk it names was taken when the file was
installed. What the handshake costs is time, because `--list` would spawn every
candidate it found, which is why the issue reaches for a marker read out of the
file instead. krew's sha256 protects the install step, and `wado` does not own
the install step.

What safety the mechanism has is the two rules already decided: a builtin is never
overridden, and no empty or relative `PATH` entry supplies a subcommand, so the
directory a user stands in never decides what `wado foo` runs.

### A defined protocol buys a second plugin form

Requiring a protocol rather than any executable sits on the other layer, and
what it buys there is a subcommand that is not a native binary. A component
answers its name, description and version out of its own metadata, so `--list`
can fill the column it leaves blank today, and reading it does not mean running
it. One artifact serves every platform. `wado` already hosts wasmtime, so
running one costs no new dependency.

Today the value of that is a subcommand written in Wado. Wado compiles to a
component, and nothing else lets a Wado program extend the tool that compiles
it.

The sandbox such a plugin runs in is not a defense of the mechanism. It would
be one only if a component were the sole form accepted, because whoever can
write to a `PATH` directory writes the native form instead.

### Both forms stay

The `PATH` executable form is not replaced. `wado-run-with-webgpu` links a
native GPU stack ([`wasi:webgpu` Bindings](./wep-2026-09-19-wasi-webgpu.md)),
which no component can do, and it is the case the mechanism was built for.

The component form is suggested here and not designed here. What it would take
is below.

## Roadmap

Nothing in this WEP is committed work. The `PATH` form ships and its roadmap is
in [External Subcommands](./wep-2026-09-19-external-subcommands.md); the
component form is a gap.

## Known gaps

- The component plugin form, in full. A `.wasm` file is not executable, so
  `PATH` cannot carry one and the plugins need a directory of their own. What a
  plugin exports, how `wado` reads its name and description without
  instantiating it, what the host grants it, and which form answers when both a
  `wado-<name>` and a component of that name exist are all open.
- The two gaps above stay open for the `PATH` form. The one cure the survey
  found is a managed install directory, which that form does not have.
