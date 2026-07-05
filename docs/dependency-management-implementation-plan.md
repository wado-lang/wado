# Dependency Management — Implementation Plan

This plan sequences the work to make external Wado dependencies (registry and
git) usable end to end. It builds on two settled WEPs:

- [Package Manifest (`wado.toml`)](./wep-2026-02-14-package-manifest.md)
- [CLI Subcommands for Package Management](./wep-2026-02-22-cli-subcommands.md)

`wado publish` now works: a component is built, its `[package]` metadata is
embedded, and `wkg oci push` uploads it as an OCI artifact. The registry can
therefore hold packages — but nothing on the consuming side can yet pull one.
Closing that loop is the goal.

## Current State

### Done

- Manifest parsing (`wado-manifest/manifest.rs`): `[package]`, `[world]`,
  `[registries]`, `[dependencies]`, `[dev-dependencies]`, `[workspace]`; all
  source kinds (git / registry / path / workspace); `deps_hash`.
- Publish-readiness validation (`validate.rs`) and OCI annotation / custom
  section mapping (`metadata.rs`).
- Lock file read/write (`lockfile.rs`).
- Resolver (`resolve.rs`): highest-compatible-per-requirement, conflict is an
  error (no backtracking). Registry transitive resolution and path traversal
  work; git and workspace return `UnsupportedSource`.
- `DependencyProvider` trait with an in-memory impl (tests) and a filesystem
  impl (`wado-cli/registry.rs`) that serves path deps only; registry and git
  methods return "backend not wired yet".
- Compiler-side `ModuleSource::Dependency`, `DependencyIndex`, and loader/name
  resolution — fully wired for path dependencies.
- `wado update`: resolves and writes `wado.lock` (path deps in practice).
- `wado publish`: builds each publishable world and `wkg oci push`es it.

### Gaps

- No OCI registry provider: registry versions and packages cannot be fetched.
- No dependency cache (the `~/wado/` ghq-style layout).
- The build path (`compile` / `run` / `test`) never reads `wado.lock`; there is
  no `--locked`, no freshness check, no auto re-resolution.
- `dependency_index_from` records only path deps, so registry/git deps never
  reach the compiler. Version-aware routing for transitive deps is not wired.
- Missing CLI commands: `add`, `remove`, `fetch`, `list`, `exec`.
- No git provider, no PubGrub, no integrity verification of fetched archives,
  no single-file `with { … }` dependency source.

## Guiding Principles

- Compiler stays agnostic (per the WEP): it only consumes `ModuleSource` values
  from the `CompilerHost`. All version-aware routing lives in the CLI host.
- Provider-agnostic fetching: `DependencyProvider` is the source seam. `fetch`
  is the only user-facing acquisition verb; OCI-pull / git-clone / path are
  provider details behind it. No source-specific `wado pull`.
- Thin, layered subcommands: a file-scoped `compile` primitive vs a
  manifest-aware `build` orchestrator, with `run` / `serve` / `test` / `publish`
  layered on `build`. The seam between them is the resolved dependency index,
  not `wado.toml`: `compile` never _resolves_ (no version solving, no fetch, no
  lock writing) but does _consume_ a resolved index — the `cargo build` → `rustc
  --extern` relationship. All resolution/lock/cache machinery lives in the
  project tier. Specified in the [CLI-subcommands WEP][cli-wep] (Command Tiers);
  Phase 0 lands it before any dependency wiring so resolution attaches to
  `build`, never to `compile`.
- The narrowest end-to-end slice that makes a registry dependency compile is the
  first target; UX commands and advanced resolution follow.

[cli-wep]: ./wep-2026-02-22-cli-subcommands.md

## Phases

Phase 0 reshapes the CLI so later phases attach dependency work to the right
command. Phases 1–4 then form the critical path to "a registry dependency
compiles". Phases 5+ are follow-ups, orderable independently.

### Phase 0 — Subcommand split (`compile` pure, `build` new)

Establish the primitive/orchestrator boundary before wiring dependencies.

- [x] Introduce `wado build`: read the manifest, build every declared world
      (`[package].lib` plus each `[world]` entry) through the `compile` primitive,
      embed `[package]` metadata, write `build/<world>.wasm`. `--lib` / `--world`
      select one world. Dependency resolve/lock is a later phase; the compile
      core already reads path deps from the nearest manifest.
- [x] Make `wado compile` a file primitive: require an explicit `.wado` file
      (no manifest-driven entry discovery), `manifest_driven = false`, no metadata
      embedding. `--lib` and the `--embed-metadata` flags moved to `build`. Kept
      `-o`, `--wat-to-stdout`, `--no-validate`, `--world`, `--allocator`, `-O*`,
      `--embed-wit`. The dependency index (path deps via `try_compile`) still
      resolves, so a project file's imports compile.
