//! Package — per-module compilation context for Wado programs
//!
//! `Package` flows through the early compilation phases (resolve → synthesis →
//! monomorphize → lower → optimize). The link phase then consumes it and
//! produces a [`crate::flat_package::FlatPackage`] for WIR building and codegen.

use crate::builtin_registry::BuiltinRegistry;
use crate::codegen_flags::CodegenFlags;
use crate::component_model::CmInterfaceRegistry;
use crate::elaborator::trait_env::TraitEnv;
use crate::hashmap::{IndexMap, IndexSet};
use crate::loader::WasmAsset;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::symbol::SymbolTable;
use crate::synthesis::effect_dispatch::ResourceWrapperIndex;
use crate::tir::{TirModule, TirTest};
use crate::world_registry::{
    self, CallbackExport, ExportMappings, GuestCallback, WorldInfo, WorldRegistry,
};
use std::sync::Arc;

/// A Wado package in per-module form.
///
/// Contains TIR modules indexed by source, plus analysis metadata. Flows
/// through resolve → synthesis → monomorphize → lower → optimize, then
/// consumed by the link phase to produce a [`crate::flat_package::FlatPackage`].
#[derive(Debug)]
pub struct Package {
    /// The entry module source
    pub entry_module_source: ModuleSource,
    /// All TIR modules indexed by module source
    pub tir_modules: IndexMap<ModuleSource, TirModule>,
    /// Symbol table from analysis phase
    pub symbols: SymbolTable,
    /// Project-wide trait knowledge built once during the resolve phase.
    /// Synthesis, monomorphize, and friends consult this instead of
    /// re-scanning all modules. Held by `Arc` because the elaborator also
    /// keeps a reference (LSP queries reuse the same indices).
    pub(crate) trait_env: Arc<TraitEnv>,
    /// Implicitly imported modules (e.g., core:prelude)
    pub implicit_modules: IndexSet<ModuleSource>,
    /// Module name for the output (derived from filename)
    pub module_name: String,

    /// Registry of WASI imports from lib/wasi/*.wado
    pub cm_interface_registry: Arc<CmInterfaceRegistry>,
    /// Registry of world definitions from lib/wasi/*.wado
    pub world_registry: Arc<WorldRegistry>,
    /// Registry of builtin function signatures from lib/core/builtin.wado
    pub builtin_registry: BuiltinRegistry,

    /// The CM functions called, by `CmFunctionInfo::cm_identifier`.
    pub used_wasi_functions: IndexSet<String>,
    /// When true, strip debug name sections for smaller binary size (-Os)
    pub strip_names: bool,
    /// The `org.wado-lang.coverage` section payload, under `wado test --coverage`
    /// or `--pgo`.
    pub coverage_section: Option<Vec<u8>>,
    /// Fine-grained codegen feature flags from the CLI's `-f <flag>` option.
    /// Threaded unchanged through to the WIR emitter, which consults them to
    /// pick alternative lowerings (e.g. native `array.copy`).
    pub codegen_flags: CodegenFlags,
    /// When true, skip Wasm validation after code generation.
    /// Returns raw bytes even if invalid — useful for debugging codegen.
    pub skip_validation: bool,
    /// Target world fully-qualified name (e.g., "wasi:cli/command", "wasi:http/service")
    pub target_world: String,
    /// Synthesized library world for `wado compile --lib`. `Package::world_registry`
    /// is a `&'static` stdlib singleton that cannot hold a per-package world, so
    /// the library world is carried here (owned) and consumers prefer it over the
    /// static registry via [`Self::active_world_info`]. Mirrors how the synthetic
    /// `test` world is special-cased.
    pub lib_world_info: Option<WorldInfo>,
    /// What each `export(World::name)` in the entry module names.
    pub export_mappings: ExportMappings,
    /// The tests kept as component exports (test world only); the rest are
    /// dropped before adapter synthesis so early DCE removes their bodies.
    pub test_selection: TestSelection,

