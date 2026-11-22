//! Per-thread snapshot of a compile's dependency packages, seeded from the
//! stdlib snapshot ([`crate::stdlib_snapshot`]) and served the same way: a
//! compile whose dependency modules are the ones a snapshot was built over,
//! parsed alike, skips elaborating them.
//!
//! A compile reports nothing in a dependency's code but errors, and a snapshot
//! is built only from code that has none, so serving one hides nothing. A build
//! that fails is not used, and the compile reports what it finds.

use std::cell::{Cell, RefCell};
use std::mem::DropGuard;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::affine_move_check::check_affine_moves_semantic;
use crate::ast::{AstIdSpace, Item, declares_extern_handle};
use crate::compiler_host::{
    CompilerHost, DependencyIndex, Diagnostic, ErasedHost, GeneratorRequest, GeneratorResponse,
    GeneratorRunnerError, LogLevel, ModuleDir, ProbeRequest, Severity, SourceError, SourceReader,
};
use crate::compiler_trace;
use crate::elaborator::liveness::LivenessScope;
use crate::hashmap::IndexSet;
use crate::loader::{LoadResult, ModuleLoader};
use crate::logger::Logger;
use crate::module_path::entry_dir_of;
use crate::module_source::ModuleSource;
use crate::name::synthesized_module_path;
use crate::remote::Integrity;
use crate::semantics::{Semantics, semantics_with_logger};
use crate::stdlib_snapshot::reparsed_snapshot_module;

/// The dependency modules a snapshot serves, each with the parse it was built
/// over and what its imports resolved to, in a fixed order.
type Key = Vec<(ModuleSource, AstIdSpace, Vec<ModuleSource>)>;

/// What this thread knows of the dependency modules it last compiled over.
enum Slot {
    /// One compile over them, which builds nothing: a snapshot pays only when a
    /// second compile reuses it, and a one-shot `wado compile` never does.
    Seen(Key),
    /// The snapshot of them, or `None` where none can serve them, so the same
    /// parses do not build again.
    Built(Key, Option<Rc<Semantics>>),
}

thread_local! {
    /// One key at a time: a thread compiles one package's programs in turn.
    static SNAPSHOT: RefCell<Option<Slot>> = const { RefCell::new(None) };
    /// Set while [`build`] runs.
    static BUILDING: Cell<bool> = const { Cell::new(false) };
}

/// True while a dependency snapshot builds. Like the stdlib's, it serves every
/// later program, so nothing built under it may be narrowed to what its
/// synthetic entry reaches.
pub(crate) fn is_building() -> bool {
    BUILDING.with(Cell::get)
}

/// The snapshot of `load`'s dependency modules, built on the second compile of
/// them in a row on this thread. `None` where `load` has none, or where no
/// snapshot can serve them.
pub(crate) async fn dependency_snapshot(
    load: &LoadResult,
    host: &ErasedHost<'_>,
) -> Option<Rc<Semantics>> {
    let key = key_of(load);
    if key.is_empty() {
        return None;
    }
    if let Some((unservable, ..)) = key.iter().find(|(source, ..)| !servable(load, source)) {
        compiler_trace!("dependency_snapshot", "cannot serve {unservable}");
        return None;
    }
    let (slot, snapshot) = match SNAPSHOT.take() {
        Some(Slot::Built(built, snapshot)) if built == key => {
            (Slot::Built(built, snapshot.clone()), snapshot)
        }
        Some(Slot::Seen(seen)) if seen == key => {
            // Built where the compile reads them, it serves the compile unless
            // a module came back as another parse, as one decoded per compile does.
            let snapshot = build(load, host, &key)
                .await
                .filter(|snapshot| reparsed_snapshot_module(snapshot, &load.modules).is_none())
                .map(Rc::new);
            compiler_trace!(
                "dependency_snapshot",
                "{} over {} modules",
                if snapshot.is_some() {
                    "built"
                } else {
                    "refused"
                },
                key.len()
            );
            (Slot::Built(key, snapshot.clone()), snapshot)
        }
        Some(Slot::Built(..) | Slot::Seen(_)) | None => (Slot::Seen(key), None),
    };
    SNAPSHOT.set(Some(slot));
    snapshot
}

/// The modules `load`'s dependency modules reach outside `core:` and `wasi:`,
/// those included, ordered by name.
fn key_of(load: &LoadResult) -> Key {
    let mut reached: IndexSet<&ModuleSource> = load
        .modules
        .keys()
        .filter(|source| matches!(source, ModuleSource::Dependency { .. }))
        .collect();
    let mut next = 0;
    while let Some(source) = reached.get_index(next) {
        next += 1;
        let imported = load.imports.get(*source).map_or(&[][..], Vec::as_slice);
        reached.extend(
            imported
                .iter()
                .filter(|target| !target.is_core() && !target.is_binding()),
        );
    }
    let mut key: Key = reached
        .into_iter()
        .map(|source| {
            let imports = load.imports.get(source).cloned().unwrap_or_default();
            (source.clone(), load.modules[source].ast_id_space(), imports)
        })
        .collect();
    key.sort_by_cached_key(|(source, ..)| source.to_string());
    key
}

