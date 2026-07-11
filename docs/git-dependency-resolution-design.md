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

1. its source lives in the shared `~/wado/` cache at a pinned commit (a git
   worktree) rather than at a relative path, and
2. it must be _acquired_ (clone/fetch/worktree) before it can be read.

This reuses the entire path-dependency machinery (loader, name resolution,
transitive traversal, `[package].lib` entry discovery) and keeps the compiler
agnostic — it still only sees `ModuleSource` values.

| Aspect          | Registry dep                    | Git dep                                                              | Path dep                 |
| --------------- | ------------------------------- | -------------------------------------------------------------------- | ------------------------ |
| Artifact        | Prebuilt CM component           | Source tree @ commit                                                 | Source tree on disk      |
| Consumed as     | `components` (CM boundary)      | `resolved` (compiled in)                                             | `resolved` (compiled in) |
| Transitive deps | None (standalone)               | From its `wado.toml`                                                 | From its `wado.toml`     |
| Locked          | Yes (`integrity` = digest)      | Yes (`resolved-ref` = SHA)                                           | No (resolved fresh)      |
| Cache key       | `{host}/{ns}/{name}/{version}/` | `{host}/{owner}/{repo}/.worktrees/{ver}-{short-ref}/` (git worktree) | n/a                      |

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

| Provider method      | git command                                                                | Notes                                                                                                                                                  |
| -------------------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `list_git_tags`      | `git ls-remote --tags <url>`                                               | Parse `refs/tags/*`, drop `^{}` peel lines, strip a leading `[a-zA-Z]+` prefix (reuse `registry::parse_version_tag`), keep semver tags, map tag → SHA. |
| `resolve_git_ref`    | `git ls-remote <url> <ref>`                                                | Named ref → SHA. A ref that is itself a SHA (no ls-remote hit) resolves during the fetch below.                                                        |
| `fetch_git_manifest` | clone-if-absent + `git fetch`, then `git show <sha>:<directory>/wado.toml` | Reads the manifest from a blob — no worktree checkout (see Acquisition).                                                                               |

### Cache layout: a canonical clone with nested per-version worktrees

The cache must satisfy two requirements that pull against each other: stay
**ghq-compatible** — `~/wado/{host}/{owner}/{repo}` is itself a real, browsable
git working tree, so `GHQ_ROOT=~/wado ghq list` and editor/fuzzy-cd tooling see
one clean entry per repo — and hold **multiple versions of one repo at once**,
since different consumers pin different commits. The WEP's original layout
(`{owner}/{repo}/{version}-{ref}/`) fails the first: `{owner}/{repo}` becomes a
_container of version dirs_, not a checkout.

`git worktree`, with the version worktrees **nested inside the canonical clone**,
resolves the tension:

```text
~/wado/github.com/user/router/                        # canonical clone: default
                                                       # branch, ghq entry, object
                                                       # store + worktree admin
~/wado/github.com/user/router/.worktrees/1.0.2-abc1234d/  # linked worktree @ commit
~/wado/github.com/user/router/.worktrees/2.1.0-def56789/  # linked worktree @ commit
```

