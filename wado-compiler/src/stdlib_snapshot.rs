//! Per-thread snapshot of the stdlib closure's post-`semantics_with_logger`
//! state, so each compile seeds from it instead of re-running loader → analyze →
//! elaborate over the same stdlib AST. Thread-local because [`Semantics`] is
//! `!Send`, and built over an empty entry source so the loader yields the real
//! closure.

use std::cell::{Cell, OnceCell, RefCell};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, Waker};

use crate::analyze::Analyzer;
use crate::ast;
use crate::cm_type_decls::CmTypeDecls;
use crate::compiler_host::{
    CompilerHost, DependencyIndex, Diagnostic, GeneratorRequest, GeneratorResponse,
    GeneratorRunnerError, LogLevel, SourceError,
};
use crate::coverage::CoverageScope;
use crate::defs::{DefId, DefKind, DefTable};
use crate::elaborator::liveness::LivenessScope;
use crate::hashmap::{IndexMap, IndexSet};
use crate::loader::{ModuleLoader, cached_stdlib_module};
use crate::logger::Logger;
use crate::module_source::ModuleSource;
use crate::resolve::Resolutions;
use crate::semantics::{Semantics, semantics_with_logger};
use crate::stdlib;
use crate::tir::{TirFunction, TirModule};
use crate::type_table::TypeTable;

thread_local! {
    static SNAPSHOT: OnceCell<Rc<Semantics>> = const { OnceCell::new() };
    /// The snapshot a compile measuring the stdlib seeds from, its stdlib
    /// reified with probes.
    static PROBED_SNAPSHOT: OnceCell<Rc<Semantics>> = const { OnceCell::new() };
    /// Re-entry guard for [`get_or_init_snapshot`].  Set to `true`
    /// while [`build_snapshot`] is running.  The synthetic snapshot
    /// build calls through `semantics_with_logger` (which itself looks up
    /// the snapshot) so we must report "no snapshot yet" to the inner
    /// invocation rather than try to enter the `OnceCell` initialiser
    /// recursively — `OnceCell::get_or_init` panics on re-entry.
    static BUILDING: Cell<bool> = const { Cell::new(false) };
}

/// True while [`build_snapshot`] runs. The cache it produces is consumed by
/// every later compile, so nothing built under it may be narrowed to what the
/// synthetic empty entry happens to reach.
pub(crate) fn is_building() -> bool {
    BUILDING.with(Cell::get)
}

/// Return the current thread's stdlib [`Semantics`] snapshot for a compile
/// measuring `coverage`, building it on first call by driving the loader and
/// [`semantics_with_logger`] over an empty entry source. [`None`] under
/// [`build_snapshot`] itself, where reading the still-being-built cache would
/// re-enter the `OnceCell` initialiser.
///
/// # Panics
/// If the stdlib closure fails to load or analyze — a build-time inconsistency
/// in the shipped stdlib, which is not recoverable.
pub(crate) fn get_or_init_snapshot(coverage: Option<CoverageScope>) -> Option<Rc<Semantics>> {
    if is_building() {
        return None;
    }
    let probed = coverage.filter(|scope| scope.stdlib);
    let slot = if probed.is_some() {
        &PROBED_SNAPSHOT
    } else {
        &SNAPSHOT
    };
    Some(slot.with(|cell| {
        cell.get_or_init(|| {
            // Drop guard so the flag is cleared even if `build_snapshot`
            // panics — otherwise an upstream `catch_unwind` (test
            // harness, fuzzer) would leave `BUILDING` stuck at `true`
            // for the rest of the thread's lifetime, permanently
            // disabling the cache on that thread.
            struct ResetGuard;
            impl Drop for ResetGuard {
                fn drop(&mut self) {
                    BUILDING.with(|c| c.set(false));
                }
            }
            BUILDING.with(|c| c.set(true));
            let _guard = ResetGuard;
            Rc::new(build_snapshot(probed))
        })
        .clone()
    }))
}

/// Build the current thread's snapshot for compiles measuring `coverage` now,
/// ahead of the first `semantics_with_logger` call.  Intended for parallel
/// batch drivers (e.g. `wado test`) to amortise the snapshot build across
/// worker threads before any compile work is scheduled, instead of paying the
/// cost on each worker's first compile.
///
/// No-op if the snapshot is already built on this thread, or if
/// called re-entrantly from inside `build_snapshot`.
pub fn prewarm(coverage: Option<CoverageScope>) {
    let _ = get_or_init_snapshot(coverage);
}

