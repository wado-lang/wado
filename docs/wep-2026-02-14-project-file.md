# WEP: Project File (`wado.toml`)

## Context

Wado currently supports single-file execution (`wado run file.wado`) and local module imports (`./`, `../`). As the ecosystem grows, projects need:

- Package metadata (name, version, namespace) for publishing to registries
- External dependency management (git, registry, local path, remote URL)
- Separation of production and development dependencies
- Reproducible builds via lock files
- Transitive dependency resolution

The design must preserve Wado's simplicity: a single `.wado` file without `wado.toml` must continue to work.

## Decision

### Project File Format

The project manifest is `wado.toml`, placed at the project root.

```toml
[package]
namespace = "myorg"
name = "my-app"
version = "0.1.0"
command = "src/main.wado"
lib = "src/lib.wado"

[registries]
wa = "https://wa.dev"

[dependencies]
router = { git = "https://github.com/user/router.git", ref = "v1.0.0" }
regex = { registry = "wa", package = "docs:regex", version = "0.1.0" }
shared = { path = "../shared" }
logger = { url = "https://example.com/logger-0.2.0/wado.toml" }

[dev-dependencies]
bench = { git = "https://gitlab.com/user/bench.git", ref = "main" }
```

### `[package]`

| Field       | Type     | Required | Description                                        |
| ----------- | -------- | -------- | -------------------------------------------------- |
| `namespace` | `string` | Yes      | Organization or user namespace (e.g., `"myorg"`)   |
| `name`      | `string` | Yes      | Package name (e.g., `"my-app"`)                    |
| `version`   | `string` | Yes      | Semver version (e.g., `"0.1.0"`)                   |
| `command`   | `string` | No       | Entry point for `wasi:cli/command` world            |
| `service`   | `string` | No       | Entry point for `wasi:http/service` world           |
| `lib`       | `string` | No       | Library interface file                              |

`namespace` and `name` are used for registry publishing. The registry identity is `namespace:name` (e.g., `myorg:my-app`).

At least one of `command`, `service`, or `lib` should be specified. A package can have multiple entry points (e.g., both `command` and `lib`).

### Entry Points and Worlds

Each entry point field corresponds to a WASI world or a library interface:

| Field     | WASI World           | CLI Command    | Required Export                           |
| --------- | -------------------- | -------------- | ----------------------------------------- |
| `command` | `wasi:cli/command`   | `wado run`     | `export fn run()`                         |
| `service` | `wasi:http/service`  | `wado serve`   | `export fn handle(request: Request) -> …` |
| `lib`     | (none — interface)   | (none)         | `export` items become the public API      |

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

| Modifier | Scope | Use |
| -------- | ----- | --- |
| (none)   | Module-private | Implementation details |
| `pub`    | Project-internal | Shared across modules within the same project |
| `export` | CM boundary | Package's public API, visible to consumers |

