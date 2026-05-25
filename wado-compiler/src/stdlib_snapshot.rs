//! Per-thread snapshot of the stdlib closure's post-`annotate_loaded`
//! state.
//!
//! Every `compile_with_options` invocation that targets non-stdlib user
//! code re-runs the loader → analyze → annotate → `lower_tir` pipeline
//! over the same stdlib AST closure (`core:prelude` and its transitive
//! imports plus `core:libm.wat`). Measurements on
//! `package-gale` (257 compiles, see WEP comments in
//! `wado-cli/src/test.rs`) put the per-compile stdlib portion of
//! `resolve/lower_tir` at ~112 ms — adding up to ~28 s of CPU duplicated
//! across one `wado test` run.
//!
//! [`get_or_init_snapshot`] returns a thread-local [`Annotated`] that
//! has been driven through the same pipeline on a synthetic empty
//! entry source, so the loader's implicit-modules pass produces the
//! exact same closure as a real compile. Per-compile consumers can
//! clone the snapshot's [`TypeTable`], decl maps, [`BuiltinRegistry`],
//! [`TraitEnv`] and pre-lowered [`TirModule`]s as the seed for their
//! own [`AnnotateState`] / `resolve/lower_tir` work and run those
//! passes only over the user modules that come on top.
//!
//! ## Why thread-local
//!
//! [`Annotated`] holds `Rc<RefCell<…>>` for the type table and
//! per-function bodies, so it is `!Send + !Sync`. A process-global
//! `OnceLock<Annotated>` would not type-check. The thread-local
//! `OnceCell` strategy matches how `wado test` schedules compile work
//! (one current-thread tokio runtime per blocking worker thread, with
//! the same worker thread typically handling many sequential compiles
//! from `buffer_unordered`), so each worker pays the snapshot build
//! cost once and amortises it across every subsequent compile on that
//! thread.
//!
//! ## Why an empty entry source
//!
//! Constructing the snapshot via the real `ModuleLoader::load_all`
//! avoids reimplementing the loader's implicit-modules pass and Wasm
//! asset synthesis (notably the `core:libm.wat` `ModuleSource::Wasm`
//! that `core:prelude/primitive.wado` imports). The entry source is
//! empty, so the resulting closure is exactly the stdlib subset every
//! real compile transitively loads.

use std::cell::{Cell, OnceCell, RefCell};
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use crate::annotate::{Annotated, annotate_loaded};
use crate::compiler_host::{
    CompilerHost, Diagnostic, GeneratorRequest, GeneratorResponse, GeneratorRunnerError, LogLevel,
    SourceError,
};
use crate::hashmap::{IndexMap, IndexSet};
use crate::loader::ModuleLoader;
use crate::logger::Logger;
use crate::module_source::ModuleSource;
use crate::tir::{TirFunction, TirModule, TypeTable};

thread_local! {
    static SNAPSHOT: OnceCell<Rc<Annotated>> = const { OnceCell::new() };
    /// Re-entry guard for [`get_or_init_snapshot`].  Set to `true`
    /// while [`build_snapshot`] is running.  The synthetic snapshot
    /// build calls through `annotate_loaded` (which itself looks up
    /// the snapshot) so we must report "no snapshot yet" to the inner
    /// invocation rather than try to enter the `OnceCell` initialiser
    /// recursively — `OnceCell::get_or_init` panics on re-entry.
    static BUILDING: Cell<bool> = const { Cell::new(false) };
}