/// Every name `core:prelude` puts in scope in every module, with what it
/// declares, sorted by name.
///
/// # Panics
/// If called from inside [`build_snapshot`].
pub fn prelude_names() -> Vec<(String, DefKind)> {
    let snap =
        get_or_init_snapshot(None).expect("prelude_names is not called while the snapshot builds");
    let resolutions = snap.resolutions().expect("the stdlib snapshot is complete");
    let defs = resolutions.defs();
    let mut names: Vec<(String, DefKind)> = resolutions
        .prelude_names()
        .map(|(name, def)| (name.to_string(), defs.kind(def)))
        .collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    names
}

/// Drive the full loader + `semantics_with_logger` pipeline on an empty
/// entry source.  The loader's implicit-modules pass pulls in
/// `core:prelude` and its transitive closure, matching the stdlib
/// subset every real compile loads. Under `coverage`, the stdlib takes the
/// probes a compile measuring it would.
fn build_snapshot(coverage: Option<CoverageScope>) -> Semantics {
    let host = SnapshotHost;
    let logger = Logger::new(&host, LogLevel::Error);

    // `wado-compiler` has no async runtime dependency (it must compile
    // to `wasm32-unknown-unknown`, see crate-level `AGENTS.md`).  The
    // loader future is driven by hand with a no-op waker: every `await`
    // inside it bottoms out either at a `cached_stdlib_module()` lookup or at
    // `SnapshotHost::load_source` (which returns immediately), so a
    // single poll completes the whole pipeline.  No actual suspension
    // can occur — if `Poll::Pending` ever surfaces it means an
    // unexpected I/O point was introduced into the loader, which is a
    // regression worth catching loudly.
    let load_result = poll_to_completion(async {
        ModuleLoader::new(&host, LogLevel::Off)
            .load_all("", Some("<stdlib-snapshot>"))
            .await
    })
    .expect("stdlib snapshot loader should succeed");

    // The snapshot caches stdlib TIR for batch reuse, so build it.
    let sem = semantics_with_logger(
        load_result,
        &logger,
        true,
        coverage,
        &LivenessScope::Compilation,
    );
    assert!(
        sem.is_complete(),
        "stdlib snapshot should compute semantics cleanly",
    );
    sem
}

/// Drive a future to completion under the assumption that it never
/// truly suspends.  Used to obtain a synchronous result from the
/// loader/semantics pipeline without introducing an async-runtime
/// dependency in `wado-compiler` (which would break the crate's
/// `wasm32-unknown-unknown` build).
///
/// Panics on `Poll::Pending`: the snapshot's loader/host pair never
/// awaits real I/O, so a pending result indicates a regression in the
/// pipeline (e.g. a tokio-specific await snuck into the loader).
fn poll_to_completion<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!(
            "stdlib snapshot future yielded Pending; the snapshot pipeline must not perform real I/O"
        ),
    }
}

/// `ModuleSource` variants whose state is captured by a snapshot.
///
/// Only modules with names that are stable across compiles in the same
/// process can be served from the cache — that is exactly the set of
/// stdlib modules served from `cached_stdlib_module()` plus the `core:libm.wat`
/// Wasm asset module. Returns the subset of `snap.tir_modules` whose
/// keys match.
pub(crate) fn stdlib_sources(snap: &Semantics) -> IndexSet<ModuleSource> {
    snap.tir_modules
        .keys()
        .filter(|ms| {
            matches!(
                ms,
                ModuleSource::Core { .. }
                    | ModuleSource::Binding { .. }
                    | ModuleSource::Wasm { .. }
            )
        })
        .cloned()
        .collect()
}

/// The first module `snap` cached that `modules` carries as a different parse —
/// the condition the snapshot may seed a compile under, its facts keying on
/// `DefId`s only that parse mints (WEP 2026-08-12 §1).
pub(crate) fn reparsed_snapshot_module<'a>(
    snap: &Semantics,
    modules: &'a IndexMap<ModuleSource, ast::Module>,
) -> Option<&'a ModuleSource> {
    let cached = stdlib_sources(snap);
    modules.iter().find_map(|(ms, module)| {
        let reparsed =
            cached.contains(ms) && snap.space_modules.get(&module.ast_id_space()) != Some(ms);
        reparsed.then_some(ms)
    })
}

