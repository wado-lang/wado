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

cm-catalog is consumed from the registry (`[dependencies]` + `wado fetch`). gale
is referenced by **local path** (`module: "../../../package-gale"`) because
consuming a _published_ Kiln generator from the registry is not wired yet — see
[Consuming a registry generator](#consuming-a-registry-generator-not-yet).

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

## Consuming a registry generator (not yet)

gale is already published as a Kiln generator at
`ghcr.io/wado-lang/gale/core-kiln-generator` (the `core:kiln/generator` world of
the `wado-lang:gale` package). The goal is to consume it from the registry the
same way cm-catalog is:

```toml
# wado.toml — not wired yet
[build-dependencies]
"wado-lang:gale" = { version = "^0.1.0" }
```

```wado
use calc from "./Calc.g4" with { generator: { module: "wado-lang:gale" } };
```

Three gaps block this today (tracked in
[the dependency-management plan](../../docs/dependency-management-implementation-plan.md)):

1. Fetch the generator at its world sub-path. `wado fetch` pulls the bare
   repository (`<ns>/<pkg>`); a generator lives at `<ns>/<pkg>/core-kiln-generator`.
2. Run a _prebuilt_ generator component. The Kiln pipeline compiles a generator
   from source (`GeneratorModule::LocalPath`); a fetched component would need a
   "run these component bytes" path, and `GeneratorModule::Spec("ns:name@ver")`
   is currently deferred.
3. Recover the generator's options descriptor from the component. Kiln encodes
   `options: { … }` against a schema extracted from the generator source; a
   prebuilt component must carry (or omit) that descriptor.

A related rough edge: a bare `[build-dependencies]` key (`"gale"`) is the only
form the `module: "gale"` build-dep lookup resolves, yet the manifest validator
deprecates bare keys in favor of coordinates / `lib:` nicknames — which the
lookup does not accept. Reconciling the two is part of closing gap 2.
