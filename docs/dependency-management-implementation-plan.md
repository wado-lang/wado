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

- [x] `wado fetch`: resolve, then pull each registry component into the shared
      cache. Verified live end to end: `example/hello-packages` runs
      `update` → `fetch` → `run` and round-trips values through the published
      `wado-lang:cm-catalog` component.
- [x] Cache moved to `~/wado/`, overridable by `WADO_ROOT` (`wado-cli::cache`);
      the `build/` bridge is retired for every fetched registry artifact. Library
      components (`dep_component`), Kiln generator components (`build_dep` /
      `kiln_provider`), and `wado fetch` all read and write this one tree, so a
      pre-fetch is a warm cache hit at build time. `build/kiln/generators/` now
      holds only source-compiled (`LocalPath`) generators — a genuine build
      output, not a download.
- [x] ghq-style layout: a library component at
      `{host}/{ns}/{name}/{version}/component.wasm`, a generator at
      `{host}/{ns}/{name}/core-kiln-generator/{version}/component.wasm` (its
      publish world sub-path), so both artifacts of one package share the tree
      without colliding. The registry prefix folds into `{host}/…` via the
      `oci::reference` repository mapping. Git's canonical clone
      `{host}/{owner}/{repo}` with nested `.worktrees/{version}-{short-ref}/`
      waits on the git provider (Phase 6).
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

- [x] Represent a prebuilt-component dependency distinctly from a source
      dependency: `DependencyIndex.components` (coordinate/specifier → cached
      `.wasm`) sits beside `resolved` (path/source deps), and the loader resolves
      a component import to a `ModuleSource::Wasm` composed across the CM boundary.
- [x] Map a registry dependency key to its cached component so `use { … } from
      "ns:pkg"` type-checks against the component's exported interface (WIT
      decoded from the component itself, WEP 2026-06-26).
- [x] Every entry point resolves component imports, not just `build`/`run`:
      `check` and `query` fetch through the shared resolver (offline on a warm
      cache); the `wado lsp` server reads the warm `~/wado/` cache offline via
      `dependency_index_from` (cold cache → an `unresolved` `wado fetch` hint).
      Fixed a latent bug where the Engine's `DiagnosticCollector` dropped the
      host's dependency index entirely (path deps included).
- [ ] `wado lsp` server: resolve a **single-file inline** component source
      (`use … from "ns:pkg@ver" with { registry }`, no `wado.toml`). The server's
      `dependency_index()` builds from the nearest manifest and never sees the
      open document's text, so an inline `with` in a manifest-less script is not
      resolved in the editor — `wado check`/`query` (which parse the source)
      handle it. Needs the server to parse the active document's `use` clauses
      into the index, like the CLI does.
