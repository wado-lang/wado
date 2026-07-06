# hello-packages

A hello-world for package dependencies. `src/main.wado` uses two kinds of
dependency:

- [`wado-lang:cm-catalog`](../../package-cm-catalog) — a Component Model
  **library** pulled from an OCI registry by `wado fetch`, imported as a local
  wasm asset and exercised through its `CmCatalog::id_*` identity functions.
- [`gale`](../../package-gale) — a Kiln **generator** that turns `src/Calc.g4`
  into a calculator parser at compile time; `main.wado` parses `1 + 2 * 3`
  through it.

## Registry vs local, today

Both dependencies are consumed from the OCI registry:

- cm-catalog is a `[dependencies]` **library**, pulled by `wado fetch` and
  imported as a local wasm asset.
- gale is a `[build-dependencies]` **generator** (`module: "wado-lang:gale"`).
  `wado compile` resolves the coordinate against the registry, pulls the
  `core:kiln/generator` component at its world sub-path, and reads its options
  shape back from the component WIT. No local `package-gale` checkout is needed.

## Run

```sh
wado update                     # resolve wado-lang:cm-catalog → wado.lock
wado fetch                      # download the component into ./build (gitignored)
wado run example/hello-packages # compile + run
```

`build/` is gitignored — the fetched component is not committed. `wado fetch`
pulls it from `ghcr.io/wado-lang/cm-catalog` into `build/cm-catalog.wasm`, which
`src/main.wado` imports as a local wasm asset. (Once import resolution for
registry dependencies lands, `use { CmCatalog } from "wado-lang:cm-catalog"`
will resolve directly and the `build/` bridge goes away.)

To build the component locally instead of pulling it, build `package-cm-catalog`'s
library world into place:

```sh
( cd package-cm-catalog && wado build --lib -o ../example/hello-packages/build/cm-catalog.wasm )
wado run example/hello-packages
```

## Publishing the component (one-time)

The component lives at `ghcr.io/wado-lang/cm-catalog`: the open coordinate
`wado-lang:cm-catalog` with no registry prefix, so the `wado-lang` namespace is
the GitHub org (`[registries].default = "oci://ghcr.io"`). Publishing needs a
ghcr token with `write:packages` for the `wado-lang` org and is done with
[`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools) (Wado does not wrap
publishing):

```sh
( cd package-cm-catalog && wado build --lib -o ../cm-catalog.wasm )
mise run ghcr-login
wkg oci push ghcr.io/wado-lang/cm-catalog:0.1.0 cm-catalog.wasm \
  --annotation org.opencontainers.image.source=https://github.com/wado-lang/wado \
  --annotation org.opencontainers.image.licenses=MIT \
  --annotation org.opencontainers.image.version=0.1.0
```

Make the package public (GitHub → wado-lang → Packages → cm-catalog) for
unauthenticated pulls. After publishing, `wado update` resolves
`wado-lang:cm-catalog` against the OCI registry and `wado fetch` downloads the
component into `build/`.

## Consuming a registry generator

gale is published as a Kiln generator at `ghcr.io/wado-lang/gale/core-kiln-generator`
(the `core:kiln/generator` world of the `wado-lang:gale` package), declared here as:

```toml
[build-dependencies]
"wado-lang:gale" = { version = "^0.0.9" }
```

```wado
use calc from "./Calc.g4"
    with { generator: { module: "wado-lang:gale", options: { highlight: false, trace: false } } };
```

On compile, `GeneratorModule::Spec("wado-lang:gale")` resolves the coordinate
against `[build-dependencies]`, picks the highest published version matching the
requirement, pulls the component from the generator world sub-path into
`build/kiln/generators/` (cached; a published version is immutable), and recovers
its options descriptor from the component WIT. The generator then runs as a
prebuilt component through the same driver path as a source generator.

### Options and defaults

A registry generator's options shape comes from its component WIT, and a WIT
`record` has no notion of a field default — so **every non-`option<T>` field is
required** at the consuming site (`{ highlight: false, trace: false }` above,
even though `trace` defaults to `false` in gale's source). Source-level defaults
do not cross the registry boundary; supply each field explicitly.

### Remaining follow-ups

Tracked in
[the dependency-management plan](../../docs/dependency-management-implementation-plan.md):

- The generator is resolved/pulled lazily by the compiler, so it is **not** yet
  recorded in `wado.lock` (no integrity pin) and `wado fetch` does not pre-pull
  it. Folding `[build-dependencies]` into the lock/fetch path is a follow-up.
- Carrying source-level option defaults across the boundary (so an omitted field
  falls back to the generator's default) needs the component to encode them.