    /// Maps world export name → adapter function name.
    /// Populated by `synthesis::cm_binding` when export adapters are synthesized.
    /// For example: `"run"` → `"$cm_export__run"`.
    pub export_binding_names: IndexMap<String, String>,
    /// Maps world export name → `post-return` function name, for the sync-lifted
    /// exports that return indirectly. Populated alongside
    /// `export_binding_names`; absent for every other export, which gets no
    /// `post-return` canonical option.
    pub post_return_binding_names: IndexMap<String, String>,
    /// One per closure signature an import takes. Populated by `synthesis::cm_binding`.
    pub callback_exports: Vec<CallbackExport>,
    /// The guest-effect interfaces a provider component satisfies, which no
    /// handler of the program answers.
    pub provided_import_fqs: IndexSet<String>,
    /// One per operation of a guest effect a dependency imports and a handler
    /// of the program answers. Populated by `synthesis::effect_dispatch`, and
    /// given its binding by `synthesis::cm_binding`.
    pub guest_callbacks: Vec<GuestCallback>,

    /// Wasm assets loaded by the loader from
    /// `use ... from "<path>" with { type: "wat"|"wasm" }` declarations.
    ///
    /// Keyed by canonical namespace string (matches `namespace` in
    /// `#[canonical("wasm:<path>", "<export>")]` attributes). Consumed by
    /// the codegen `embed_imported_wasm_modules` pass and (in a follow-up)
    /// by the elaborator, which synthesises Wado declarations from each
    /// asset's exports.
    pub wasm_assets: IndexMap<String, WasmAsset>,

    /// The resource half of the wrapper index the same early pass built, kept
    /// past link for the calls monomorphize has yet to make concrete.
    pub resource_wrappers: ResourceWrapperIndex,

    /// `ModuleSource` interner shared with the elaborator. Synthesis
    /// passes (`cm_binding`, `effect_dispatch`) borrow this when they
    /// need to construct fresh `ModuleSource` values for synthesised
    /// items. Dropped at the `Package → FlatPackage` boundary —
    /// downstream phases (link, monomorphize, lower, optimize, codegen)
    /// only consume existing `ModuleSource` values.
    pub interner: std::rc::Rc<std::cell::RefCell<ModuleSourceInterner>>,
}

/// Which test blocks a test-world compile keeps. The default keeps every one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TestSelection {
    /// `--test-name` substring filters. Matching follows `cargo test`:
    /// case-sensitive substring against the original (lossless) test name.
    /// Empty selects every test, and several combine with OR. Unnamed tests
    /// (`test { … }`) carry no name to match, so they are dropped whenever any
    /// filter is active.
    pub names: Vec<String>,
    /// Substring filters a kept test must not match, as `cargo test --skip`'s.
    /// An unnamed test matches none.
    pub skipped: Vec<String>,
    /// Only the `#[workload]` tests, which `wado test --pgo` records.
    pub workloads_only: bool,
}

impl TestSelection {
    #[must_use]
    pub fn selects(&self, test: &TirTest) -> bool {
        if self.workloads_only && !test.is_workload {
            return false;
        }
        if self.skips(test.name.as_deref()) {
            return false;
        }
        if self.names.is_empty() {
            return true;
        }
        test.name
            .as_deref()
            .is_some_and(|name| self.names.iter().any(|f| name.contains(f.as_str())))
    }

    /// Whether `skipped` leaves out a test named `name`.
    #[must_use]
    pub fn skips(&self, name: Option<&str>) -> bool {
        name.is_some_and(|name| self.skipped.iter().any(|f| name.contains(f.as_str())))
    }
}

impl Package {
    /// Create a new Package from compilation artifacts (before optimization).
    pub fn new(
        entry_module_source: ModuleSource,
        tir_modules: IndexMap<ModuleSource, TirModule>,
        symbols: SymbolTable,
        trait_env: Arc<TraitEnv>,
        implicit_modules: IndexSet<ModuleSource>,
        module_name: String,
        cm_interface_registry: Arc<CmInterfaceRegistry>,
        world_registry: Arc<WorldRegistry>,
        builtin_registry: BuiltinRegistry,
        interner: std::rc::Rc<std::cell::RefCell<ModuleSourceInterner>>,
    ) -> Self {
        Self {
            entry_module_source,
            tir_modules,
            symbols,
            trait_env,
            implicit_modules,
            module_name,
            cm_interface_registry,
            world_registry,
            builtin_registry,
            interner,
            // Usage analysis fields default to empty/false
            used_wasi_functions: IndexSet::default(),
            // Codegen options
            strip_names: false,
            coverage_section: None,
            codegen_flags: CodegenFlags::default(),
            skip_validation: false,
            target_world: "wasi:cli/command".to_string(),
            lib_world_info: None,
            export_mappings: ExportMappings::default(),
            test_selection: TestSelection::default(),
            // CM export adapter mapping
            export_binding_names: IndexMap::default(),
            post_return_binding_names: IndexMap::default(),
            callback_exports: Vec::new(),
            provided_import_fqs: IndexSet::default(),
            guest_callbacks: Vec::new(),
            // Wasm assets loaded from `use _ from "<path>" with { type: ... }`
            wasm_assets: IndexMap::default(),
            resource_wrappers: IndexMap::default(),
        }
    }

