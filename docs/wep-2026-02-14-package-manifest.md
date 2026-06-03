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
command = "src/main.wado"
lib = "src/lib.wado"

[registries]
default = "https://wa.dev"

[dependencies]
router = { git = "https://github.com/user/router.git", version = "^1.0.0" }
regex = { package = "docs:regex", version = "^0.1.0" }
shared = { path = "../shared" }

[dev-dependencies]
bench = { git = "https://gitlab.com/user/bench.git", ref = "main" }
```

### `[package]`

| Field       | Type     | Required | Description                                      |
| ----------- | -------- | -------- | ------------------------------------------------ |
| `namespace` | `string` | No       | Organization or user namespace (e.g., `"myorg"`) |
| `name`      | `string` | Yes      | Package name (e.g., `"my-app"`)                  |
| `version`   | `string` | Yes      | Semver version (e.g., `"0.1.0"`)                 |
| `command`   | `string` | No       | Entry point for `wasi:cli/command` world         |
| `service`   | `string` | No       | Entry point for `wasi:http/service` world        |
| `lib`       | `string` | No       | Library interface file                           |

`namespace` and `name` together form the registry identity (`namespace:name`, e.g., `myorg:my-app`). Without `namespace`, the package cannot be published to a registry — this is the natural state for closed-source applications and internal tools.

#### Name and Namespace Validation

Both `namespace` and `name` must match `[a-zA-Z0-9_-]+` (minimum 1 character, maximum 64 characters).

Dependency keys in `[dependencies]` follow the same rules. This ensures they are valid TOML bare keys and unambiguous in import paths.

At least one of `command`, `service`, or `lib` should be specified. A package can have multiple entry points (e.g., both `command` and `lib`).

### Entry Points and Worlds

Each entry point field corresponds to a hosted world or a library world:

| Field     | Category      | WASI World          | CLI Command  | Required Export                           |
| --------- | ------------- | ------------------- | ------------ | ----------------------------------------- |
| `command` | hosted world  | `wasi:cli/command`  | `wado run`   | `export fn run()`                         |
| `service` | hosted world  | `wasi:http/service` | `wado serve` | `export fn handle(request: Request) -> …` |
| `lib`     | library world | (none — interface)  | (none)       | `export` items become the public API      |

```toml
# CLI tool with library
[package]
name = "markdown"
command = "src/cli.wado"
lib = "src/lib.wado"

# HTTP service only
[package]
name = "my-api"
service = "src/server.wado"

# Development tool with CLI + Web UI
[package]
name = "devtool"
command = "src/cli.wado"
service = "src/dashboard.wado"
lib = "src/lib.wado"

