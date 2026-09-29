# hello-packages

A hello-world for package dependencies. `src/main.wado` uses two kinds of
dependency:

- [`wado-lang:cm-catalog`](../../package-cm-catalog) — a Component Model
  **library** pulled from an OCI registry by `wado fetch`, imported by its
  coordinate and exercised through its `CmCatalog::id_*` identity functions.
- [`gale`](../../package-gale) — a Kiln **generator** that turns `src/Calc.g4`
  into a calculator parser at compile time; `main.wado` parses `1 + 2 * 3`
  through it.

## Where the dependencies come from

Both dependencies are consumed from the OCI registry:

- cm-catalog is a `[dependencies]` **library**, pulled by `wado fetch`.
  `use { CmCatalog } from "wado-lang:cm-catalog"` imports it across the
  Component Model boundary, as a prebuilt component.
- gale is a `[build-dependencies]` **generator** (`module: "wado-lang:gale"`).
  `wado compile` resolves the coordinate against the registry, pulls the
  `core:kiln/generator` component at its world sub-path, and reads its options
  shape back from the component WIT. No local `package-gale` checkout is needed.

## Run

```sh
wado update                     # resolve wado-lang:cm-catalog → wado.lock
wado fetch                      # download the components into the shared cache
wado run example/hello-packages # compile + run
```

`wado fetch` pulls the component from `ghcr.io/wado-lang/cm-catalog` into the
shared dependency cache, `~/wado/ghcr.io/wado-lang/cm-catalog/0.1.0/component.wasm`
(`WADO_ROOT` moves the `~/wado` root). Every project reads that one copy, and
nothing is written into this directory.

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
component into the shared cache.

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

`wado update` resolves the coordinate, picks the highest published version
matching the requirement, and records it in `wado.lock` as a `[[build-dependency]]`
with the generator artifact's integrity digest. `wado fetch` then pre-pulls the
component from the generator world sub-path into the shared cache, under
`~/wado/ghcr.io/wado-lang/gale/core-kiln-generator/`. On
compile, `GeneratorModule::Spec("wado-lang:gale")` resolves to the locked version,
reuses the fetched component (a published version is immutable, so the cache is
sound), and recovers its options descriptor from the component WIT. The generator
then runs as a prebuilt component through the same driver path as a source
generator. Without a lock the compiler resolves and pulls lazily.

### Options and defaults

A registry generator's options shape comes from its component WIT, and a WIT
`record` has no field defaults. So every field is required at the use site
unless its type is an `Option`, a `List` or a `TreeMap`
([Options](../../docs/spec-kiln.md#options)). That is why the clause above
writes `trace: false`, although `trace` defaults to `false` in gale's source.

### Remaining follow-ups

- [ ] Carry source-level option defaults across the registry boundary, so an
      omitted field falls back to the generator's default.
- [ ] Enforce the integrity `wado.lock` records on fetch and compile, with a
      `--locked` / `--offline` mode.