    /// Get the entry module TIR.
    pub fn entry_module(&self) -> &TirModule {
        self.tir_modules
            .get(&self.entry_module_source)
            .expect("entry module should exist in TIR modules")
    }

    /// Check if the project targets the synthetic test world.
    ///
    /// When true, test functions are the component's exports and everything
    /// else (including world exports like `run`) is subject to DCE.
    pub fn is_test_world(&self) -> bool {
        self.target_world == world_registry::TEST_WORLD
    }

    /// Whether this package targets a synthesized library world (`wado compile
    /// --lib`). Single source of truth for the fact; gates the freelist
    /// allocator default, the synchronous CM lift, and the static-registry
    /// validation bypass.
    pub fn is_lib_world(&self) -> bool {
        self.lib_world_info.is_some()
    }

    /// The active world for export synthesis and component planning.
    ///
    /// Prefers the synthesized library world (`--lib`) over the `&'static`
    /// stdlib registry, since the latter cannot hold a per-package world.
    pub fn active_world_info(&self) -> Option<&WorldInfo> {
        self.lib_world_info
            .as_ref()
            .or_else(|| self.world_registry.get(&self.target_world))
    }

    /// Check if any function from the given WASI interface is used.
    /// Interface names are like "Stdout", "Stderr", "Environment", etc.
    pub fn has_interface(&self, interface_name: &str) -> bool {
        let prefix = format!("{interface_name}::");
        self.used_wasi_functions
            .iter()
            .any(|f| f.starts_with(&prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::TestSelection;
    use crate::tir::TirTest;
    use crate::token::Span;

    fn test(name: Option<&str>, is_workload: bool) -> TirTest {
        TirTest {
            name: name.map(str::to_string),
            function_name: "$test_0".to_string(),
            line: 1,
            span: Span::default(),
            expect_trap: false,
            is_todo: false,
            timeout_ms: None,
            is_synopsis: false,
            is_workload,
        }
    }

    fn by_names(names: &[&str]) -> TestSelection {
        TestSelection {
            names: names.iter().map(|n| (*n).to_string()).collect(),
            ..TestSelection::default()
        }
    }

    fn selects(selection: &TestSelection, name: Option<&str>) -> bool {
        selection.selects(&test(name, false))
    }

    #[test]
    fn empty_filters_select_everything() {
        assert!(selects(&by_names(&[]), Some("anything")));
        assert!(selects(&by_names(&[]), None));
    }

    #[test]
    fn substring_match_is_case_sensitive() {
        let filters = by_names(&["alpha"]);
        assert!(selects(&filters, Some("alpha addition")));
        assert!(selects(&filters, Some("the alpha test")));
        assert!(!selects(&filters, Some("beta")));
        // Case-sensitive, like `cargo test`.
        assert!(!selects(&filters, Some("ALPHA")));
    }

    #[test]
    fn multiple_filters_are_or() {
        let filters = by_names(&["beta", "gamma"]);
        assert!(selects(&filters, Some("beta one")));
        assert!(selects(&filters, Some("gamma two")));
        assert!(!selects(&filters, Some("alpha")));
    }

    #[test]
    fn unnamed_tests_are_dropped_when_a_filter_is_active() {
        assert!(!selects(&by_names(&["x"]), None));
    }

    #[test]
    fn matches_multibyte_names() {
        let filters = by_names(&["日本語"]);
        assert!(selects(&filters, Some("日本語 ok")));
        assert!(!selects(&filters, Some("english only")));
    }

    #[test]
    fn workloads_only_drops_every_other_test() {
        let workloads = TestSelection {
            workloads_only: true,
            ..TestSelection::default()
        };
        assert!(workloads.selects(&test(None, true)));
        assert!(!workloads.selects(&test(None, false)));
    }

    #[test]
    fn a_skipped_name_drops_the_tests_it_matches() {
        let selection = TestSelection {
            skipped: vec!["fuel".to_string()],
            ..TestSelection::default()
        };
        assert!(!selects(&selection, Some("fuel")));
        assert!(selects(&selection, Some("smoke")));
        assert!(selects(&selection, None));
    }
}
