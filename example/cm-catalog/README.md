# cm-catalog example

Imports the [`wado:cm-catalog`](../../package-cm-catalog) Component Model
component from an OCI registry and round-trips values through its identity
functions (the full value-type ABI surface).

`wado.toml` declares the component as an OCI registry dependency; `src/main.wado`
imports the fetched `.wasm` component directly and calls `CmCatalog::id_*`.

## Run

```sh
wado update                       # fetch wado:cm-catalog into ./build (gitignored)
wado run example/cm-catalog       # compile + run
```

`build/` is gitignored — the component is not committed. Until the component is
published (below), place it manually:

```sh
wado compile --lib package-cm-catalog -o example/cm-catalog/build/cm-catalog.wasm
wado run example/cm-catalog
```

## Publishing the component (one-time)

The component lives at `ghcr.io/wado-lang/wado/cm-catalog` — the open coordinate
`wado:cm-catalog` under the registry's `wado-lang` prefix. Publishing needs a
ghcr token with `write:packages` for the `wado-lang` org and is done with
[`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools) (Wado does not wrap
publishing):

```sh
# 1. Build the component
wado compile --lib package-cm-catalog -o cm-catalog.wasm

# 2. Authenticate to ghcr.io (e.g. `docker login ghcr.io`, or a wkg credential)

# 3. Publish — name/version (wado:cm-catalog@0.1.0) are read from the component.
#    Map the `wado` namespace to ghcr.io/wado-lang in the wkg config first
#    (~/.config/wasm-pkg/config.toml); see the wkg docs.
wkg publish --registry ghcr.io cm-catalog.wasm
```

After publishing, `wado update` resolves `wado:cm-catalog` against the OCI
registry and fetches the component into `build/`.
