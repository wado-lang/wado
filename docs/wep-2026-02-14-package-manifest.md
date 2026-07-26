# WEP: Package Manifest (`wado.toml`)

## Context

Wado currently supports single-file execution (`wado run file.wado`) and local module imports (`./`, `../`). As the ecosystem grows, projects need:

- Package metadata (name, version, namespace) for publishing to registries
- External dependency management (git, registry, local path)
- Separation of production and development dependencies
- Reproducible builds via lock files
- Transitive dependency resolution

The design must preserve Wado's simplicity: a single `.wado` file without `wado.toml` must continue to work.

## Decision

### Package Manifest Format

The project manifest is `wado.toml`, placed at the project root.

```toml
[package]
namespace = "myorg"
name = "my-app"
version = "0.1.0"
lib = "src/lib.wado"
description = "A fast widget toolkit"
homepage = "https://wado-lang.org"
repository = "https://github.com/myorg/my-app"
documentation = "https://docs.wado-lang.org"
license = "MIT OR Apache-2.0"
authors = ["Alice <alice@example.com>"]
wado-version = ">=0.5"

[world]
"wasi:cli/command" = "src/main.wado"

[registries]
default = "oci://ghcr.io/acme"

[dependencies]
"docs:regex" = { version = "^0.1.0" }                                            # direct coordinate
"user:router" = { git = "https://github.com/user/router.git", version = "^1.0.0" }  # coordinate, git source
"lib:shared" = { path = "../shared" }                                            # nickname (no public coordinate)

[dev-dependencies]
"lib:bench" = { git = "https://gitlab.com/user/bench.git", ref = "main" }
```

Each key is byte-identical to the `from "..."` specifier it backs (see
[Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md)):
an open coordinate `ns:pkg`, or a `lib:` nickname for indirection. Bare keys
(`router`) are rejected.

### `[package]`

| Field                  | Type       | Required | Description                                                                                   |
| ---------------------- | ---------- | -------- | --------------------------------------------------------------------------------------------- |
| `namespace`            | `string`   | No       | Organization or user namespace (e.g., `"myorg"`)                                              |
| `name`                 | `string`   | Yes      | Package name (e.g., `"my-app"`)                                                               |
| `version`              | `string`   | Yes      | Semver version (e.g., `"0.1.0"`)                                                              |
| `lib`                  | `string`   | No       | Entry module of the package's library world                                                   |
| `description`          | `string`   | No       | Short, human-readable summary                                                                 |
| `homepage`             | `string`   | No       | Project home page URL (defaults to `repository`)                                              |
| `repository`           | `string`   | No       | Source repository URL (bare repo URL, no subdirectory)                                        |
| `repository-directory` | `string`   | No       | Subdirectory holding the package within a monorepo (Wado-custom; not an OCI key)              |
| `documentation`        | `string`   | No       | Documentation URL (defaults to `repository`)                                                  |
| `license`              | `string`   | No       | SPDX License Expression (e.g., `"MIT OR Apache-2.0"`). Mutually exclusive with `license-file` |
| `license-file`         | `string`   | No       | Path to a non-standard license file. Mutually exclusive with `license`                        |
| `authors`              | `string[]` | No       | Contact details of the people or organization responsible                                     |
| `wado-version`         | `string`   | No       | Minimum Wado compiler version required to build (e.g., `">=0.5"`)                             |
| `publish`              | `bool`     | No       | `false` opts a namespaced package out of publishing. Default `true`                           |

`namespace` and `name` together form the registry identity (`namespace:name`, e.g., `myorg:my-app`). Without `namespace`, the package cannot be published to a registry — this is the natural state for closed-source applications and internal tools. A namespaced package can still opt out explicitly with `publish = false`.

