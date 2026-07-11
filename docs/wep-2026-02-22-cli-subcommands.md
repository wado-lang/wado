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
wado clean                         # evict derived cache state (git worktrees)
wado exec <dep-name> [args...]     # run dependency's command entry point
```

### Command Tiers: `compile` vs `build`

The CLI splits into a file-scoped compiler primitive and a manifest-aware
project orchestrator. Dependency _resolution_ — version solving, registry/git
fetching, lock writing, caching — and metadata embedding belong to the
orchestrator; the primitive does none of it.

```
compile   file.wado + resolved index → wasm/wat    no resolution, deterministic   ← primitive
   ↑
build     project → build/<world>.wasm    resolve deps · lock · embed metadata · multi-world   ← orchestrator
   ↑
run / serve / test / publish      build, then execute / serve / test / push    ← drivers
```

The seam between the tiers is the resolved dependency index (name → resolved
module path), not `wado.toml`. `compile` never _resolves_; it _consumes_ an
already-resolved index — exactly as `cargo build` hands `rustc` its
`--extern name=path` set rather than making `rustc` resolve. So the primitive
still sees dependencies and builds real files; it just never does the resolving.

- `wado compile <file>` compiles one explicit target against a resolved index,
  never resolving. Standalone (no project) it uses an empty index, so a
  dependency import (`ns:pkg` / `lib:nick`) needs a single-file `with { … }`
  source or it errors. Inside a project it assembles the index offline from
  `wado.lock` (plus path deps read fresh from `wado.toml`) and compiles the one
  file; a registry/git dependency with no lock entry is an error pointing at
  `wado build` / `wado update` (compile still never resolves). This keeps it the
  deterministic path for tests, the LSP, `--wat-to-stdout`, `--no-validate`, and
  debugging. Precedent: `rustc` vs `cargo build`, `go tool compile` vs
  `go build`, `zig build-exe` vs `zig build`.
- `wado build` owns the project tier: read the manifest, resolve/lock
  dependencies, compile each declared world through the `compile` primitive with
  the resolved index, embed `[package]` metadata, and write `build/<world>.wasm`.
  All dependency, lock, and cache machinery lives here — one home.
- `run` / `serve` / `test` / `publish` share the `build` core in project mode.
  A bare-file argument (`wado run file.wado`) routes through the standalone
  `compile` path instead.

| Tier                  | Commands                                                             | Resolves deps                  |
| --------------------- | -------------------------------------------------------------------- | ------------------------------ |
| Compiler primitives   | `compile` `check` `dump` `query` `format` `doc` `wit` `syntax` `lsp` | No (consumes a resolved index) |
| Project build & run   | `build` `run` `serve` `test` `publish`                               | Yes                            |
| Dependency management | `add` `remove` `update` `fetch` `list` `clean` `exec`                | Yes                            |
| Scaffolding           | `init`                                                               | —                              |

This supersedes the earlier model where a bare `wado compile` (no argument or a
directory) performed the manifest-driven project build. That behavior moves to
`wado build`; `wado compile` now always compiles a single explicit target
against a resolved index it never computes itself.

### Subcommand ↔ Cargo Mapping and Dependency Consistency

The dependency model must be coherent across every subcommand. Each command
plays exactly one role toward the dependency graph:

- resolve — computes the graph, writes `wado.lock`, may fetch (owns resolution)
- consume — reads the resolved graph to compile/analyze user code; never
  resolves itself (a stale lock triggers a re-resolve through the shared build
  core, not per-command)
- cache — reads the on-disk dependency cache only
- none — touches no dependency state

| wado      | cargo analog                | tier         | dependency role                 |
| --------- | --------------------------- | ------------ | ------------------------------- |
| `init`    | `cargo new` / `init`        | scaffold     | none                            |
| `add`     | `cargo add`                 | manifest-op  | resolve (+ edit toml)           |
| `remove`  | `cargo remove`              | manifest-op  | resolve (+ edit toml)           |
| `update`  | `cargo update`              | manifest-op  | resolve                         |
| `fetch`   | `cargo fetch`               | manifest-op  | resolve if no lock, + download  |
| `list`    | `cargo tree` (cache view)   | inspect      | cache                           |
| `clean`   | `cargo clean` (cache scope) | inspect      | cache (evicts derived state)    |
| `build`   | `cargo build`               | orchestrator | consume (auto-resolve if stale) |
| `run`     | `cargo run`                 | driver       | consume                         |
| `serve`   | `cargo run` (service world) | driver       | consume                         |
| `test`    | `cargo test`                | driver       | consume                         |
| `publish` | `cargo publish`             | driver       | consume (needs a full lock)     |
| `exec`    | `cargo run -p` / a tool     | driver       | consume                         |
| `check`   | `cargo check`               | analyze      | consume                         |
| `doc`     | `cargo doc` / `rustdoc`     | analyze      | consume¹                        |
| `wit`     | — (emit component contract) | analyze      | consume                         |
| `dump`    | `rustc -Z unpretty`         | analyze      | consume                         |
| `query`   | rust-analyzer queries       | analyze      | consume                         |
| `compile` | `rustc`                     | primitive    | consume (never resolves)        |
| `format`  | `cargo fmt` / rustfmt       | file tool    | none                            |
| `lsp`     | rust-analyzer               | server       | consume (on demand)             |
| `syntax`  | — (grammar codegen)         | tooling      | none                            |

¹ `doc` today parses one module and extracts docs from its AST — it resolves no
imports, so it cannot follow dependency-referenced types or `pub use`
re-exports across packages. Acceptable as a syntactic public-API view; a
resolving mode is future work.

This refines the coarse [Command Tiers](#command-tiers-compile-vs-build) table:
its "does not resolve" group is `compile` (primitive) + the `analyze` /
`file tool` / `server` / `tooling` rows here; its project tier is the
`orchestrator` + `driver` rows; its dependency-management tier is `manifest-op`

- `inspect`. The `analyze` commands are file-scoped like `compile` but, unlike
  it, still resolve imports through the host (they consume the graph).

Rules that keep this consistent:

- One resolver. Only the manifest-op tier resolves and writes `wado.lock`;
  `build` auto-resolves when the lock is stale, and the drivers reach that same
  resolver through the build core. Nothing else resolves.
- Uniform consumption. Every `consume` command reads the resolved graph through
  the `CompilerHost`. Today the host resolves the nearest manifest's path deps
  on demand — `attach_manifest_deps` merely precomputes that same index — so
  every consumer (orchestrator, drivers, analyze commands) shares one seam;
  Phase 3/4 makes it lock/cache-backed and they all gain registry/git resolution
  at once, with no per-command wiring.
- `compile` is the sole primitive that consumes but never resolves (`rustc`),
  the deterministic seam of [Command Tiers](#command-tiers-compile-vs-build).

#### Reproducibility flags (`--locked` / `--offline` / `--frozen`)

Cargo accepts these on every command that reads the graph; Wado has none yet.
They belong uniformly to every tier that reads the graph (resolve, orchestrator,
driver, analyze), never to the pure primitives:

- `--locked` — fail if `wado.lock` is stale (would need re-resolution); CI reproducibility
- `--offline` — never hit the network; use only cache + lock; fail if a needed package is absent
- `--frozen` — `--locked` and `--offline` together

Consistency TODOs (do not implement piecemeal — land them together in Phase 3):

- [ ] Add `--locked` / `--offline` / `--frozen` to every graph-reading tier:
      resolve (`update`, `add`, `remove`, `fetch`), orchestrator (`build`),
      driver (`run`, `serve`, `test`, `publish`, `exec`), and analyze (`check`,
      `doc`, `wit`, `dump`, `query`). Reject them on the primitives (`compile`,
      `format`, `init`, `syntax`, `lsp`) so the flag surface stays honest.
- [ ] `publish` must verify against a full, fresh lock before uploading (a stale
      or partial lock is an error, like `cargo publish`'s verification build).
- [ ] `exec` consumes the same resolved graph (lock + cache) as the other
      drivers — no separate resolution path (Phase 5).
- [ ] Decide whether `doc` gains a resolving mode or stays syntactic; record the
      choice in the [doc WEP](./wep-2026-02-28-doc-command.md).

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
RUN wado build                     # rebuilds only when source changes
```

