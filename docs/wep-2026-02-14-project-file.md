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
[project]
namespace = "myorg"
name = "my-app"
version = "0.1.0"
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

### `[project]`

| Field       | Type     | Required | Description                                        |
| ----------- | -------- | -------- | -------------------------------------------------- |
| `namespace` | `string` | Yes      | Organization or user namespace (e.g., `"myorg"`)   |
| `name`      | `string` | Yes      | Package name (e.g., `"my-app"`)                    |
| `version`   | `string` | Yes      | Semver version (e.g., `"0.1.0"`)                   |

`namespace` and `name` are used for registry publishing. The registry identity is `namespace:name` (e.g., `myorg:my-app`).

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

#### Resolution Strategy

Wado uses **maximal version unification** within semver-compatible ranges:

- For a given package (identified by `registry + package` or `git + repo`), all semver-compatible version requirements are unified to the highest version satisfying all constraints.
- Semver-incompatible versions (different major version, or different `0.x` minor version) are treated as **separate modules** and can coexist.

Example:

```
my-app
├── router 1.2.0 (depends on utils ^1.0)
└── auth 0.5.0 (depends on utils ^1.1)

Resolved: utils 1.1.x (satisfies both ^1.0 and ^1.1)
```

Example with multiple major versions:

```
my-app
├── legacy-lib (depends on http 1.x)
└── new-lib (depends on http 2.x)

Resolved: http 1.x AND http 2.x coexist as separate modules
```

Each major version is a distinct `ModuleSource::Dependency` with a distinct import path. The dependent libraries each see their own version.

#### Diamond Dependency Handling

When two dependencies require the same transitive dependency:

- **Compatible versions**: unified to one resolved version (highest compatible)
- **Incompatible versions**: both coexist; each dependent sees its own version
- **Conflict**: if constraints within the same major version are unsatisfiable (e.g., `=1.2.0` vs `=1.3.0`), the resolver emits an error

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
resolved-ref = "abc1234def5678..."

[[package]]
key = "regex"
source = "registry"
registry = "https://wa.dev"
package = "docs:regex"
version = "0.1.2"
checksum = "sha256:..."

[[package]]
key = "regex/utils"
source = "registry"
registry = "https://wa.dev"
package = "docs:regex-utils"
version = "0.3.0"
checksum = "sha256:..."
dependents = ["regex"]
```

| Field          | Description                                            |
| -------------- | ------------------------------------------------------ |
| `key`          | Import key (top-level or `parent/transitive`)          |
| `source`       | Source type: `git`, `registry`, `path`, `url`          |
| `resolved-ref` | For git: exact commit SHA                              |
| `version`      | For registry: exact resolved version                   |
| `checksum`     | Integrity hash of the package contents                 |
| `dependents`   | Which packages depend on this (for transitive deps)    |

Properties:

- Deterministic: entries sorted by key, fields in consistent order
- Human-readable TOML
- Committed to version control
- `path` dependencies are not locked (always resolved fresh)

### Single-File Mode

When no `wado.toml` exists, the compiler operates in single-file mode:

- Only `core:*`, `wasi:*`, `./`, `../`, and `https://` imports are available
- Bare name imports produce a clear error: `unknown module "foo" (no wado.toml found)`
- No behavioral change from current behavior

### CLI Integration

```sh
wado init                          # create wado.toml interactively
wado add router --git https://github.com/user/router.git --ref v1.0.0
wado add regex --registry wa --package docs:regex --version 0.1.0
wado remove router
wado update                        # update wado.lock
wado update regex                  # update specific dependency
```

These are future CLI commands. The initial implementation focuses on `wado.toml` parsing and module resolution.

## Consequences

### Positive

- Single `.wado` files continue to work without any project file
- Git dependencies work with any hosting provider (not GitHub-specific)
- `ref` is always explicit — no implicit branch resolution
- `dev-dependencies` keep test-only code out of production builds
- Registry aliases avoid URL repetition and enable easy migration
- Bare name imports are short and ergonomic (`"router"` not `"dep:router"`)
- No reserved namespaces — `core:` and `wasi:` are resolved by scheme syntax, not by name reservation
- Multiple major versions can coexist, matching Wasm Component Model's type isolation
- Lock file ensures reproducible builds

### Negative

- Adding `wado.toml` introduces project-level concepts to a language that started as single-file
- Transitive dependency resolution adds complexity to the compiler/CLI
- Lock file merge conflicts are a known pain point (mitigated by deterministic ordering)

### Trade-offs

- `ref` is required for git dependencies (no implicit `main`/`HEAD`). This is more verbose but prevents silent breakage when upstream branches change.
- Registry names are per-project, not global. Each project must declare its registries. This avoids global configuration but requires repetition across projects.
- Bare name resolution requires `wado.toml` lookup at compile time. The compiler must locate and parse the project file during module resolution.