The human-facing fields are backend-agnostic metadata kept in `[package]` (not a registry-flavored section); the OCI mapping is just a serialization detail (see [Package Metadata and Publishing](#package-metadata-and-publishing)).

#### Name and Namespace Validation

Both `namespace` and `name` must match `[a-zA-Z0-9_-]+` (minimum 1 character, maximum 64 characters).

Dependency keys in `[dependencies]` are quoted specifiers — an open coordinate `"ns:pkg"` or a `"lib:nick"` nickname — each segment matching the same `[a-zA-Z0-9_-]+` rule. The `lib:`-or-coordinate form makes a real registry identity and a local indirection distinguishable on sight.

A package must declare at least one world: a `[world]` entry, `[package].lib`, or both.

#### Unknown keys

Keys the schema does not recognize — at the top level, in `[package]`, or in `[workspace.package]` — are reported as warnings, never errors. A typo (`descripton`, `licence`) is surfaced without breaking the build, and an inherited `[workspace.package]` typo surfaces on the member that inherits it.

### Package Metadata and Publishing

The human-facing `[package]` fields are universal package metadata; on publish to OCI (see [Registry backend](#registry-backend)) each maps to a standard `org.opencontainers.image.*` annotation:

| `[package]` field      | OCI annotation                                       | Notes                                                                |
| ---------------------- | ---------------------------------------------------- | -------------------------------------------------------------------- |
| `description`          | `org.opencontainers.image.description`               | Short human-readable summary                                         |
| `homepage`             | `org.opencontainers.image.url`                       |                                                                      |
| `repository`           | `org.opencontainers.image.source`                    | Bare repo URL — enables registry → repo auto-linking                 |
| `documentation`        | `org.opencontainers.image.documentation`             |                                                                      |
| `license`              | `org.opencontainers.image.licenses`                  | SPDX License Expression                                              |
| `authors`              | `org.opencontainers.image.authors`                   | Array, serialized comma-separated                                    |
| `version`              | `org.opencontainers.image.version`                   | Package semver                                                       |
| (git commit SHA)       | `org.opencontainers.image.revision`                  | Auto-derived at build time; clean tree only                          |
| `repository-directory` | `org.wado-lang.package.repository-directory`         | Wado-custom annotation (no OCI key); embedded, not promoted by `wkg` |
| `license-file`         | `org.opencontainers.image.licenses` = `LicenseRef-…` | License text embedded in the `org.wado-lang.license` custom section  |

`created` is not modeled (the registry tool owns publish timestamps);
`keywords`/`categories` are omitted (no OCI key, so they would not reach the registry).

Metadata with no standard OCI key uses the Wado namespace
`org.wado-lang.package.*`, keyed by the `wado.toml` field name (e.g.
`org.wado-lang.package.repository-directory`). Wado-proprietary binary custom
sections use the same `org.wado-lang.*` namespace.

#### License

`license` is an SPDX expression (`"MIT OR Apache-2.0"`), validated at parse time; the SPDX id is the canonical reference, so no file ships. A non-standard license uses `license-file` instead — the annotation becomes `LicenseRef-<name>` and the file's text is embedded. The two are mutually exclusive; publishing requires one.

#### Repository subdirectory (monorepo)

Neither OCI nor git addresses a repository subdirectory, so `repository` stays a bare repo URL (for registry → repo linking) and the package's location is recorded in `repository-directory` — embedded as Wado-custom metadata, not an OCI annotation. The consuming side is served by the git dependency's `directory` field (see [Git](#git)).

#### Metadata embedding and the publish backend

`wado build` embeds the metadata into the component in the `wasm-metadata`
custom-section format. Embedding is a project-tier concern — it happens in
`wado build` (which reads the cwd or `<dir>/wado.toml`), never in the
manifest-free `wado compile <file>` primitive, which compiles a standalone
target and embeds nothing (see the [CLI-subcommands WEP][cli-subcommands]
Command Tiers). `--no-embed-metadata` opts out, and `-Os` (strip symbols for
minimal frontend delivery) drops the metadata too, matching the WIT section;
`--embed-metadata` forces it back on under `-Os` (mirroring `--embed-wit`).

[cli-subcommands]: ./wep-2026-02-22-cli-subcommands.md

`wado publish` builds each publishable world through the same build path and
shells out to `wkg` (wasm-pkg-tools), which derives the OCI annotations. There is no `wkg.toml` —
`wado.toml` is the only source of truth, and `wkg` is an implementation detail
(a missing `wkg` errors with install guidance). Authentication is delegated to
`wkg`: either the ambient OCI credential store (`docker login <registry>`, or
docker/login-action in GitHub Actions) or the `WKG_OCI_USERNAME` /
`WKG_OCI_PASSWORD` environment variables. For GHCR the username is the GitHub
account (`github.actor` in Actions) and the password a token with the
`write:packages` scope (e.g. `GITHUB_TOKEN`). Wado stores no credentials.
`revision` is the git commit SHA, derived at build time; a dirty tree omits it,
warned only at publish.

### Entry Points and Worlds

A package targets one or more Component Model worlds. Hosted worlds are declared in the `[world]` table, keyed by the fully-qualified world name; the library world is declared by `[package].lib`.

| Declaration                     | World                 | Driver       | Required export                           |
| ------------------------------- | --------------------- | ------------ | ----------------------------------------- |
| `[world]."wasi:cli/command"`    | `wasi:cli/command`    | `wado run`   | `export fn run()`                         |
| `[world]."wasi:http/service"`   | `wasi:http/service`   | `wado serve` | `export fn handle(request: Request) -> …` |
| `[world]."core:kiln/generator"` | `core:kiln/generator` | Kiln         | `export fn generate(...)`                 |
| `[package].lib`                 | the library world     | (none)       | `export` items become the public API      |

The library world's name is the package name. It is the contract other packages compose against, but it is not observable in a wado-to-wado source dependency: the CM boundary is skipped and the dependency's modules compile into the consumer's component (see "Wado-to-Wado Optimization"). The world materializes only when the package is built as a standalone `.wasm` component.

```toml
# CLI tool with a library world
[package]
name = "markdown"
lib = "src/lib.wado"

[world]
"wasi:cli/command" = "src/cli.wado"

# HTTP service only
[package]
name = "my-api"

[world]
"wasi:http/service" = "src/server.wado"

# Library only (world name = "json")
[package]
name = "json"
lib = "src/lib.wado"
```

### Visibility and Component Boundary

Visibility is two orthogonal axes — the `internal` / `pub` scope ladder and the
`export` CM flag. See [WEP: Visibility — `internal` / `pub` /
`export`](./wep-2026-06-25-visibility-internal-pub-export.md) for the full
model; the package-relevant points:

| Modifier   | Scope                          | Use                                        |
| ---------- | ------------------------------ | ------------------------------------------ |
| (none)     | File-private                   | Implementation details                     |
| `internal` | Package-internal               | Shared across files within the package     |
| `pub`      | Library boundary (Wado-native) | The package's public API to Wado packages  |
| `export`   | + CM boundary (`export ⟹ pub`) | Public API also exposed to any CM consumer |

```wado
// src/lib.wado (in the "markdown" package)
fn tokenize(input: String) -> List<Token> { ... }            // private
internal fn build_ast(tokens: List<Token>) -> Document { ... } // package-internal
pub fn parse(input: String) -> Document { ... }                // library API (Wado-native)
export fn render(doc: Document) -> String { ... }              // library API + CM boundary
```

When another project depends on the `markdown` package (declared `"lib:markdown" = { ... }`, since it has no public namespace), `pub` and `export` items from the `lib` entry point are visible:

```wado
// In a consuming Wado project
use { parse, render } from "lib:markdown"; // OK: pub / export
// use { build_ast } from "lib:markdown";   // ERROR: internal, not part of the API
// use { tokenize } from "lib:markdown";    // ERROR: private
```

When published as a `.wasm` component (e.g., to an OCI registry), only `export` items appear in the component's CM interface; `pub`-only items reach Wado consumers via the provider-metadata path below (see [Provider Metadata](./wep-2026-07-26-provider-metadata.md)).

Crossing the package boundary requires `pub` (or `export`): a consumer may import only the `pub` / `export` items of a dependency's `lib`, never its `internal` or private items. This is a settled rule; enforcing it for wado-to-wado source dependencies is not yet implemented.

### Wado-to-Wado Optimization

A cross-package reference resolves against the dependency's library API (`pub` and `export` items). For an `export` item consumed by an arbitrary CM component, the reference goes through the CM Canonical ABI. When both producer and consumer are Wado, the compiler skips the CM ABI (lifting/lowering) and shares Wasm GC types directly — and a `pub`-only item (generic, closure-taking, trait-based) is reachable only on this path, since it has no CM representation.

| Consumer → Producer                               | Path                                         |
| ------------------------------------------------- | -------------------------------------------- |
| Wado → Wado (source dependency)                   | CM binding skipped; GC types shared directly |
| Wado → Wado (`.wasm` with Wado provider metadata) | Provider detected; GC types shared directly  |
| Wado → arbitrary `.wasm`                          | CM canonical ABI (lifting/lowering)          |
| Arbitrary → Wado `.wasm`                          | CM canonical ABI                             |

This optimization is transparent to the developer. For `export` items the observable semantics are CM boundary semantics; the optimization only affects performance — cross-package calls between Wado projects have no overhead compared to project-internal calls.

### `[registries]`

Named registry aliases. Keys are short names; values are registry URLs. The special key `default` sets the default registry — a registry dependency with no `registry` field uses it automatically.

```toml
[registries]
default = "oci://ghcr.io/acme"
custom = "https://registry.example.com"
```

```toml
[dependencies]
"docs:regex" = { version = "^0.1.0" }                                       # uses default registry
"lib:special" = { registry = "custom", package = "ns:lib", version = "^1.0.0" }  # uses named registry
```

A registry dependency with no `registry` field requires `default` to be set. If `default` is not defined and `registry` is omitted, it is an error.

### Registry backend

Registry resolution and publishing use OCI (the OCI Distribution Spec): a component is an OCI artifact in a container registry (e.g. `ghcr.io`), and the content digest provides integrity. A registry URL takes the form `oci://<host>/<prefix>`; an open coordinate `ns:pkg` resolves to the repository `<host>/<prefix>/<ns>/<pkg>`, with the version as an image tag. That repository holds the library world; a package's other worlds get a `/<world>` sub-path (see [Publishable Worlds and OCI Layout](#publishable-worlds-and-oci-layout)).

The earlier warg protocol is dropped. Its registry (`bytecodealliance/registry`) is archived and the ecosystem (`wasm-pkg-tools`) defaults to OCI. A warg-only registry such as wa.dev is reachable only through the external `wkg` tool, not natively; Wado neither implements nor wraps warg. Publishing is likewise done with `wkg`, not a Wado subcommand.

Consuming (pulling), unlike publishing, does not go through `wkg`: a published package is a standalone Wasm Component Model artifact (one `application/wasm` layer), and resolving a dependency is a hot path that must not require an external binary. So the CLI pulls with a native OCI client (the `oci-client` crate), authenticating exactly as publish does (`WKG_OCI_USERNAME` / `WKG_OCI_PASSWORD`, else anonymous). `integrity` is the OCI manifest digest — one layer, one digest, which is also why the package's source rides in a custom section of that same `.wasm` rather than a second layer.

The artifact carries both a prebuilt component and the package's Wado source, so a registry dependency does have transitive Wado dependencies, and a Wado consumer resolves it on the source-sharing path — see [Provider Metadata](./wep-2026-07-26-provider-metadata.md) for the artifact layout and the all-or-nothing selection rule, and [Wado-to-Wado Optimization](#wado-to-wado-optimization) for the two consumption paths. A `--no-source` artifact is a prebuilt component only, consumed across the Component Model boundary with `export` items alone.

### `[dependencies]` and `[dev-dependencies]`

Each key is the specifier used in Wado source code, byte-for-byte (`"docs:regex"`, `"lib:shared"`). Values are inline tables specifying the dependency source. See [Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md) for the key forms and resolution rules.

`[dev-dependencies]` are only available during `wado test` and are excluded from production builds.

### Dependency Source Types

Each dependency must have exactly one primary source type (`git`, `registry`, or `path`). The exception is `path`, which can be combined with `registry` or `git` for publishing (see Publishing).

#### Git

```toml
# Semver on git tags (identity = the coordinate key, source = git)
"user:router" = { git = "https://github.com/user/router.git", version = "^1.0.0" }

# Exact git ref (tag, branch, or SHA)
"user:router" = { git = "https://github.com/user/router.git", ref = "v1.0.0" }
"user:router" = { git = "https://github.com/user/router.git", ref = "main" }
```

| Field       | Required | Description                                                                                   |
| ----------- | -------- | --------------------------------------------------------------------------------------------- |
| `git`       | Yes      | Full git URL (any host: GitHub, GitLab, etc.)                                                 |
| `version`   | XOR      | Semver range on git tags (e.g., `"^1.0.0"`)                                                   |
| `ref`       | XOR      | Exact git ref (tag, branch, or commit SHA)                                                    |
| `directory` | No       | Subdirectory holding the package within the repository (monorepo). Defaults to the repo root. |

```toml
# Package in a subdirectory of a monorepo
"org:foo" = { git = "https://github.com/org/monorepo.git", version = "^1.0.0", directory = "packages/foo" }
```

`directory` is an explicit field rather than URL-encoded: git has no subdirectory URL syntax and the ecosystem conventions that bolt one on (`//subdir`, `#subdirectory=`, `?path=`) are not interoperable, so a dedicated key keeps the path unambiguous and host-independent.

Exactly one of `version` or `ref` must be specified. `version` resolves against semver-tagged releases in the repository. `ref` pins to an exact git ref — use explicit branch names (e.g., `"main"`) rather than implicit defaults.

#### Registry

```toml
"docs:regex" = { version = "^0.1.0" }                                       # direct coordinate, default registry
"lib:rx" = { package = "docs:regex", version = "^0.1.0" }                   # nickname → coordinate
"lib:special" = { registry = "custom", package = "ns:lib", version = "^1.0.0" }  # named registry
```

| Field      | Required            | Description                                                                                |
| ---------- | ------------------- | ------------------------------------------------------------------------------------------ |
| `registry` | No                  | Registry alias (defined in `[registries]`). Defaults to `default`.                         |
| `package`  | `lib:` aliases only | Real coordinate in `namespace:name` format. Omitted when the key is itself the coordinate. |
| `version`  | Yes                 | Semver version range (e.g., `"^0.1.0"`)                                                    |

When the key is an open coordinate (`"docs:regex"`), it _is_ the package
identity and `package` is omitted. `package` appears only on a `lib:` nickname
that aliases a registry coordinate.

#### Local Path

```toml
"lib:shared" = { path = "../shared" }
```

| Field  | Required | Description                                  |
| ------ | -------- | -------------------------------------------- |
| `path` | Yes      | Relative path to a directory or `.wado` file |

Local path dependencies are resolved relative to the `wado.toml` location. They are not locked (always resolved fresh). During development, only the `path` is used — any accompanying `registry` or `git` source is ignored entirely.

For publishing, `path` can be combined with a registry or git source. When publishing (`wado publish`), the `path` is stripped and the accompanying source is used in the published package manifest:

```toml
"lib:shared" = { path = "../shared", package = "myorg:shared", version = "^0.1.0" }
"lib:shared" = { path = "../shared", git = "https://github.com/org/shared.git", version = "^0.1.0" }
```

### Module Resolution with Dependencies

The specifier grammar and the reserved = bundled rule are defined in
[Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md).
A dependency-backed specifier is one of:

- an open coordinate `ns:pkg` (`ns` ∉ {`wasi`, `core`}), or
- a `lib:` nickname.

Both resolve by looking up the byte-identical key in `[dependencies]` (or
`[dev-dependencies]` during test). `wasi:`/`core:` resolve to bundled sources,
`./`/`../` to local files, `http(s)://` to a remote URL.

```wado
use { println } from "core:cli";        // bundled
use { Request } from "wasi:http";        // bundled
use { helper }  from "./utils.wado";     // local file
use { Regexp }  from "docs:regex";       // open coordinate → wado.toml → registry
use { Router }  from "lib:router";       // nickname → wado.toml
```

A dependency specifier binds to the dependency's library world — its
`[package].lib` entry module — and resolves the imported symbols against that
module's `export` items. Only the consuming project resolves its own
`[dependencies]`: a dependency specifier from within a dependency module does
not bind to the consumer's dependencies.

#### `ModuleSource` Extension

```rust
pub enum ModuleSource {
    Core { name: String },
    Wasi { interface: String },
    Local { path: String },
    Remote { url: String },
    EntryPoint { filename: Option<String> },
    // A dependency's library-world module. Identified by its resolved entry
    // module so that two specifiers for the same package unify: the resolved
    // path for a path dependency; the resolved package id for a
    // registry/git dependency.
    Dependency { path: String },
}
```

### Transitive Dependency Resolution

When a dependency itself has a `wado.toml` with dependencies, those are transitive dependencies.

#### Resolution Algorithm: PubGrub

Wado uses the PubGrub algorithm for dependency resolution. PubGrub is a conflict-driven nogood learning (CDCL) solver, originally designed for Dart's `pub` and adopted by `uv` (Python), Swift Package Manager, and others.

Why PubGrub over alternatives:

| Approach                 | Pros                                                     | Cons                                              |
| ------------------------ | -------------------------------------------------------- | ------------------------------------------------- |
| Go MVS (minimum version) | O(n), deterministic without lock file                    | Users get old/buggy versions; no upper bounds     |
| Cargo-style backtracking | Proven at scale                                          | Weaker conflict learning; less informative errors |
| PubGrub (CDCL)           | Best error messages; efficient pruning; state of the art | NP-hard worst case (acceptable in practice)       |

PubGrub provides:

- Conflict-driven learning: when a conflict is found, the solver derives a precise incompatibility that explains _why_ this combination fails and never re-explores it
- Human-readable error messages: each resolution failure comes with a derivation chain (e.g., "because A requires utils ^1.0 and B requires utils ^2.0, and your project requires both A and B, version solving failed")
- Efficient pruning: near-polynomial performance in practice despite NP-hard worst case

The Rust crate `pubgrub` provides a ready-made implementation.

#### Version Specifiers

The `version` field requires an explicit range operator — bare versions are errors.

| Prefix | Meaning            | Example  | Range             |
| ------ | ------------------ | -------- | ----------------- |
| `^`    | Caret (compatible) | `^1.2.3` | `>=1.2.3, <2.0.0` |
| `^`    | Caret (pre-1.0)    | `^0.2.3` | `>=0.2.3, <0.3.0` |
| `~`    | Tilde (patch-only) | `~1.2.3` | `>=1.2.3, <1.3.0` |
| `=`    | Exact              | `=1.2.3` | `=1.2.3`          |
| (none) | Error              | `1.2.3`  | compile error     |

```
version = "^1.0.0"   # OK: caret range
version = "~1.0.0"   # OK: tilde range
version = "=1.0.0"   # OK: exact pin
version = "1.0.0"    # ERROR: bare version requires explicit prefix
```

Requiring an explicit prefix eliminates ambiguity — the user always knows exactly what range semantics are in effect. This applies uniformly to registry dependencies and git dependencies with `version`.

#### Git Tag Format

When resolving `version` for git dependencies, the resolver scans git tags and strips an optional letter prefix to extract the semver version:

```
v1.0.0    → 1.0.0    (strip "v")
release1.2.3 → 1.2.3 (strip "release")
1.0.0     → 1.0.0    (no prefix)
```

The rule: ignore the first `[a-zA-Z]+` prefix if present. Tags that do not contain a valid semver after stripping are silently ignored. This matches the convention used by most ecosystems (Go, npm, Cargo) where `v` prefix is common.

#### Semver Compatibility

Two requirements are semver-compatible if they share the same compatibility range (same major version for `>=1.0.0`, same major.minor for `0.x`). Within a compatibility range, the resolver selects exactly one version — the highest that satisfies all constraints.

#### Multiple Version Coexistence

Semver-incompatible versions of the same package can coexist in the dependency tree as separate module instances. This matches Wasm Component Model's type isolation — types from different component instances are inherently distinct.

```
my-app
├── router 1.2.0 (depends on utils ^1.0)
└── auth 0.5.0 (depends on utils ^1.1)

Resolved: utils 1.1.x (one instance, satisfies both)
```

```
my-app
├── legacy-lib (depends on http ^1.0)
└── new-lib (depends on http ^2.0)

Resolved: http 1.x AND http 2.x (two separate instances)
```

Within a single `wado.toml`, a user can also explicitly depend on multiple major versions through `lib:` nicknames, each pinning a different range of the same coordinate:

```toml
[dependencies]
"lib:http1" = { package = "std:http", version = "^1.0.0" }
"lib:http2" = { package = "std:http", version = "^2.0.0" }
```

#### Transitive Version Isolation

The resolver runs on the full dependency graph and produces a flat resolution map. Each resolved package is identified by `(package identity, compatibility range)`:

```
package identity = registry URL + namespace:name  (for registry deps)
                 = git URL                         (for git deps)

resolution key   = (package identity, major version)
                   e.g., (ghcr.io/acme/std:http, 1) and (ghcr.io/acme/std:http, 2)
```

When two transitive dependencies require semver-incompatible versions of the same package, they each get their own resolved instance. The compiler does not need to know about this — it simply receives module sources from `CompilerHost`. The resolver (in the CLI) handles mapping.

The existing `resolve_import(from_module_source, import_source)` signature already provides the necessary context. The `from_module_source` tells the `CompilerHost` _which package is doing the importing_, so the same specifier `"myns:foo"` resolves to different packages depending on the caller:

```
resolve_import(from=EntryPoint, "myns:foo")
  → CompilerHost looks up my-app's wado.toml → "myns:foo" version 2.0.0
  → returns ModuleSource::Dependency { id: "registry+oci://ghcr.io/acme/myns:foo@2.0.0" }

resolve_import(from=Dependency{id="registry+oci://ghcr.io/acme/user:router@1.0.0"}, "myns:foo")
  → CompilerHost looks up router's wado.toml → "myns:foo" version 1.0.0
  → returns ModuleSource::Dependency { id: "registry+oci://ghcr.io/acme/myns:foo@1.0.0" }
```

The compiler sees distinct `ModuleSource::Dependency` values (different `id`) and compiles each independently. No changes to the compiler are needed — the `CompilerHost` implementation in the CLI handles all version-aware routing. Type isolation is natural — two separately compiled modules never share types.

#### Diamond Dependency Handling

When two dependencies require the same transitive dependency:

- Compatible versions: unified to one resolved instance (highest compatible). PubGrub finds this automatically.
- Incompatible versions: coexist as separate instances. Each dependent sees its own version. Types do not cross boundaries.
- Unsatisfiable: if constraints within a compatibility range conflict (e.g., `=1.2.0` and `=1.3.0`), PubGrub reports a precise error with derivation chain.

#### Cyclic Dependency Detection

The resolver detects cycles in the dependency graph and reports a clear error:

```
error: cyclic dependency detected
  → my-app depends on auth ^1.0
  → auth 1.2.0 depends on core ^0.5
  → core 0.5.1 depends on my-app ^0.1
```

Cyclic dependencies are always an error. Unlike some ecosystems that allow weak/optional cycles, Wado prohibits them — each package must form a directed acyclic graph (DAG). This is consistent with Wasm Component Model's instantiation order requirements.

### Lock File (`wado.lock`)

The lock file captures the complete dependency graph with exact resolved versions. It is self-sufficient — when the lock file exists, the build system does not need to read each dependency's `wado.toml`.

```toml
# This file is auto-generated by wado. Do not edit manually.
version = 1
deps-hash = "sha256:9f8e7d6c5b4a..."

[[package]]
id = "git+https://gitlab.com/user/bench.git/bench-tool"
version = "0.1.0"
resolved-ref = "def5678901234567890abcdef12345678abc1234d"
dev = true
world = { "wasi:cli/command" = "src/main.wado" }
deps = []

[[package]]
id = "registry+oci://ghcr.io/acme/docs:regex"
version = "0.1.2"
integrity = "sha256:a1b2c3d4e5f6..."
lib = "src/lib.wado"
deps = ["registry+oci://ghcr.io/acme/docs:regex-utils@0.3.0"]

[[package]]
id = "registry+oci://ghcr.io/acme/docs:regex-utils"
version = "0.3.0"
integrity = "sha256:f6e5d4c3b2a1..."
lib = "src/lib.wado"
deps = []

[[package]]
id = "registry+oci://ghcr.io/acme/std:json"
version = "1.2.0"
integrity = "sha256:c3d4e5f6a1b2..."
lib = "src/lib.wado"
deps = []

[[package]]
id = "registry+oci://ghcr.io/acme/tools:utils"
version = "0.5.1"
integrity = "sha256:b2c3d4e5f6a1..."
lib = "src/lib.wado"
deps = []

[[package]]
id = "git+https://github.com/user/router.git/user:router"
version = "1.0.2"
resolved-ref = "abc1234def5678901234567890abcdef12345678"
lib = "src/lib.wado"
deps = ["registry+oci://ghcr.io/acme/tools:utils@0.5.1", "registry+oci://ghcr.io/acme/std:json@1.2.0"]
```

Each `[[package]]` entry is uniquely identified by `(id, version)`. The `id` field is the resolved package id — the source prefix combined with the package identity (e.g., `registry+oci://ghcr.io/acme/docs:regex` or `git+https://github.com/user/router.git/user:router`). The `deps` array references other entries using `id@version` format.

#### Header Fields

| Field       | Description                                                                                                                          |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `version`   | Lock file format version                                                                                                             |
| `deps-hash` | Hash of `[dependencies]` and `[dev-dependencies]` sections from `wado.toml`. Used for staleness detection (see Lock File Freshness). |

#### Package Fields

| Field          | Applies to    | Description                                                                                      |
| -------------- | ------------- | ------------------------------------------------------------------------------------------------ |
| `id`           | all           | Resolved package id: `source/package-identity` (e.g., `registry+URL/ns:name`, `git+URL/ns:name`) |
| `version`      | all           | Exact resolved version                                                                           |
| `resolved-ref` | git only      | Exact commit SHA (40 hex chars)                                                                  |
| `integrity`    | registry only | Content hash with algorithm prefix (see below)                                                   |
| `dev`          | dev-deps only | `true` for dev-only packages (excluded from production)                                          |
| `world`        | optional      | CM world FQ name → entry path, mirroring the dependency's `[world]` table (inline table)         |
| `lib`          | optional      | Library-world entry module (from the dependency's `[package].lib`)                               |
| `deps`         | all           | List of `id@version` strings referencing other entries                                           |

The `world` table and `lib` are copied from the dependency's `wado.toml` at resolution time. This makes the lock file self-sufficient — the `CompilerHost` can resolve all imports and locate all source files using only the root `wado.toml` and `wado.lock`.

`path` dependencies are not locked (always resolved fresh).

#### Build Flow

```
Without lock file:  wado.toml → fetch deps → read each wado.toml → resolve → compile
With lock file:     wado.toml + wado.lock → fetch (exact refs known) → compile
```

When the lock file exists, the resolver is skipped entirely. The dependency graph, entry points, and exact versions are all read from `wado.lock`. Each dependency's `wado.toml` is not read.

#### Properties

- Deterministic: entries sorted by `id` then `version` lexicographically
- Human-readable TOML
- Committed to version control
- `path` dependencies are not locked (always resolved fresh, not listed)
- Self-sufficient: contains the full dependency graph and entry points

#### Lock File Freshness

When `wado.toml` changes (dependency added, removed, or version constraint changed), the lock file may become stale. The behavior depends on context:

```sh
wado run                               # auto re-resolves if wado.toml changed
wado build                             # auto re-resolves if wado.toml changed
wado build --locked                    # ERROR if lock file is stale
```

(Project builds go through `wado build`; `wado compile` is a manifest-free
primitive — see the [CLI-subcommands WEP](./wep-2026-02-22-cli-subcommands.md)
Command Tiers.)

`--locked` is intended for CI environments where reproducibility is critical. When `--locked` is specified, the build fails with an error if `wado.toml` has changed since the last `wado update`, rather than silently re-resolving.

Auto re-resolution detects staleness via the `deps-hash` field in the lock file header, which is a hash of the `[dependencies]` and `[dev-dependencies]` sections of `wado.toml`. If the hash changes, the resolver runs again and updates the lock file.

### Integrity Verification

The `integrity` field uses a prefixed format: `algorithm:hex-encoded-hash`.

```
integrity = "sha256:a1b2c3d4e5f6..."
```

The prefix makes the format extensible — if SHA-256 is ever deprecated, a new algorithm can be introduced without changing the lock file schema.

#### Calculation Method

| Source     | Integrity                                                                                                                                              |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Registry   | Hash of the archive as downloaded from the registry. The registry defines the canonical archive format.                                                |
| Git        | `resolved-ref` (commit SHA) serves as integrity. Git's content-addressable storage already guarantees integrity. No separate `integrity` field needed. |
| Local path | Not locked. Always resolved fresh.                                                                                                                     |

For registry packages, the hash input is the downloaded archive bytes (not individual source files concatenated). This matches how registries distribute packages and avoids ambiguity about file ordering or line endings.

The initial algorithm is SHA-256. The resolver verifies integrity on every install: if the computed hash does not match `integrity`, the install fails with an error.

### Conceptual Model

```
wado.toml            = project manifest (the file)
[package]            = CM package (the distributable unit)
[workspace]          = package group (multi-package development)
[dependencies]       = package dependencies (shipped with the package)
[dev-dependencies]   = project dependencies (development only, not shipped)
```

The file itself represents the project. `[package]` describes the distributable unit within it — its identity, entry points, and public API. A package without `namespace` is not publishable, which is the natural state for closed-source applications.

### `[workspace]`

A workspace groups multiple packages for co-development. The workspace root has a `wado.toml` with a `[workspace]` section:

```toml
# workspace root: wado.toml
[workspace]
members = ["packages/*"]

[workspace.package]
version = "0.1.0"
repository = "https://github.com/myorg/monorepo"
namespace = "myorg"
license = "MIT"
authors = ["Alice <alice@example.com>"]

[workspace.dependencies]
"std:json" = { version = "^1.0.0" }

[workspace.dev-dependencies]
"lib:bench" = { git = "https://gitlab.com/user/bench.git", version = "^0.1.0" }
```

```toml
# packages/core/wado.toml
[package]
name = "core"
description = "Shared core types"
lib = "src/lib.wado"
# version / repository / namespace / license / authors inherited
```

```toml
# packages/cli/wado.toml
[package]
name = "my-tool"
description = "The command-line tool"
repository-directory = "packages/cli"

[world]
"wasi:cli/command" = "src/main.wado"

[dependencies]
"myorg:core" = { path = "../core" }
```

| Field     | Type       | Required | Description                                  |
| --------- | ---------- | -------- | -------------------------------------------- |
| `members` | `string[]` | Yes      | Glob patterns for member package directories |

#### `[workspace.package]` — Shared Package Metadata

`[workspace.package]` holds metadata shared by every member. Inheritance is automatic — unlike dependencies, members do not write `{ workspace = true }`. Override depends on the field:

| Field          | Inheritance                     | Member override          |
| -------------- | ------------------------------- | ------------------------ |
| `version`      | Forced                          | Error if set in a member |
| `repository`   | Forced                          | Error if set in a member |
| `namespace`    | Forced                          | Error if set in a member |
| `license`      | Default                         | Allowed                  |
| `license-file` | Default (path relative to root) | Allowed                  |
| `authors`      | Default                         | Allowed                  |
| `wado-version` | Default                         | Allowed                  |

Only these seven fields are inheritable; any other key (`name`, `lib`, `description`, `homepage`, `documentation`, `repository-directory`, `publish`) is package-specific. A member that re-declares a forced field is an error (`version is inherited from [workspace.package]; remove it here`) — see the [Lockstep Contract](#lockstep-contract).

`license` and `license-file` are one slot: a member setting either replaces it whole (still mutually exclusive within a manifest), and an inherited `license-file` resolves relative to the root. A field a workspace does not want to share is simply omitted, and each member sets its own.

`[workspace.dependencies]` and `[workspace.dev-dependencies]` declare shared dependency versions. Member packages reference them with explicit opt-in (unlike metadata):

```toml
# In a workspace member's wado.toml
[dependencies]
"std:json" = { workspace = true }   # inherits from [workspace.dependencies]

[dev-dependencies]
"lib:bench" = { workspace = true }  # inherits from [workspace.dev-dependencies]
```

A workspace root `wado.toml` can have both `[workspace]` and `[package]` — the root itself is both a workspace and a package (like Cargo). The root's own `[package]` is the authority, not a governed member: it does not force-inherit from `[workspace.package]` and declares its own `version`/`repository`/`namespace` directly.

Properties:

- All member packages share one `wado.lock` at the workspace root
- `wado` commands run from any member directory discover the workspace root automatically
- Each member has its own `[package]` with an independent `name` and entry points; `version`/`repository`/`namespace` are inherited from `[workspace.package]`
- Members can depend on each other via `path` dependencies

### Lockstep Contract

Workspace metadata inheritance and root-only publishing together guarantee that a workspace's packages cannot drift to mismatched public versions, enforced in two places:

- Repository: `version`, `repository`, and `namespace` are declared once in `[workspace.package]` and cannot be overridden, so there is one definition site and drift is impossible by construction, not merely linted.
- Registry: `wado publish` runs only from the workspace root and publishes every member together at that shared version (see [Publishing](#publishing)), so members can't be pushed piecemeal at different versions.

Membership is the boundary: a package is either inside the workspace (bound) or a standalone project (free). There is deliberately no per-field opt-out to diverge while staying in — and none is planned.

This is the departure from Cargo, whose `[workspace.package]` is opt-in per field (`version.workspace = true`): a crate that omits it silently keeps its own version, so same-repo skew (familiar from `wasmtime` / `wasm-tools`) is routine. Wado makes sharing the only in-workspace option for identity fields, so that drift cannot arise.

### Single-File Mode

When no `wado.toml` exists, the compiler operates in single-file mode:

- `core:*`, `wasi:*`, `./`, `../`, and `https://` imports work as always.
- A dependency specifier (`ns:pkg` or `lib:nick`) must carry an inline
  `with { ... }` supplying its source — the same vocabulary as a
  `[dependencies]` value, with an exact `version` (no lock to resolve a
  range). See [Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md).
- A dependency specifier without `with` produces a clear error:
  `dependency "lib:foo" needs a source (add a with-clause or a wado.toml)`.
- Inline `with` and a `[dependencies]` entry for the same specifier are mutually
  exclusive (single-file uses `with`; a manifest project uses the table).

```wado
use { Regexp } from "docs:regex@1.0.0";   // exact pin, default registry
use { Router } from "lib:router" with { git = "https://github.com/user/router.git", ref = "v1.0" };
```

### Path Dependencies to Single Files

`path` dependencies can point to a single `.wado` file (not just directories). The referenced file is implicitly treated as `lib = <that file>`:

```toml
"lib:shared" = { path = "../shared.wado" }    # treated as lib = "shared.wado"
"lib:utils"  = { path = "../utils" }          # reads ../utils/wado.toml for entry points
```

Which items a dependency exposes follows from whether its source is available, not from how it was fetched:

| Dependency type                      | Path for a Wado consumer | Visible items       |
| ------------------------------------ | ------------------------ | ------------------- |
| Registry                             | Source (from the artifact's section) | `pub` + `export` |
| Registry, published `--no-source`    | CM boundary              | `export` items only |
| Git                                  | Source                   | `pub` + `export`    |
| Path to directory (with `wado.toml`) | Source                   | `pub` + `export`    |
| Path to single `.wado` file          | Source                   | `pub` + `export` (implicit `lib`) |

A non-Wado CM consumer sees `export` items only in every row.

### CLI Integration

See [WEP: CLI Subcommands for Package Management](./wep-2026-02-22-cli-subcommands.md).

### Publishing

`wado publish` builds each publishable world's component, embeds the
`[package]` metadata, and delegates the OCI upload to `wkg` (see [Metadata
embedding and the publish backend](#metadata-embedding-and-the-publish-backend)
and [Publishable Worlds and OCI Layout](#publishable-worlds-and-oci-layout)).
The following validations apply:

- `namespace` and `name` must be present
- `version` must be present and valid semver
- `publish` must not be `false`
- `description`, `repository`, and `authors` must be present
- exactly one of `license` or `license-file` must be present
- every shipped dependency must carry a concrete source: a `path` dependency needs an accompanying registry/git source, and a `workspace = true` dependency must resolve to one (the workspace context is gone once the package is extracted)

The descriptive fields are required because a published package must carry its metadata. The rest stay optional: `homepage`/`documentation` default to `repository`, `repository-directory` is monorepo-only, and `wado-version` is a build constraint.

`wado publish --dry-run` runs these checks and reports every problem at once
(it does not build or upload). A bare `wado publish` builds each publishable
world's component (metadata embedded), resolves the OCI reference from
`[registries].default` — the only supported publish destination; a missing or
non-`oci://` default is an error — and uploads each via `wkg oci push`. A
package with no publishable world has nothing to publish and is rejected.

#### Publishable Worlds and OCI Layout

A package can target several worlds (`[package].lib` plus the `[world]` table),
and each is a distinct behavior — a library, a Kiln generator, a CLI tool — so
each is published as its own OCI artifact. A world is not selectable inside a
component (it is the component's whole import/export signature) and one OCI tag
holds one component, so multi-world packages cannot collapse into a single
artifact.

The world is encoded as an OCI repository **path segment**, keeping the
`namespace:name` coordinate clean (the `use` / `module:` reference never carries
a world) and leaving the version as the sole image tag:

| World                     | OCI repository                        |
| ------------------------- | ------------------------------------- |
| `[package].lib` (library) | `<host>/<prefix>/<ns>/<name>`         |
| any `[world]` entry       | `<host>/<prefix>/<ns>/<name>/<world>` |

`<world>` is the world FQ sanitized to one path segment (`wasi:cli/command` →
`wasi-cli-command`, `core:kiln/generator` → `core-kiln-generator`). The library
world stays at the bare repository so the common library case matches the
[Registry backend](#registry-backend) mapping; other worlds get a sub-path. The
resolver derives the same path from the world it needs — a `use` import resolves
the bare repository, a `module:` generator reference appends
`core-kiln-generator` — so push and pull share one `world → repository` rule.

Example: `wado-lang:gale@0.1.0` (a CLI tool and a Kiln generator) publishes two
artifacts, `…/wado-lang/gale/wasi-cli-command:0.1.0` and
`…/wado-lang/gale/core-kiln-generator:0.1.0`; `wado-lang:cm-catalog@0.1.0` (a
library) publishes one at `…/wado-lang/cm-catalog:0.1.0`.

By default every declared world is published. A single world opts out with
`publish = false` on its `[world]` entry, which then takes the table form:

```toml
[world]
"wasi:cli/command" = "src/main.wado"                                 # published
"core:kiln/generator" = "src/generator.wado"                         # published
"wasi:http/service" = { entry = "src/server.wado", publish = false } # not published
```

`[package].publish = false` still opts the whole package out, overriding any
per-world setting.

In a workspace, `wado publish` is gated to the workspace root: it publishes every
publishable member (and the root's own `[package]`, if any) together at the
shared version, all against the root's `[registries]` (a member's own
`[registries]` is not consulted, since publish is a root-only operation).
Members that are not publishable (`publish = false` or no `namespace`) are
skipped, and any member with unmet requirements aborts the whole publish.
Running `wado publish` from a member directory is an error pointing at the root.
This is the registry half of the [Lockstep Contract](#lockstep-contract).

#### Path Dependency Replacement

`path` dependencies that also specify a registry or git source are automatically replaced with the non-path source when publishing, similar to Cargo:

```toml
[dependencies]
# During development: resolved via path (fast, local edits)
# When published: resolved via registry (self-contained)
"lib:shared" = { path = "../shared", package = "myorg:shared", version = "^0.1.0" }
```

Path dependencies without a registry or git fallback are errors:

```
error: cannot publish with path-only dependency
  → "lib:utils" = { path = "../utils" }
  help: add registry or git source: "lib:utils" = { path = "../utils", package = "myorg:utils", version = "^0.1.0" }
```

This enables seamless local development while ensuring published packages are self-contained.

## Consequences

### Positive

- Single `.wado` files continue to work without a package manifest
- Git dependencies work with any hosting provider (not GitHub-specific)
- Git deps support both semver (`version`) and exact pinning (`ref`) — XOR ensures clarity
- `dev-dependencies` keep test-only code out of production builds, tracked in lock file with `dev = true`
- Registry aliases avoid URL repetition and enable easy migration
- A dependency key is byte-identical to its `from "..."` specifier — no key-level indirection between manifest and source
- Reserved namespace ⇔ bundled namespace (`wasi`, `core`); every other coordinate namespace is open, with `lib:` as the single home for indirection (see [Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md))
- PubGrub provides best-in-class error messages for resolution failures
- Cyclic dependencies are detected early with clear error messages
- Multiple semver-incompatible versions coexist naturally, matching Wasm Component Model's type isolation
- Lock file is self-sufficient — contains full dependency graph and entry points, eliminating per-dependency `wado.toml` reads during builds
- Lock file entries identified by `id@version` (resolved package id) — globally unique and decoupled from dependency chains
- Lock file with `integrity` ensures reproducible and tamper-evident builds for registry deps
- Auto re-resolve keeps lock file fresh; `--locked` ensures CI reproducibility
- Compiler remains agnostic to dependency resolution — `CompilerHost` handles all mapping
- The `[world]` table and `[package].lib` map directly to CM worlds; `wado run` / `wado serve` select a hosted world by its FQ name
- `export` as CM boundary gives clear, consistent public API semantics across all consumption modes
- Wado-to-Wado optimization eliminates CM overhead for same-language dependencies without changing semantics
- `namespace` absence naturally indicates non-publishable packages; a namespaced package can still opt out explicitly with `publish = false`
- Path deps with dual source (`path` + `registry`) enable seamless dev-to-publish workflow
- Workspace support enables multi-package development with shared lock files and dependency declarations
- Name/namespace validation (`[a-zA-Z0-9_-]+` per segment) keeps dependency keys (`"ns:pkg"`, `"lib:nick"`) unambiguous as specifiers
- Human-facing metadata lives in `[package]` with `wado.toml` as the single source of truth, mapped to standard OCI annotations on publish (no `wkg.toml`); `license` is validated as an SPDX expression
- `[workspace.package]` removes metadata duplication across members; force-inherited `version`/`repository`/`namespace` make in-repo drift impossible, and root-only `wado publish` extends that guarantee to the registry
- Unknown manifest keys warn instead of erroring, so typos are caught without breaking builds

### Negative

- Adding `wado.toml` introduces project-level concepts to a language that started as single-file
- PubGrub is NP-hard worst case (acceptable in practice — pathological cases are rare in real ecosystems)
- Multiple coexisting versions increase binary size (mitigated by Wasm's tree-shaking-friendly module system)
- Lock file merge conflicts are a known pain point (mitigated by deterministic ordering and simple TOML structure)

### Trade-offs

- PubGrub over MVS: PubGrub selects the highest compatible version (users get security patches automatically) at the cost of needing a lock file for reproducibility. MVS would give O(n) resolution and reproducibility without a lock file, but users would be stuck on old versions unless every library author proactively bumps minimums. For an ecosystem that values security and freshness, PubGrub is the better default.
- `version` XOR `ref` for git: `version` enables semver resolution on tags (like Go/Swift PM), `ref` pins to an exact ref. XOR ensures the intent is always unambiguous — no implicit defaults.
- Bare version = error: more verbose than Cargo's implicit caret, but eliminates a source of confusion ("does `1.0.0` mean exact or `^1.0.0`?"). Every `version` field is self-documenting.
- Registry names per-project: avoids global configuration but requires repetition across projects. The `default` registry mitigates this for the common case. A future `~/.wado/config.toml` could provide user-level defaults.
- Dependency specifier resolution: an open coordinate or `lib:` nickname requires a `wado.toml` lookup at compile time, adding a project-discovery step. The compiler itself is not affected — only `CompilerHost` implementations need to handle this.
- Self-sufficient lock file: duplicates entry points and dependency edges from each package's `wado.toml`. This makes the lock file larger and introduces a potential staleness risk (if a dependency's `wado.toml` changes entry points without version bump). The trade-off is worth it — builds skip all transitive manifest I/O, and staleness is caught by `wado update` or integrity mismatch.
- Archive-level integrity (not source-level): simpler and unambiguous, but means the hash depends on the registry's archive format. If a registry changes its packaging format, hashes change even if sources are identical.
- Lockstep over per-field opt-in (see [Lockstep Contract](#lockstep-contract)): `[workspace.package]` inherits automatically (no `{ workspace = true }` marker) and forced fields can't be overridden, plus root-only publishing — trading per-member independence for one definition site and structurally drift-free releases. Independence means leaving the workspace, not an in-workspace opt-out.
- `[world]` table keyed by FQ world name: hosted worlds are declared by their Component Model world name (`"wasi:cli/command"`) rather than a short alias (`command`/`bin`/`cli`). The key is the world the entry conforms to, so new worlds need no new manifest field and the mapping to the CM world is explicit. The library world is the one exception — it has no externally-fixed FQ name, so it is named after the package and declared by `[package].lib`.
- `[package]` over `[project]`: `[package]` aligns with CM's "package" concept (`package ns:name@version` in WIT). The file itself represents the project; `[package]` describes the distributable unit within it. `[workspace]` > `[package]` hierarchy is natural, whereas `[workspace]` > `[project]` would be confusing.
- `path` + `registry` dual source: adds complexity to the dependency spec but eliminates the "path deps can't be published" problem. The alternative (Cargo's separate `[patch]` section) is more complex and harder to maintain.

### Not Included

- URL dependencies (`url = "..."`): Not included in this WEP. Remote module imports via `use ... from "https://..."` remain a source-level feature (not a `wado.toml` dependency). A `url` dependency source type may be added in a future WEP if a compelling use case emerges that cannot be served by git or registry dependencies.
