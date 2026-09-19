# WEP: Subcommand Plugin Forms

## Context

`wado` resolves a subcommand it does not know as `wado-<name>` on `PATH`
([External Subcommands](./wep-2026-09-19-external-subcommands.md)). That WEP
leaves two gaps: an external carries no one-line description, and nothing tells
a file written for the mechanism from any other file on `PATH` named that way.

Every ecosystem that answers them has left `PATH` to do it. Docker requires a
`docker-cli-plugin-metadata` handshake and looks only in
`~/.docker/cli-plugins`. Helm reads a `plugin.yaml` beside the binary in
`$HELM_PLUGINS`. krew installs from a curated index whose manifest carries a
short description, a version, and a sha256.

The ones that stayed on `PATH` answer neither. git says nothing about either
gap. `kubectl plugin list` warns about a file that is not executable, one an
earlier `PATH` entry shadows, and one whose name collides with a builtin. Those
warnings diagnose confusion; none of them describes a plugin. cargo is still
deciding: [cargo#10662](https://github.com/rust-lang/cargo/issues/10662)
proposes reading an ELF note rather than running the binary, and is open.

## Decision

### Those two gaps are about developer experience, not safety

Whoever can write `wado-<name>` into a `PATH` directory has already won: the
user will run that file. So closing the two gaps makes the mechanism pleasanter
and not safer.

Running a candidate to ask it for a description is not dangerous either, since
`wado <name>` runs that same file anyway. cargo#10662 calls the handshake a
security risk, but the risk it names was taken when the file was installed.
What the handshake costs is time. `--list` would spawn every candidate it
found, and that is why the issue reaches for a marker it can read out of the
file instead.

Provenance is settled where a plugin is installed, by krew's sha256 and by the
index it comes from. `wado` does not install anything.

What safety the mechanism has is the two rules already decided: a builtin is
never overridden, and no empty or relative `PATH` entry supplies a subcommand,
so the directory a user stands in never decides what `wado foo` runs.

### A defined protocol buys a second plugin form

Demanding a protocol instead of accepting any executable does not make the
mechanism safer either. What it buys is a subcommand that need not be a native
binary, and a component is the form to reach for. It carries its name,
description and version as metadata, so `--list` can fill the column it leaves
blank today without starting anything. One artifact serves every platform.
`wado` already hosts wasmtime, so running one costs no new dependency.

Today the value of that is a subcommand written in Wado. Wado compiles to a
component, and nothing else lets a Wado program extend the tool that compiles
it.

The sandbox such a plugin runs in is not a defense of the mechanism. It would
be one only if a component were the sole form accepted, because whoever can
write to a `PATH` directory writes the native form instead.

### Both forms stay

The `PATH` executable form is not replaced. The case the mechanism was built
for is `wado-run-with-webgpu`, which links a native GPU stack
([`wasi:webgpu` Bindings](./wep-2026-09-19-wasi-webgpu.md)). No component can
do that.

The component form is suggested here and not designed here.

## Roadmap

Nothing here is committed work. The `PATH` form ships, and its roadmap is in
[External Subcommands](./wep-2026-09-19-external-subcommands.md).

## Known gaps

- The component plugin form, in full. A `.wasm` file is not executable, so
  `PATH` cannot carry one and the plugins need a directory of their own. What a
  plugin exports, how `wado` reads its name and description without
  instantiating it, and what the host grants it are open. So is which form
  answers when a `wado-<name>` and a component of that name both exist.
- The description and the marker stay missing for the `PATH` form. The one cure
  the ecosystems show is a directory the tool installs into, which this form
  does not have.