- The canonical clone at `{owner}/{repo}` is a normal (non-bare) working tree on
  the remote's default branch — what ghq and browsing tools see — and it hosts
  the shared object store and worktree metadata. A bare clone would save one
  working-tree's worth of disk but forfeit ghq compatibility (no working tree at
  `{owner}/{repo}`); decided in favor of compat, since Wasm packages are small
  (the WEP's stance).
- Each consumed commit is a **linked worktree** under
  `{owner}/{repo}/.worktrees/{version}-{short-ref}`. Nesting _inside_ the repo is
  what keeps ghq clean: `ghq list` stops descending the moment it finds the
  repo's `.git`, so nested worktrees are never enumerated as their own entries.
- Worktrees **share the object store**, so a version costs only its checked-out
  working files, not a second copy of history. Even a redundant worktree of an
  already-present commit (a monorepo whose two subdirectory packages carry
  different `[package].version`s at one commit) costs only working-file bytes —
  which is why the dir name keeps the readable `{version}-{short-ref}` rather
  than collapsing to a bare commit id.

The worktrees live in `.worktrees/`, not `build/`: a git dependency is itself a
Wado package whose own `wado build` writes `build/<world>.wasm`, so `build/` is a
path the dependency may legitimately track — nesting our checkouts there could
collide with the upstream tree. `.worktrees/` is dedicated and dot-hidden. On
creating the canonical clone, the tool appends `.worktrees/` to
`{repo}/.git/info/exclude` so the canonical working tree never reports our
checkouts as untracked and a stray `git status`/`git clean` stays quiet.

`short-ref` is the first 8 hex of the resolved SHA; it disambiguates two commits
that resolve to the same version tag.

A worktree is **disposable derived state**: it is reproducible from the locked
SHA via `git worktree add`, so it can be deleted and rebuilt at will (see
[`wado clean`](#wado-clean)). The sources of truth are the canonical clone's
object store and the lock's `resolved-ref` — never the checked-out files.

### The Wado root and its config

The cache tree is the **Wado root** — the same concept ghq calls its "root", a
flat `{host}/{owner}/{repo}` store of cloned source. Naming and control:

- The term is `root`, not `home`: `home` (à la `CARGO_HOME`) connotes the tool's
  whole private state dir, whereas this is a source-checkout root meant to be
  interchangeable with ghq's. The existing `WADO_ROOT` env var and
  `cache_root()` resolver already use "root".
- An XDG config file `$XDG_CONFIG_HOME/wado/config.toml` (defaulting to
  `~/.config/wado/config.toml` when `XDG_CONFIG_HOME` is unset) sets it:

  ```toml
  root = "~/ghq"   # point the Wado root at an existing ghq root
  ```

- Resolution precedence: `WADO_ROOT` (env) → `root` in
  `$XDG_CONFIG_HOME/wado/config.toml` → default `~/wado`. `~` and `$VARS`
  expand. The resolver lives where both the CLI (which fetches) and the LSP
  (which reads offline) already share `cache_root()`, so both agree on one
  location.

Pointing the root at `~/ghq` is a first-class use case, and it is precisely the
nested-`.worktrees/` layout that makes it safe: a git dependency's canonical
clone lands at `~/ghq/{host}/{owner}/{repo}` — exactly where `ghq get` would put
it, so the two tools interoperate — while the per-version worktrees stay hidden
inside it and never pollute `ghq list`. The `@ref`-sibling layout, by contrast,
would have littered a real ghq root with `repo@ver` entries.

Registry components continue to live under the same root
(`{host}/{ns}/{name}/{version}/component.wasm`); they carry no `.git`, so ghq
ignores them.

### Worktrees are global, not the project's `build/`

The per-version worktrees live under the Wado root, **not** in the consuming
project's `build/`. `build/` holds only _this_ project's compiled outputs
(`build/<world>.wasm`, `build/kiln/…`); dependency source is shared machine-wide,
mirroring the existing rule for registry components (see `wado-cli/src/cache.rs`:
the `~/wado/` cache exists so packages are shared "instead of re-downloading into
each project's `build/`").

Materializing into `build/` was considered and rejected:

- It would **not remove the global clone or its locking** — the object store and
  the ghq-compatible canonical checkout still have to live under the root — so it
  only relocates the leaf checkout while adding a coupling: `rm -rf build/` (a
  routine wipe) orphans `git worktree` admin entries in the global clone until a
  `prune`.
- It **loses cross-project sharing**: N projects on one machine would each
  re-check-out the same commit.
- It **diverges from registry deps**, which are global — git deps behaving
  differently would be a needless inconsistency.

The global model keeps one shared, ghq-browsable copy per commit and confines
per-build churn to `wado clean`.

### Acquisition: resolve reads a blob, materialize adds a worktree

Two tiers, so resolution never pays for a working-tree checkout:

Resolution-time (no worktree) — used by the resolver to read a manifest and its
transitive deps:

1. Ensure the canonical clone exists (`git clone <url> <repo>` if absent).
2. Ensure the commit's objects are present: `git fetch origin <ref>` (prefer a
   shallow `--depth 1`; fall back to an unshallowed fetch when the server
   rejects a by-SHA want, `uploadpack.allowReachableSHA1InWant` off).
3. Read the manifest without checking anything out:
   `git -C <repo> show <sha>:<directory>/wado.toml`.

Materialize-time (worktree) — used by `wado fetch` / `build` when source files
must be on disk:

1. Compute the worktree dir `{owner}/{repo}/.worktrees/{version}-{short-ref}`.
2. Warm hit if it exists and `git -C <worktree> rev-parse HEAD == <sha>` — done
   (a commit is immutable, so no re-fetch).
3. Otherwise `git -C <repo> worktree add --detach <worktree> <sha>` (objects are
   already present from the fetch above — no second network trip).

### Concurrency and crash safety

Multiple processes (parallel `build`s, the LSP alongside a CLI invocation) may
clone/fetch/`worktree add` the same repo at once. Mutations of one repo are
serialized by a per-repo advisory file lock (`flock` on `<repo>/.git` via the
already-present `libc` on unix; `LockFileEx`, or a small cross-platform lock
crate such as `fs4`, on Windows). Reads of an already-materialized worktree take
**no lock** — a completed worktree is immutable.

Crash safety: `worktree add` is not atomic, so a crash can leave a partial
worktree. The lock holder verifies `HEAD == <sha>` after adding; a worktree that
fails the check is `git worktree remove --force`d and rebuilt. A worktree dir
deleted out from under the clone (by `wado clean` or by hand) is reconciled by
`git worktree prune`. Because every mutation runs under the per-repo lock, a
second process never races a partial tree — it waits, then sees either a good
worktree or repairs the leftover.

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
`registry_cache_relative`. Two paths: the canonical clone and the per-version
worktree.

```rust
pub fn git_repo_relative(url: &str) -> Option<String>;
// → "{host}/{owner}/{repo}"                                      (canonical clone / ghq path)

pub fn git_worktree_relative(url: &str, version: &str, resolved_ref: &str) -> Option<String>;
// → "{host}/{owner}/{repo}/.worktrees/{version}-{short-ref}"     (short-ref = first 8 hex of the SHA)
```

URL parsing normalizes `https://`, `git@host:owner/repo`, and a trailing
`.git`/`/` into `host/owner/repo`. `directory` is **not** part of either path —
it selects the entry _within_ the worktree, and two subdirectory packages of one
monorepo at the same commit deliberately share one worktree. Returns `None` for
an unparseable URL.

Wire concrete `PathBuf`s in `wado-cli/src/cache.rs` (`git_repo_path(url)`,
`git_worktree_path(url, version, resolved_ref)`), matching the existing
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
2. `cache_root().join(git_worktree_relative(url, version, sha))` → worktree dir.
3. Entry = `dependency_entry_path(worktree/directory)` — the existing helper
   that reads `[package].lib` (honoring `directory`, defaulting to the repo
   root).
4. Present on disk → `index.resolved.insert(name, relative_path)`; missing →
   `index.unresolved.insert(name, "… not cached; run`wado fetch`")`, matching
   the registry cold-cache path.

This makes every entry point that already consumes `dependency_index_from`
(`build`, `run`, `serve`, `test`, `check`, `query`, and the `wado lsp` server)
resolve git deps offline from a warm cache with no further per-command work.

### 6. `FilesystemProvider` (CLI)

Implement the three git methods via the git shell-out described above. Resolution
methods stay checkout-free: `fetch_git_manifest` ensures the canonical clone and
the commit's objects, then reads the manifest from a blob
(`git show <sha>:<directory>/wado.toml`). Materialization (`git worktree add`)
is a separate step invoked only by `wado fetch` / `build`.

### 7. CLI commands

- **`wado update`**: no code change beyond the resolver — the git arm now
  produces git lock entries, so `wado.lock` gains `[[package]]` rows with
  `resolved-ref`.
- **`wado fetch`**: add a git branch to the fetch loop. Registry deps pull a
  component; git deps materialize a worktree into the cache. Both are idempotent
  and warm-cache-skipping.
- **`build`/`run`/`serve`/`test`/`check`/`query`/`lsp`**: unchanged — they
  consume the index seam.

### `wado clean`

A new subcommand that evicts derived cache state — the natural GC for git
worktrees, which are reproducible from the lock:

- Removes every `{owner}/{repo}/.worktrees/` directory under the cache root and
  runs `git worktree prune` on each affected canonical clone to drop stale admin
  entries. Safe because a subsequent `wado fetch` rebuilds any worktree from its
  locked SHA.
- Leaves the canonical clones (the shared object stores) in place by default, so
  a re-materialize needs no network. A `--all` flag additionally removes the
  clones and the fetched registry components.

The command scans the cache only; it needs no project context (mirroring
`wado list`). Registry components can share the same eviction pass.

### DependencyProvider trait changes

One adjustment, cheap because git is not yet wired at the CLI:

- `fetch_git_manifest(url, sha, directory: Option<&str>)` — the transitive
  manifest lives at the subdirectory, so the resolver must pass it. The
  in-memory provider keys its stored manifests to match.

Materialization needs no new trait method: it is a CLI-only concern
(`wado fetch` / `build`) and the compiler-side index recomputes the worktree
path purely from the lock via `git_worktree_relative`. The provider trait stays
`wasm32`-compatible — the shell-out lives only in the CLI impl; the in-memory
impl stays pure.

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
- **Cache path**: unit-test `git_repo_relative` / `git_worktree_relative` like
  `registry_cache_relative` (https/ssh/`.git` URL forms, short-ref truncation,
  the `.worktrees/{version}-{short-ref}` nested suffix).
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
2. `wado_manifest::cache::git_repo_relative` / `git_worktree_relative` +
   `wado-cli` `git_repo_path` / `git_worktree_path` + tests.
3. Resolver git arm (against the in-memory provider), transitive traversal,
   remove `UnsupportedSource{git}` + red/green tests.
4. `FilesystemProvider` git methods via `git` shell-out: canonical clone,
   fetch, blob-read for resolution, `worktree add` for materialization, per-repo
   file lock + e2e tests.
5. `dependency_index_from` git arm + `locked_git_refs` lock reader.
6. Wado-root resolver: `WADO_ROOT` env → `$XDG_CONFIG_HOME/wado/config.toml`
   `root` → `~/wado`, in the shared `cache_root()` + tests.
7. `wado fetch` git branch; confirm `update` writes git lock entries.
8. `wado clean` subcommand (`.worktrees/` eviction + `git worktree prune`,
   `--all` for clones/components).
9. `example/` e2e; docs (mark Phase 6 items done, note submodule limitation).

## Open questions

- **Shallow-fetch fallback**: how aggressively to attempt a by-SHA shallow
  fetch before falling back to a full fetch. Start conservative (fetch the ref,
  add a worktree at the SHA) and optimize once measured.
- **Canonical clone: non-bare vs bare**: the design keeps a non-bare
  default-branch clone at `{owner}/{repo}` for ghq compatibility, at the cost of
  one extra working tree. A bare clone would save that disk but forfeit compat.
  Decided in favor of compat; revisit if the extra checkout ever matters.
- **Cross-platform locking**: `flock` (libc) covers unix; Windows needs
  `LockFileEx` or a lock crate (`fs4`). Decide whether to take the small
  dependency or hand-roll the platform split.
- **`wado clean` scope**: decided to ship it in Phase 6 as the worktree GC
  (`.worktrees/` eviction + prune). Open only on the details: whether `--all`
  also evicts registry components, and whether a bare `wado clean` should prune
  worktrees not referenced by the current project's lock vs. all worktrees.
- **Config file scope**: location (`$XDG_CONFIG_HOME/wado/config.toml`), the
  `root` key, and precedence are decided; open only on whether other
  machine-global settings (default registry auth, `--offline` default, …)
  eventually share this file.
- **Submodules**: left unrecursed initially. Revisit if a real dependency needs
  them; would become a `--recurse-submodules`-style opt-in, not a default.
- **Lock `directory`**: not recorded in the lock (the consumer's manifest still
  carries it, and the cache key excludes it). Revisit only if a future feature
  needs to reconstruct the entry purely from the lock.