```wado
// src/lib.wado (in the "markdown" package)
fn tokenize(input: String) -> Array<Token> { ... }       // private
pub fn build_ast(tokens: Array<Token>) -> Document { ... } // project-internal
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

| Consumer → Producer | Path |
| ------------------- | ---- |
| Wado → Wado (source dependency) | CM adapter skipped; GC types shared directly |
| Wado → Wado (`.wasm` with Wado provider metadata) | Provider detected; GC types shared directly |
| Wado → arbitrary `.wasm` | CM canonical ABI (lifting/lowering) |
| Arbitrary → Wado `.wasm` | CM canonical ABI |

This optimization is transparent to the developer. The visible API is always determined by `export`, and the semantics are always CM boundary semantics. The optimization only affects performance — cross-package calls between Wado projects have no overhead compared to project-internal calls.

### `[registries]`

Named registry aliases. Keys are short names; values are registry URLs.

```toml
[registries]
wa = "https://wa.dev"
custom = "https://registry.example.com"
```

These names are referenced by the `registry` field in dependency entries.

### `[dependencies]` and `[dev-dependencies]`

Each key is the **import name** used in Wado source code. Values are inline tables specifying the dependency source.

`[dev-dependencies]` are only available during `wado test` and are excluded from production builds.

### Dependency Source Types

Exactly one source type must be specified per dependency.

#### Git

```toml
router = { git = "https://github.com/user/router.git", ref = "v1.0.0" }
```

| Field | Required | Description                                   |
| ----- | -------- | --------------------------------------------- |
| `git` | Yes      | Full git URL (any host: GitHub, GitLab, etc.) |
| `ref` | Yes      | Git ref (tag, branch, or commit SHA)          |

`ref` is always required. Use explicit branch names (e.g., `"main"`) rather than implicit defaults.

#### Registry

```toml
regex = { registry = "wa", package = "docs:regex", version = "0.1.0" }
```

| Field      | Required | Description                                    |
| ---------- | -------- | ---------------------------------------------- |
| `registry` | Yes      | Registry alias (defined in `[registries]`)     |
| `package`  | Yes      | Package identity in `namespace:name` format    |
| `version`  | Yes      | Semver version requirement (e.g., `"0.1.0"`)  |

#### Local Path

```toml
shared = { path = "../shared" }
```

| Field  | Required | Description                                |
| ------ | -------- | ------------------------------------------ |
| `path` | Yes      | Relative path to a directory or `.wado` file |

Local path dependencies are not published. They are resolved relative to the `wado.toml` location.

#### Remote URL

```toml
logger = { url = "https://example.com/logger-0.2.0/wado.toml" }
```

| Field | Required | Description                            |
| ----- | -------- | -------------------------------------- |
| `url` | Yes      | URL to a `wado.toml` or `.wado` file   |

### Module Resolution with Dependencies

The existing module resolution (WEP-2026-01-24) is extended with one new rule: **bare name** resolution.

A bare name is an import path that does not contain `:`, does not start with `./`, `../`, or `http(s)://`.

Resolution order:

1. `"scheme:path"` (contains `:`) — scheme-based (`core:`, `wasi:`, etc.)
2. `"./path"` or `"../path"` — relative file
3. `"https://url"` — remote URL
4. **bare name** — look up key in `[dependencies]` (or `[dev-dependencies]` during test)

```wado
use { println } from "core:cli";           // scheme → built-in
use { Request } from "wasi:http";           // scheme → built-in
use { helper } from "./utils.wado";         // relative → local file
use { Router } from "router";              // bare → wado.toml dependency
use { Middleware } from "router/middleware"; // bare → sub-module of dep
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
    Dependency { key: String, subpath: Option<String> },
}
```

### Transitive Dependency Resolution

When a dependency itself has a `wado.toml` with dependencies, those are transitive dependencies.

#### Resolution Algorithm: PubGrub

Wado uses the **PubGrub** algorithm for dependency resolution. PubGrub is a conflict-driven nogood learning (CDCL) solver, originally designed for Dart's `pub` and adopted by `uv` (Python), Swift Package Manager, and others.

Why PubGrub over alternatives:

| Approach | Pros | Cons |
| -------- | ---- | ---- |
| Go MVS (minimum version) | O(n), deterministic without lock file | Users get old/buggy versions; no upper bounds |
| Cargo-style backtracking | Proven at scale | Weaker conflict learning; less informative errors |
| PubGrub (CDCL) | Best error messages; efficient pruning; state of the art | NP-hard worst case (acceptable in practice) |

PubGrub provides:

- **Conflict-driven learning**: when a conflict is found, the solver derives a precise incompatibility that explains *why* this combination fails and never re-explores it
- **Human-readable error messages**: each resolution failure comes with a derivation chain (e.g., "because A requires utils ^1.0 and B requires utils ^2.0, and your project requires both A and B, version solving failed")
- **Efficient pruning**: near-polynomial performance in practice despite NP-hard worst case

The Rust crate `pubgrub` provides a ready-made implementation.

#### Semver Compatibility and Version Ranges

Version requirements use caret syntax (`^`), following the same semantics as Cargo:

| Requirement | Range |
| ----------- | ----- |
| `^1.2.3` | `>=1.2.3, <2.0.0` |
| `^0.2.3` | `>=0.2.3, <0.3.0` |
| `^0.0.3` | `>=0.0.3, <0.0.4` |

When `version` is specified without `^`, it is treated as `^version` (caret is the default).

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
http-v1 = { registry = "wa", package = "std:http", version = "1.0.0" }
http-v2 = { registry = "wa", package = "std:http", version = "2.0.0" }
```

#### Transitive Version Isolation

The resolver runs on the **full dependency graph** and produces a flat resolution map. Each resolved package is identified by `(package identity, compatibility range)`:

```
package identity = registry URL + namespace:name  (for registry deps)
                 = git URL + repo                  (for git deps)

