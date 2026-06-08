# WEP: CLI Subcommands for Package Management

## Context

[WEP: Package Manifest (`wado.toml`)](./wep-2026-02-14-package-manifest.md) defines the manifest format, dependency resolution, and lock file design. This WEP covers the CLI commands that operate on `wado.toml` and `wado.lock`.

Cargo's dependency update story has evolved piecemeal — `cargo update` only touches `Cargo.lock`, updating `Cargo.toml` requires third-party tools (`cargo-edit`'s `cargo upgrade`), and breaking updates are nightly-only (`cargo update --breaking`). Wado can design a unified command set from the start.

The key insight: **libraries should keep loose version specs** (giving consumers flexibility), while **applications should tighten specs** to match tested versions. The CLI should support both workflows naturally.

## Decision

### Command Overview

```sh
wado init                          # create wado.toml interactively
wado add <name> [options]          # add dependency
wado remove <name>                 # remove dependency
wado update [name]                 # update lock file (within version specs)
wado update --pin [name]           # update lock file + tighten toml specs
wado update --breaking [name]      # update across major versions
wado fetch                         # download dependencies without building
wado list [filter]                 # list cached packages
wado exec <dep-name> [args...]     # run dependency's command entry point
```

### `wado init`

Create a new `wado.toml` interactively.

```sh
wado init                          # interactive prompts
wado init --name my-app            # non-interactive with defaults
```

### `wado add`

Add a dependency to `wado.toml` and update `wado.lock`.

```sh
# Registry dependency
wado add regex --package docs:regex --version "^0.1.0"

# Git dependency (semver on tags)
wado add router --git https://github.com/user/router.git --version "^1.0.0"

# Git dependency (exact ref)
wado add router --git https://github.com/user/router.git --ref main

# Local path dependency
wado add shared --path ../shared

# Dev dependency
wado add bench --dev --git https://gitlab.com/user/bench.git --version "^0.1.0"
```

| Flag                  | Description                          |
| --------------------- | ------------------------------------ |
| `--package <ns:name>` | Registry package identity            |
| `--version <spec>`    | Version specifier (e.g., `"^1.0.0"`) |
| `--registry <name>`   | Registry alias (default: `default`)  |
| `--git <url>`         | Git repository URL                   |
| `--ref <ref>`         | Exact git ref (tag, branch, or SHA)  |
| `--path <path>`       | Local path                           |
| `--dev`               | Add to `[dev-dependencies]` instead  |

The `<name>` argument becomes the dependency key (the import name in Wado source). `--version` is required for registry and git-with-semver dependencies. `--version` and `--ref` are mutually exclusive for git dependencies.

### `wado remove`

Remove a dependency from `wado.toml` and update `wado.lock`.

```sh
wado remove router                 # remove from [dependencies]
wado remove bench --dev            # remove from [dev-dependencies]
```

If the dependency exists in both `[dependencies]` and `[dev-dependencies]`, `--dev` disambiguates. Without `--dev`, the command removes from `[dependencies]`.

### `wado update`

Update dependencies. Three modes cover the library-vs-application spectrum:

#### Default: Lock File Only

```sh
wado update                        # update all deps in wado.lock
wado update regex                  # update specific dependency
```

Resolves the latest versions **within existing `wado.toml` version specs** and writes `wado.lock`. Does not modify `wado.toml`. This is the default for library authors who want loose specs.

```
# wado.toml (unchanged)
regex = { package = "docs:regex", version = "^1.0.0" }

# wado.lock: regex 1.8.0 → 1.10.2
```

#### `--pin`: Lock + Tighten Specs

```sh
wado update --pin                  # update all, tighten toml
wado update --pin regex            # update specific, tighten toml
```

Updates `wado.lock` like the default, then **writes the resolved version back to `wado.toml`**, preserving the version operator. This is for application developers who want their `wado.toml` to reflect the minimum version they've actually tested against.

```
# Before
regex = { package = "docs:regex", version = "^1.0.0" }
# wado.lock: regex 1.8.0

# After `wado update --pin`
regex = { package = "docs:regex", version = "^1.10.2" }
# wado.lock: regex 1.10.2
```

