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

## Guiding Principle

The compiler stays agnostic (per the WEP): it only consumes `ModuleSource`
values from the `CompilerHost`. All version-aware routing lives in the CLI host.
The narrowest end-to-end slice that makes a registry dependency compile is the
first target; UX commands and advanced resolution follow.

## Phases

Phases 1–4 form the critical path to "a registry dependency compiles". Phases
5+ are follow-ups, orderable independently.

### Phase 1 — OCI registry provider

Give `resolve` real registry data. Implement the `list_registry_versions` /
`fetch_registry_package` methods of the CLI provider against OCI, mirroring how
`publish` shells out to `wkg oci push`.

- [ ] `list_registry_versions`: list tags for `<host>/<prefix>/<ns>/<pkg>`
      (`wkg oci` / the OCI tags API), strip an optional `[a-zA-Z]+` prefix,
      keep valid semver tags.
- [ ] `fetch_registry_package`: pull the artifact for a version, read the
      embedded `wado.toml` metadata (the `org.wado-lang.*` / WIT sections
      `publish` writes) into a `Manifest`, and compute the archive `integrity`.
- [ ] Decide the mechanism: shell out to `wkg oci pull` (consistent with
      publish, zero new deps) vs a native OCI client crate. Default to `wkg`
      for symmetry; revisit only if pull needs data `wkg` will not surface.
- [ ] Map registry URL forms (`oci://<host>/<prefix>`) to repository paths per
      the WEP's [Registry backend](./wep-2026-02-14-package-manifest.md) rule.

### Phase 2 — Dependency cache + `wado fetch`

Materialize resolved packages on disk so the compiler can load their source.

- [ ] Cache root: `~/wado/`, overridable by `WADO_ROOT`.
- [ ] ghq-style layout: `{host}/{ns}/{name}/{version}/` for registry,
      `{host}/{owner}/{repo}/{version}-{short-ref}/` for git.
- [ ] Extract the pulled component's Wado source tree (or, for wado-to-wado, the
      provider-metadata source) into the version directory alongside its
      `wado.toml`.
- [ ] `wado fetch`: resolve if no lock, then download every locked package into
      the cache. Idempotent; the CI/Docker caching case in the CLI WEP.
- [ ] Verify `integrity` on every fetch; mismatch aborts.

### Phase 3 — Lock file in the build path

Make `compile` / `run` / `test` lock-aware.

- [ ] On build, load `wado.lock` if present; else resolve and write it.
- [ ] Freshness: compare `deps_hash` against the current manifest; stale →
      auto re-resolve (default) or error under `--locked`.
- [ ] Add `--locked` to build commands (reject stale locks — CI reproducibility).
- [ ] When the lock exists, skip the resolver: read the graph, versions, and
      entry points straight from `wado.lock` (self-sufficient per the WEP).

### Phase 4 — Registry/git deps reach the compiler

Extend the `DependencyIndex` construction beyond path deps and wire
version-aware routing.

- [ ] Extend `dependency_index_from` (or a lock-driven successor) to map a
      registry/git dependency key to its cached entry-module path, producing a
      `ModuleSource::Dependency`.
- [ ] Version-aware routing: `resolve_import(from, spec)` resolves against the
      importing package's own deps, so semver-incompatible versions of one
      package become distinct `Dependency` ids (WEP "Transitive Version
      Isolation"). This is the host's job; the compiler already keys modules by
      resolved path/id.
- [ ] E2E: a fixture project with a registry dependency compiles and runs
      against a mocked/local OCI registry.

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

- M1 (Phases 1–4): a published registry package can be declared in
  `[dependencies]` and compiled/run — the core loop closes.
- M2 (Phase 5): day-to-day dependency editing without hand-writing `wado.toml`.
- M3 (Phases 6–8): git deps, robust resolution, and the remaining WEP surface.

## Open Questions

- Pull mechanism: `wkg oci pull` (symmetry, no new deps) vs a native OCI client
  (finer control, more code). Leaning `wkg`.
- Cached form for wado-to-wado deps: extract source from the component's
  provider metadata, or require a source sidecar at publish time? Affects what
  `publish` must embed.
- Whether `test` should fetch dev-dependencies eagerly or lazily.
  </content>
  </invoke>