/// Deep-clone a cached [`TirModule`] for a per-compile pipeline. A naïve `Clone`
/// would only bump the snapshot's shared `Rc`s, letting an optimiser pass
/// corrupt it, so the function `Rc`s are rebuilt fresh — memoised by pointer
/// identity, preserving aliasing within the module — and `type_table` repointed.
/// Embedded `TypeId`s stay valid, the per-compile table being a seeded clone.
///
/// `live` stands in for the gating reify would have applied had the module been
/// reified here: a cached function this program cannot reach is dropped, not
/// cloned.
pub(crate) fn rehydrate_tir_module(
    snap_module: &TirModule,
    fresh_type_table: &Rc<RefCell<TypeTable>>,
    live: Option<&IndexSet<DefId>>,
    fn_remap: &mut IndexMap<*const RefCell<TirFunction>, Rc<RefCell<TirFunction>>>,
) -> TirModule {
    // A synthesized function declares nothing, so nothing in the graph can name
    // it and it is never dropped. Nor is a declaration stating its facts: the
    // call naming it may be one synthesis mints later, and link snapshots it.
    let reachable = |f: &Rc<RefCell<TirFunction>>| {
        let f = f.borrow();
        match (live, f.def_id) {
            (Some(live), Some(def)) => f.declared.is_some() || live.contains(&def),
            _ => true,
        }
    };
    let mut new_module = snap_module.clone();
    new_module.type_table = Rc::clone(fresh_type_table);
    new_module.functions = snap_module
        .functions
        .iter()
        .filter(|rc| reachable(rc))
        .map(|rc| clone_fn_rc(rc, fn_remap))
        .collect();
    new_module
}

fn clone_fn_rc(
    rc: &Rc<RefCell<TirFunction>>,
    remap: &mut IndexMap<*const RefCell<TirFunction>, Rc<RefCell<TirFunction>>>,
) -> Rc<RefCell<TirFunction>> {
    let key: *const RefCell<TirFunction> = Rc::as_ptr(rc);
    if let Some(existing) = remap.get(&key) {
        return existing.clone();
    }
    let fresh = Rc::new(RefCell::new(rc.borrow().clone()));
    remap.insert(key, fresh.clone());
    fresh
}

/// The bundled CM bindings as the loader serves them, and what each type they
/// sign resolves to. The registry reads every binding whatever a compile loads,
/// so the resolver answers them once over all of them.
pub(crate) struct StdlibBindings {
    pub modules: Vec<(&'static str, ast::Module)>,
    pub type_decls: CmTypeDecls,
}

pub(crate) fn stdlib_bindings() -> &'static StdlibBindings {
    static BINDINGS: OnceLock<StdlibBindings> = OnceLock::new();
    BINDINGS.get_or_init(resolve_stdlib_bindings)
}