/// Return the current thread's stdlib [`Annotated`] snapshot.
///
/// On first call the snapshot is built by driving the full loader +
/// [`annotate_loaded`] pipeline over an empty entry source. Returns
/// [`None`] if the current call is itself running underneath
/// [`build_snapshot`] — that is the call path the snapshot builder
/// takes through `annotate_loaded`, and trying to satisfy it from the
/// (still-being-built) cache would re-enter the `OnceCell` initialiser.
///
/// # Panics
///
/// Panics if the stdlib closure fails to load or annotate.  The stdlib
/// is shipped with the compiler and must always compile cleanly; a
/// failure here indicates a build-time inconsistency in the stdlib
/// itself, which is a non-recoverable bug.
pub(crate) fn get_or_init_snapshot() -> Option<Rc<Annotated>> {
    if BUILDING.with(Cell::get) {
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
/// `annotate_loaded` call.  Intended for parallel batch drivers (e.g.
/// `wado test`) to amortise the ~120 ms snapshot build across worker
/// threads before any compile work is scheduled, instead of paying
/// the cost on each worker's first compile.
///
/// No-op if the snapshot is already built on this thread, or if
/// called re-entrantly from inside [`build_snapshot`].
pub fn prewarm() {
    let _ = get_or_init_snapshot();
}

/// Drive the full loader + `annotate_loaded` pipeline on an empty
/// entry source.  The loader's implicit-modules pass pulls in
/// `core:prelude` and its transitive closure, matching the stdlib
/// subset every real compile loads.
fn build_snapshot() -> Annotated {
    let host = SnapshotHost;
    let logger = Logger::new(&host, LogLevel::Warn);

    // `wado-compiler` has no async runtime dependency (it must compile
    // to `wasm32-unknown-unknown`, see crate-level `CLAUDE.md`).  The
    // loader future is driven by hand with a no-op waker: every `await`
    // inside it bottoms out either at a `cached_stdlib()` lookup or at
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

    let annotated = annotate_loaded(load_result, &logger);
    assert!(
        annotated.is_complete(),
        "stdlib snapshot should annotate cleanly",
    );
    annotated
}

/// Drive a future to completion under the assumption that it never
/// truly suspends.  Used to obtain a synchronous result from the
/// loader/annotate pipeline without introducing an async-runtime
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
/// stdlib modules served from `cached_stdlib()` plus the `core:libm.wat`
/// Wasm asset module. Returns the subset of `snap.tir_modules` whose
/// keys match.
pub(crate) fn stdlib_sources(snap: &Annotated) -> IndexSet<ModuleSource> {
    snap.tir_modules
        .keys()
        .filter(|ms| {
            matches!(
                ms,
                ModuleSource::Core { .. } | ModuleSource::Wasi { .. } | ModuleSource::Wasm { .. }
            )
        })
        .cloned()
        .collect()
}

/// Deep-clone a cached [`TirModule`] for use in a per-compile pipeline.
///
/// The snapshot holds [`TirModule`]s whose `Rc<RefCell<TirFunction>>`
/// values are shared across all stdlib modules in the snapshot, and
/// whose `type_table` `Rc<RefCell<TypeTable>>` points at the snapshot's
/// frozen table.  A naïve `Clone` would only bump those `Rc` refcounts,
/// so a per-compile optimiser pass mutating a function body would
/// corrupt the cached snapshot.
///
/// This helper rebuilds the function `Rc`s into fresh allocations,
/// memoising by the source `Rc`'s pointer identity so aliasing within
/// the module (e.g. between `functions` and `generic_functions`) is
/// preserved.  The `type_table` field is repointed to the per-compile
/// shared table; `TypeIds` embedded in function bodies remain valid
/// because the per-compile table is seeded from a clone of the
/// snapshot's table and stdlib entries occupy the same indices.
pub(crate) fn rehydrate_tir_module(
    snap_module: &TirModule,
    fresh_type_table: &Rc<RefCell<TypeTable>>,
    fn_remap: &mut IndexMap<*const RefCell<TirFunction>, Rc<RefCell<TirFunction>>>,
) -> TirModule {
    let mut new_module = snap_module.clone();
    new_module.type_table = Rc::clone(fresh_type_table);
    new_module.functions = snap_module
        .functions
        .iter()
        .map(|rc| clone_fn_rc(rc, fn_remap))
        .collect();
    new_module.generic_functions = snap_module
        .generic_functions
        .iter()
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
/// module is a stdlib module served from `cached_stdlib()`, so
/// `load_source` is unreachable in steady state.
struct SnapshotHost;

impl CompilerHost for SnapshotHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        // The snapshot's closure is the stdlib only; the loader resolves
        // those via `cached_stdlib()` without touching this method. A
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
        let must_have = ["core:prelude", "core:builtin", "core:internal"];
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
                | ModuleSource::Wasi { .. }
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