# Library only
[package]
name = "json"
lib = "src/lib.wado"
```

### Visibility and Component Boundary

The `export` keyword defines what is visible at the **Component Model boundary** — the package's public API. This is distinct from `pub`, which is project-internal visibility.

| Modifier | Scope            | Use                                           |
| -------- | ---------------- | --------------------------------------------- |
| (none)   | Module-private   | Implementation details                        |
| `pub`    | Package-internal | Shared across modules within the same project |
| `export` | CM boundary      | Package's public API, visible to consumers    |

```wado
// src/lib.wado (in the "markdown" package)
fn tokenize(input: String) -> List<Token> { ... }       // private
pub fn build_ast(tokens: List<Token>) -> Document { ... } // project-internal
export fn parse(input: String) -> Document { ... }         // public API
```

When another project depends on `"markdown"`, only `export` items from the `lib` entry point are visible:

```wado
// In a consuming project
use { parse } from "markdown";        // OK: exported from lib
// use { build_ast } from "markdown";  // ERROR: pub but not exported
// use { tokenize } from "markdown";   // ERROR: private
```

When published as a `.wasm` component (e.g., to wa.dev), only `export` items appear in the component's interface.

### Wado-to-Wado Optimization

Semantically, cross-package references always go through the CM boundary (`export` items only). However, when both producer and consumer are Wado, the compiler can skip the CM canonical ABI (lifting/lowering) and share Wasm GC types directly.

| Consumer → Producer                               | Path                                         |
| ------------------------------------------------- | -------------------------------------------- |
| Wado → Wado (source dependency)                   | CM binding skipped; GC types shared directly |
| Wado → Wado (`.wasm` with Wado provider metadata) | Provider detected; GC types shared directly  |
| Wado → arbitrary `.wasm`                          | CM canonical ABI (lifting/lowering)          |
| Arbitrary → Wado `.wasm`                          | CM canonical ABI                             |

This optimization is transparent to the developer. The visible API is always determined by `export`, and the semantics are always CM boundary semantics. The optimization only affects performance — cross-package calls between Wado projects have no overhead compared to project-internal calls.

### `[registries]`

Named registry aliases. Keys are short names; values are registry URLs. The special key `default` sets the default registry — dependencies with `package` but no `registry` use it automatically.

```toml
[registries]
default = "https://wa.dev"
custom = "https://registry.example.com"
```

```toml
[dependencies]
regex = { package = "docs:regex", version = "^0.1.0" }              # uses default registry
special = { registry = "custom", package = "ns:lib", version = "^1.0.0" }  # uses named registry
```

A dependency with `package` and `version` but no `registry` requires `default` to be set. If `default` is not defined and `registry` is omitted, it is an error.

### `[dependencies]` and `[dev-dependencies]`

Each key is the **import name** used in Wado source code. Values are inline tables specifying the dependency source.

`[dev-dependencies]` are only available during `wado test` and are excluded from production builds.

### Dependency Source Types

Each dependency must have exactly one primary source type (`git`, `registry`, or `path`). The exception is `path`, which can be combined with `registry` or `git` for publishing (see Publishing).

#### Git

```toml
# Semver on git tags
router = { git = "https://github.com/user/router.git", version = "^1.0.0" }

# Exact git ref (tag, branch, or SHA)
router = { git = "https://github.com/user/router.git", ref = "v1.0.0" }
router = { git = "https://github.com/user/router.git", ref = "main" }
```

| Field     | Required | Description                                   |
| --------- | -------- | --------------------------------------------- |
| `git`     | Yes      | Full git URL (any host: GitHub, GitLab, etc.) |
| `version` | XOR      | Semver range on git tags (e.g., `"^1.0.0"`)   |
| `ref`     | XOR      | Exact git ref (tag, branch, or commit SHA)    |

Exactly one of `version` or `ref` must be specified. `version` resolves against semver-tagged releases in the repository. `ref` pins to an exact git ref — use explicit branch names (e.g., `"main"`) rather than implicit defaults.

#### Registry

```toml
regex = { package = "docs:regex", version = "^0.1.0" }                      # uses default registry
special = { registry = "custom", package = "ns:lib", version = "^1.0.0" }   # uses named registry
```

| Field      | Required | Description                                                        |
| ---------- | -------- | ------------------------------------------------------------------ |
| `registry` | No       | Registry alias (defined in `[registries]`). Defaults to `default`. |
| `package`  | Yes      | Package identity in `namespace:name` format                        |
| `version`  | Yes      | Semver version range (e.g., `"^0.1.0"`)                            |

#### Local Path

```toml
shared = { path = "../shared" }
```

| Field  | Required | Description                                  |
| ------ | -------- | -------------------------------------------- |
| `path` | Yes      | Relative path to a directory or `.wado` file |

Local path dependencies are resolved relative to the `wado.toml` location. They are not locked (always resolved fresh). During development, only the `path` is used — any accompanying `registry` or `git` source is ignored entirely.

For publishing, `path` can be combined with a registry or git source. When publishing (`wado publish`), the `path` is stripped and the accompanying source is used in the published package manifest:

```toml
shared = { path = "../shared", package = "myorg:shared", version = "^0.1.0" }
shared = { path = "../shared", git = "https://github.com/org/shared.git", version = "^0.1.0" }
```

### Module Resolution with Dependencies

The existing module resolution (WEP-2026-01-24) is extended with one new rule: **bare name** resolution.

A bare name is an import path that does not contain `:` and does not start with `./`, `../`, or `http(s)://`.