fn resolve_stdlib_bindings() -> StdlibBindings {
    let paths: Vec<&'static str> = stdlib::all_binding_modules()
        .iter()
        .map(|(path, _)| *path)
        .chain(stdlib::wit_core_submodules().map(|(path, _)| path))
        .collect();
    let mut entry = String::new();
    for (i, path) in paths.iter().enumerate() {
        writeln!(entry, "use binding{i} from \"{path}\";").expect("writing to a String");
    }
    let host = SnapshotHost;
    let logger = Logger::new(&host, LogLevel::Error);
    let load = poll_to_completion(async {
        ModuleLoader::new(&host, LogLevel::Off)
            .load_all(&entry, Some("<stdlib-bindings>"))
            .await
    })
    .expect("every bundled binding loads");
    let mut analyzer = Analyzer::new(&logger).with_interner(Rc::new(RefCell::new(load.interner)));
    analyzer
        .analyze_loaded_modules(
            &load.modules,
            &load.entry_module_source,
            load.implicit_modules,
        )
        .expect("every bundled binding analyzes");
    let symbols = analyzer.into_symbols();
    let defs = Arc::new(DefTable::build(&load.modules, &symbols));
    let resolutions = Resolutions::build(&load.modules, &symbols, defs);
    let modules: Vec<(&'static str, ast::Module)> = paths
        .into_iter()
        .map(|path| {
            let module = cached_stdlib_module(path).expect("a bundled binding is cached");
            (path, module.clone())
        })
        .collect();
    let mut type_decls = CmTypeDecls::default();
    type_decls.bind(
        load.modules.values(),
        modules.iter().map(|(_, module)| module),
        &resolutions,
    );
    StdlibBindings {
        modules,
        type_decls,
    }
}

/// In-memory [`CompilerHost`] used solely for building the stdlib
/// snapshot.  The entry source is empty and every transitively-loaded
/// module is a stdlib module served from `cached_stdlib_module()`, so
/// `load_source` is unreachable in steady state.
struct SnapshotHost;

impl CompilerHost for SnapshotHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        // The snapshot's closure is the stdlib only; the loader resolves
        // those via `cached_stdlib_module()` without touching this method. A
        // call here would mean the loader tried to pull in a non-stdlib
        // module, which is a regression we want surfaced clearly.
        Err(SourceError::NotFound {
            path: path.to_string(),
        })
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        // The stdlib is ours and compiles clean, and the `Logger` above filters
        // below `Error`: anything reaching here is a broken stdlib, not
        // something a user's program can provoke or act on.
        let where_ = diagnostic.span.as_ref().map_or_else(
            || "<stdlib>".to_string(),
            |s| format!("{}:{}:{}", s.file, s.line, s.column),
        );
        panic!(
            "stdlib snapshot {}: {} at {}",
            diagnostic.severity, diagnostic.message, where_
        );
    }

    /// The snapshot is the stdlib itself, shared by every compile, each of
    /// which reports the modules it takes.
    fn stdlib_module_read(&self, _import_path: &str) {}

    /// The stdlib lies in no package a manifest roots.
    fn package_root(&self, _path: &str) -> Option<String> {
        None
    }

    fn package_index(&self, root: &str) -> DependencyIndex {
        unreachable!("no module lies in a package, so none is asked for: {root}")
    }

    async fn run_generator(
        &self,
        _component_wasm: &[u8],
        _request: GeneratorRequest,
    ) -> Result<GeneratorResponse, GeneratorRunnerError> {
        // Stdlib has no Kiln invocations.
        Err(GeneratorRunnerError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component_model::for_each_signed_type;
    use crate::module_source::ModuleSource;

    #[test]
    fn prelude_names_are_the_prelude_tier_and_its_cases() {
        let names = prelude_names();
        let kind_of = |name: &str| names.iter().find(|(n, _)| n == name).map(|(_, k)| *k);
        assert_eq!(kind_of("i32"), Some(DefKind::BuiltinType));
        assert_eq!(kind_of("Option"), Some(DefKind::Variant));
        assert_eq!(kind_of("Some"), Some(DefKind::VariantCase));
        assert_eq!(kind_of("panic"), Some(DefKind::Function));
        assert!(names.is_sorted_by(|a, b| a.0 <= b.0));
    }

    #[test]
    fn snapshot_builds_and_contains_stdlib_closure() {
        let snap = get_or_init_snapshot(None).expect("not re-entering the builder");

        // The implicit-modules pass always pulls in `core:prelude` and
        // its closure.  Verify the snapshot covers them so downstream
        // consumers can rely on the cache hitting for these names.
        let must_have = ["core:prelude", "core:builtin", "core:rt"];
        for name in must_have {
            let found = snap.tir_modules.keys().any(|ms| ms.to_string() == name);
            assert!(
                found,
                "snapshot missing stdlib module {name}: have {:?}",
                snap.tir_modules
                    .keys()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            );
        }

        // The snapshot is constructed from an empty synthetic entry,
        // so we expect at most one `EntryPoint` module (carrying no
        // items); every other entry must come from a stdlib variant.
        // Consumers in later phases filter by these variants when
        // deciding which modules can be served from the cache.
        let mut entry_count = 0;
        for ms in snap.tir_modules.keys() {
            match ms {
                ModuleSource::Core { .. }
                | ModuleSource::Binding { .. }
                | ModuleSource::Wasm { .. } => {}
                ModuleSource::EntryPoint { .. } => entry_count += 1,
                _ => panic!("snapshot contains non-stdlib module: {ms:?}"),
            }
        }
        assert!(
            entry_count <= 1,
            "expected at most one EntryPoint module, got {entry_count}"
        );
    }

    /// The precondition must hold for an ordinary compile, or every compile
    /// silently re-elaborates the stdlib.
    #[test]
    fn a_plain_compile_carries_the_snapshot_parses() {
        let snap = get_or_init_snapshot(None).expect("not re-entering the builder");
        let host = SnapshotHost;
        let load_result = poll_to_completion(async {
            ModuleLoader::new(&host, LogLevel::Off)
                .load_all("fn main() {}", Some("plain.wado"))
                .await
        })
        .expect("loader should succeed");

        assert_eq!(
            reparsed_snapshot_module(&snap, &load_result.modules),
            None,
            "a cached module is being re-parsed per compile"
        );
    }

    /// Every type a binding signs names a declaration; a resource method's
    /// `Self` names the resource.
    #[test]
    fn every_binding_type_resolves() {
        let bindings = stdlib_bindings();
        let mut unbound = Vec::new();
        for (path, module) in &bindings.modules {
            for item in &module.items {
                for_each_signed_type(item, &mut |ty| {
                    ty.for_each(&mut |t| match t {
                        ast::Type::Named(n) if bindings.type_decls.decl_at(n.id).is_none() => {
                            unbound.push(format!("{path}: {}", n.name));
                        }
                        ast::Type::Generic(g) if bindings.type_decls.decl_at(g.id).is_none() => {
                            unbound.push(format!("{path}: {}", g.name));
                        }
                        _ => {}
                    });
                });
            }
        }
        assert!(unbound.is_empty(), "{unbound:?}");
    }

    #[test]
    fn snapshot_is_cached_per_thread() {
        let a = get_or_init_snapshot(None).expect("not building");
        let b = get_or_init_snapshot(None).expect("not building");
        assert!(
            Rc::ptr_eq(&a, &b),
            "second call on same thread must return the cached Rc"
        );
    }
}