The operator (`^`, `~`, `=`) is preserved. Only the base version is updated to match the resolved version. For `=` (exact pin), the version is simply updated to the new exact version. For git dependencies with `version`, the same logic applies. For git dependencies with `ref`, `--pin` is a no-op (already pinned by definition).

#### `--breaking`: Cross Major Versions

```sh
wado update --breaking             # update all, including major bumps
wado update --breaking regex       # update specific
```

Resolves the **latest version regardless of current specs**, updates both `wado.lock` and `wado.toml`. The operator is preserved but the base version jumps to the latest.

```
# Before
regex = { package = "docs:regex", version = "^1.0.0" }

# After `wado update --breaking`
regex = { package = "docs:regex", version = "^2.0.0" }
# wado.lock: regex 2.0.0
```

This may introduce breaking API changes. The compiler will catch incompatibilities at build time.

#### Summary

| Mode                     | `wado.lock`            | `wado.toml`                   | Use case                   |
| ------------------------ | ---------------------- | ----------------------------- | -------------------------- |
| `wado update`            | updated (within specs) | unchanged                     | Library: keep specs loose  |
| `wado update --pin`      | updated (within specs) | version bumped, operator kept | App: track tested versions |
| `wado update --breaking` | updated (any version)  | version bumped, operator kept | Major upgrade              |

### `wado fetch`

Download all dependencies to the local cache without building.

```sh
wado fetch                         # download all dependencies
wado fetch --target wasm32         # target-specific (future)
```

Intended for CI and Dockerfile layer caching:

```dockerfile
COPY wado.toml wado.lock ./
RUN wado fetch                     # cached layer: dependencies only

COPY src/ ./src/
RUN wado compile -o app.wasm       # rebuilds only when source changes
```

If `wado.lock` exists, `wado fetch` downloads the exact versions recorded in the lock file. If `wado.lock` does not exist, it runs resolution first (generates `wado.lock`), then downloads.

### Dependency Cache Layout