If `wado.lock` exists, `wado fetch` downloads the exact versions recorded in the lock file. If `wado.lock` does not exist, it runs resolution first (generates `wado.lock`), then downloads.

### Dependency Cache Layout

Dependencies are stored in a structured directory tree under `~/wado/`, mirroring the source URL hierarchy (inspired by [ghq](https://github.com/x-motemen/ghq)). Not hidden — the cache is a first-class part of the filesystem, just like `~/ghq/`. This makes cached packages browsable with standard tools.

```
~/wado/
├── ghcr.io/                          # registry host
│   └── docs/regex/0.1.2/component.wasm
└── github.com/                       # git host
    └── user/router/                  # canonical clone (default branch, ghq entry)
        └── .worktrees/
            ├── 1.0.2-abc1234d/       # linked worktree @ pinned commit
            └── 2.1.0-def56789/
```

#### Path Convention

| Source   | Path pattern                                                  | Example                                             |
| -------- | ------------------------------------------------------------- | --------------------------------------------------- |
| Registry | `{registry-host}/{namespace}/{name}/{version}/`               | `ghcr.io/docs/regex/0.1.2/`                         |
| Git      | `{git-host}/{owner}/{repo}/.worktrees/{version}-{short-ref}/` | `github.com/user/router/.worktrees/1.0.2-abc1234d/` |

A git dependency is source, so it clones to `{host}/{owner}/{repo}` (a real,
ghq-browsable checkout that hosts the shared object store), and each pinned
commit is a linked git worktree nested in `.worktrees/`. Nesting keeps `ghq list`
to one entry per repo, and the short commit prefix (8 hex) distinguishes commits
sharing a version tag. Worktrees are disposable derived state (rebuilt from the
lock by `wado fetch`, evicted by `wado clean`); registry dependencies are
prebuilt components at the exact version. See the
[git dependency design](./git-dependency-resolution-design.md) for the acquisition
and concurrency model.

#### Cache Root

The cache tree is the **Wado root**. Its location resolves as `WADO_ROOT` (env) →
`root` in `$XDG_CONFIG_HOME/wado/config.toml` → default `~/wado/`. Point it at an
existing ghq root to share checkouts with `ghq`:

```toml
# $XDG_CONFIG_HOME/wado/config.toml
root = "~/ghq"
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
docs/regex                0.1.2       ghcr.io
std/json                  1.2.0       ghcr.io
user/router               1.0.2       github.com
user/bench                0.1.0       gitlab.com
```

Columns: package identity, version, source host. Sorted by source host then package identity.

#### Path Output

```
$ wado list --path
/home/user/wado/ghcr.io/docs/regex/0.1.2
/home/user/wado/ghcr.io/std/json/1.2.0
/home/user/wado/github.com/user/router/.worktrees/1.0.2-abc1234d
/home/user/wado/gitlab.com/user/bench/.worktrees/0.1.0-def56789
```

One absolute path per line. Designed for piping into other tools:

```sh
# Open a cached package source in your editor
code $(wado list --path | fzf)

# Find all wado.toml files in cache
wado list --path | xargs -I{} ls {}/wado.toml
```

`wado list` scans the cache directory — it does not require a `wado.toml` or project context. It reports what is physically present on disk, regardless of whether any project currently depends on it.

### `wado clean`

Evicts derived cache state, the counterpart to `wado list`. It removes the git
worktrees (`{owner}/{repo}/.worktrees/`) and prunes their admin entries, leaving
the canonical clones and registry components so a rebuild needs no network;
`--all` additionally removes those. Worktrees are reproducible from the lock, so
this is always safe — `wado fetch` rebuilds them. Like `wado list`, it scans the
cache and needs no project context.

### Entry Point and CLI Commands

When `wado.toml` is present, the project-tier commands use the entry point
fields (see [Command Tiers](#command-tiers-compile-vs-build)):

```sh
# Without wado.toml (single-file mode, unchanged)
wado run file.wado
wado serve file.wado

# With wado.toml (entry point auto-discovered)
wado build                         # builds every declared world → build/<world>.wasm
wado run                           # uses [package].command
wado serve                         # uses [package].service
```

`wado compile` no longer reads `wado.toml`; project builds go through
`wado build`. When a file argument is provided to a project-tier command, it
overrides the entry point from `wado.toml` (routing that one target through the
standalone `compile` path).

#### Default output path

Without `-o`, `wado build` writes `<manifest_root>/build/<world>.wasm`, keeping
artifacts out of the source tree and giving each world its own file (mirroring
kiln's `build/` layout). `<world>` is the target Component Model world FQ
sanitized to a path segment (`@version` dropped, `:`/`/` → `-`), e.g.
`build/wasi-cli-command.wasm`; `--lib` writes `build/lib.wasm`. A standalone
`wado compile <file>` keeps the old `<input>.wasm` beside the source.

#### `--lib` — pending

`wado build --lib` (build the `[package].lib` entry as a library) is abolished
pending a world model that fits libraries. A library has no command entry point,
so it does not map onto `wasi:cli/command`; the previous implementation compiled
the lib into that world and stubbed the absent `run`, which never surfaced the
library's `export` API as component exports. The `[package].lib` manifest field
and `EntryPointKind::Lib` resolution are retained as the data model; the CLI
flag and its build path return once a proper library/component-export world is
designed.

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
- The `compile`/`build` split gives dependency, lock, cache, and metadata logic one home (`build`) and keeps `compile` a pure, deterministic primitive for tests, the LSP, and debugging

### Negative

- Three update modes add cognitive load compared to Cargo's single `cargo update`
- `--pin` modifies `wado.toml` automatically, which some users may find surprising

### Trade-offs

- **`--pin` over `--sync-toml`**: `--pin` is shorter and conveys intent ("pin to what I resolved"). `--sync-toml` is more descriptive but verbose.
- **`--breaking` as a flag, not a separate command**: keeps the update family unified. A separate `wado upgrade` command (like cargo-edit) would fragment the mental model.
- **No `--locked` here**: `--locked` is a build-time flag (`wado build --locked`, `wado run --locked`) that rejects stale lock files. It belongs on the project-tier commands, not on `wado update` which always writes the lock file.
- **`build` as a new command over overloading `compile`**: a bare `wado compile` used to mean "project build". Splitting it into a pure `compile` primitive and a `build` orchestrator removes the argument-shape-dependent behavior and the conditional metadata embedding, at the cost of one more command name. The layered `rustc`/`cargo build` precedent makes the two roles familiar.
- **ghq-style cache over content-addressed store**: Cargo uses a content-addressed store (`~/.cargo/registry/cache/` with `.crate` archives + `src/` extraction). The ghq-style `host/owner/name/version/` layout trades deduplication for direct browsability — `cd` into any package without extraction or special tooling. For a Wasm ecosystem where packages are typically small, the storage overhead is negligible.
- **`~/wado/` over `~/.wado/`**: ghq uses `~/ghq/` — visible, not hidden. Dependencies are source code you depend on; hiding them behind a dot prefix makes them feel opaque. A visible directory signals that the cache is a transparent, browsable workspace.