- [ ] Standalone-in-project `wado compile <file>`: consume the resolved index
      offline from `wado.lock` for registry/git deps; no lock entry → error
      pointing at `wado build` / `wado update`. (Deferred to Phase 3/4 — no
      registry deps exist yet; today only path deps are indexed.)
- [x] Make `run` / `serve` build-tier drivers (like `cargo run` / `cargo test`):
      in a project they build the cli/command or http/service world through the
      shared build core (`build_for_driver` → `build_world_component`), embedding
      metadata and writing `build/<world>.wasm`, then execute it; a bare file
      with no project stays on the in-memory compile primitive. `publish` builds
      each world via the shared `for_world_build` constructor. This puts every
      driver on one build core, so Phase 3/4 dependency resolution reaches them
      uniformly. `test` remains a per-fixture test-world driver; it already
      shares the compile core's dependency-index seam and needs no reroute.
- [x] Specify the split in the [CLI-subcommands WEP][cli-wep] (Command Tiers)
      and fix the `wado compile` project-build references in the manifest WEP.
- [x] Migrate the manifest-driven `wado compile` tests (`cli.rs`,
      `manifest_integration.rs`, `cli_parse.rs`) to `wado build`.

### Phase 1 — OCI registry provider

Give `resolve` real registry data. A published Wado package is a **standalone
Wasm Component Model artifact** (`wado publish` → `wkg oci push`): one
`application/wasm` layer under a `application/vnd.wasm.config` config. So a
registry dependency resolves to a prebuilt component, not a Wado source tree —
it carries no transitive Wado dependencies and no source entry module.

- [x] Mechanism decided: a **native OCI client** (`oci-client` crate, `wado-cli/src/oci.rs`),
      not `wkg`. Consuming a dependency is a hot path that must not require an
      external binary; publish keeps shelling to `wkg`. The two are asymmetric on
      purpose. `oci-client` uses rustls (deduped with the workspace); extra CA
      roots are loaded from `SSL_CERT_FILE` so custom/proxy CAs verify.
- [x] `list_registry_versions`: OCI tags API for `<host>/<prefix>/<ns>/<pkg>`,
      strip an optional `[a-zA-Z]+` prefix, keep valid semver tags (ignore
      `latest` etc.). Registry URL `oci://<host>/<prefix>` → repository mapping.
- [x] `fetch_registry_package`: `integrity` = the OCI manifest digest (no blob
      download at resolve time). Returns an empty `Manifest` — a standalone
      component has no transitive Wado deps. Auth mirrors publish
      (`WKG_OCI_USERNAME` / `WKG_OCI_PASSWORD`, else anonymous).
- [x] Verified live: `wado update` against `oci://ghcr.io` resolves
      `wado-lang:cm-catalog` to a real version + digest and writes `wado.lock`.
- [x] `oci::pull_component` exercised by `wado fetch` (see Phase 2 bridge).

### Phase 2 — Dependency cache + `wado fetch`

Materialize resolved packages on disk so the compiler can load them.

- [x] `wado fetch` (bridge): resolve, then pull each registry component into
      `<root>/build/<name>.wasm` — the local wasm-asset location a project
      imports today, since registry-dep import resolution is Phase 4. Verified
      live end to end: `example/cm-catalog` runs `update` → `fetch` → `run` and
      round-trips values through the published `wado-lang:cm-catalog` component.
- [ ] Move the cache to `~/wado/`, overridable by `WADO_ROOT`, once Phase 4
      resolves registry-dep imports from it (retire the `build/` bridge).
- [ ] ghq-style layout: `{host}/{ns}/{name}/{version}/` for registry,
      `{host}/{owner}/{repo}/{version}-{short-ref}/` for git.
- [ ] Extract the pulled component's Wado source tree (or, for wado-to-wado, the
      provider-metadata source) into the version directory alongside its
      `wado.toml`.
- [ ] `wado fetch`: resolve if no lock, then download every locked package into
      the cache. Idempotent; the CI/Docker caching case in the CLI WEP.
- [ ] Verify `integrity` on every fetch; mismatch aborts.

### Phase 3 — Lock file in the build path

Make the `build` core lock-aware (inherited by `run` / `serve` / `test` /
`publish`). The pure `compile` primitive stays lock-free.

- [ ] In `build`, load `wado.lock` if present; else resolve and write it.
- [ ] Freshness: compare `deps_hash` against the current manifest; stale →
      auto re-resolve (default) or error under `--locked`.