- [ ] Version-aware routing: `resolve_import(from, spec)` resolves against the
      importing package's own deps, so semver-incompatible versions of one
      package become distinct dependency ids (WEP "Transitive Version
      Isolation"). Natural for components (CM instances are type-isolated).
- [x] E2E: `example/hello-packages` depends on `wado-lang:cm-catalog` and builds
      and runs against ghcr (`wado run`), round-tripping values through the
      composed component.

### Phase 5 — Dependency-editing CLI

The manipulation commands from the CLI WEP.

- [ ] `wado add <name> [--package/--version/--registry/--git/--ref/--path/--dev]`.
- [ ] `wado remove <name> [--dev]`.
- [ ] `wado update --pin` / `--breaking` (the base `wado update` exists).
- [ ] `wado list [filter] [--path]` — scans the cache, no project context.
- [ ] `wado exec <dep> [args…]` — run a dependency's command world.

### Phase 6 — Git provider

Designed in detail in the
[git dependency design](./git-dependency-resolution-design.md): a git dep is a
source dependency (compiled in like a path dep), cloned to a ghq-compatible Wado
root with per-version git worktrees, `wado clean` as their GC.

- [x] Parse the git `directory` field onto `DependencySource::Git`.
- [x] Implement the git methods of the CLI provider (`list_git_tags`,
      `resolve_git_ref`, `fetch_git_manifest`) via `git` shell-out
      (`wado-cli/src/git.rs`); removed the `UnsupportedSource { kind: "git" }`
      path in `resolve.rs` and the resolver now traverses a git dep's transitive
      deps.
- [x] Cache git deps under the Wado root as a canonical clone
      `{host}/{owner}/{repo}` with nested worktrees `.worktrees/{version}-{short-ref}/`
      (`git worktree`, per-repo `flock`); resolve the root via `WADO_ROOT` →
      `$XDG_CONFIG_HOME/wado/config.toml` → `~/wado` (config parsed only in
      wado-cli, exported as `WADO_ROOT`, so wasm-facing crates stay TOML-free).
- [x] Wire git deps into `dependency_index_from`; `wado fetch` materializes
      worktrees; added `wado clean`. Verified end to end
      (`tests/git_dependency.rs`): `update` → `fetch` → `run`.
- [x] Auto-materialize git worktrees inside `build`/`run` (like registry
      `fetch_component_dependencies`) so a locked git dep builds without an
      explicit `wado fetch` (`manifest_and_component_index`).
- [ ] Submodules (`--recurse-submodules`) and a bare-mirror/shallow-fetch
      optimization; a git dep in a monorepo subgroup (`host/group/sub/repo`) is
      currently keyed as local.

### Phase 7 — PubGrub resolver

Replace the single-pass resolver with PubGrub for backtracking and precise
conflict errors, once real registries make multi-constraint graphs common.

- [ ] Adopt the `pubgrub` crate; adapt `DependencyProvider` to its interface.
- [ ] Preserve current behavior (highest-compatible, coexisting
      semver-incompatible majors) and add derivation-chain error messages.
- [ ] Cyclic-dependency detection with the WEP's error format.

### Phase 8 — Remaining WEP surface

- [~] Single-file `with { … }` inline dependency source (no `wado.toml`). Done
  for Kiln generators: `generator: { module, version, registry }` supplies a
  registry generator's source inline, so a single `.wado` file consumes a
  published generator with no manifest (Kiln WEP "Single-file mode: inline
  generator source"). Done for registry library components:
  `use { X } from "ns:pkg@ver" with { registry }` fetches the prebuilt
  component inline (exact pin, no lock), keyed by the verbatim specifier;
  an inline `with` for a specifier also in `[dependencies]` is rejected as
  mutually exclusive. A `lib:nick` alias fetches the coordinate named by
  `with { package }` while keeping the nickname as the loader's lookup key,
  in both single-file (`with { package, registry, version }`) and manifest
  (a `lib:nick` `[dependencies]` entry) mode. Done for git sources:
  `use { … } from "<name>" with { git, ref[, directory] }` resolves the ref,
  materializes the worktree, and compiles the git-sourced library into the
  script (source dep, `resolved` map); a `version` range is rejected inline
  (no lock to pin it). Verified end to end
  (`tests/git_dependency.rs::inline_git_source_in_a_single_file_script`).
- [ ] Workspace publish/resolve edge cases beyond what `publish` covers.
- [ ] `wado.lock` integrity extensibility (algorithm prefix already in schema).

#### Registry Kiln generators

> **Redesign (Kiln WEP "Protocol revision 3").** Options move from an opaque
> CBOR `list<u8>` blob to a **typed WIT argument** on `generate`, in each
> generator's own world. This retires the `describe-options` mechanism (the
> options shape is read directly from the generator's component WIT) and the
> whole options-blob subsystem. The `[x]` items below under "Add the
> `describe-options` export" are superseded — the encoder/decoder/synthesis
> landed on this branch and are reverted by the redesign (a shallow wound, all
> within this branch). See the new work list at the end of this subsection.

A Kiln generator can be published (gale is, at
`ghcr.io/wado-lang/gale/core-kiln-generator`), but a project can only consume
one from a **local path** today (`example/hello-packages` uses
`module: "../../../package-gale"`). Consuming a _published_ generator
(`module: "wado-lang:gale"`, a `[build-dependencies]` registry entry) needs:

- [ ] Fetch a dependency's generator at its world sub-path (`<ns>/<pkg>/core-kiln-generator`),
      not just the bare repository — extend `wado fetch` / the provider.
- [~] Run a prebuilt generator component. The driver already runs a generator
  from component bytes (`run_generator(generator.wasm, …)`), so a prebuilt
  component needs no new execution path — only its `OptionsDescriptor`, which
  a prebuilt component carries via `describe-options` rather than source.
  - [x] Decode `describe-options` CBOR → `OptionsDescriptor`
        (`kiln::decode_options_schema`, the inverse of the encoder; exact for
        bool/string/enum/object/`Option`, integer widths coalesce to `i64`).
  - [x] Run a prebuilt component's `describe-options`
        (`kiln_runtime::run_describe_options` +
        `FilesystemCompilerHost::run_describe_options`), sharing the AOT cache.
        Verified end to end: a compiled generator's baked schema decodes back to
        the source descriptor (`kiln_compile::describe_options_roundtrips…`).
  - [ ] Implement `GeneratorModule::Spec("ns:name@ver")` resolution: resolve the
        coordinate against `[build-dependencies]`, fetch the component at its
        world sub-path into `build/`, run `describe-options` for the descriptor,
        return a `ResolvedGenerator`. Needs a seam for the provider to run
        `describe-options` (it has no engine today; the host does) and a live
        registry + republished gale to validate the fetch half.
- [x] Decided how a prebuilt generator carries its options schema: the
      `core:kiln/generator` world gains `describe-options: func() -> list<u8>`
      returning a JSON Schema (Draft 2020-12 subset), CBOR-encoded — shape is
      [Jade](./wep-2026-06-13-jade.md)'s `Schema`, wire is CBOR. Language-agnostic
      (any CM host reads a standard schema); see the
      [Kiln WEP](./wep-2026-04-12-kiln.md) "Options introspection".
- [x] `package-jade` minimal (Jade capability A: the `Schema` document model +
      JSON/CBOR serialize) exists and is a workspace member.
- [ ] Add the `describe-options` export: compiler synthesizes it from the
      source-extracted `OptionsDescriptor` → `package-jade` `Schema` → CBOR; the
      consumer validates options against a decoded schema. Republish gale under
      the new world (existing 0.0.x predate the export).
  - [x] `OptionsDescriptor` → JSON-Schema (Jade shape) → CBOR encoder in Rust
        (`kiln::describe_options::describe_options_cbor`), TDD against the WEP's
        gale example; primitives follow `package-jade`'s constructors, a field
        with a default is optional with `default`, no-default lands in
        `required`, a no-payload `enum` is a string `enum`, `Option<T>` is the
        nullable `type` union.
  - [x] Add `describe-options: func() -> list<u8>` to the `core:kiln/generator`
        world and synthesize its body when compiling a generator-world
        component: inject an `export fn describe_options` stub pre-analysis, then
        patch its `BytesLiteral` with the CBOR schema once the `Options`
        descriptor is extracted. Added a `CmExportType::List` boundary type + a
        pre-interned `list<u8>` for the return. Verified: the schema is baked
        into the component and a generator compiles, validates, and runs
        (`kiln_generator_world`, `kiln_build_dep`).
  - [x] Fix the underlying compiler bug (issue #1523): a sync value-returning CM
        export in an async world generated invalid Wasm. The sync canon lift was
        only wired for `--lib` worlds, so `describe-options` was mis-lifted via
        the async `task.return` path and carried `CanonicalOption::Async` on a
        sync function type. Fix: `sync_lift = !is_async` (canon lift matches the
        function type); route any sync non-`Result` value-returning export to the
        synchronous lift; delete the dead async general adapter. `describe-options`
        stays sync per the WIT — the WIT is not bent around a codegen bug.
  - [ ] Republish gale under the new world (gale's version syncs with the
        workspace, currently 0.0.9).
- [ ] Reconcile the `[build-dependencies]` bare-key deprecation: `module: "gale"`
      resolves only a bare `"gale"` key, but the manifest validator deprecates
      bare keys in favor of coordinates / `lib:` nicknames the lookup rejects.

##### Revision 3 typed-options work list

The redesign replaces the options-blob subsystem with typed WIT arguments:

- [ ] Revert the `describe-options` blob work landed on this branch (encoder,
      decoder, world export + synthesis, `CmExportType::List` if unused
      elsewhere). Keep the sync-lift compiler-bug fix (issue #1523) — it is
      independently correct.
- [ ] Synthesize the generator world with `generate(primary, inputs, options:
      <options-record>)`: lower the generator's `Options` struct to a WIT record
      argument (exact widths, enums, nested records, `option<T>`); drop
      `raw-request` and the CBOR options field. A no-`Options` generator omits
      the argument.
- [ ] Generator-side adapter: assemble `Request<Options>` from the three typed
      arguments instead of `bind_request` + `core:cbor::from_bytes`.
- [ ] Consumer: type-check the user's `options = { … }` against the generator's
      options-record type (from source for path deps, from the component WIT for
      registry deps via WEP 2026-06-26) and lower it through the canonical ABI.
- [ ] Host: invoke `generate` dynamically (wasmtime component `Val` calls) since
      the signature is per-generator; shared `kiln-host` linking is unchanged.
- [ ] Cache key: canonically encode the validated options value as the key
      function only (no wire blob).
- [x] `GeneratorModule::Spec` resolution (`kiln_provider::resolve_spec`): resolve
      the coordinate against `[build-dependencies]` + `[registries]`, pick the
      highest published version matching the requirement, pull the component from
      its `core-kiln-generator` world sub-path (versions listed on the sub-path
      repository — public on GHCR while the bare repo may not be), and recover
      the options descriptor from the component WIT (`kiln_wit`). Cached under
      `build/kiln/generators/` keyed by the spec; a warm cache skips the registry
      round-trip. WIT records carry no field defaults, so a registry generator's
      options fields are all required (no source-level defaults across the
      boundary).
- [x] Republish gale under the new world (0.0.9) and validate
      `example/hello-packages` against the registry: `module: "wado-lang:gale"`
      compiles and runs end to end (`calc::parse("1 + 2 * 3")`).
- [x] Fold `[build-dependencies]` into the lock/fetch path (`wado-cli::build_dep`):
      `wado update` resolves each registry build-dependency at its generator world
      sub-path and writes a `[[build-dependency]]` lock entry with the artifact's
      integrity digest; `deps_hash` now covers `[build-dependencies]`; `wado fetch`
      pre-pulls the generator into `build/kiln/generators/`; and `resolve_spec`
      prefers the locked version (skipping the version listing) and reuses the
      fetched component. Enforcing the recorded integrity on fetch/compile waits on
      the `--locked` / `--offline` reproducibility flags (Phase 3).
- [ ] Carry source-level option defaults across the registry boundary (encode
      them in the component) so an omitted field falls back to the default.
- [x] Reconcile the `module:` specifier forms with `[build-dependencies]` keys.
      `module:` now follows the same rules as a `use ... from` clause: a relative
      path, or a `[build-dependencies]` specifier (open coordinate or `lib:`
      nickname) resolved against the manifest and dispatched on the entry's
      source — a path entry compiles from source, a registry entry pulls a
      prebuilt component. The syntax no longer selects the resolution path (the
      old colon→registry / bare→path split is gone), the `GeneratorModule::Spec`
      / `BuildDep` variants collapsed to one, cache/lock identity keys on the
      resolved coordinate (so a coordinate and a nickname for one package share
      an entry), and a bare `module:` name is rejected, as bare dependency keys
      are.

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