resolution key   = (package identity, major version)
                   e.g., (wa/std:http, 1) and (wa/std:http, 2)
```

When two transitive dependencies require semver-incompatible versions of the same package, they each get their own resolved instance. The compiler does not need to know about this — it simply receives module sources from `CompilerHost`. The resolver (in the CLI) handles mapping.

The existing `resolve_import(from_module_source, import_source)` signature already provides the necessary context. The `from_module_source` tells the `CompilerHost` *which package is doing the importing*, so the same bare name `"foo"` resolves to different packages depending on the caller:

```
resolve_import(from=EntryPoint, "foo")
  → CompilerHost looks up my-app's wado.toml → foo@2.0.0
  → returns ModuleSource::Dependency { key: "foo", ... }

resolve_import(from=Dependency{key="router"}, "foo")
  → CompilerHost looks up router's wado.toml → foo@1.0.0
  → returns ModuleSource::Dependency { key: "router>foo", ... }
```

The compiler sees distinct `ModuleSource::Dependency` values (different `key`) and compiles each independently. No changes to the compiler are needed — the `CompilerHost` implementation in the CLI handles all version-aware routing. Type isolation is natural — two separately compiled modules never share types.

#### Diamond Dependency Handling

When two dependencies require the same transitive dependency:

- **Compatible versions**: unified to one resolved instance (highest compatible). PubGrub finds this automatically.
- **Incompatible versions**: coexist as separate instances. Each dependent sees its own version. Types do not cross boundaries.
- **Unsatisfiable**: if constraints within a compatibility range conflict (e.g., `=1.2.0` and `=1.3.0`), PubGrub reports a precise error with derivation chain.

### Lock File (`wado.lock`)

The lock file captures exact resolved versions for reproducible builds.

```toml
# This file is auto-generated by wado. Do not edit manually.
version = 1

[[package]]
key = "router"
source = "git"
git = "https://github.com/user/router.git"
ref = "v1.0.0"
resolved-ref = "abc1234def5678901234567890abcdef12345678"

[[package]]
key = "regex"
source = "registry"
registry = "https://wa.dev"
package = "docs:regex"
version = "0.1.2"
integrity = "sha256:a1b2c3d4e5f6..."

[[package]]
key = "regex>utils"
source = "registry"
registry = "https://wa.dev"
package = "docs:regex-utils"
version = "0.3.0"
integrity = "sha256:f6e5d4c3b2a1..."
```

| Field          | Description                                            |
| -------------- | ------------------------------------------------------ |
| `key`          | Import key. `>` separates transitive dependency chains (e.g., `regex>utils` means utils as required by regex) |
| `source`       | Source type: `git`, `registry`, `url`                  |
| `resolved-ref` | For git: exact commit SHA (40 hex chars)               |
| `version`      | For registry: exact resolved version                   |
| `integrity`    | Content hash with algorithm prefix (see below)         |

Properties:

- Deterministic: entries sorted by `key` lexicographically, fields in declaration order
- Human-readable TOML
- Committed to version control
- `path` dependencies are not locked (always resolved fresh, not listed)

### Integrity Verification

The `integrity` field uses a prefixed format: `algorithm:hex-encoded-hash`.

```
integrity = "sha256:a1b2c3d4e5f6..."
```

The prefix makes the format extensible — if SHA-256 is ever deprecated, a new algorithm can be introduced without changing the lock file schema.

#### Calculation Method

| Source | Integrity |
| ------ | --------- |
| Registry | Hash of the archive as downloaded from the registry. The registry defines the canonical archive format. |
| Git | `resolved-ref` (commit SHA) serves as integrity. Git's content-addressable storage already guarantees integrity. No separate `integrity` field needed. |
| Remote URL | Hash of the downloaded content (the `.wado` file or `wado.toml` + referenced sources). |
| Local path | No integrity check. Always resolved fresh. |

For registry packages, the hash input is the **downloaded archive bytes** (not individual source files concatenated). This matches how registries distribute packages and avoids ambiguity about file ordering or line endings.

The initial algorithm is SHA-256. The resolver verifies integrity on every install: if the computed hash does not match `integrity`, the install fails with an error.

### Single-File Mode

When no `wado.toml` exists, the compiler operates in single-file mode:

- Only `core:*`, `wasi:*`, `./`, `../`, and `https://` imports are available
- Bare name imports produce a clear error: `unknown module "foo" (no wado.toml found)`
- No behavioral change from current behavior