- [ ] Add `--locked` / `--offline` / `--frozen` uniformly across the resolve and
      driver tiers, rejected on the primitives — per the CLI-subcommands WEP
      [Reproducibility flags](./wep-2026-02-22-cli-subcommands.md) consistency
      TODOs. Land them together, not piecemeal.
- [ ] When the lock exists, skip the resolver: read the graph, versions, and
      entry points straight from `wado.lock` (self-sufficient per the WEP).

### Phase 4 — Registry/git deps reach the compiler

Wire the resolved graph into the compiler. A registry dependency is a **prebuilt
CM component**, so — unlike a path/source dependency that compiles into the
consumer — it is consumed across the Component Model boundary via
[Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) (the
provider-metadata fast path when the component carries Wado GC-type metadata,
else the canonical ABI). This is the key modeling decision Phase 4 must settle:
`ModuleSource::Dependency { path }` today loads Wado source; a prebuilt-component
dependency needs its own representation (the cached `.wasm` + its WIT), distinct
from source deps.

- [ ] Represent a prebuilt-component dependency distinctly from a source
      dependency (path deps stay source; registry deps are components).
- [ ] Map a registry dependency key to its cached component + WIT so `use { … }
      from "ns:pkg"` type-checks against the component's exported interface.
- [ ] Version-aware routing: `resolve_import(from, spec)` resolves against the
      importing package's own deps, so semver-incompatible versions of one
      package become distinct dependency ids (WEP "Transitive Version
      Isolation"). Natural for components (CM instances are type-isolated).
- [ ] E2E: a fixture project depending on `wado-lang:cm-catalog` builds and runs
      against ghcr (`wado build` / `wado run`).

### Phase 5 — Dependency-editing CLI

The manipulation commands from the CLI WEP.

- [ ] `wado add <name> [--package/--version/--registry/--git/--ref/--path/--dev]`.
- [ ] `wado remove <name> [--dev]`.
- [ ] `wado update --pin` / `--breaking` (the base `wado update` exists).
- [ ] `wado list [filter] [--path]` — scans the cache, no project context.
- [ ] `wado exec <dep> [args…]` — run a dependency's command world.

### Phase 6 — Git provider

- [ ] Implement the git methods of the CLI provider (`list_git_tags`,
      `resolve_git_ref`, `fetch_git_manifest`): clone/fetch, enumerate tags,
      resolve refs to SHAs, read `wado.toml` (honoring `directory`).
- [ ] Remove the `UnsupportedSource { kind: "git" }` path in `resolve.rs`.
- [ ] Cache git deps as `{host}/{owner}/{repo}/{version}-{short-ref}/`.

### Phase 7 — PubGrub resolver

Replace the single-pass resolver with PubGrub for backtracking and precise
conflict errors, once real registries make multi-constraint graphs common.

- [ ] Adopt the `pubgrub` crate; adapt `DependencyProvider` to its interface.
- [ ] Preserve current behavior (highest-compatible, coexisting
      semver-incompatible majors) and add derivation-chain error messages.
- [ ] Cyclic-dependency detection with the WEP's error format.

### Phase 8 — Remaining WEP surface

- [ ] Single-file `with { … }` inline dependency source (no `wado.toml`).
- [ ] Workspace publish/resolve edge cases beyond what `publish` covers.
- [ ] `wado.lock` integrity extensibility (algorithm prefix already in schema).

## Milestones

- M1 (Phases 0–4): after the `compile`/`build` split, a published registry
  package can be declared in `[dependencies]` and built/run — the core loop
  closes.
- M2 (Phase 5): day-to-day dependency editing without hand-writing `wado.toml`.
- M3 (Phases 6–8): git deps, robust resolution, and the remaining WEP surface.

## Open Questions

- Resolved (Phase 1): OCI pull uses a native `oci-client`, not `wkg` — consuming
  a dependency must not require an external binary. Publish stays on `wkg`.
- Resolved (Phase 1): a registry dependency is the published standalone
  component (no source sidecar). It is consumed across the CM boundary (Phase 4),
  so the Wado→Wado source-sharing optimization does not apply to registry deps —
  only to `path` deps, which compile in from source. If source sharing across a
  registry ever matters, `publish` would need to embed a Wado source/provider
  section; not planned.
- How a prebuilt-component dependency is modeled in the compiler (its
  `ModuleSource` / lock representation and WIT-driven type-checking) — the Phase 4
  design decision.
- Whether `test` should fetch dev-dependencies eagerly or lazily.
