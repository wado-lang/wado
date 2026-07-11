# Git Dependency Resolution — Design

Design for making `wado-cli` resolve, fetch, and build git-repository
dependencies. This is Phase 6 of the
[Dependency Management Implementation Plan](./dependency-management-implementation-plan.md);
the user-facing surface is already fixed by the
[Package Manifest WEP](./wep-2026-02-14-package-manifest.md#git) and the
[CLI Subcommands WEP](./wep-2026-02-22-cli-subcommands.md). This document
settles the implementation model and the seams each layer touches.

## Context

A git dependency is declared in `wado.toml` today and parses correctly:

```toml
"user:router" = { git = "https://github.com/user/router.git", version = "^1.0.0" }
"user:router" = { git = "https://github.com/user/router.git", ref = "main" }
"org:foo"     = { git = "https://github.com/org/monorepo.git", version = "^1.0.0", directory = "packages/foo" }
```

Everything downstream of parsing is stubbed:

- `resolve.rs` returns `ResolveError::UnsupportedSource { kind: "git" }`.
- `FilesystemProvider`'s three git methods return "git dependency backend is
  not wired yet".
- `wado_lsp::host::dependency_index_from` skips `DependencySource::Git`, so a
  git dep never reaches the compiler.
- `RawDependency` does not deserialize `directory`, so that field is silently
  dropped even though the WEP specifies it.

The scaffolding that _is_ in place: the `DependencyProvider` trait already has
git methods with an in-memory implementation, `LockFile`/`LockedPackage`
already model a git entry (`git+…` id + `resolved-ref`, no `integrity`), and
the path-dependency source-compilation pipeline is fully wired.

## Core Decision: a git dependency is a source dependency

The single most important modeling choice. A **registry** dependency is a
published, standalone Component Model artifact consumed _across the CM
boundary_ (WEP 2026-06-26). A **git** dependency is the opposite: a source
repository carrying Wado source plus its own `wado.toml` with transitive
dependencies. There is no prebuilt component to pull.

Therefore a git dependency is materialized as a **source working tree in the
cache and compiled into the consumer exactly like a `path` dependency** — via
`DependencyIndex.resolved` and `ModuleSource::Dependency`, not via
`DependencyIndex.components`. The only differences from a path dep are:

1. its source lives in the shared `~/wado/` cache at a pinned commit rather
   than at a relative path, and
2. it must be _acquired_ (clone/fetch/checkout) before it can be read.

This reuses the entire path-dependency machinery (loader, name resolution,
transitive traversal, `[package].lib` entry discovery) and keeps the compiler
agnostic — it still only sees `ModuleSource` values.

| Aspect          | Registry dep                    | Git dep                                    | Path dep                 |
| --------------- | ------------------------------- | ------------------------------------------ | ------------------------ |
| Artifact        | Prebuilt CM component           | Source tree @ commit                       | Source tree on disk      |
| Consumed as     | `components` (CM boundary)      | `resolved` (compiled in)                   | `resolved` (compiled in) |
| Transitive deps | None (standalone)               | From its `wado.toml`                       | From its `wado.toml`     |
| Locked          | Yes (`integrity` = digest)      | Yes (`resolved-ref` = SHA)                 | No (resolved fresh)      |
| Cache key       | `{host}/{ns}/{name}/{version}/` | `{host}/{owner}/{repo}/{ver}-{short-ref}/` | n/a                      |

## Git backend: shell out to the system `git`

The `DependencyProvider` git methods are implemented by invoking the system
`git` binary as a subprocess, mirroring how `wado publish` shells out to
`wkg`.

Rationale, and the trade-off against the plan's "consuming a dependency must
not require an external binary" principle:

- That principle was written for the _registry_ hot path, where every import
  may trigger a network pull; it justified a native `oci-client` over `wkg`.
  Git acquisition is comparatively cold: a repo is cloned once per
  `(url, commit)` and then served from a warm checkout.
- `git` is effectively universal on developer and CI machines, far more so
  than `wkg`. A pure-Rust git (`gix`/`git2`) adds a large dependency surface
  (transport, auth, protocol) for a cold-path feature.
- Cargo itself defaults to shelling out to `git`.

The provider trait is the seam: a later swap to `gix` changes only
`FilesystemProvider`, never the resolver or the compiler wiring. A missing
`git` binary is reported as a clear, actionable `ProviderError`, not a panic.

Submodules are **not** recursed initially (documented limitation); Wado has no
build scripts, so a checkout is inert source with no code-execution risk.

### git invocations

| Provider method      | git command                                               | Notes                                                                                                                                                  |
| -------------------- | --------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `list_git_tags`      | `git ls-remote --tags <url>`                              | Parse `refs/tags/*`, drop `^{}` peel lines, strip a leading `[a-zA-Z]+` prefix (reuse `registry::parse_version_tag`), keep semver tags, map tag → SHA. |
| `resolve_git_ref`    | `git ls-remote <url> <ref>`                               | Named ref → SHA. A ref that is itself a SHA (no ls-remote hit) resolves during materialization.                                                        |
| `fetch_git_manifest` | materialize, then read `<checkout>/<directory>/wado.toml` | See below.                                                                                                                                             |

### Materialization (the cache checkout)

Acquiring a checkout is a new provider capability (not currently a trait
method — see [Trait changes](#dependencyprovider-trait-changes)):

1. Compute the cache dir `{host}/{owner}/{repo}/{version}-{short-ref}/`.
2. If it already contains a `wado.toml`, it is a warm hit — done (a commit is
   immutable, so no re-fetch).
3. Otherwise fetch into a bare per-repo git dir shared across versions
   (`{host}/{owner}/{repo}/.git-cache/`), then `git worktree`/`archive` the
   pinned SHA into the version dir. Prefer a shallow fetch of the specific
   ref (`git fetch --depth 1 origin <ref>`); fall back to an unshallowed
   fetch when the server rejects a by-SHA want
   (`uploadpack.allowReachableSHA1InWant` off).
4. Materialization is atomic: build in a temp sibling dir, then rename into
   place (same discipline as `cache::write_atomic`), so a warm-cache
   `is_file()` check never trusts a half-written tree.

## Layer-by-layer changes

### 1. Manifest: parse `directory`

`RawDependency` gains `directory: Option<String>`. `DependencySource::Git`
gains `directory: Option<String>`:

```rust
Git { url: String, pin: GitPin, directory: Option<String> },
```

- `build_git_source` reads `raw.directory`.
- `directory` is only meaningful with `git`; combining it with a non-git
  source is a `ConflictingSource` error.
- `source_fingerprint` includes `directory` so `deps_hash` (lock staleness)
  changes when the subdirectory changes:
  `git|{url}|{version|ref}|{pin}|dir={directory?}`.

Path-with-git publish source (`{ path = …, git = …, version = … }`) carries
`directory` through to its `publish_source` git arm.

### 2. Cache path helper (pure, shared)

Add to `wado_manifest::cache` (pure string logic, no I/O — shared by the CLI
that fetches and the LSP that reads offline), alongside
`registry_cache_relative`:

```rust
pub fn git_cache_relative(url: &str, version: &str, resolved_ref: &str) -> Option<String>;
// → "{host}/{owner}/{repo}/{version}-{short-ref}"  (short-ref = first 8 hex of the SHA)
```

URL parsing normalizes `https://`, `git@host:owner/repo`, and a trailing
`.git`/`/` into `host/owner/repo`. `directory` is **not** part of the cache
key — it selects the entry _within_ the checkout, and two packages in one
monorepo already differ by coordinate. Returns `None` for an unparseable URL.

Wire concrete `PathBuf`s in `wado-cli/src/cache.rs`
(`git_checkout_path(url, version, resolved_ref)`), matching the existing
`component_path` / `generator_path` helpers.

### 3. Resolver: the git arm

Remove the `UnsupportedSource { kind: "git" }` branch. Add a git arm to the
`resolve` loop that parallels the registry arm:

1. Pick the commit:
   - `GitPin::Version(req)`: `list_git_tags` → filter by the semver
     requirement → highest → its SHA. No match → `NoMatchingVersion`.
   - `GitPin::Ref(r)`: `resolve_git_ref(url, r)` → SHA.
2. `fetch_git_manifest(url, sha, directory)` → the package's own manifest.
3. Determine the locked version:
   - version pin → the chosen tag's semver;
   - ref pin → the fetched manifest's `[package].version`.
4. Compute the lock id `git+{url}/{coordinate}` (coordinate = the dependency
   key's `ns:pkg`; matches the `LockFile` roundtrip test's shape).
5. **Traverse transitive deps** from the fetched manifest — enqueue its
   `[dependencies]` (registry, path, and nested git) just as the registry arm
   enqueues children. This is the key behavioral gap vs registry deps, whose
   manifests are empty.
6. Emit `LockedPackage { id, version, resolved_ref: Some(sha),
   integrity: None, world: <from manifest>, deps }`.

De-duplication/conflict keys on the id, same as registry: a second requirement
on the same git id that disagrees on the resolved ref is a `VersionConflict`.

### 4. Lockfile

No schema change — `git+…` ids and `resolved-ref` already round-trip. The git
arm simply populates `resolved_ref: Some(sha)` and leaves `integrity: None`.
The immutable commit SHA _is_ the integrity anchor for git deps.

### 5. Compiler wiring: `dependency_index_from`

Replace the `DependencySource::Git { .. } => {}` no-op in
`wado_lsp::host::dependency_index_from` with a git arm, mirroring the path arm
but resolving the entry against the cache checkout:

1. Read the git dep's `resolved_ref` and `version` from `wado.lock`
   (a `locked_git_refs(manifest_dir)` helper next to
   `locked_registry_versions`).
2. `cache_root().join(git_cache_relative(url, version, sha))` → checkout dir.
3. Entry = `dependency_entry_path(checkout/directory)` — the existing helper
   that reads `[package].lib` (honoring `directory`, defaulting to the repo
   root).
4. Present on disk → `index.resolved.insert(name, relative_path)`; missing →
   `index.unresolved.insert(name, "… not cached; run`wado fetch`")`, matching
   the registry cold-cache path.

This makes every entry point that already consumes `dependency_index_from`
(`build`, `run`, `serve`, `test`, `check`, `query`, and the `wado lsp` server)
resolve git deps offline from a warm cache with no further per-command work.

### 6. `FilesystemProvider` (CLI)

Implement the three git methods via the git shell-out described above, and add
the materialization step. `fetch_git_manifest` materializes (idempotent) then
reads the manifest at `directory`.

### 7. CLI commands

- **`wado update`**: no code change beyond the resolver — the git arm now
  produces git lock entries, so `wado.lock` gains `[[package]]` rows with
  `resolved-ref`.
- **`wado fetch`**: add a git branch to the fetch loop. Registry deps pull a
  component; git deps materialize a checkout into the cache. Both are
  idempotent and warm-cache-skipping.
- **`build`/`run`/`serve`/`test`/`check`/`query`/`lsp`**: unchanged — they
  consume the index seam.

### DependencyProvider trait changes

Two adjustments, both cheap because git is not yet wired at the CLI:

1. `fetch_git_manifest(url, sha, directory: Option<&str>)` — the transitive
   manifest lives at the subdirectory, so the resolver must pass it. The
   in-memory provider keys its stored manifests to match.
2. Add a materialization method (e.g.
   `materialize_git_checkout(url, version, sha, directory) -> Result<PathBuf>`),
   or fold acquisition into `fetch_git_manifest` and derive the checkout path
   from the cache helper. Recommended: keep `fetch_git_manifest` as the single
   acquisition trigger (it already must produce a checkout to read the
   manifest), and let the compiler-side index recompute the path purely from
   the lock — no extra trait surface.

The trait stays `wasm32`-compatible: the shell-out lives only in the CLI impl;
the in-memory impl stays pure.

## Reproducibility, offline, integrity

- The `resolved-ref` SHA pins the exact tree; a warm checkout under `--offline`
  needs no network. Optionally assert `git rev-parse HEAD == resolved_ref` on a
  warm checkout to detect tampering.
- `--locked` forbids re-resolution: a git dep whose manifest requirement no
  longer matches its lock entry is an error, not a silent re-resolve.
- These land with the Phase 3 `--locked`/`--offline`/`--frozen` flags; git deps
  need no special-casing beyond honoring them.

## Testing (TDD)

- **Resolver** (red first): the in-memory provider already supports git, so
  replace `git_source_is_not_yet_supported` with tests that resolve a
  version-pinned and a ref-pinned git dep, verify `resolved_ref`, and verify a
  transitive dep of a git dep is locked. No new test infrastructure.
- **Cache path**: unit-test `git_cache_relative` like `registry_cache_relative`
  (https/ssh/`.git` URL forms, short-ref truncation, monorepo `directory`).
- **Manifest**: `directory` parses onto `DependencySource::Git`; `deps_hash`
  changes with `directory`.
- **Provider shell-out** (e2e): create a throwaway repo with `git init` in a
  `tempdir`, tag it, and drive `list_git_tags`/`resolve_git_ref`/
  materialization against a `file://` URL — no network.
- **End-to-end**: an `example/` project depending on a small git repo that
  `update` → `fetch` → `run` builds and executes, round-tripping a value
  through the git-sourced library (mirrors `example/hello-packages`).

## Work breakdown

1. Manifest `directory` field (`DependencySource::Git`, `RawDependency`,
   `source_fingerprint`, validation) + tests.
2. `wado_manifest::cache::git_cache_relative` + `wado-cli` `git_checkout_path`
   - tests.
3. Resolver git arm (against the in-memory provider), transitive traversal,
   remove `UnsupportedSource{git}` + red/green tests.
4. `FilesystemProvider` git methods via `git` shell-out + checkout
   materialization + e2e tests.
5. `dependency_index_from` git arm + `locked_git_refs` lock reader.
6. `wado fetch` git branch; confirm `update` writes git lock entries.
7. `example/` e2e; docs (mark Phase 6 items done, note submodule limitation).

## Open questions

- **Shallow-fetch fallback**: how aggressively to attempt a by-SHA shallow
  fetch before falling back to a full clone. Start conservative (full fetch of
  the repo, checkout the SHA) and optimize once measured.
- **`.git-cache` sharing**: whether to keep one bare mirror per repo (fetch
  once, materialize many versions) or clone per version. The bare-mirror model
  saves bandwidth for a monorepo referenced at several commits; adds a small
  amount of concurrency care.
- **Submodules**: left unrecursed initially. Revisit if a real dependency needs
  them; would become a `--recurse-submodules`-style opt-in, not a default.
- **Lock `directory`**: not recorded in the lock (the consumer's manifest still
  carries it, and the cache key excludes it). Revisit only if a future feature
  needs to reconstruct the entry purely from the lock.
