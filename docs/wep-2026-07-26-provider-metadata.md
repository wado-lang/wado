# WEP: Provider Metadata — Source-Bundled Package Artifacts

## Context

Two established rules collide at the package boundary:

- The CM ABI cannot represent closures, generics, traits, or effect polymorphism, so a published component exposes `export` items only ([Visibility](./wep-2026-06-25-visibility-internal-pub-export.md)).
- `pub` is the Wado-native library API, reachable only on the GC-sharing path ([Package Manifest](./wep-2026-02-14-package-manifest.md) §Wado-to-Wado Optimization).

A generic, higher-order, or trait-based library therefore has no registry distribution path — the API that makes it worth publishing disappears at publish time, leaving git source dependencies as the only channel. Several documents already route around this by citing a "Wado provider metadata" path, but no WEP specifies what that metadata is.

Monomorphization settles the format question. A consumer instantiating a generic needs the body, not just the signature — Rust ships MIR in rlibs, Go ships bodies in export data. Wado must ship bodies too; only the serialization is open. This also means no format buys secrecy: shipping IR instead of source obfuscates, it does not close.

## Decision

A published package is a source package with a prebuilt CM binary attached, delivered as one `.wasm`.

### The artifact

The package's Wado source travels in a custom section named `wado:package`:

| Content            | Purpose                                            |
| ------------------ | -------------------------------------------------- |
| Format version     | Consumer compatibility gate                        |
| Compiler version   | Producing compiler, for the degradation rule below |
| `wado.toml`        | The package's own dependencies and entry points    |
| The package's `.wado` sources | Bodies for `pub` items, and everything they reach |

The section content is deterministic: sorted file order, no timestamps, fixed compression level. Same input, same bytes, same digest — otherwise `integrity` and reproducible builds break.

Sources are included whole rather than pruned to the `pub`-reachable set. A `pub` generic body calls `internal` helpers, so most of a package is reachable anyway; pruning can follow if size proves to matter.

A custom section is not instantiated, so this costs distribution bytes only, never runtime size.

### Why a section, not a second OCI layer

OCI is the primary registry, and the single-`application/wasm`-layer convention is what generic OCI and wasm tooling already handles — custom sections propagate unchanged through the toolchain ([WIT Bundling](./wep-2026-03-21-wit-bundling.md)). A section also keeps the artifact self-describing outside any registry (`path` dependencies, `https://` imports, release assets), so one rule covers every channel, and `integrity` stays a single manifest digest with no per-layer verification scope to define.

### Consumer selection is all or nothing

| Consumer | `wado:package` present and supported | Path                                     |
| -------- | ------------------------------------- | ---------------------------------------- |
| Wado     | Yes                                   | Source; the compiled component is unused |
| Wado     | No                                    | CM canonical ABI, `export` items only    |
| Other CM | —                                     | CM canonical ABI, `export` items only    |

A package is consumed one way or the other, never both. Mixing would compile the same declaration twice into two distinct nominal types — the split identity [Module Loader](./wep-2026-01-24-module-loader.md) §"Canonical module identity" avoids for local paths — and would put two compiler generations inside one package, so a fix present in one half is absent in the other.

### The compiled component is the compatibility floor

Because selection can fall back, the attached binary is not a duplicate of the source: it is what a consumer uses when the section's format or compiler version is out of range. Degradation is narrower than the source path, so it must not be silent when it matters:

- Only `export` items are used — proceed on the CM path.
- A `pub`-only item is used — error, naming the cause and the missing item.

### Stripping

- `wado publish` attaches the section; `wado compile` does not.
- Final link drops the sections of statically composed dependencies ([Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)), at every optimization level. An application must not ship its dependencies' sources.
- This is independent of `-Os` symbol stripping.

### Transitive dependencies

A source package carries its own `wado.toml`, so a registry dependency now has transitive Wado dependencies — revising [Package Manifest](./wep-2026-02-14-package-manifest.md) §Registry backend, which states the opposite for prebuilt components. Resolution and locking reuse the git-dependency path unchanged.

Single-file mode has no lock file, so a source package whose manifest carries a version range is an error there, with the same remedy as any other unlockable range: pin exactly, or add a `wado.toml`.

### `--no-source`

`wado publish --no-source` emits a CM-only artifact. `pub`-only items are then unreachable to every consumer — the stated cost of withholding source, not a silent difference.

## Consequences

- The registry becomes a complete channel: a generic or trait-based library is publishable with its real API, and git source dependencies stop being the only way to ship one.
- `pub` / `export` lose their distribution meaning. Visibility says who may name an item, `export` says which ABI carries it, and the artifact says how it ships — three independent axes.
- Dependencies gain hover, go-to-definition, and `wado doc` for free, since their source is present.
- Every consumer downloads both representations. Compression absorbs most of it, and the binary earns its bytes as the fallback path.
- One artifact now presents two interfaces depending on who reads it. This mirrors Rust's rlib / `cdylib` split, but it makes the conformance requirement below load-bearing: a divergence would surface only for one class of consumer.

## Implementation

- [ ] Conformance test first: one fixture consumed through both paths, asserting identical observable behavior for `export` items. [Package Manifest](./wep-2026-02-14-package-manifest.md) §Wado-to-Wado Optimization claims this ("the optimization only affects performance"); until it is tested it is an assumption, and the rest of this WEP rests on it.
- [ ] `wado:package` section format, with the determinism requirements.
- [ ] Publish path: attach on `wado publish`, `--no-source` opt-out.
- [ ] Consumer selection and the degradation diagnostics.
- [ ] Strip composed dependencies' sections at final link.
- [ ] Transitive resolution for registry source packages; revise Package Manifest §Registry backend.
- [ ] LSP reads the section for dependency navigation.

## References

- [Visibility — `internal` / `pub` / `export`](./wep-2026-06-25-visibility-internal-pub-export.md)
- [Package Manifest (`wado.toml`)](./wep-2026-02-14-package-manifest.md)
- [WIT Bundling in Component Binaries](./wep-2026-03-21-wit-bundling.md)
- [Wasm CM Component Import (`use`-based)](./wep-2026-06-26-wasm-cm-component-import.md)
- [Module Loader Design](./wep-2026-01-24-module-loader.md)