/// Whether a snapshot can serve `source`: a module whose parse a later compile
/// takes again, made of its own source alone, declaring no resource a program
/// could extend, since `extends` validation reads the parent's methods from a
/// pass a served module skips.
fn servable(load: &LoadResult, source: &ModuleSource) -> bool {
    let module = &load.modules[source];
    source.keeps_parse()
        && module.include_paths().is_empty()
        && !module
            .items
            .iter()
            .any(|item| matches!(item, Item::Resource(decl) if declares_extern_handle(&decl.attrs)))
}

/// Load the modules `key` names from an empty entry beside `load`'s, so they
/// resolve as `load`'s did, and elaborate them on the stdlib snapshot. `None`
/// where they load other modules or parses, or report an error.
async fn build(load: &LoadResult, host: &ErasedHost<'_>, key: &Key) -> Option<Semantics> {
    let build_host = BuildHost {
        host,
        failed: AtomicBool::new(false),
    };
    let host = ErasedHost::new(&build_host);
    let dir = entry_dir_of(Some(&load.entry_module_source));
    let entry = synthesized_module_path(&dir, "dependencies");
    let roots = key.iter().map(|(source, ..)| source.clone()).collect();
    let loaded = ModuleLoader::new(&host, LogLevel::Off)
        .with_invocations(load.invocations.clone())
        .with_roots(roots)
        .load_all("", Some(&entry))
        .await
        .ok()?;
    if key_of(&loaded) != *key {
        return None;
    }
    BUILDING.with(|building| building.set(true));
    let _guard = DropGuard::new((), |()| BUILDING.with(|building| building.set(false)));
    let logger = Logger::new(&host, LogLevel::Error);
    let semantics = semantics_with_logger(
        loaded,
        &logger,
        true,
        None,
        None,
        &LivenessScope::Compilation,
        None,
        &IndexSet::default(),
    );
    // A compile seeded from the snapshot skips its modules in the move check.
    let clean = semantics.is_complete()
        && !build_host.failed.load(Ordering::Relaxed)
        && check_affine_moves_semantic(&semantics).is_empty();
    clean.then_some(semantics)
}

/// The compile's host, for a snapshot build: it reads what the compile reads,
/// and keeps the diagnostics, which are the compile's to report.
struct BuildHost<'a> {
    host: &'a ErasedHost<'a>,
    failed: AtomicBool,
}

#[deny(clippy::missing_trait_methods)]
impl CompilerHost for BuildHost<'_> {
    fn load_source(&self, path: &str) -> impl Future<Output = Result<Vec<u8>, SourceError>> + Send {
        self.host.load_source(path)
    }

    fn source_exists(&self, path: &str) -> impl Future<Output = bool> + Send {
        self.host.source_exists(path)
    }

    fn open_source(
        &self,
        path: &str,
        integrity: Option<Integrity>,
    ) -> impl Future<Output = Result<Arc<dyn SourceReader>, SourceError>> + Send {
        self.host.open_source(path, integrity)
    }

    fn load_source_prefix(
        &self,
        path: &str,
        extent: Option<u64>,
    ) -> impl Future<Output = Result<Vec<u8>, SourceError>> + Send {
        self.host.load_source_prefix(path, extent)
    }

    fn load_remote(
        &self,
        url: &str,
        extent: Option<u64>,
        integrity: Option<Integrity>,
    ) -> impl Future<Output = Result<Vec<u8>, SourceError>> + Send {
        self.host.load_remote(url, extent, integrity)
    }

    fn load_file(
        &self,
        path: &str,
        extent: Option<u64>,
        integrity: Option<Integrity>,
    ) -> impl Future<Output = Result<Vec<u8>, SourceError>> + Send {
        self.host.load_file(path, extent, integrity)
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        if matches!(diagnostic.severity, Severity::Fatal | Severity::Error) {
            self.failed.store(true, Ordering::Relaxed);
        }
    }

    fn save_internal_artifact(&self, file_stem: &str, bytes: &[u8]) -> Option<String> {
        self.host.save_internal_artifact(file_stem, bytes)
    }

    fn run_generator(
        &self,
        component_wasm: &[u8],
        request: GeneratorRequest,
    ) -> impl Future<Output = Result<GeneratorResponse, GeneratorRunnerError>> + Send {
        self.host.run_generator(component_wasm, request)
    }

    fn probe_generator(
        &self,
        component_wasm: &[u8],
        request: &ProbeRequest,
    ) -> impl Future<Output = Result<Vec<Option<u64>>, GeneratorRunnerError>> + Send {
        self.host.probe_generator(component_wasm, request)
    }

    fn dependency_index(&self) -> DependencyIndex {
        self.host.dependency_index()
    }

    fn serves_core(&self) -> bool {
        self.host.serves_core()
    }

    fn module_dir(&self, path: &str) -> ModuleDir {
        self.host.module_dir(path)
    }

    fn package_index(&self, root: &str) -> DependencyIndex {
        self.host.package_index(root)
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.host.env_var(name)
    }

    fn stdlib_module_read(&self, import_path: &str) {
        self.host.stdlib_module_read(import_path);
    }
}
