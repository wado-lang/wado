# cm-catalog example

Imports the [`wado-lang:cm-catalog`](../../package-cm-catalog) Component Model
component from an OCI registry and round-trips values through its identity
functions (the full value-type ABI surface).

`wado.toml` declares the component as an OCI registry dependency; `src/main.wado`
imports the fetched `.wasm` component directly and calls `CmCatalog::id_*`.

## Run

```sh
wado update                       # fetch wado-lang:cm-catalog into ./build (gitignored)
wado run example/cm-catalog       # compile + run
```

`build/` is gitignored — the component is not committed. Until the component is
published (below), place it manually:

```sh
wado compile --lib package-cm-catalog -o example/cm-catalog/build/cm-catalog.wasm
wado run example/cm-catalog
```

## Publishing the component (one-time)

The component lives at `ghcr.io/wado-lang/cm-catalog`: the open coordinate
`wado-lang:cm-catalog` with no registry prefix, so the `wado-lang` namespace is
the GitHub org (`[registries].default = "oci://ghcr.io"`). Publishing needs a
ghcr token with `write:packages` for the `wado-lang` org and is done with
[`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools) (Wado does not wrap
publishing):

```sh
wado compile --lib package-cm-catalog -o cm-catalog.wasm
mise run ghcr-login
wkg oci push ghcr.io/wado-lang/cm-catalog:0.1.0 cm-catalog.wasm \
  --annotation org.opencontainers.image.source=https://github.com/wado-lang/wado \
  --annotation org.opencontainers.image.licenses=MIT \
  --annotation org.opencontainers.image.version=0.1.0
```

Make the package public (GitHub → wado-lang → Packages → cm-catalog) for
unauthenticated pulls. After publishing, `wado update` resolves
`wado-lang:cm-catalog` against the OCI registry and fetches the component into
`build/`.
