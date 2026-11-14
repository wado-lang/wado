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
use crate::ast::{AstIdSpace, Item, declares_unrestricted};
use crate::compiler_host::{
    CompilerHost, DependencyIndex, Diagnostic, LogLevel, ModuleDir, Severity, SourceError,
    SourceReader,
};
use crate::elaborator::liveness::LivenessScope;
use crate::loader::{LoadResult, ModuleLoader};
use crate::logger::Logger;
use crate::module_source::ModuleSource;
use crate::name::{INTERNAL_PREFIX, entry_dir_of};
use crate::remote::Integrity;
use crate::semantics::{Semantics, semantics_with_logger};
use crate::stdlib_snapshot::reparsed_snapshot_module;

/// The dependency modules a snapshot serves, each with the parse it was built
/// over, in a fixed order.
type Key = Vec<(ModuleSource, AstIdSpace)>;

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
pub(crate) async fn dependency_snapshot<H: CompilerHost>(
    load: &LoadResult,
    host: &H,
) -> Option<Rc<Semantics>> {
    let key = key_of(load);
    if key.is_empty() || !key.iter().all(|(source, _)| servable(load, source)) {
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
            crate::compiler_trace!(
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

/// The dependency modules `load` holds, ordered by name.
fn key_of(load: &LoadResult) -> Key {
    let mut key: Key = load
        .modules
        .iter()
        .filter(|(source, _)| matches!(source, ModuleSource::Dependency { .. }))
        .map(|(source, module)| (source.clone(), module.ast_id_space()))
        .collect();
    key.sort_by_cached_key(|(source, _)| source.to_string());
    key
}

/// Whether a snapshot can serve the dependency module `source`: what it is
/// made of is its own source and what a snapshot serves, and it declares no
/// resource a program could extend, since `extends` validation reads the
/// parent's methods from a pass a served module skips.
fn servable(load: &LoadResult, source: &ModuleSource) -> bool {
    let module = &load.modules[source];
    module.include_paths().is_empty()
        && !module
            .items
            .iter()
            .any(|item| matches!(item, Item::Resource(decl) if declares_unrestricted(&decl.attrs)))
        && load.imports.get(source).is_none_or(|imported| {
            imported.iter().all(|target| {
                matches!(
                    target,
                    ModuleSource::Dependency { .. }
                        | ModuleSource::Core { .. }
                        | ModuleSource::Binding { .. }
                )
            })
        })
}

/// Load the modules `key` names from an empty entry beside `load`'s, so they
/// resolve as `load`'s did, and elaborate them on the stdlib snapshot. `None`
/// where they load other modules or parses, or report an error.
async fn build<H: CompilerHost>(load: &LoadResult, host: &H, key: &Key) -> Option<Semantics> {
    let host = BuildHost {
        host,
        failed: AtomicBool::new(false),
    };
    let dir = entry_dir_of(Some(&load.entry_module_source));
    let entry = if dir.is_empty() {
        format!("{INTERNAL_PREFIX}dependencies.wado")
    } else {
        format!("{dir}/{INTERNAL_PREFIX}dependencies.wado")
    };
    let roots = key.iter().map(|(source, _)| source.clone()).collect();
    let loaded = ModuleLoader::new(&host, LogLevel::Off)
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
    );
    // A compile seeded from the snapshot skips its modules in the move check.
    let clean = semantics.is_complete()
        && !host.failed.load(Ordering::Relaxed)
        && check_affine_moves_semantic(&semantics).is_empty();
    clean.then_some(semantics)
}

/// The compile's host, for a snapshot build: it reads what the compile reads,
/// and keeps the diagnostics, which are the compile's to report.
struct BuildHost<'a, H> {
    host: &'a H,
    failed: AtomicBool,
}

impl<H: CompilerHost> CompilerHost for BuildHost<'_, H> {
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
