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

The human-facing fields (`description`, `homepage`, `repository`, `documentation`, `license`, `authors`) are backend-agnostic package metadata; they live in `[package]` rather than a registry-flavored section, and map to OCI annotations only as a serialization detail. See [Package Metadata and Publishing](#package-metadata-and-publishing).

#### Name and Namespace Validation

Both `namespace` and `name` must match `[a-zA-Z0-9_-]+` (minimum 1 character, maximum 64 characters).

Dependency keys in `[dependencies]` are quoted specifiers — an open coordinate `"ns:pkg"` or a `"lib:nick"` nickname — each segment matching the same `[a-zA-Z0-9_-]+` rule. The `lib:`-or-coordinate form makes a real registry identity and a local indirection distinguishable on sight.

A package must declare at least one world: a `[world]` entry, `[package].lib`, or both.

### Package Metadata and Publishing

The human-facing `[package]` fields are universal package metadata that happen
to map to OCI annotations. The registry backend is OCI (see [Registry backend](#registry-backend)), so on publish each field is serialized to a
standard `org.opencontainers.image.*` annotation:

| `[package]` field      | OCI annotation                                       | Notes                                                |
| ---------------------- | ---------------------------------------------------- | ---------------------------------------------------- |
| `description`          | `org.opencontainers.image.description`               | Short human-readable summary                         |
| `homepage`             | `org.opencontainers.image.url`                       |                                                      |
| `repository`           | `org.opencontainers.image.source`                    | Bare repo URL — enables registry → repo auto-linking |
| `documentation`        | `org.opencontainers.image.documentation`             |                                                      |
| `license`              | `org.opencontainers.image.licenses`                  | SPDX License Expression                              |
| `authors`              | `org.opencontainers.image.authors`                   | Array, serialized comma-separated                    |
| `version`              | `org.opencontainers.image.version`                   | Set by the registry tool at publish time             |
| (git commit SHA)       | `org.opencontainers.image.revision`                  | Auto-derived at build time                           |
| `repository-directory` | — (Wado-custom)                                      | No OCI key exists; embedded in the component only    |
| `license-file`         | `org.opencontainers.image.licenses` = `LicenseRef-…` | License text embedded as a custom section            |

`created` is not modeled: no embeddable field exists for it, and the registry
tool owns publish-time timestamps. `keywords`/`categories` are omitted — OCI has
no standard key for them, so they would not reach an OCI registry.

#### License

`license` (an SPDX expression such as `"MIT OR Apache-2.0"`) is the primary
form. For a standard license the SPDX identifier is the canonical reference, so
no file is shipped. A non-standard or proprietary license uses `license-file`
instead: the annotation becomes `LicenseRef-<name>` (SPDX's syntax for custom
licenses) and the file's text is embedded in the component. `license` and
`license-file` are mutually exclusive; publishing requires one of them.

#### Repository subdirectory (monorepo)

Neither OCI nor git has a standard way to address a subdirectory within a
repository. `repository` therefore stays a bare repo URL (so the registry can
link the artifact back to its repository); a package's location inside a
monorepo is recorded in `repository-directory`. This value is not emitted as an
OCI annotation — it is embedded as Wado-custom metadata and preserved in the
component for Wado tooling.

The same need on the consuming side — depending on a package that lives in a
subdirectory of a git repository — is served by the git dependency's
`directory` field (see [Git](#git)).

#### Metadata embedding and the publish backend

Metadata is embedded into the compiled component using the `wasm-metadata`
custom-section format that the registry tooling reads. `wado publish` shells out
to `wkg` (wasm-pkg-tools), which derives the OCI annotations from the embedded
metadata. There is no `wkg.toml`: `wado.toml` is the single source of truth, and
users interact only with `wado publish` — `wkg` is an implementation detail. The
only requirement is that `wkg` is installed; a missing `wkg` produces an error
with install guidance and exits.

Registry authentication is delegated to the ambient OCI credential store
(`docker login`, read by `wkg`), with an environment-variable token override for
CI. Wado stores no credentials of its own.

`revision` (the git commit SHA) is derived at build time. When the working tree
is dirty, the revision is omitted (with a warning) rather than recording an
unreproducible state.

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

When published as a `.wasm` component (e.g., to an OCI registry), only `export` items appear in the component's CM interface; `pub`-only items reach Wado consumers via the provider-metadata path below.

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

Registry resolution and publishing use **OCI** (the OCI Distribution Spec): a component is an OCI artifact in a container registry (e.g. `ghcr.io`), and the content digest provides integrity. A registry URL takes the form `oci://<host>/<prefix>`; an open coordinate `ns:pkg` resolves to the repository `<host>/<prefix>/<ns>/<pkg>`, with the version as an image tag.

The earlier **warg** protocol is dropped. Its registry (`bytecodealliance/registry`) is archived and the ecosystem (`wasm-pkg-tools`) defaults to OCI. A warg-only registry such as wa.dev is reachable only through the external `wkg` tool, not natively; Wado neither implements nor wraps warg. Publishing is likewise done with `wkg`, not a Wado subcommand.

### `[dependencies]` and `[dev-dependencies]`

Each key is the **specifier** used in Wado source code, byte-for-byte (`"docs:regex"`, `"lib:shared"`). Values are inline tables specifying the dependency source. See [Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md) for the key forms and resolution rules.

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

`directory` addresses the subdirectory through an explicit field rather than
encoding it into the URL — git has no URL syntax for subdirectories, and the
ecosystem conventions that bolt one on (`//subdir`, `#subdirectory=`, `?path=`)
are not interoperable. The inline table already has room for a dedicated key, so
the path is unambiguous and host-independent.

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

- an **open coordinate** `ns:pkg` (`ns` ∉ {`wasi`, `core`}), or
- a **`lib:` nickname**.

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

Wado uses the **PubGrub** algorithm for dependency resolution. PubGrub is a conflict-driven nogood learning (CDCL) solver, originally designed for Dart's `pub` and adopted by `uv` (Python), Swift Package Manager, and others.

Why PubGrub over alternatives:

| Approach                 | Pros                                                     | Cons                                              |
| ------------------------ | -------------------------------------------------------- | ------------------------------------------------- |
| Go MVS (minimum version) | O(n), deterministic without lock file                    | Users get old/buggy versions; no upper bounds     |
| Cargo-style backtracking | Proven at scale                                          | Weaker conflict learning; less informative errors |
| PubGrub (CDCL)           | Best error messages; efficient pruning; state of the art | NP-hard worst case (acceptable in practice)       |

PubGrub provides:

- **Conflict-driven learning**: when a conflict is found, the solver derives a precise incompatibility that explains _why_ this combination fails and never re-explores it
- **Human-readable error messages**: each resolution failure comes with a derivation chain (e.g., "because A requires utils ^1.0 and B requires utils ^2.0, and your project requires both A and B, version solving failed")
- **Efficient pruning**: near-polynomial performance in practice despite NP-hard worst case

The Rust crate `pubgrub` provides a ready-made implementation.

#### Version Specifiers

The `version` field requires an explicit range operator — bare versions are errors.

| Prefix | Meaning            | Example  | Range             |
| ------ | ------------------ | -------- | ----------------- |
| `^`    | Caret (compatible) | `^1.2.3` | `>=1.2.3, <2.0.0` |
| `^`    | Caret (pre-1.0)    | `^0.2.3` | `>=0.2.3, <0.3.0` |
| `~`    | Tilde (patch-only) | `~1.2.3` | `>=1.2.3, <1.3.0` |
| `=`    | Exact              | `=1.2.3` | `=1.2.3`          |
| (none) | **Error**          | `1.2.3`  | compile error     |

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

Two requirements are **semver-compatible** if they share the same compatibility range (same major version for `>=1.0.0`, same major.minor for `0.x`). Within a compatibility range, the resolver selects **exactly one version** — the highest that satisfies all constraints.

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

The resolver runs on the **full dependency graph** and produces a flat resolution map. Each resolved package is identified by `(package identity, compatibility range)`:

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

- **Compatible versions**: unified to one resolved instance (highest compatible). PubGrub finds this automatically.
- **Incompatible versions**: coexist as separate instances. Each dependent sees its own version. Types do not cross boundaries.
- **Unsatisfiable**: if constraints within a compatibility range conflict (e.g., `=1.2.0` and `=1.3.0`), PubGrub reports a precise error with derivation chain.

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
wado compile -o out.wasm               # auto re-resolves if wado.toml changed
wado compile --locked -o out.wasm      # ERROR if lock file is stale
```

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

For registry packages, the hash input is the **downloaded archive bytes** (not individual source files concatenated). This matches how registries distribute packages and avoids ambiguity about file ordering or line endings.

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

[workspace.dependencies]
"std:json" = { version = "^1.0.0" }

[workspace.dev-dependencies]
"lib:bench" = { git = "https://gitlab.com/user/bench.git", version = "^0.1.0" }
```

```toml
# packages/core/wado.toml
[package]
namespace = "myorg"
name = "core"
version = "0.1.0"
lib = "src/lib.wado"
```

```toml
# packages/cli/wado.toml
[package]
name = "my-tool"
version = "0.1.0"

[world]
"wasi:cli/command" = "src/main.wado"

[dependencies]
"myorg:core" = { path = "../core" }
```

| Field     | Type       | Required | Description                                  |
| --------- | ---------- | -------- | -------------------------------------------- |
| `members` | `string[]` | Yes      | Glob patterns for member package directories |

`[workspace.dependencies]` and `[workspace.dev-dependencies]` declare shared dependency versions. Member packages reference them without repeating version information:

```toml
# In a workspace member's wado.toml
[dependencies]
"std:json" = { workspace = true }   # inherits from [workspace.dependencies]

[dev-dependencies]
"lib:bench" = { workspace = true }  # inherits from [workspace.dev-dependencies]
```

A workspace root `wado.toml` can have both `[workspace]` and `[package]` — the root itself is both a workspace and a package (like Cargo).

Properties:

- All member packages share one `wado.lock` at the workspace root
- `wado` commands run from any member directory discover the workspace root automatically
- Each member has its own `[package]` with independent `name`, `version`, and entry points
- Members can depend on each other via `path` dependencies

### Single-File Mode

When no `wado.toml` exists, the compiler operates in single-file mode:

- `core:*`, `wasi:*`, `./`, `../`, and `https://` imports work as always.
- A dependency specifier (`ns:pkg` or `lib:nick`) must carry an inline
  `with { ... }` supplying its source — the same vocabulary as a
  `[dependencies]` value, with an **exact** `version` (no lock to resolve a
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

`path` dependencies can point to a single `.wado` file (not just directories). The referenced file is implicitly treated as `lib = <that file>` — only `export` items are visible at the CM boundary:

```toml
"lib:shared" = { path = "../shared.wado" }    # treated as lib = "shared.wado"
"lib:utils"  = { path = "../utils" }          # reads ../utils/wado.toml for entry points
```

| Dependency type                      | Boundary    | Visible items                        |
| ------------------------------------ | ----------- | ------------------------------------ |
| Registry / Git (with `wado.toml`)    | CM boundary | `export` items only                  |
| Path to directory (with `wado.toml`) | CM boundary | `export` items only                  |
| Path to single `.wado` file          | CM boundary | `export` items only (implicit `lib`) |

### CLI Integration

See [WEP: CLI Subcommands for Package Management](./wep-2026-02-22-cli-subcommands.md).

### Publishing

`wado publish` builds the component, embeds the `[package]` metadata, and
delegates the OCI upload to `wkg` (see [Metadata embedding and the publish
backend](#metadata-embedding-and-the-publish-backend)). The following
validations apply:

- `namespace` and `name` must be present
- `version` must be present and valid semver
- `publish` must not be `false`
- `description`, `repository`, and `authors` must be present
- exactly one of `license` or `license-file` must be present
- All `path` dependencies must have accompanying registry or git source information

A published package must carry its descriptive metadata, so the non-exclusive
fields above are required. The exceptions: `homepage` and `documentation` are
redundant with `repository` and default to it when omitted; `repository-directory`
is meaningful only for a monorepo; and `wado-version` is a build constraint, not
descriptive metadata. These four stay optional even when publishing.

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

### Negative

- Adding `wado.toml` introduces project-level concepts to a language that started as single-file
- PubGrub is NP-hard worst case (acceptable in practice — pathological cases are rare in real ecosystems)
- Multiple coexisting versions increase binary size (mitigated by Wasm's tree-shaking-friendly module system)
- Lock file merge conflicts are a known pain point (mitigated by deterministic ordering and simple TOML structure)

### Trade-offs

- **PubGrub over MVS**: PubGrub selects the highest compatible version (users get security patches automatically) at the cost of needing a lock file for reproducibility. MVS would give O(n) resolution and reproducibility without a lock file, but users would be stuck on old versions unless every library author proactively bumps minimums. For an ecosystem that values security and freshness, PubGrub is the better default.
- **`version` XOR `ref` for git**: `version` enables semver resolution on tags (like Go/Swift PM), `ref` pins to an exact ref. XOR ensures the intent is always unambiguous — no implicit defaults.
- **Bare version = error**: more verbose than Cargo's implicit caret, but eliminates a source of confusion ("does `1.0.0` mean exact or `^1.0.0`?"). Every `version` field is self-documenting.
- **Registry names per-project**: avoids global configuration but requires repetition across projects. The `default` registry mitigates this for the common case. A future `~/.wado/config.toml` could provide user-level defaults.
- **Dependency specifier resolution**: an open coordinate or `lib:` nickname requires a `wado.toml` lookup at compile time, adding a project-discovery step. The compiler itself is not affected — only `CompilerHost` implementations need to handle this.
- **Self-sufficient lock file**: duplicates entry points and dependency edges from each package's `wado.toml`. This makes the lock file larger and introduces a potential staleness risk (if a dependency's `wado.toml` changes entry points without version bump). The trade-off is worth it — builds skip all transitive manifest I/O, and staleness is caught by `wado update` or integrity mismatch.
- **Archive-level integrity** (not source-level): simpler and unambiguous, but means the hash depends on the registry's archive format. If a registry changes its packaging format, hashes change even if sources are identical.
- **`[world]` table keyed by FQ world name**: hosted worlds are declared by their Component Model world name (`"wasi:cli/command"`) rather than a short alias (`command`/`bin`/`cli`). The key is the world the entry conforms to, so new worlds need no new manifest field and the mapping to the CM world is explicit. The library world is the one exception — it has no externally-fixed FQ name, so it is named after the package and declared by `[package].lib`.
- **`[package]` over `[project]`**: `[package]` aligns with CM's "package" concept (`package ns:name@version` in WIT). The file itself represents the project; `[package]` describes the distributable unit within it. `[workspace]` > `[package]` hierarchy is natural, whereas `[workspace]` > `[project]` would be confusing.
- **`path` + `registry` dual source**: adds complexity to the dependency spec but eliminates the "path deps can't be published" problem. The alternative (Cargo's separate `[patch]` section) is more complex and harder to maintain.

### Not Included

- **URL dependencies (`url = "..."`)**: Not included in this WEP. Remote module imports via `use ... from "https://..."` remain a source-level feature (not a `wado.toml` dependency). A `url` dependency source type may be added in a future WEP if a compelling use case emerges that cannot be served by git or registry dependencies.