### CLI Integration

#### Project Commands

```sh
wado init                          # create wado.toml interactively
wado add router --git https://github.com/user/router.git --ref v1.0.0
wado add regex --registry wa --package docs:regex --version 0.1.0
wado remove router
wado update                        # update wado.lock
wado update regex                  # update specific dependency
```

These are future CLI commands. The initial implementation focuses on `wado.toml` parsing and module resolution.

#### Entry Point and CLI Commands

When `wado.toml` is present, the existing CLI commands use the entry point fields:

```sh
# Without wado.toml (single-file mode, unchanged)
wado run file.wado
wado serve file.wado

# With wado.toml (entry point auto-discovered)
wado run                           # uses [package].command
wado serve                         # uses [package].service
wado compile -o out.wasm           # compiles the command entry point
wado compile --lib -o out.wasm     # compiles the lib entry point
```

When a file argument is provided, it overrides the entry point from `wado.toml`.

#### `wado exec` for Dependency Entry Points

```sh
wado exec <dep-name>               # run dependency's command entry point
wado exec <dep-name> [args...]     # pass arguments to the dependency
```

`wado exec` looks up `<dep-name>` in `[dependencies]`, finds the dependency's `wado.toml`, and runs its `command` entry point. This enables tool packages (formatters, linters, generators) to be installed as dependencies and executed directly.

## Consequences

### Positive

- Single `.wado` files continue to work without any project file
- Git dependencies work with any hosting provider (not GitHub-specific)
- `ref` is always explicit — no implicit branch resolution
- `dev-dependencies` keep test-only code out of production builds
- Registry aliases avoid URL repetition and enable easy migration
- Bare name imports are short and ergonomic (`"router"` not `"dep:router"`)
- No reserved namespaces — `core:` and `wasi:` are resolved by scheme syntax, not by name reservation
- PubGrub provides best-in-class error messages for resolution failures
- Multiple semver-incompatible versions coexist naturally, matching Wasm Component Model's type isolation
- Lock file with `integrity` ensures reproducible and tamper-evident builds
- Compiler remains agnostic to dependency resolution — `CompilerHost` handles all mapping
- Entry point fields (`command`, `service`, `lib`) map directly to WASI worlds and CLI commands
- `export` as CM boundary gives clear, consistent public API semantics across all consumption modes
- Wado-to-Wado optimization eliminates CM overhead for same-language dependencies without changing semantics

### Negative

- Adding `wado.toml` introduces project-level concepts to a language that started as single-file
- PubGrub is NP-hard worst case (acceptable in practice — pathological cases are rare in real ecosystems)
- Multiple coexisting versions increase binary size (mitigated by Wasm's tree-shaking-friendly module system)
- Lock file merge conflicts are a known pain point (mitigated by deterministic ordering and simple TOML structure)

### Trade-offs

- **PubGrub over MVS**: PubGrub selects the highest compatible version (users get security patches automatically) at the cost of needing a lock file for reproducibility. MVS would give O(n) resolution and reproducibility without a lock file, but users would be stuck on old versions unless every library author proactively bumps minimums. For an ecosystem that values security and freshness, PubGrub is the better default.
- **`ref` required for git**: more verbose but prevents silent breakage when upstream branches change.
- **Registry names per-project**: avoids global configuration but requires repetition across projects. A future `~/.wado/config.toml` could provide user-level defaults.
- **Bare name resolution**: requires `wado.toml` lookup at compile time, adding a project-discovery step. The compiler itself is not affected — only `CompilerHost` implementations need to handle this.
- **Archive-level integrity** (not source-level): simpler and unambiguous, but means the hash depends on the registry's archive format. If a registry changes its packaging format, hashes change even if sources are identical.
- **`command` over `bin`/`cli`**: `command` matches the WASI world name (`wasi:cli/command`) directly, making the mapping explicit. `bin` (Cargo's term) describes artifact format, which is less meaningful in the Wasm world. `cli` describes the interface but doesn't match the world name.