Dependencies are stored in a structured directory tree under `~/wado/`, mirroring the source URL hierarchy (inspired by [ghq](https://github.com/x-motemen/ghq)). Not hidden — the cache is a first-class part of the filesystem, just like `~/ghq/`. This makes cached packages browsable with standard tools.

```
~/wado/
├── wa.dev/                          # registry host
│   ├── docs/regex/
│   │   └── 0.1.2/
│   │       ├── wado.toml
│   │       └── src/
│   └── std/json/
│       └── 1.2.0/
├── github.com/                      # git host
│   └── user/router/
│       └── 1.0.2-abc1234d/          # version + short commit
│           ├── wado.toml
│           └── src/
└── gitlab.com/
    └── user/bench/
        └── 0.1.0-def56789/
```

#### Path Convention

| Source   | Path pattern                                       | Example                                  |
| -------- | -------------------------------------------------- | ---------------------------------------- |
| Registry | `{registry-host}/{namespace}/{name}/{version}/`    | `wa.dev/docs/regex/0.1.2/`               |
| Git      | `{git-host}/{owner}/{repo}/{version}-{short-ref}/` | `github.com/user/router/1.0.2-abc1234d/` |

Git dependencies include a short commit prefix (8 hex chars) in the directory name to distinguish different commits that resolve to the same version tag. Registry dependencies use the exact resolved version.

#### Cache Root

The default cache root is `~/wado/`. This can be overridden via the `WADO_ROOT` environment variable:

```sh
WADO_ROOT=/tmp/wado-cache wado fetch    # custom cache location
```

### `wado list`

List packages in the local dependency cache.

```sh
wado list                            # all cached packages
wado list regex                      # filter by name substring
wado list --path                     # show full filesystem paths
```

#### Default Output

```
$ wado list
docs/regex                0.1.2       wa.dev
std/json                  1.2.0       wa.dev
user/router               1.0.2       github.com
user/bench                0.1.0       gitlab.com
```

Columns: package identity, version, source host. Sorted by source host then package identity.

#### Path Output

```
$ wado list --path
/home/user/wado/wa.dev/docs/regex/0.1.2
/home/user/wado/wa.dev/std/json/1.2.0
/home/user/wado/github.com/user/router/1.0.2-abc1234d
/home/user/wado/gitlab.com/user/bench/0.1.0-def56789
```

One absolute path per line. Designed for piping into other tools:

```sh
# Open a cached package source in your editor
code $(wado list --path | fzf)

# Find all wado.toml files in cache
wado list --path | xargs -I{} ls {}/wado.toml
```

`wado list` scans the cache directory — it does not require a `wado.toml` or project context. It reports what is physically present on disk, regardless of whether any project currently depends on it.

### Entry Point and CLI Commands

When `wado.toml` is present, the existing CLI commands use the entry point fields:

```sh
# Without wado.toml (single-file mode, unchanged)
wado run file.wado
wado serve file.wado

# With wado.toml (entry point auto-discovered)
wado run                           # uses [package].command
wado serve                         # uses [package].service
wado compile -o out.wasm           # compiles the command entry point
```

When a file argument is provided, it overrides the entry point from `wado.toml`.

#### `--lib` — pending

`wado compile --lib` (compile the `[package].lib` entry as a library) is
abolished pending a world model that fits libraries. A library has no command
entry point, so it does not map onto `wasi:cli/command`; the previous
implementation compiled the lib into that world and stubbed the absent `run`,
which never surfaced the library's `export` API as component exports. The
`[package].lib` manifest field and `EntryPointKind::Lib` resolution are retained
as the data model; the CLI flag and its compile path return once a proper
library/component-export world is designed.

### `wado exec`

Run a dependency's command entry point.

```sh
wado exec <dep-name>               # run dependency's command entry point
wado exec <dep-name> [args...]     # pass arguments to the dependency
```

`wado exec` looks up `<dep-name>` in `[dependencies]` and `[dev-dependencies]`, resolves the dependency (using `wado.lock` if present), and runs its `command` entry point. This enables tool packages (formatters, linters, generators) to be installed as dependencies and executed directly.

The lock file's `command` field for the dependency determines which source file to compile and run. If the dependency has no `command` entry point, `wado exec` reports an error. If the dependency is a dev-dependency and dev-dependencies have not been fetched, `wado exec` reports an error.

## Consequences

### Positive

- Unified command set covers library and application workflows without third-party tools
- `wado update` (default) keeps specs loose for libraries — no accidental tightening
- `wado update --pin` gives applications a single command to "lock what I tested"
- `wado update --breaking` makes major upgrades explicit and intentional
- `wado fetch` enables efficient CI/Docker caching from day one
- Version operator preservation means upgrading never silently changes the compatibility contract
- Structured cache layout makes dependencies browsable with standard filesystem tools — no special commands needed to inspect cached source
- `wado list` enables quick discovery and integrates naturally with Unix pipelines (`fzf`, `xargs`, etc.)

### Negative

- Three update modes add cognitive load compared to Cargo's single `cargo update`
- `--pin` modifies `wado.toml` automatically, which some users may find surprising

### Trade-offs

- **`--pin` over `--sync-toml`**: `--pin` is shorter and conveys intent ("pin to what I resolved"). `--sync-toml` is more descriptive but verbose.
- **`--breaking` as a flag, not a separate command**: keeps the update family unified. A separate `wado upgrade` command (like cargo-edit) would fragment the mental model.
- **No `--locked` here**: `--locked` is a build-time flag (`wado compile --locked`, `wado run --locked`) that rejects stale lock files. It belongs on build commands, not on `wado update` which always writes the lock file.
- **ghq-style cache over content-addressed store**: Cargo uses a content-addressed store (`~/.cargo/registry/cache/` with `.crate` archives + `src/` extraction). The ghq-style `host/owner/name/version/` layout trades deduplication for direct browsability — `cd` into any package without extraction or special tooling. For a Wasm ecosystem where packages are typically small, the storage overhead is negligible.
- **`~/wado/` over `~/.wado/`**: ghq uses `~/ghq/` — visible, not hidden. Dependencies are source code you depend on; hiding them behind a dot prefix makes them feel opaque. A visible directory signals that the cache is a transparent, browsable workspace.
