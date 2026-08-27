//! Per-thread snapshot of the stdlib closure's post-`semantics_with_logger`
//! state, so each compile seeds from it instead of re-running loader → analyze →
//! elaborate over the same stdlib AST (~112 ms apiece, ~28 s per `wado test`
//! run on package-gale). Thread-local because [`Semantics`] is `!Send`, and
//! built over an empty entry source so the loader yields the real closure.

use std::cell::{Cell, OnceCell, RefCell};
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use crate::compiler_host::{
    CompilerHost, Diagnostic, GeneratorRequest, GeneratorResponse, GeneratorRunnerError, LogLevel,
    SourceError,
};
use crate::hashmap::{IndexMap, IndexSet};
use crate::loader::ModuleLoader;
use crate::logger::Logger;
use crate::module_source::ModuleSource;
use crate::semantics::{Semantics, semantics_with_logger};
use crate::tir::{TirFunction, TirModule, TypeTable};

thread_local! {
    static SNAPSHOT: OnceCell<Rc<Semantics>> = const { OnceCell::new() };
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

/// Return the current thread's stdlib [`Semantics`] snapshot, building it on
/// first call by driving the loader and [`semantics_with_logger`] over an empty
/// entry source. [`None`] under [`build_snapshot`] itself, where reading the
/// still-being-built cache would re-enter the `OnceCell` initialiser.
///
/// # Panics
/// If the stdlib closure fails to load or analyze — a build-time inconsistency
/// in the shipped stdlib, which is not recoverable.
pub(crate) fn get_or_init_snapshot() -> Option<Rc<Semantics>> {
    if is_building() {
        return None;
    }
    Some(SNAPSHOT.with(|cell| {
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
            Rc::new(build_snapshot())
        })
        .clone()
    }))
}

/// Build the current thread's snapshot now, ahead of the first
/// `semantics_with_logger` call.  Intended for parallel batch drivers (e.g.
/// `wado test`) to amortise the ~120 ms snapshot build across worker
/// threads before any compile work is scheduled, instead of paying
/// the cost on each worker's first compile.
///
/// No-op if the snapshot is already built on this thread, or if
/// called re-entrantly from inside `build_snapshot`.
pub fn prewarm() {
    let _ = get_or_init_snapshot();
}

/// Drive the full loader + `semantics_with_logger` pipeline on an empty
/// entry source.  The loader's implicit-modules pass pulls in
/// `core:prelude` and its transitive closure, matching the stdlib
/// subset every real compile loads.
fn build_snapshot() -> Semantics {
    let host = SnapshotHost;
    let logger = Logger::new(&host, LogLevel::Warn);

    // `wado-compiler` has no async runtime dependency (it must compile
    // to `wasm32-unknown-unknown`, see crate-level `CLAUDE.md`).  The
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
    let sem = semantics_with_logger(load_result, &logger, true);
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
    modules: &'a IndexMap<ModuleSource, crate::ast::Module>,
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
    live: Option<&IndexSet<crate::defs::DefId>>,
    fn_remap: &mut IndexMap<*const RefCell<TirFunction>, Rc<RefCell<TirFunction>>>,
) -> TirModule {
    // A synthesized function declares nothing, so nothing in the graph can name
    // it and it is never dropped.
    let reachable = |f: &Rc<RefCell<TirFunction>>| match (live, f.borrow().def_id) {
        (Some(live), Some(def)) => live.contains(&def),
        _ => true,
    };
    let mut new_module = snap_module.clone();
    new_module.type_table = Rc::clone(fresh_type_table);
    new_module.functions = snap_module
        .functions
        .iter()
        .filter(|rc| reachable(rc))
        .map(|rc| clone_fn_rc(rc, fn_remap))
        .collect();
    new_module.generic_functions = snap_module
        .generic_functions
        .iter()
        .filter(|(_, v)| reachable(v))
        .map(|(k, v)| (k.clone(), clone_fn_rc(v, fn_remap)))
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
        // Surface stdlib diagnostics so a stray error or warning during
        // snapshot construction is visible to the user rather than
        // swallowed behind the `expect` in `build_snapshot`. The
        // `Logger` already filters by `LogLevel`, so anything that
        // reaches us here is worth printing.
        let where_ = diagnostic.span.as_ref().map_or_else(
            || "<stdlib>".to_string(),
            |s| format!("{}:{}:{}", s.file, s.line, s.column),
        );
        eprintln!(
            "stdlib snapshot {}: {} at {}",
            diagnostic.severity, diagnostic.message, where_
        );
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
    use crate::module_source::ModuleSource;

    #[test]
    fn snapshot_builds_and_contains_stdlib_closure() {
        let snap = get_or_init_snapshot().expect("not re-entering the builder");

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
        let snap = get_or_init_snapshot().expect("not re-entering the builder");
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

    #[test]
    fn snapshot_is_cached_per_thread() {
        let a = get_or_init_snapshot().expect("not building");
        let b = get_or_init_snapshot().expect("not building");
        assert!(
            Rc::ptr_eq(&a, &b),
            "second call on same thread must return the cached Rc"
        );
    }
}