Resolution order:

1. `"scheme:path"` (contains `:`) — scheme-based (`core:`, `wasi:`, etc.)
2. `"./path"` or `"../path"` — relative file
3. `"https://url"` — remote URL (source-level feature, not a `wado.toml` dependency)
4. **bare name** — look up key in `[dependencies]` (or `[dev-dependencies]` during test)

```wado
use { println } from "core:cli";           // scheme → built-in
use { Request } from "wasi:http";           // scheme → built-in
use { helper } from "./utils.wado";         // relative → local file
use { Router } from "router";              // bare → wado.toml dependency
```

Dependency keys must not contain `:` (TOML bare keys naturally enforce this). This makes scheme-based and bare name resolution structurally unambiguous.

`core` and `wasi` are **not reserved**. `"core:cli"` (with `:`) resolves via scheme; `"core"` (bare) resolves via `wado.toml`. These are different resolution paths.

#### `ModuleSource` Extension

```rust
pub enum ModuleSource {
    Core { name: String },
    Wasi { interface: String },
    Local { path: String },
    Remote { url: String },
    EntryPoint { filename: Option<String> },
    // New:
    Dependency { id: String },  // resolved package id (e.g., "registry+https://wa.dev/docs:regex@0.1.2")
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

Within a single `wado.toml`, a user can also explicitly depend on multiple major versions by using different import names:

```toml
[dependencies]
http-v1 = { package = "std:http", version = "^1.0.0" }
http-v2 = { package = "std:http", version = "^2.0.0" }
```

#### Transitive Version Isolation

The resolver runs on the **full dependency graph** and produces a flat resolution map. Each resolved package is identified by `(package identity, compatibility range)`:

```
package identity = registry URL + namespace:name  (for registry deps)
                 = git URL                         (for git deps)

resolution key   = (package identity, major version)
                   e.g., (wa/std:http, 1) and (wa/std:http, 2)
```

When two transitive dependencies require semver-incompatible versions of the same package, they each get their own resolved instance. The compiler does not need to know about this — it simply receives module sources from `CompilerHost`. The resolver (in the CLI) handles mapping.

The existing `resolve_import(from_module_source, import_source)` signature already provides the necessary context. The `from_module_source` tells the `CompilerHost` _which package is doing the importing_, so the same bare name `"foo"` resolves to different packages depending on the caller:

```
resolve_import(from=EntryPoint, "foo")
  → CompilerHost looks up my-app's wado.toml → "myns:foo" version 2.0.0
  → returns ModuleSource::Dependency { id: "registry+https://wa.dev/myns:foo@2.0.0" }

resolve_import(from=Dependency{id="registry+https://wa.dev/user:router@1.0.0"}, "foo")
  → CompilerHost looks up router's wado.toml → "myns:foo" version 1.0.0
  → returns ModuleSource::Dependency { id: "registry+https://wa.dev/myns:foo@1.0.0" }
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
command = "src/main.wado"
deps = []

[[package]]
id = "registry+https://wa.dev/docs:regex"
version = "0.1.2"
integrity = "sha256:a1b2c3d4e5f6..."
lib = "src/lib.wado"
deps = ["registry+https://wa.dev/docs:regex-utils@0.3.0"]

[[package]]
id = "registry+https://wa.dev/docs:regex-utils"
version = "0.3.0"
integrity = "sha256:f6e5d4c3b2a1..."
lib = "src/lib.wado"
deps = []

[[package]]
id = "registry+https://wa.dev/std:json"
version = "1.2.0"
integrity = "sha256:c3d4e5f6a1b2..."
lib = "src/lib.wado"
deps = []

[[package]]
id = "registry+https://wa.dev/tools:utils"
version = "0.5.1"
integrity = "sha256:b2c3d4e5f6a1..."
lib = "src/lib.wado"
deps = []

[[package]]
id = "git+https://github.com/user/router.git/user:router"
version = "1.0.2"
resolved-ref = "abc1234def5678901234567890abcdef12345678"
lib = "src/lib.wado"
deps = ["registry+https://wa.dev/tools:utils@0.5.1", "registry+https://wa.dev/std:json@1.2.0"]
```

Each `[[package]]` entry is uniquely identified by `(id, version)`. The `id` field is the resolved package id — the source prefix combined with the package identity (e.g., `registry+https://wa.dev/docs:regex` or `git+https://github.com/user/router.git/user:router`). The `deps` array references other entries using `id@version` format.

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
| `command`      | optional      | Entry point for `wasi:cli/command` (from dependency's `wado.toml`)                               |
| `service`      | optional      | Entry point for `wasi:http/service` (from dependency's `wado.toml`)                              |
| `lib`          | optional      | Library entry point (from dependency's `wado.toml`)                                              |
| `deps`         | all           | List of `id@version` strings referencing other entries                                           |

Entry point fields (`command`, `service`, `lib`) are copied from the dependency's `wado.toml` at resolution time. This makes the lock file self-sufficient — the `CompilerHost` can resolve all imports and locate all source files using only the root `wado.toml` and `wado.lock`.

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
json = { package = "std:json", version = "^1.0.0" }

[workspace.dev-dependencies]
bench = { git = "https://gitlab.com/user/bench.git", version = "^0.1.0" }
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
command = "src/main.wado"

[dependencies]
core = { path = "../core" }
```

| Field     | Type       | Required | Description                                  |
| --------- | ---------- | -------- | -------------------------------------------- |
| `members` | `string[]` | Yes      | Glob patterns for member package directories |

`[workspace.dependencies]` and `[workspace.dev-dependencies]` declare shared dependency versions. Member packages reference them without repeating version information:

```toml
# In a workspace member's wado.toml
[dependencies]
json = { workspace = true }  # inherits from [workspace.dependencies]

[dev-dependencies]
bench = { workspace = true }  # inherits from [workspace.dev-dependencies]
```

A workspace root `wado.toml` can have both `[workspace]` and `[package]` — the root itself is both a workspace and a package (like Cargo).

Properties:

- All member packages share one `wado.lock` at the workspace root
- `wado` commands run from any member directory discover the workspace root automatically
- Each member has its own `[package]` with independent `name`, `version`, and entry points
- Members can depend on each other via `path` dependencies

### Single-File Mode

When no `wado.toml` exists, the compiler operates in single-file mode:

- Only `core:*`, `wasi:*`, `./`, `../`, and `https://` imports are available
- Bare name imports produce a clear error: `unknown module "foo" (no wado.toml found)`
- No behavioral change from current behavior

### Path Dependencies to Single Files

`path` dependencies can point to a single `.wado` file (not just directories). The referenced file is implicitly treated as `lib = <that file>` — only `export` items are visible at the CM boundary:

```toml
shared = { path = "../shared.wado" }    # treated as lib = "shared.wado"
utils  = { path = "../utils" }          # reads ../utils/wado.toml for entry points
```

| Dependency type                      | Boundary    | Visible items                        |
| ------------------------------------ | ----------- | ------------------------------------ |
| Registry / Git (with `wado.toml`)    | CM boundary | `export` items only                  |
| Path to directory (with `wado.toml`) | CM boundary | `export` items only                  |
| Path to single `.wado` file          | CM boundary | `export` items only (implicit `lib`) |

### CLI Integration

See [WEP: CLI Subcommands for Package Management](./wep-2026-02-22-cli-subcommands.md).

### Publishing

When publishing a package to a registry (`wado publish`), the following validations apply:

- `namespace` and `name` must be present
- `version` must be present and valid semver
- All `path` dependencies must have accompanying registry or git source information

#### Path Dependency Replacement

`path` dependencies that also specify a registry or git source are automatically replaced with the non-path source when publishing, similar to Cargo:

```toml
[dependencies]
# During development: resolved via path (fast, local edits)
# When published: resolved via registry (self-contained)
shared = { path = "../shared", package = "myorg:shared", version = "^0.1.0" }
```

Path dependencies without a registry or git fallback are errors:

```
error: cannot publish with path-only dependency
  → utils = { path = "../utils" }
  help: add registry or git source: utils = { path = "../utils", package = "myorg:utils", version = "^0.1.0" }
```

This enables seamless local development while ensuring published packages are self-contained.

## Consequences

### Positive

- Single `.wado` files continue to work without a package manifest
- Git dependencies work with any hosting provider (not GitHub-specific)
- Git deps support both semver (`version`) and exact pinning (`ref`) — XOR ensures clarity
- `dev-dependencies` keep test-only code out of production builds, tracked in lock file with `dev = true`
- Registry aliases avoid URL repetition and enable easy migration
- Bare name imports are short and ergonomic (`"router"` not `"dep:router"`)
- No reserved namespaces — `core:` and `wasi:` are resolved by scheme syntax, not by name reservation
- PubGrub provides best-in-class error messages for resolution failures
- Cyclic dependencies are detected early with clear error messages
- Multiple semver-incompatible versions coexist naturally, matching Wasm Component Model's type isolation
- Lock file is self-sufficient — contains full dependency graph and entry points, eliminating per-dependency `wado.toml` reads during builds
- Lock file entries identified by `id@version` (resolved package id) — globally unique and decoupled from dependency chains
- Lock file with `integrity` ensures reproducible and tamper-evident builds for registry deps
- Auto re-resolve keeps lock file fresh; `--locked` ensures CI reproducibility
- Compiler remains agnostic to dependency resolution — `CompilerHost` handles all mapping
- Entry point fields (`command`, `service`, `lib`) map directly to WASI worlds and CLI commands
- `export` as CM boundary gives clear, consistent public API semantics across all consumption modes
- Wado-to-Wado optimization eliminates CM overhead for same-language dependencies without changing semantics
- `namespace` absence naturally indicates non-publishable packages — no extra `publish = false` flag needed
- Path deps with dual source (`path` + `registry`) enable seamless dev-to-publish workflow
- Workspace support enables multi-package development with shared lock files and dependency declarations
- Name/namespace validation (`[a-zA-Z0-9_-]+`) ensures valid TOML bare keys and unambiguous import paths

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
- **Bare name resolution**: requires `wado.toml` lookup at compile time, adding a project-discovery step. The compiler itself is not affected — only `CompilerHost` implementations need to handle this.
- **Self-sufficient lock file**: duplicates entry points and dependency edges from each package's `wado.toml`. This makes the lock file larger and introduces a potential staleness risk (if a dependency's `wado.toml` changes entry points without version bump). The trade-off is worth it — builds skip all transitive manifest I/O, and staleness is caught by `wado update` or integrity mismatch.
- **Archive-level integrity** (not source-level): simpler and unambiguous, but means the hash depends on the registry's archive format. If a registry changes its packaging format, hashes change even if sources are identical.
- **`command` over `bin`/`cli`**: `command` matches the WASI world name (`wasi:cli/command`) directly, making the mapping explicit. `bin` (Cargo's term) describes artifact format, which is less meaningful in the Wasm world. `cli` describes the interface but doesn't match the world name.
- **`[package]` over `[project]`**: `[package]` aligns with CM's "package" concept (`package ns:name@version` in WIT). The file itself represents the project; `[package]` describes the distributable unit within it. `[workspace]` > `[package]` hierarchy is natural, whereas `[workspace]` > `[project]` would be confusing.
- **`path` + `registry` dual source**: adds complexity to the dependency spec but eliminates the "path deps can't be published" problem. The alternative (Cargo's separate `[patch]` section) is more complex and harder to maintain.

### Not Included

- **URL dependencies (`url = "..."`)**: Not included in this WEP. Remote module imports via `use ... from "https://..."` remain a source-level feature (not a `wado.toml` dependency). A `url` dependency source type may be added in a future WEP if a compelling use case emerges that cannot be served by git or registry dependencies.
